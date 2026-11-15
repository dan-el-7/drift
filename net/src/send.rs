//! Sending (PC → phone). The phone is the advertiser, so *it* drives the
//! bandwidth upgrade; we act as the BWU client.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use drift_core::payload::{self, BwuEvent};
use drift_core::proto::connections as nc;
use drift_core::secure;
use drift_core::send::{Action, OutgoingFile, SendSession, State};
use drift_core::ukey2::{ClientHandshake, HandshakeResult, Role};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc};

use crate::channel::{self, BoxRead, BoxWrite, ReaderSwitch, Writer, read_frame, spawn_reader, spawn_writer, write_frame};
use crate::handshake::LocalIdentity;
use crate::health::Health;
use crate::upgrade::{Platform, Rung, Timeouts, Upgrader};

type Medium = nc::connection_request_frame::Medium;
type UpiMedium = nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;

const CHUNK: usize = 512 * 1024;
/// Chunk size on the bootstrap link (Bluetooth, ~200 KB/s). Small, so that at
/// most INFLIGHT × this is buffered when an upgrade hands over: everything
/// already written must drain over Bluetooth before the phone switches.
const SLOW_CHUNK: usize = 32 * 1024;
const INFLIGHT: usize = 8;

pub struct ConnectedOut {
    pub handshake: HandshakeResult,
    /// The receiver is Drift (from its advertisement); set by the caller, used for trust.
    pub peer_drift: bool,
    pub rd: BoxRead,
    pub wr: BoxWrite,
}

impl ConnectedOut {
    pub fn pin(&self) -> String {
        self.handshake.pin()
    }
}

/// Mediums we offer. A receiving phone answers with an UPGRADE_PATH_REQUEST
/// listing the ones we share, so this is what it may ask us to host. Our
/// MediumRole says hotspot *host* only: the PC never joins the phone's hotspot
/// as a station (that would knock it off its own Wi-Fi).
pub fn default_offer() -> Vec<Medium> {
    vec![Medium::WifiHotspot, Medium::WifiDirect, Medium::WifiLan, Medium::Bluetooth]
}

