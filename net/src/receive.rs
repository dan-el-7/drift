//! Drive one incoming share: session state machine + file output + upgrade ladder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use drift_core::payload::{self, BwuEvent};
use drift_core::proto::connections as nc;
use drift_core::secure;
use drift_core::sharing::{Action, Event, IncomingFile, IncomingText, ReceiveSession, State};
use drift_core::ukey2::Role;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use crate::channel::{self, spawn_reader, spawn_writer};
use crate::handshake::{Connected, LocalIdentity};
use crate::health::Health;
use crate::upgrade::{Platform, Rung, Timeouts, Upgrader};

#[derive(Clone, Debug)]
pub struct ReceiveConfig {
    pub download_dir: PathBuf,
    /// Upgrade rungs, in order. Empty = stay on the bootstrap medium.
    pub ladder: Vec<Rung>,
    /// See `channel::spawn_reader` (coex workaround when bootstrapped over Bluetooth).
    pub hold_after_switch: Option<Duration>,
    /// Shared across connections (failure catalogue F6).
    pub health: Arc<Health>,
    pub timeouts: Timeouts,
    /// F4: an upgraded link that carries no file data for this long while a
    /// file is incomplete is declared stalled.
    pub stall_timeout: Duration,
    /// Notified when the user cancels on the PC: the phone is told (sharing CANCEL) and the
    /// connection closes cleanly.
    pub cancel: Option<Arc<tokio::sync::Notify>>,
}

/// What the connection is currently running over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Medium {
    /// The medium the phone connected on (Bluetooth, or LAN via mDNS).
    Bootstrap,
    Upgraded(Rung),
    /// LAN over a hotspot one of us hosts (a laptop on the phone's hotspot): one hop, its own name.
    SameHotspot,
}

pub enum ReceiveEvent {
    /// The phone wants to send. Reply on `decision` (true = accept).
    /// `drift`: the sender is Drift. `peer_key`: its device key, proven in this session (trust).
    Request { peer: String, pin: String, files: Vec<IncomingFile>, texts: Vec<IncomingText>, drift: bool, peer_key: Option<drift_core::trust::PeerKey>, decision: oneshot::Sender<bool> },
    MediumChanged { medium: Medium, setup: Option<Duration> },
    Progress { received: u64, total: u64, bytes_per_sec: f64 },
    Text { item: IncomingText, content: String },
    FileSaved { path: PathBuf, bytes: u64 },
    Completed,
    Declined,
    Cancelled,
    /// F4: the upgraded link stopped carrying data. The partial file is kept;
    /// the medium is benched so a retry uses the next rung.
    Stalled { medium: Medium, after: Duration },
}

struct OpenFile {
    part: PathBuf,
    dest: PathBuf,
    file: tokio::io::BufWriter<tokio::fs::File>,
    bytes: u64,
    started: Instant,
}

fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base.chars().map(|c| if c.is_control() || r#"<>:"|?*"#.contains(c) { '_' } else { c }).collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() { "file".into() } else { cleaned }
}

/// The receive ladder for this peer: for a Drift peer, the phone-hosted Wi-Fi Direct link, then LAN,
/// then whatever else the configured ladder has.
pub(crate) fn drift_ladder(configured: &[Rung], peer: Option<&drift_core::endpoint::EndpointInfo>, single_hop: bool) -> Vec<Rung> {
    let Some(info) = peer.filter(|i| i.drift_version().is_some()) else { return configured.to_vec() };
    // The phone's Wi-Fi Direct group first, then LAN hosted by the phone (fails silently where the
    // network keeps devices apart), then the rest. The phone hosts both: it can take inbound
    // connections without a firewall change. Tried LAN first with Direct taking over (Upgrader::
    // improvable, still supported): a busy LAN link starves the Direct setup (Windows holds its
    // scans while the adapter is busy; the phone barely listens), so 200 MB stayed on LAN at
    // 9.8 MB/s (21 s) instead of ~11 s via Direct. NOTES.md.
    let (lan_first, direct) = if cfg!(target_os = "android") {
        (Some(Rung::Lan), Some(Rung::PcHostedDirect)) // we are the phone: host LAN, then our group
    } else if info.device_type == drift_core::endpoint::DeviceType::Phone {
        (Some(Rung::PeerHostedLan), Some(Rung::PhoneHosted))
    } else {
        (configured.contains(&Rung::Lan).then_some(Rung::Lan), None)
    };
    let mut front: Vec<Rung> = if single_hop { lan_first.into_iter().chain(direct).collect() } else { direct.into_iter().chain(lan_first).collect() };
    // No prompts between Drift devices: the hotspot rungs make the phone ask before it joins a network
    // (Android's sheet). If Wi-Fi Direct and LAN both fail, the share stays on Bluetooth.
    let rest: Vec<Rung> = configured
        .iter()
        .copied()
        .filter(|r| !front.contains(r) && !matches!(r, Rung::PcHostedHotspot | Rung::PeerHostedHotspot))
        .collect();
    front.extend(rest);
    front
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() && !dir.join(format!("{name}.part")).exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists() && !PathBuf::from(format!("{}.part", p.display())).exists())
        .unwrap()
}