/// Client side of connection setup: ConnectionRequest → UKEY2 → ConnectionResponse.
pub async fn connect<S>(stream: S, local: &LocalIdentity, endpoint_info: &[u8], offer: &[Medium]) -> Result<ConnectedOut>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (rd, wr) = tokio::io::split(stream);
    let (mut rd, mut wr): (BoxRead, BoxWrite) = (Box::new(rd), Box::new(wr));
    let step = |what: &'static str| move |_| anyhow!("timed out waiting for {what}");

    let meta = nc::MediumMetadata {
        supports_5_ghz: Some(true),
        // a Drift receiver uses it to see whether we share one network hop (its hotspot / ours)
        ip_address: crate::upgrade::advertised_ip().map(|i| i.octets().to_vec()),
        medium_role: Some(nc::MediumRole {
            support_wifi_direct_group_owner: Some(true),
            support_wifi_direct_group_client: Some(true),
            support_wifi_hotspot_host: Some(true),
            support_wifi_hotspot_client: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let req = payload::connection_request(&local.endpoint_id_str(), endpoint_info, offer, meta);
    write_frame(&mut wr, &req.encode_to_vec()).await?;

    let (hs, client_init) = ClientHandshake::new();
    write_frame(&mut wr, &client_init).await?;
    let server_init = tokio::time::timeout(Duration::from_secs(15), read_frame(&mut rd)).await.map_err(step("UKEY2 server init"))??;
    let (handshake, client_finish) = hs.handle_server_init(&server_init).context("UKEY2 server init")?;
    write_frame(&mut wr, &client_finish).await?;

    let ours = payload::connection_response_accept(local.os, local.wifi_direct_device_name.as_deref());
    write_frame(&mut wr, &ours.encode_to_vec()).await?;
    let theirs = tokio::time::timeout(Duration::from_secs(30), read_frame(&mut rd)).await.map_err(step("connection response"))??;
    let theirs = nc::OfflineFrame::decode(theirs.as_slice())?;
    let accepted = theirs
        .v1
        .and_then(|v| v.connection_response)
        .and_then(|r| r.response)
        .is_some_and(|r| r == nc::connection_response_frame::ResponseStatus::Accept as i32);
    if !accepted {
        bail!("phone rejected the connection");
    }
    Ok(ConnectedOut { handshake, peer_drift: false, rd, wr })
}

#[derive(Debug)]
pub enum SendEvent {
    /// The receiver is a Drift device and proved its device key in this session (trust).
    PeerKey(drift_core::trust::PeerKey),
    /// Waiting for the user to accept on the phone.
    WaitingForAccept,
    Accepted,
    MediumChanged { medium: &'static str, setup: Duration },
    Progress { sent: u64, total: u64, bytes_per_sec: f64 },
    Declined(String),
    Cancelled,
    Completed,
}

/// BWU client: connect to the path the phone offered, introduce ourselves.
async fn join_path(
    info: nc::bandwidth_upgrade_negotiation_frame::UpgradePathInfo,
    eid: String,
    platform: Arc<dyn Platform>,
    // Some for a Drift receiver: on Wi-Fi Direct it connects out to us instead (offer_reverse)
    reverse: Option<Writer>,
) -> Result<(BoxRead, BoxWrite, bool, &'static str, Option<crate::upgrade::Guard>)> {
    let medium = info.medium.and_then(|m| UpiMedium::try_from(m).ok());
    let encrypted = !info.supports_disabling_encryption.unwrap_or(false);
    let mut guard = None;
    let (sock, label) = match medium {
        // a Drift PC receiving from the Android companion hosts on its Wi-Fi Direct adapter
        Some(UpiMedium::WifiHotspot) => {
            let (s, g) = crate::upgrade::join_hotspot(&*platform, &info).await?;
            guard = Some(g);
            (s, "Hotspot")
        }
        // a Drift phone receiving hosts a Wi-Fi Direct group; we join it on our P2P adapter
        Some(UpiMedium::WifiDirect) if reverse.is_some() => {
            let joined = crate::upgrade::join_group_only(&*platform, &info).await?;
            let (rd, wr) = crate::upgrade::offer_reverse(reverse.as_ref().unwrap(), &joined.local_ip, Duration::from_secs(20)).await?;
            return Ok((rd, wr, encrypted, Rung::PhoneHosted.label(), Some(joined.guard)));
        }
        Some(UpiMedium::WifiDirect) => {
            let (s, g) = crate::upgrade::join_direct(&*platform, &info).await?;
            guard = Some(g);
            (s, Rung::PhoneHosted.label())
        }
        Some(UpiMedium::WifiLan) => {
            let s = info.wifi_lan_socket.ok_or_else(|| anyhow!("no WIFI_LAN socket"))?;
            let ip: [u8; 4] = s.ip_address.as_deref().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("bad LAN ip"))?;
            let addr = (std::net::Ipv4Addr::from(ip), s.wifi_port.unwrap_or(0) as u16);
            (tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(addr)).await.map_err(|_| anyhow!("LAN connect timeout"))??, "Wi-Fi LAN")
        }
        other => bail!("unsupported upgrade medium {other:?}"),
    };
    sock.set_nodelay(true)?;
    crate::channel::tune(&sock);
    let (mut rd, mut wr) = tokio::io::split(sock);
    write_frame(&mut wr, &payload::bwu_client_introduction(&eid).encode_to_vec()).await?;
    if info.supports_client_introduction_ack.unwrap_or(false) {
        let ack = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd)).await.map_err(|_| anyhow!("no intro ack"))??;
        let ack = nc::OfflineFrame::decode(ack.as_slice())?;
        let ok = ack.v1.and_then(|v| v.bandwidth_upgrade_negotiation).and_then(|b| b.event_type)
            == Some(BwuEvent::ClientIntroductionAck as i32);
        if !ok {
            bail!("expected CLIENT_INTRODUCTION_ACK");
        }
    }
    Ok((Box::new(rd), Box::new(wr), encrypted, label, guard))
}

fn upgrade_failure(medium: Option<i32>) -> nc::OfflineFrame {
    payload::bwu(nc::BandwidthUpgradeNegotiationFrame {
        event_type: Some(BwuEvent::UpgradeFailure as i32),
        upgrade_path_info: Some(nc::bandwidth_upgrade_negotiation_frame::UpgradePathInfo { medium, ..Default::default() }),
        ..Default::default()
    })
}

/// Open a file to send. The Android app hands files over as links to its own open descriptors
/// (`/proc/self/fd/N`, from a content URI it was granted); reopening that path goes back through the
/// file's real location, which the app may not be allowed to open (a file another app made), so use
/// the descriptor itself: a duplicate, read from the start.
async fn open_for_send(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    #[cfg(unix)]
    if let Ok(target) = std::fs::read_link(path) {
        if let Some(n) = target.to_str().and_then(|t| t.strip_prefix("/proc/self/fd/")).and_then(|n| n.parse::<i32>().ok()) {
            use std::io::Seek;
            use std::os::fd::BorrowedFd;
            // SAFETY: the app keeps fd N open for the whole send (it closes them when sends end).
            let owned = unsafe { BorrowedFd::borrow_raw(n) }.try_clone_to_owned()?;
            let mut f = std::fs::File::from(owned);
            f.seek(std::io::SeekFrom::Start(0))?;
            return Ok(tokio::fs::File::from_std(f));
        }
    }
    tokio::fs::File::open(path).await
}

/// Medium label for LAN over a hotspot this phone hosts (ffi: "same_hotspot").
pub const SAME_HOTSPOT: &str = "Same hotspot";

/// Stream every file in order with flow control. Returns bytes sent.
async fn stream_files(files: Vec<(PathBuf, OutgoingFile)>, writer: Writer, sent: Arc<AtomicU64>, fast: Arc<AtomicBool>, hold: Arc<AtomicBool>) -> Result<()> {
    let inflight = Arc::new(Semaphore::new(INFLIGHT));
    crate::stats::reset();
    let (t_all, mut all_bytes) = (Instant::now(), 0u64);
    for (path, f) in files {
        let mut file = open_for_send(&path).await.with_context(|| format!("open {}", path.display()))?;
        let mut offset: i64 = 0;
        loop {
            while hold.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let mut buf = vec![0u8; if fast.load(Ordering::Relaxed) { CHUNK } else { SLOW_CHUNK }];
            // Fill the chunk: one read can come back short (Android's storage layer hands out ~128 KB
            // at a time), and every frame costs a seal, an HMAC and a header.
            let mut n = 0;
            while n < buf.len() {
                let t = Instant::now();
                let r = file.read(&mut buf[n..]).await?;
                crate::stats::FILE_READ.add(t);
                if r == 0 {
                    break;
                }
                n += r;
            }
            if n == 0 {
                break;
            }
            buf.truncate(n);
            if !writer.send_data(payload::file_chunk(f.payload_id, f.size, &f.name, offset, buf, false), &inflight).await {
                bail!("connection closed while sending");
            }
            offset += n as i64;
            all_bytes += n as u64;
            sent.fetch_add(n as u64, Ordering::Relaxed);
        }
        if offset != f.size {
            bail!("{} changed size while sending ({} != {})", f.name, offset, f.size);
        }
        writer.send_data(payload::file_chunk(f.payload_id, f.size, &f.name, offset, vec![], true), &inflight).await;
    }
    // Wait until everything queued has actually been written.
    let _ = inflight.acquire_many(INFLIGHT as u32).await;
    crate::stats::report("send pipeline", all_bytes, t_all.elapsed());
    Ok(())
}