/// Run a connected share to completion. Returns when the phone disconnects,
/// the share finishes, or an error occurs.
pub async fn run(
    conn: Connected,
    local: &LocalIdentity,
    cfg: &ReceiveConfig,
    platform: Arc<dyn Platform>,
    events: mpsc::UnboundedSender<ReceiveEvent>,
) -> Result<()> {
    let pin = conn.pin();
    let peer = conn.peer.clone();
    let conn_auth = conn.handshake.auth_string;
    let (sealer, opener) = secure::split(&conn.handshake.session_keys(Role::Server));
    let (writer, writer_task) = spawn_writer(conn.wr, sealer);
    let (mut frames, switch) = spawn_reader(conn.rd, opener, cfg.hold_after_switch);

    // Keep-alives: Android drops connections that stay silent.
    let ka_writer = writer.clone();
    let keepalive = tokio::spawn(async move {
        let mut seq = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            seq = seq.wrapping_add(1);
            ka_writer.send(payload::keep_alive(false, seq));
        }
    });

    let platform_for_cleanup = platform.clone();
    // Drift to Drift: the phone hosts a Wi-Fi Direct group first (both keep their Wi-Fi; no prompt
    // after the first pairing; ~35 MB/s phone to PC here vs ~15 on LAN, NOTES.md), then the shared
    // network, then the rest. A phone hosts it itself (offered as WIFI_DIRECT); a PC receiving from
    // a Drift phone asks the phone to host (role switch).
    // One hop between us (a laptop on the phone's hotspot): plain LAN is the fastest link there is.
    let single_hop = peer.ip.is_some_and(|ip| crate::upgrade::on_my_hotspot(ip) || platform.internet_gateway() == Some(ip));
    if single_hop {
        tracing::info!(peer_ip = ?peer.ip, "on the same hotspot as the peer: LAN first");
    }
    let ladder = drift_ladder(&cfg.ladder, peer.info.as_ref(), single_hop);
    let (mut upgrader, mut upgrade_events) =
        Upgrader::new(
            ladder,
            platform,
            cfg.health.clone(),
            cfg.timeouts,
            peer.ip,
            local.endpoint_id_str(),
            writer.clone(),
            switch,
        );
    upgrader.set_peer_drift(peer.info.as_ref().and_then(|i| i.drift_version()).is_some());
    upgrader.set_single_hop(single_hop);
    // Upgrade early: non-disruptive rungs start now, in parallel with the accept prompt.
    upgrader.advance();

    let mut session = ReceiveSession::new();
    let peer_drift = peer.info.as_ref().and_then(|i| i.drift_version()).is_some();
    if let Some(k) = &local.device_key {
        session.set_trust(k.clone(), conn_auth, peer_drift);
    }
    let mut decision: Option<oneshot::Receiver<bool>> = None;
    let mut open: HashMap<i64, OpenFile> = HashMap::new();
    let mut total: u64 = 0;
    let mut received: u64 = 0;
    let mut window = (Instant::now(), 0u64);
    // F4 stall watchdog: last time file bytes arrived while a file was in progress.
    let mut last_data = Instant::now();
    let mut watchdog = tokio::time::interval(Duration::from_millis(500));
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_report = Instant::now();

    let result: Result<()> = async {
        loop {
            let actions: Vec<Action> = tokio::select! {
                f = frames.recv() => {
                    let frame = match f {
                        Some(Ok(f)) => f,
                        Some(Err(e)) => {
                            if matches!(session.state(), State::Done | State::Rejected) { return Ok(()); }
                            return Err(anyhow!(e));
                        }
                        None => return Ok(()),
                    };
                    let v1 = frame.v1.as_ref();
                    match payload::frame_type(&frame) {
                        nc::v1_frame::FrameType::KeepAlive => {
                            let ka = v1.and_then(|v| v.keep_alive.clone()).unwrap_or_default();
                            if !ka.ack.unwrap_or(false) {
                                writer.send(payload::keep_alive(true, ka.seq_num.unwrap_or(0)));
                            }
                            vec![]
                        }
                        nc::v1_frame::FrameType::BandwidthUpgradeNegotiation => {
                            let b = v1.and_then(|v| v.bandwidth_upgrade_negotiation.clone()).unwrap_or_default();
                            match BwuEvent::try_from(b.event_type.unwrap_or(0)) {
                                Ok(e @ (BwuEvent::LastWriteToPriorChannel | BwuEvent::SafeToClosePriorChannel)) => {
                                    channel::on_peer_bwu_shutdown(&writer, e);
                                }
                                _ => upgrader.on_peer_frame(&b),
                            }
                            vec![]
                        }
                        nc::v1_frame::FrameType::BandwidthUpgradeRetry => {
                            upgrader.on_peer_retry();
                            vec![]
                        }
                        _ => session.on_frame(&frame)?,
                    }
                }
                _ = watchdog.tick() => {
                    // The bootstrap (Bluetooth) is slow but steady (~3.4 s per 512 KB
                    // chunk), so it gets a 4x longer limit than an upgraded link.
                    let limit = if upgrader.active.is_some() { cfg.stall_timeout } else { cfg.stall_timeout * 4 };
                    let stalled = session.state() == State::Receiving
                        && !open.is_empty()
                        && last_data.elapsed() >= limit;
                    if stalled {
                        let medium = upgrader.active.map(Medium::Upgraded).unwrap_or(Medium::Bootstrap);
                        tracing::warn!(?medium, after = ?last_data.elapsed(), "link stalled");
                        upgrader.on_stall();
                        let _ = events.send(ReceiveEvent::Stalled { medium, after: last_data.elapsed() });
                        // Keep the .part file for resume (don't delete it below).
                        for (_, of) in open.drain() {
                            drop(of.file);
                        }
                        return Err(anyhow!("link stalled"));
                    }
                    vec![]
                }
                Some(ev) = upgrade_events.recv() => {
                    // Setup time is only known on success; capture it before handing over.
                    let setup = match &ev { crate::upgrade::UpgradeEvent::Ready { setup, .. } => Some(*setup), _ => None };
                    if let Some(rung) = upgrader.on_event(ev) {
                        let medium = if single_hop && matches!(rung, Rung::Lan | Rung::PeerHostedLan) { Medium::SameHotspot } else { Medium::Upgraded(rung) };
                        let _ = events.send(ReceiveEvent::MediumChanged { medium, setup });
                    }
                    vec![]
                }
                _ = async { cfg.cancel.as_ref().unwrap().notified().await }, if cfg.cancel.is_some() => {
                    tracing::info!("cancelled on the PC");
                    decision = None;
                    session.cancel_local()
                }
                d = async { decision.as_mut().unwrap().await }, if decision.is_some() => {
                    decision = None;
                    if d.unwrap_or(false) {
                        upgrader.allow_disruptive();
                        session.accept()
                    } else {
                        session.reject()
                    }
                }
            };

            for a in actions {
                match a {
                    Action::Send(f) => writer.send(f),
                    Action::Event(Event::Introduction { files, texts }) => {
                        total = files.iter().map(|f| f.size.max(0) as u64).sum::<u64>() + texts.iter().map(|t| t.size.max(0) as u64).sum::<u64>();
                        let (tx, rx) = oneshot::channel();
                        decision = Some(rx);
                        let _ = events.send(ReceiveEvent::Request { peer: peer.name(), pin: pin.clone(), files, texts, drift: peer_drift, peer_key: session.peer_key().cloned(), decision: tx });
                    }
                    Action::FileOpen(f) => {
                        tokio::fs::create_dir_all(&cfg.download_dir).await?;
                        let dest = unique_path(&cfg.download_dir, &sanitize(&f.name));
                        let part = PathBuf::from(format!("{}.part", dest.display()));
                        let file = tokio::fs::File::create(&part).await.with_context(|| format!("create {}", part.display()))?;
                        crate::stats::reset();
                        open.insert(f.payload_id, OpenFile { part, dest, file: tokio::io::BufWriter::with_capacity(1 << 20, file), bytes: 0, started: Instant::now() });
                    }
                    Action::FileWrite { payload_id, data, .. } => {
                        let of = open.get_mut(&payload_id).ok_or_else(|| anyhow!("write to unopened file"))?;
                        let t = Instant::now();
                        of.file.write_all(&data).await?;
                        crate::stats::DISK_WRITE.add(t);
                        of.bytes += data.len() as u64;
                        received += data.len() as u64;
                        last_data = Instant::now();
                    }
                    Action::FileClose { payload_id } => {
                        if let Some(mut of) = open.remove(&payload_id) {
                            of.file.flush().await?;
                            crate::stats::report("receive pipeline", of.bytes, of.started.elapsed());
                            drop(of.file);
                            tokio::fs::rename(&of.part, &of.dest).await?;
                            let _ = events.send(ReceiveEvent::FileSaved { path: of.dest, bytes: of.bytes });
                        }
                    }
                    Action::FileAbort { payload_id } => {
                        if let Some(of) = open.remove(&payload_id) {
                            drop(of.file);
                            let _ = tokio::fs::remove_file(&of.part).await;
                        }
                    }
                    Action::Event(Event::FileProgress { .. }) => {
                        if last_report.elapsed() >= Duration::from_millis(250) {
                            let dt = window.0.elapsed().as_secs_f64().max(1e-3);
                            let bps = (received - window.1) as f64 / dt;
                            window = (Instant::now(), received);
                            last_report = Instant::now();
                            let _ = events.send(ReceiveEvent::Progress { received, total, bytes_per_sec: bps });
                        }
                    }
                    Action::Event(Event::Text { item, content }) => {
                        received += content.len() as u64;
                        let _ = events.send(ReceiveEvent::Text { item, content });
                    }
                    Action::Event(Event::Completed) => {
                        let _ = events.send(ReceiveEvent::Progress { received, total, bytes_per_sec: 0.0 });
                        let _ = events.send(ReceiveEvent::Completed);
                    }
                    Action::Event(Event::Cancelled) => {
                        let _ = events.send(ReceiveEvent::Cancelled);
                        return Ok(());
                    }
                    Action::Disconnect => {
                        if session.state() == State::Rejected {
                            let _ = events.send(ReceiveEvent::Declined);
                        }
                        // The phone shows "sent" once the receiver disconnects.
                        writer.send(payload::disconnection());
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;

    keepalive.abort();
    let was_joined = upgrader.active == Some(Rung::PhoneHosted);
    // Abandoned partial files.
    for (_, of) in open.drain() {
        drop(of.file);
        let _ = tokio::fs::remove_file(&of.part).await;
    }
    drop(upgrader);
    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(3), writer_task).await;
    // F5: a P2P-client session can leave a default route behind. Clean up only after we've left the
    // group (dropping the upgrader closes the link and its live route goes with it).
    if was_joined {
        let p = platform_for_cleanup.clone();
        match tokio::task::spawn_blocking(move || p.cleanup_routes()).await {
            Ok(Ok(n)) if n > 0 => tracing::info!(n, "removed stale Wi-Fi Direct routes"),
            Ok(Err(e)) => tracing::warn!("route cleanup failed: {e:#}"),
            _ => {}
        }
    }
    result
}