/// What to host for a phone's UPGRADE_PATH_REQUEST, in our preference order:
/// Hotspot (the phone joins it as a normal network, works with Windows' GO;
/// Windows rejects Android's fast Wi-Fi Direct join, NOTES.md attempt 4), then
/// Direct, then LAN (both on the same network; slower but never disruptive).
fn rungs_for_request(req: &nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info::UpgradePathRequest) -> Vec<Rung> {
    let wants = |m: UpiMedium| req.mediums.contains(&(m as i32));
    [(UpiMedium::WifiHotspot, Rung::PcHostedHotspot), (UpiMedium::WifiDirect, Rung::PcHostedDirect), (UpiMedium::WifiLan, Rung::Lan)]
        .into_iter()
        .filter(|(m, _)| wants(*m))
        .map(|(_, r)| r)
        .collect()
}

/// Hosting because the receiver asked (a Drift phone): it may have to show Android's "connect to
/// this network?" sheet first, so give a person time to answer.
fn peer_request_timeouts() -> Timeouts {
    Timeouts { hosted: Duration::from_secs(65), ..Timeouts::default() }
}

/// When file data may go on the bootstrap link (F16).
///
/// While an upgrade is being set up, file data stays off Bluetooth: Windows'
/// Bluetooth stack buffers ~0.5 MB we can't shrink, and once we host a network
/// Bluetooth on a shared Wi-Fi/BT chip drops to ~15 KB/s, so every control
/// frame (the offer, LAST_WRITE) would wait seconds behind queued data.
///
/// The hold starts at accept (phones ask for an upgrade ~75 ms later) and lasts
/// as long as any rung is in progress, including the wait for the phone's own
/// UPGRADE_FAILURE (F13), so a fallback rung starts on an empty link. It ends
/// when the upgrade lands, when no upgrade was asked for within `ASK_GRACE`,
/// when every rung failed, or at `MAX` (only if something hangs).
#[derive(Debug, Default)]
struct HoldGate {
    since: Option<Instant>,
    holding: bool,
    small: bool,
}

impl HoldGate {
    const ASK_GRACE: Duration = Duration::from_millis(1500);
    const MAX: Duration = Duration::from_secs(90);
    /// Small sends finish over Bluetooth before an upgrade would help.
    const SMALL: u64 = 256 * 1024;

    fn new(total: u64) -> Self {
        Self { small: total <= Self::SMALL, ..Default::default() }
    }

    /// Begin holding (idempotent). Returns whether data should now be held.
    fn start(&mut self, now: Instant) -> bool {
        if self.small || self.since.is_some() {
            return self.holding;
        }
        self.since = Some(now);
        self.holding = true;
        true
    }

    /// Returns (how long it held, why) once, when the hold ends.
    fn poll(&mut self, now: Instant, upgrading: bool, upgraded: bool) -> Option<(Duration, &'static str)> {
        let since = self.since.filter(|_| self.holding)?;
        let held = now.duration_since(since);
        let why = if upgraded {
            "upgraded"
        } else if !upgrading && held >= Self::ASK_GRACE {
            "no upgrade in progress"
        } else if held >= Self::MAX {
            "hold cap reached"
        } else {
            return None;
        };
        self.holding = false;
        Some((held, why))
    }
}

pub async fn run(
    conn: ConnectedOut,
    local: &LocalIdentity,
    paths: Vec<PathBuf>,
    platform: Arc<dyn Platform>,
    health: Arc<Health>,
    allowed: Option<Vec<Rung>>,
    events: mpsc::UnboundedSender<SendEvent>,
    cancel: Option<Arc<tokio::sync::Notify>>,
) -> Result<()> {
    run_with(conn, local, paths, vec![], platform, health, allowed, events, cancel).await
}

/// `run`, plus links/text to send alongside (or instead of) files.
pub async fn run_with(
    conn: ConnectedOut,
    local: &LocalIdentity,
    paths: Vec<PathBuf>,
    texts: Vec<drift_core::send::OutgoingText>,
    platform: Arc<dyn Platform>,
    health: Arc<Health>,
    allowed: Option<Vec<Rung>>,
    events: mpsc::UnboundedSender<SendEvent>,
    cancel: Option<Arc<tokio::sync::Notify>>,
) -> Result<()> {
    let mut files = vec![];
    for p in paths {
        let meta = tokio::fs::metadata(&p).await.with_context(|| format!("stat {}", p.display()))?;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let f = OutgoingFile::new(&name, meta.len() as i64, drift_core::send::mime_for(&name));
        files.push((p, f));
    }
    let total: u64 = files.iter().map(|(_, f)| f.size as u64).sum();

    let (sealer, opener) = secure::split(&conn.handshake.session_keys(Role::Client));
    let (writer, writer_task) = spawn_writer(conn.wr, sealer);
    let (mut frames, switch): (_, ReaderSwitch) = spawn_reader(conn.rd, opener, None);
    // Hosting side of a role-switched upgrade reuses the receive-side ladder.
    let (mut upgrader, mut hosted_events) =
        Upgrader::new(vec![], platform.clone(), health, peer_request_timeouts(), None, local.endpoint_id_str(), writer.clone(), switch.clone());
    let join_platform = platform;
    upgrader.allow_disruptive(); // only ever runs after the user accepted
    upgrader.set_peer_drift(conn.peer_drift);
    let peer_drift = conn.peer_drift;
    let ka = writer.clone();
    let keepalive = tokio::spawn(async move {
        let mut seq = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            seq = seq.wrapping_add(1);
            ka.send(payload::keep_alive(false, seq));
        }
    });

    let mut session = SendSession::with_texts(files.iter().map(|(_, f)| f.clone()).collect(), texts);
    if let Some(k) = &local.device_key {
        session.set_trust(k.clone(), conn.handshake.auth_string, conn.peer_drift);
    }
    for a in session.start() {
        if let Action::Send(f) = a {
            writer.send(f);
        }
    }

    let sent = Arc::new(AtomicU64::new(0));
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Result<()>>();
    let (up_tx, mut up_rx) = mpsc::unbounded_channel::<Result<(BoxRead, BoxWrite, bool, &'static str, Option<crate::upgrade::Guard>, Duration)>>();
    // Joins of a path the receiver offered, still running: file data stays off Bluetooth meanwhile (F16).
    let mut joins = 0usize;
    // The receiver's address, when it told us (path requests): to tell "on our hotspot" apart.
    let mut peer_lan_ip: Option<std::net::Ipv4Addr> = None;
    // the network we joined for the upgrade, if any: left when the share ends
    let mut _joined: Option<crate::upgrade::Guard> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut window = (Instant::now(), 0u64);
    let mut rate = 0.0f64; // smoothed; chunk-sized steps make the raw rate jumpy
    let fast = Arc::new(AtomicBool::new(false));
    let hold = Arc::new(AtomicBool::new(false));
    let mut gate = HoldGate::new(total);
    // (payload id, size) in send order: what to cancel, and where, if the user cancels.
    let order: Vec<(i64, i64)> = files.iter().map(|(_, f)| (f.payload_id, f.size)).collect();
    let mut files_opt = Some(files);
    let eid = local.endpoint_id_str();

    let mut streamer: Option<tokio::task::AbortHandle> = None;
    let mut peer_key_told = false;
    let result: Result<()> = async {
        loop {
            let actions = tokio::select! {
                _ = async { cancel.as_ref().unwrap().notified().await }, if cancel.is_some() => {
                    tracing::info!("cancelled on the PC");
                    if let Some(s) = streamer.take() { s.abort(); }
                    // Nearby payload cancel for each unfinished file, then the sharing CANCEL.
                    let mut done = sent.load(Ordering::Relaxed) as i64;
                    for &(id, size) in &order {
                        let at = done.clamp(0, size);
                        done -= size;
                        if at < size && session.state() != State::Done {
                            writer.send(payload::payload_canceled(id, size, at));
                        }
                    }
                    let out = session.cancel();
                    if !out.is_empty() { let _ = events.send(SendEvent::Cancelled); }
                    out
                }
                f = frames.recv() => {
                    let frame = match f {
                        Some(Ok(f)) => f,
                        Some(Err(e)) => {
                            if session.state() == State::Sent { return Ok(()); }
                            return Err(anyhow!(e));
                        }
                        None => return Ok(()),
                    };
                    let v1 = frame.v1.as_ref();
                    match payload::frame_type(&frame) {
                        nc::v1_frame::FrameType::KeepAlive => {
                            let k = v1.and_then(|v| v.keep_alive.clone()).unwrap_or_default();
                            if !k.ack.unwrap_or(false) {
                                writer.send(payload::keep_alive(true, k.seq_num.unwrap_or(0)));
                            }
                            vec![]
                        }
                        nc::v1_frame::FrameType::BandwidthUpgradeNegotiation => {
                            let b = v1.and_then(|v| v.bandwidth_upgrade_negotiation.clone()).unwrap_or_default();
                            match BwuEvent::try_from(b.event_type.unwrap_or(0)).unwrap_or(BwuEvent::UnknownEventType) {
                                e @ (BwuEvent::LastWriteToPriorChannel | BwuEvent::SafeToClosePriorChannel) => {
                                    channel::on_peer_bwu_shutdown(&writer, e);
                                }
                                // our hosted group's joiner says where to connect (reverse, Drift peers)
                                BwuEvent::UpgradePathAvailable if upgrader.wants_path() => upgrader.on_peer_frame(&b),
                                BwuEvent::UpgradePathAvailable => {
                                    let info = b.upgrade_path_info.clone().unwrap_or_default();
                                    tracing::info!(medium = ?info.medium, "phone offers upgrade path");
                                    joins += 1;
                                    let (tx, eid, pf, w) = (up_tx.clone(), eid.clone(), join_platform.clone(), writer.clone());
                                    tokio::spawn(async move {
                                        let t0 = Instant::now();
                                        let r = join_path(info, eid, pf, peer_drift.then_some(w)).await.map(|(r, w, e, l, g)| (r, w, e, l, g, t0.elapsed()));
                                        let _ = tx.send(r);
                                    });
                                }
                                BwuEvent::UpgradePathRequest => {
                                    let info = b.upgrade_path_info.clone().unwrap_or_default();
                                    let req = info.upgrade_path_request.clone().unwrap_or_default();
                                    let names: Vec<String> = req.mediums.iter()
                                        .map(|m| UpiMedium::try_from(*m).map(|m| format!("{m:?}")).unwrap_or_else(|_| m.to_string()))
                                        .collect();
                                    tracing::info!(medium = ?info.medium, requested = ?names,
                                        role = ?req.medium_meta_data.as_ref().and_then(|m| m.medium_role.clone()),
                                        "phone asks us to host an upgrade path");
                                    let peer_ip = req.medium_meta_data.as_ref().and_then(|m| m.ip_address.as_deref())
                                        .and_then(|b| <[u8; 4]>::try_from(b).ok()).map(std::net::Ipv4Addr::from);
                                    tracing::info!(?peer_ip, "phone's Wi-Fi address");
                                    if let Some(ip) = peer_ip { upgrader.set_peer_ip(ip); peer_lan_ip = Some(ip); }
                                    let mut rungs = rungs_for_request(&req);
                                    if let Some(a) = &allowed { rungs.retain(|r| a.contains(r)); }
                                    if rungs.is_empty() {
                                        writer.send(upgrade_failure(info.medium));
                                    } else if !upgrader.take_request(rungs) {
                                        tracing::info!("already upgrading; ignoring repeat request");
                                    } else {
                                        if !fast.load(Ordering::Relaxed) && gate.start(Instant::now()) {
                                            hold.store(true, Ordering::Relaxed);
                                        }
                                    }
                                }
                                BwuEvent::UpgradeFailure => upgrader.on_peer_frame(&b),
                                other => tracing::debug!(?other, "bwu frame"),
                            }
                            vec![]
                        }
                        nc::v1_frame::FrameType::BandwidthUpgradeRetry => vec![],
                        nc::v1_frame::FrameType::Disconnection if session.state() == State::Streaming => {
                            // The receiver hangs up right after the last chunk; that can
                            // overtake the streaming task's "done" signal. Settle it first.
                            if let Ok(r) = done_rx.try_recv() {
                                r?;
                                session.all_sent();
                            } else if sent.load(Ordering::Relaxed) >= total {
                                session.all_sent();
                            }
                            session.on_frame(&frame)
                        }
                        _ => session.on_frame(&frame),
                    }
                }
                Some(r) = up_rx.recv() => {
                    joins = joins.saturating_sub(1);
                    match r {
                        Ok((rd, wr, enc, label, guard, setup)) => {
                            _joined = guard;
                            tracing::info!(medium = label, ?setup, encrypted = enc, "upgrade channel ready; handing over");
                            writer.pause_for(wr, enc);
                            writer.send_on_prior(payload::bwu_event(BwuEvent::LastWriteToPriorChannel));
                            switch.switch_to(rd, enc);
                            fast.store(true, Ordering::Relaxed);
                            let _ = events.send(SendEvent::MediumChanged { medium: label, setup });
                        }
                        Err(e) => {
                            tracing::warn!("upgrade failed: {e:#}");
                            writer.send(upgrade_failure(None));
                        }
                    }
                    vec![]
                }
                Some(ev) = hosted_events.recv() => {
                    let setup = match &ev { crate::upgrade::UpgradeEvent::Ready { setup, .. } => Some(*setup), _ => None };
                    // Hosting what the receiver asked for failed before it was told anything (a phone
                    // can't start a group while its hotspot is on): say so now, or it waits ~10 s.
                    let failed_quietly = matches!(&ev, crate::upgrade::UpgradeEvent::Failed { informed: false, .. });
                    let r = upgrader.on_event(ev);
                    if failed_quietly && !upgrader.busy() {
                        writer.send(upgrade_failure(None));
                    }
                    if let Some(rung) = r {
                        fast.store(true, Ordering::Relaxed);
                        // LAN to a laptop on our own hotspot: say so (one hop, not the shared network)
                        let on_ours = rung == Rung::Lan && peer_lan_ip.is_some_and(crate::upgrade::on_my_hotspot);
                        let label = if on_ours { SAME_HOTSPOT } else { rung.label() };
                        let _ = events.send(SendEvent::MediumChanged { medium: label, setup: setup.unwrap_or_default() });
                    }
                    vec![]
                }
                Some(r) = done_rx.recv() => {
                    r?;
                    session.all_sent();
                    vec![]
                }
                _ = tick.tick() => {
                    if let Some((held, why)) = gate.poll(Instant::now(), upgrader.busy() || joins > 0, fast.load(Ordering::Relaxed)) {
                        hold.store(false, Ordering::Relaxed);
                        tracing::info!(?held, why, "resuming file data");
                    }
                    if session.state() == State::Streaming {
                        let s = sent.load(Ordering::Relaxed);
                        let dt = window.0.elapsed().as_secs_f64().max(1e-3);
                        rate = 0.8 * rate + 0.2 * ((s - window.1) as f64 / dt);
                        let bps = rate;
                        window = (Instant::now(), s);
                        let _ = events.send(SendEvent::Progress { sent: s, total, bytes_per_sec: bps });
                    }
                    vec![]
                }
            };
            if !peer_key_told && let Some(k) = session.peer_key() {
                peer_key_told = true;
                let _ = events.send(SendEvent::PeerKey(k.clone()));
            }
            for a in actions {
                match a {
                    Action::Send(f) => {
                        writer.send(f);
                        if session.state() == State::AwaitingResponse {
                            let _ = events.send(SendEvent::WaitingForAccept);
                        }
                    }
                    Action::StartStreaming => {
                        let _ = events.send(SendEvent::Accepted);
                        if !fast.load(Ordering::Relaxed) && gate.start(Instant::now()) {
                            hold.store(true, Ordering::Relaxed);
                        }
                        let (w, s, d, fa, ho) = (writer.clone(), sent.clone(), done_tx.clone(), fast.clone(), hold.clone());
                        let fs = files_opt.take().unwrap_or_default();
                        streamer = Some(tokio::spawn(async move {
                            let _ = d.send(stream_files(fs, w, s, fa, ho).await);
                        }).abort_handle());
                    }
                    Action::Declined(status) => {
                        let _ = events.send(SendEvent::Declined(format!("{status:?}")));
                    }
                    Action::Cancelled => {
                        let _ = events.send(SendEvent::Cancelled);
                    }
                    Action::Completed => {
                        let _ = events.send(SendEvent::Progress { sent: total, total, bytes_per_sec: 0.0 });
                        let _ = events.send(SendEvent::Completed);
                        return Ok(());
                    }
                    Action::Disconnect => {
                        writer.send(payload::disconnection());
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;

    keepalive.abort();
    drop(upgrader); // holds a writer clone; drop it so the writer can finish
    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(3), writer_task).await;
    result
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    const BIG: u64 = 20_000_000;

    #[test]
    fn holds_through_a_slow_fallback_until_the_upgrade_lands() {
        let t0 = Instant::now();
        let mut g = HoldGate::new(BIG);
        assert!(g.start(t0));
        // First rung fails after 21 s, phone's failure arrives, next rung starts:
        // the upgrader stays busy the whole time, so we keep holding.
        assert_eq!(g.poll(t0 + Duration::from_secs(21), true, false), None);
        assert_eq!(g.poll(t0 + Duration::from_secs(45), true, false), None);
        assert_eq!(g.poll(t0 + Duration::from_secs(46), true, true).map(|r| r.1), Some("upgraded"));
        assert_eq!(g.poll(t0 + Duration::from_secs(47), true, true), None, "reports once");
    }

    #[test]
    fn releases_when_nothing_is_asked_or_everything_failed() {
        let t0 = Instant::now();
        let mut g = HoldGate::new(BIG);
        g.start(t0);
        assert_eq!(g.poll(t0 + Duration::from_millis(500), false, false), None, "grace for the phone's request");
        assert_eq!(g.poll(t0 + HoldGate::ASK_GRACE, false, false).map(|r| r.1), Some("no upgrade in progress"));
    }

    #[test]
    fn capped_if_an_upgrade_hangs() {
        let t0 = Instant::now();
        let mut g = HoldGate::new(BIG);
        g.start(t0);
        assert_eq!(g.poll(t0 + HoldGate::MAX, true, false).map(|r| r.1), Some("hold cap reached"));
    }

    #[test]
    fn small_sends_never_hold_and_start_is_idempotent() {
        let t0 = Instant::now();
        assert!(!HoldGate::new(1000).start(t0));
        let mut g = HoldGate::new(BIG);
        assert!(g.start(t0));
        assert!(g.start(t0 + Duration::from_secs(5)), "second start keeps the first deadline");
        assert_eq!(g.poll(t0 + HoldGate::MAX, true, false).map(|r| r.1), Some("hold cap reached"));
    }
}
