//! Bandwidth-upgrade ladder (receiver = BWU initiator).
//!
//! Each rung is one attempt to move the connection off its bootstrap medium.
//! A failed attempt (timeout, error, or the phone's UPGRADE_FAILURE) moves to
//! the next rung, so a transfer only stays on Bluetooth when nothing faster
//! works. Platform-specific pieces (hosting/joining Wi-Fi Direct groups,
//! firewall) come from a [`Platform`] implementation.

use std::any::Any;
use std::collections::VecDeque;
use std::future::Future;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use drift_core::payload::{self, BwuEvent};
use drift_core::proto::connections as nc;
use prost::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::channel::{BoxRead, BoxWrite, ReaderSwitch, Writer, read_frame, write_frame};
use crate::health::Health;

type Upi = nc::bandwidth_upgrade_negotiation_frame::UpgradePathInfo;
type UpiMedium = nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium;
type WifiDirectCredentials = nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info::WifiDirectCredentials;

/// One way to get off Bluetooth, in the order we try them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rung {
    /// PC hosts a legacy Wi-Fi Direct group; phone joins it as a normal network.
    /// Fast and the PC stays online; a phone without STA+STA concurrency briefly
    /// leaves its own Wi-Fi.
    PcHostedHotspot,
    /// Same PC-hosted group, offered as WIFI_DIRECT (phone joins on p2p0).
    /// Windows rejects Android's fast P2P join today; kept for drivers that allow it.
    PcHostedDirect,
    /// Role switch: phone hosts a Wi-Fi Direct group, PC joins as P2P client.
    /// Both stay online, but pairing may prompt on the phone.
    PhoneHosted,
    /// Plain TCP over the shared network.
    Lan,
    /// Drift peers, first stage: we (the receiver, a PC) ask the sender (a Drift phone) to open a LAN
    /// socket and connect out to it (comes up in ~0.1 s; our firewall isn't involved). Provisional:
    /// the Wi-Fi Direct rung after it still runs and takes over when ready. Fails silently.
    PeerHostedLan,
    /// Role switch for Drift peers: we (the receiver) ask the sender to host a hotspot and join it
    /// as a plain client. For the Android companion receiving from a Drift PC: the PC keeps its
    /// own Wi-Fi (it hosts on its Wi-Fi Direct adapter), the phone joins like any network.
    PeerHostedHotspot,
}

impl Rung {
    /// Rungs that may take the phone off its own Wi-Fi: only start them after
    /// the user accepted, so a declined share costs nothing.
    pub fn disrupts_phone(self) -> bool {
        matches!(self, Rung::PcHostedHotspot | Rung::PeerHostedHotspot)
    }
    pub fn label(self) -> &'static str {
        match self {
            Rung::PcHostedHotspot => "Hotspot (PC-hosted)",
            Rung::PcHostedDirect => "Wi-Fi Direct (PC-hosted)",
            Rung::PhoneHosted => "Wi-Fi Direct (phone-hosted)",
            Rung::PeerHostedHotspot => "Hotspot (sender-hosted)",
            Rung::PeerHostedLan => "Wi-Fi LAN (phone-hosted)",
            Rung::Lan => "Wi-Fi LAN",
        }
    }
}

pub type Guard = Box<dyn Any + Send>;

pub struct HostedGroup {
    pub ssid: String,
    pub password: String,
    pub ip: Ipv4Addr,
    /// Our P2P device name, for a joiner that finds groups by device (Windows); None if unknown.
    pub device_name: Option<String>,
    /// Operating frequency in MHz, or -1.
    pub frequency: i32,
    /// Keeps the group up while held.
    pub guard: Guard,
}

pub struct JoinedGroup {
    pub remote_ip: String,
    /// Our address on the group (for the reverse connection, see `offer_reverse`).
    pub local_ip: String,
    pub guard: Guard,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Opens TCP connections over a joined network (Android: through that network's own socket
/// factory, or they'd go out on the phone's normal Wi-Fi/mobile data).
pub type NetConnector = Box<dyn Fn(std::net::SocketAddrV4) -> BoxFuture<'static, Result<TcpStream>> + Send + Sync>;

/// A Wi-Fi network the other device hosts, which we joined as a plain client.
pub struct JoinedWifi {
    /// None = the OS routes to it normally.
    pub connect: Option<NetConnector>,
    /// Leaves the network when dropped.
    pub guard: Guard,
}

/// TCP to `addr` over a joined network.
pub async fn connect_via(connect: &Option<NetConnector>, addr: std::net::SocketAddrV4) -> Result<TcpStream> {
    match connect {
        Some(c) => c(addr).await,
        None => Ok(TcpStream::connect(addr).await?),
    }
}

/// OS services the ladder needs. Default methods report "unsupported".
pub trait Platform: Send + Sync + 'static {
    fn host_group(&self) -> BoxFuture<'_, Result<HostedGroup>> {
        Box::pin(async { bail!("hosting a Wi-Fi Direct group is not supported on this platform") })
    }
    fn join_group(&self, _device_name: String, _pin: String) -> BoxFuture<'_, Result<JoinedGroup>> {
        Box::pin(async { bail!("joining a Wi-Fi Direct group is not supported on this platform") })
    }
    /// Join a network the other device hosts (WIFI_HOTSPOT credentials), as a plain client.
    fn join_wifi(&self, _ssid: String, _password: String, _frequency: i32) -> BoxFuture<'_, Result<JoinedWifi>> {
        Box::pin(async { bail!("joining a Wi-Fi network is not supported on this platform") })
    }
    /// The next hop our internet traffic takes (to tell "we're on that phone's hotspot"), if known.
    fn internet_gateway(&self) -> Option<Ipv4Addr> {
        None
    }
    /// Make sure inbound connections on the hosted group's subnet are allowed.
    fn ensure_firewall(&self) -> Result<()> {
        Ok(())
    }
    /// F5: remove default routes left on Wi-Fi Direct subnets. Returns how many.
    fn cleanup_routes(&self) -> Result<usize> {
        Ok(0)
    }
}

pub struct NoPlatform;
impl Platform for NoPlatform {}

pub enum UpgradeEvent {
    Ready { rung: Rung, rd: BoxRead, wr: BoxWrite, encrypted: bool, guard: Option<Guard>, setup: Duration },
    /// `informed`: the phone had already been sent this path (F13).
    Failed { rung: Rung, reason: String, informed: bool },
    PeerWaitExpired { rung: Rung },
}

/// Local IPv4 address of the interface that routes to `peer`.
pub fn local_ip_towards(peer: Ipv4Addr) -> Result<Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
    s.connect((peer, 9))?;
    match s.local_addr()?.ip() {
        std::net::IpAddr::V4(ip) => Ok(ip),
        _ => bail!("no IPv4 route to {peer}"),
    }
}

struct Attempt {
    rung: Rung,
    task: JoinHandle<()>,
    /// Role switch: where to deliver the phone's UPGRADE_PATH_AVAILABLE.
    path_tx: Option<oneshot::Sender<Upi>>,
}

/// Timeouts for the ladder; shortened in tests.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Phone must connect to a PC-hosted group within this.
    pub hosted: Duration,
    /// Phone must connect to our LAN socket within this.
    pub lan: Duration,
    /// Phone must answer UPGRADE_PATH_REQUEST within this.
    pub role_switch: Duration,
    /// After our side gave up on a path the phone was told about, how long to wait
    /// for the phone's own UPGRADE_FAILURE before offering anything else (F13).
    pub peer_failure_wait: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            // The phone took 13 s to join our hotspot once (scan + associate + DHCP).
            hosted: Duration::from_secs(25),
            lan: Duration::from_secs(8),
            role_switch: Duration::from_secs(10),
            peer_failure_wait: Duration::from_secs(45),
        }
    }
}

pub struct Upgrader {
    ladder: VecDeque<Rung>,
    platform: Arc<dyn Platform>,
    health: Arc<Health>,
    timeouts: Timeouts,
    peer_ip: Option<Ipv4Addr>,
    endpoint_id: String,
    writer: Writer,
    switch: ReaderSwitch,
    events_tx: mpsc::UnboundedSender<UpgradeEvent>,
    current: Option<Attempt>,
    /// F13: our attempt is over but the phone may still be working on it; it
    /// ignores new offers until it reports UPGRADE_FAILURE.
    awaiting_peer: Option<Rung>,
    /// Medium in use after a successful upgrade (None = still on the bootstrap medium).
    pub active: Option<Rung>,
    guard: Option<Guard>,
    allow_disruptive: bool,
    /// Rungs the user configured (for re-queueing LAN on the phone's retry signal).
    configured: Vec<Rung>,
    lan_attempts: u32,
    /// The peer runs Drift: on Wi-Fi Direct the host connects out to us instead (see `offer_reverse`).
    peer_drift: bool,
    /// We share one hop (a laptop on the phone's hotspot): LAN is final, nothing beats it.
    single_hop: bool,
}

fn is_hosted(r: Rung) -> bool {
    matches!(r, Rung::PcHostedHotspot | Rung::PcHostedDirect)
}

impl Upgrader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ladder: Vec<Rung>,
        platform: Arc<dyn Platform>,
        health: Arc<Health>,
        timeouts: Timeouts,
        peer_ip: Option<Ipv4Addr>,
        endpoint_id: String,
        writer: Writer,
        switch: ReaderSwitch,
    ) -> (Self, mpsc::UnboundedReceiver<UpgradeEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        (
            Self {
                configured: ladder.clone(),
                lan_attempts: 0,
                ladder: ladder.into(),
                platform,
                health,
                timeouts,
                peer_ip,
                endpoint_id,
                writer,
                switch,
                events_tx,
                current: None,
                awaiting_peer: None,
                active: None,
                guard: None,
                allow_disruptive: false,
                peer_drift: false,
                single_hop: false,
            },
            events_rx,
        )
    }

    pub fn busy(&self) -> bool {
        self.current.is_some() || self.awaiting_peer.is_some()
    }

    /// On a LAN link with a Drift peer and something faster still to try: keep upgrading.
    fn improvable(&self) -> bool {
        self.peer_drift
            && !self.single_hop
            && matches!(self.active, Some(Rung::Lan | Rung::PeerHostedLan))
            && self.ladder.iter().any(|r| !matches!(r, Rung::Lan | Rung::PeerHostedLan))
    }

    /// Hosting side (a Drift phone sending): the receiver asked for a path. A Drift receiver may ask
    /// again for something faster while we're on (or still setting up) LAN: drop that attempt and host
    /// the new one. Returns false when the request should be ignored (already busy with it).
    pub fn take_request(&mut self, rungs: Vec<Rung>) -> bool {
        let on_lan = matches!(self.active, Some(Rung::Lan)) || self.current.as_ref().is_some_and(|a| a.rung == Rung::Lan);
        let better = rungs.iter().any(|r| *r != Rung::Lan);
        // A Drift receiver asking for something else while our attempt is still being set up has
        // given up on it (its join failed): drop ours and follow the new request. Ignoring it left
        // our group waiting for a joiner that never came, and the fallback never started.
        let replaces = self.peer_drift && self.active.is_none() && self.current.as_ref().is_some_and(|a| !rungs.contains(&a.rung));
        if self.busy() || self.active.is_some() {
            if !(self.peer_drift && on_lan && better) && !replaces {
                return false;
            }
            if let Some(a) = self.current.take() {
                tracing::info!(rung = ?a.rung, "the receiver asked for a faster path; dropping this attempt");
                a.task.abort();
            }
            self.awaiting_peer = None;
        }
        self.configured = rungs.clone();
        self.ladder = rungs.into();
        self.advance();
        true
    }

    /// Allow rungs that may briefly take the phone off its Wi-Fi (call once the
    /// user accepted) and continue the ladder if it was waiting on that.
    pub fn allow_disruptive(&mut self) {
        self.allow_disruptive = true;
        self.advance();
    }

    /// Start the next possible rung, if any and if nothing is in progress.
    pub fn advance(&mut self) {
        if self.busy() || (self.active.is_some() && !self.improvable()) {
            return;
        }
        if self.active.is_some() {
            // already on LAN: only something better is worth a second handover
            while matches!(self.ladder.front(), Some(Rung::Lan | Rung::PeerHostedLan)) {
                self.ladder.pop_front();
            }
            if self.ladder.is_empty() {
                return;
            }
            tracing::info!(next = ?self.ladder.front(), "on LAN for now; setting up the faster link");
        }
        // F6: skip hosted rungs while they're benched.
        while let Some(&r) = self.ladder.front() {
            if is_hosted(r) && !self.health.hosted_ok() {
                tracing::info!(rung = ?r, "skipping: PC-hosted group marked unhealthy");
                self.ladder.pop_front();
            } else {
                break;
            }
        }
        let Some(&rung) = self.ladder.front() else { return };
        if rung.disrupts_phone() && !self.allow_disruptive {
            return; // wait for accept
        }
        self.ladder.pop_front();
        if rung == Rung::Lan {
            self.lan_attempts += 1;
        }
        tracing::info!(rung = ?rung, "starting upgrade attempt");
        let events = self.events_tx.clone();
        let writer = self.writer.clone();
        let platform = self.platform.clone();
        let peer_ip = self.peer_ip;
        let eid = self.endpoint_id.clone();
        let t = self.timeouts;
        let reverse = self.peer_drift;
        let (path_tx, path_rx) = if matches!(rung, Rung::PhoneHosted | Rung::PeerHostedHotspot | Rung::PeerHostedLan) || (rung == Rung::PcHostedDirect && reverse) {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let task = tokio::spawn(async move {
            let started = Instant::now();
            let informed = AtomicBool::new(false);
            let result = match rung {
                Rung::PcHostedHotspot | Rung::PcHostedDirect => host_pc_group(rung, &*platform, &writer, &informed, &t, path_rx, &eid).await,
                Rung::Lan => host_lan(peer_ip, &writer, &informed, &t).await,
                Rung::PhoneHosted => join_phone_group(&*platform, &writer, path_rx.unwrap(), &eid, &t, reverse).await,
                Rung::PeerHostedHotspot => join_peer_hotspot(&*platform, &writer, path_rx.unwrap(), &eid, &t).await,
                Rung::PeerHostedLan => join_peer_lan(&writer, path_rx.unwrap(), &eid).await,
            };
            let ev = match result {
                Ok((rd, wr, encrypted, guard)) => {
                    UpgradeEvent::Ready { rung, rd, wr, encrypted, guard, setup: started.elapsed() }
                }
                Err(e) => UpgradeEvent::Failed { rung, reason: format!("{e:#}"), informed: informed.load(Ordering::SeqCst) },
            };
            let _ = events.send(ev);
        });
        self.current = Some(Attempt { rung, task, path_tx });
    }

    /// Handle the result of an attempt. Returns the rung that is now active, if any.
    pub fn on_event(&mut self, ev: UpgradeEvent) -> Option<Rung> {
        match ev {
            UpgradeEvent::Ready { rung, rd, wr, encrypted, guard, setup } => {
                if self.current.as_ref().map(|a| a.rung) != Some(rung) {
                    return None; // stale (attempt was cancelled)
                }
                self.current = None;
                if is_hosted(rung) {
                    self.health.record_hosted_success();
                }
                if let Some(prev) = self.active {
                    tracing::info!(from = ?prev, to = ?rung, "second handover: moving off LAN");
                }
                tracing::info!(rung = ?rung, ?setup, encrypted, "upgrade channel ready; handing over");
                self.writer.pause_for(wr, encrypted);
                self.writer.send_on_prior(payload::bwu_event(BwuEvent::LastWriteToPriorChannel));
                self.switch.switch_to(rd, encrypted);
                self.guard = guard;
                self.active = Some(rung);
                // LAN with a Drift peer is only the first stage: start on the faster link now
                self.advance();
                Some(rung)
            }
            UpgradeEvent::Failed { rung, reason, informed } => {
                if self.current.as_ref().map(|a| a.rung) != Some(rung) {
                    return None;
                }
                self.current = None;
                if is_hosted(rung) && informed {
                    self.health.record_hosted_failure();
                }
                if informed {
                    // F13: the phone is probably still retrying this path and will
                    // drop any new offer until it gives up. Wait for its failure.
                    tracing::warn!(rung = ?rung, %reason, "attempt failed; waiting for the phone to give up before the next rung");
                    self.awaiting_peer = Some(rung);
                    let tx = self.events_tx.clone();
                    let wait = self.timeouts.peer_failure_wait;
                    tokio::spawn(async move {
                        tokio::time::sleep(wait).await;
                        let _ = tx.send(UpgradeEvent::PeerWaitExpired { rung });
                    });
                } else {
                    if self.peer_drift && matches!(rung, Rung::PhoneHosted | Rung::PeerHostedHotspot | Rung::PeerHostedLan) {
                        // the phone may be hosting for us right now: tell it we gave up, so it stops
                        // waiting and takes the next request
                        tracing::warn!(rung = ?rung, %reason, "couldn't join the phone's path; telling it, trying next");
                        self.writer.send(payload::bwu(nc::BandwidthUpgradeNegotiationFrame {
                            event_type: Some(BwuEvent::UpgradeFailure as i32),
                            ..Default::default()
                        }));
                    } else {
                        tracing::warn!(rung = ?rung, %reason, "attempt failed before the phone was involved; trying next");
                    }
                    self.advance();
                }
                None
            }
            UpgradeEvent::PeerWaitExpired { rung } => {
                if self.awaiting_peer == Some(rung) {
                    tracing::warn!(rung = ?rung, "phone never reported failure; moving on");
                    self.awaiting_peer = None;
                    self.advance();
                }
                None
            }
        }
    }

    /// Sending side: the phone asked us to host (`UPGRADE_PATH_REQUEST`). Try
    /// `rung` next if we're idle on the bootstrap medium.
    pub fn offer(&mut self, rung: Rung) {
        if self.active.is_some() || self.busy() {
            return;
        }
        self.ladder.retain(|r| *r != rung);
        self.ladder.push_front(rung);
        self.advance();
    }

    /// The phone's Wi-Fi address (needed to pick our interface for LAN).
    /// On the peer's hotspot (or it on ours): LAN is final, no second handover.
    pub fn set_single_hop(&mut self, v: bool) {
        self.single_hop = v;
    }

    pub fn set_peer_drift(&mut self, drift: bool) {
        self.peer_drift = drift;
    }

    /// True while our hosting attempt waits for the peer to offer where to connect (reverse).
    pub fn wants_path(&self) -> bool {
        self.current.as_ref().is_some_and(|a| a.path_tx.is_some())
    }

    pub fn set_peer_ip(&mut self, ip: Ipv4Addr) {
        self.peer_ip = Some(ip);
    }

    /// Replace the ladder with `rungs` (the peer asked us to host one of these)
    /// and start the first; failures move down the list as usual.
    pub fn offer_ladder(&mut self, rungs: Vec<Rung>) {
        if self.active.is_some() || self.busy() {
            return;
        }
        self.configured = rungs.clone();
        self.ladder = rungs.into();
        self.advance();
    }

    /// The phone sends BANDWIDTH_UPGRADE_RETRY when it is (back) on a Wi-Fi network,
    /// e.g. after a failed hotspot attempt took it off its own Wi-Fi. If we're idle
    /// on the bootstrap medium, give LAN another go.
    pub fn on_peer_retry(&mut self) {
        if self.active.is_some() || self.busy() {
            return;
        }
        if self.configured.contains(&Rung::Lan) && self.lan_attempts < 3 && !self.ladder.contains(&Rung::Lan) {
            tracing::info!("phone is back on Wi-Fi; retrying LAN");
            self.ladder.push_back(Rung::Lan);
        }
        self.advance();
    }

    /// BWU frames from the phone other than the prior-channel shutdown pair.
    pub fn on_peer_frame(&mut self, b: &nc::BandwidthUpgradeNegotiationFrame) {
        match BwuEvent::try_from(b.event_type.unwrap_or(0)).unwrap_or(BwuEvent::UnknownEventType) {
            BwuEvent::UpgradePathAvailable => {
                if let Some(tx) = self.current.as_mut().and_then(|a| a.path_tx.take()) {
                    let _ = tx.send(b.upgrade_path_info.clone().unwrap_or_default());
                } else {
                    tracing::warn!("unsolicited UPGRADE_PATH_AVAILABLE from phone; ignoring");
                }
            }
            BwuEvent::UpgradeFailure => {
                if let Some(a) = self.current.take() {
                    tracing::warn!(rung = ?a.rung, "phone reported UPGRADE_FAILURE; trying next");
                    if is_hosted(a.rung) {
                        self.health.record_hosted_failure();
                    }
                    a.task.abort(); // drops the listener / group guard
                    self.advance();
                } else if let Some(r) = self.awaiting_peer.take() {
                    tracing::info!(rung = ?r, "phone gave up too; trying next");
                    self.advance();
                }
            }
            _ => {}
        }
    }

    /// The upgraded link stopped carrying data (F4). Bench hosted rungs if that's
    /// what we were on, so the phone's retry goes straight to the next rung.
    pub fn on_stall(&mut self) {
        if let Some(r) = self.active {
            if is_hosted(r) {
                self.health.record_hosted_stall();
            }
        }
    }
}

type Channel = (BoxRead, BoxWrite, bool, Option<Guard>);

/// Accept the phone's socket, read CLIENT_INTRODUCTION, reply ACK.
async fn accept_client(listener: TcpListener, timeout: Duration) -> Result<(BoxRead, BoxWrite)> {
    let (sock, peer) = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| anyhow!("phone did not connect within {timeout:?}"))??;
    sock.set_nodelay(true)?;
    crate::channel::tune(&sock);
    if std::env::var_os("DRIFT_PROBE").is_some() {
        spawn_link_probe(peer.ip());
    }
    let (mut rd, mut wr) = tokio::io::split(sock);
    let intro = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd))
        .await
        .map_err(|_| anyhow!("no CLIENT_INTRODUCTION from {peer}"))??;
    let intro = nc::OfflineFrame::decode(intro.as_slice())?;
    let ok = intro
        .v1
        .and_then(|v| v.bandwidth_upgrade_negotiation)
        .and_then(|b| b.event_type)
        .is_some_and(|e| e == BwuEvent::ClientIntroduction as i32);
    if !ok {
        bail!("expected CLIENT_INTRODUCTION from {peer}");
    }
    write_frame(&mut wr, &payload::bwu_event(BwuEvent::ClientIntroductionAck).encode_to_vec()).await?;
    Ok((Box::new(rd), Box::new(wr)))
}

/// Diagnostic: ping the phone over the new link every 250 ms for 20 s and log
/// every up/down transition with a timestamp.
fn spawn_link_probe(ip: std::net::IpAddr) {
    tokio::spawn(async move {
        let start = Instant::now();
        let mut last: Option<bool> = None;
        while start.elapsed() < Duration::from_secs(20) {
            let mut ping = tokio::process::Command::new("ping");
            #[cfg(windows)]
            ping.creation_flags(0x0800_0000); // no console window from the GUI app
            let ok = ping
                .args(["-n", "1", "-w", "300", &ip.to_string()])
                .stdout(std::process::Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if last != Some(ok) {
                tracing::info!(t = ?start.elapsed(), %ip, up = ok, "link probe");
                last = Some(ok);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

/// The hosted link always listens on one of these ports, so the firewall rule can name the ports
/// instead of the program path: it then survives updates, reinstalls and dev builds.
pub const HOSTED_PORTS: std::ops::RangeInclusive<u16> = 47621..=47625;

async fn bind_hosted() -> Result<TcpListener> {
    for port in HOSTED_PORTS {
        if let Ok(l) = TcpListener::bind(("0.0.0.0", port)).await {
            return Ok(l);
        }
    }
    tracing::warn!("hosted ports {HOSTED_PORTS:?} all busy; using a random port (the firewall may block it)");
    Ok(TcpListener::bind(("0.0.0.0", 0)).await?)
}

async fn host_pc_group(
    rung: Rung,
    platform: &dyn Platform,
    writer: &Writer,
    informed: &AtomicBool,
    t: &Timeouts,
    reverse: Option<oneshot::Receiver<Upi>>,
    eid: &str,
) -> Result<Channel> {
    let t0 = Instant::now();
    let group = platform.host_group().await.context("start Wi-Fi Direct group")?;
    tracing::info!(took = ?t0.elapsed(), ssid = %group.ssid, "group up");
    let t1 = Instant::now();
    if let Err(e) = platform.ensure_firewall() {
        tracing::warn!("firewall rule check failed: {e:#}");
    }
    tracing::debug!(took = ?t1.elapsed(), "firewall checked");
    let listener = bind_hosted().await?;
    let port = listener.local_addr()?.port();
    let as_direct = rung == Rung::PcHostedDirect;
    writer.send(payload::bwu_hosted_network(as_direct, &group.ssid, &group.password, &group.ip.to_string(), port, group.frequency, group.device_name.as_deref()));
    informed.store(true, Ordering::SeqCst);
    let (rd, wr) = match reverse {
        None => accept_client(listener, t.hosted).await?,
        // A Drift peer that joined tells us where to connect (some phones drop inbound connections to
        // apps): take whichever comes first.
        Some(path_rx) => tokio::select! {
            r = accept_client(listener, t.hosted) => r?,
            p = path_rx => {
                let info = p.map_err(|_| anyhow!("attempt cancelled"))?;
                connect_reverse(&info, eid).await?
            }
        },
    };
    // WPA2 protects the link, but we keep UKEY2 on (we never offer to disable it).
    Ok((rd, wr, true, Some(group.guard)))
}

/// Host side of the reverse connection: connect to the WIFI_LAN socket the joiner offered and
/// introduce ourselves, as a joiner would.
async fn connect_reverse(info: &Upi, eid: &str) -> Result<(BoxRead, BoxWrite)> {
    let s = info.wifi_lan_socket.clone().ok_or_else(|| anyhow!("reverse path without a socket"))?;
    let ip: [u8; 4] = s.ip_address.as_deref().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("bad reverse ip"))?;
    let addr = (Ipv4Addr::from(ip), s.wifi_port.unwrap_or(0) as u16);
    tracing::info!(?addr, "the joiner listens; connecting out");
    let mut last = String::new();
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr)).await {
            Ok(Ok(sock)) => {
                sock.set_nodelay(true)?;
                crate::channel::tune(&sock);
                let (mut rd, mut wr) = tokio::io::split(sock);
                write_frame(&mut wr, &payload::bwu_client_introduction(eid).encode_to_vec()).await?;
                let ack = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd)).await.map_err(|_| anyhow!("no intro ack"))??;
                let ack = nc::OfflineFrame::decode(ack.as_slice())?;
                let ok = ack.v1.and_then(|v| v.bandwidth_upgrade_negotiation).and_then(|b| b.event_type)
                    == Some(BwuEvent::ClientIntroductionAck as i32);
                if !ok {
                    bail!("expected CLIENT_INTRODUCTION_ACK");
                }
                return Ok((Box::new(rd), Box::new(wr)));
            }
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "timeout".into(),
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    bail!("connect {addr:?}: {last}")
}

/// Joiner side of the reverse connection, for a Drift host: listen on our address on its group,
/// tell it (UPGRADE_PATH_AVAILABLE, WIFI_LAN) and wait for it to connect and introduce itself.
/// The Android companion can't take inbound connections on some phones (ColorOS drops them for
/// apps, NOTES.md), but can always connect out.
pub(crate) async fn offer_reverse(writer: &Writer, local_ip: &str, wait: Duration) -> Result<(BoxRead, BoxWrite)> {
    let ip: Ipv4Addr = local_ip.parse().map_err(|_| anyhow!("bad local group address {local_ip:?}"))?;
    let listener = bind_hosted().await?; // the firewall rule covers these ports on the group subnets
    let port = listener.local_addr()?.port();
    tracing::info!(%ip, port, "listening on the group; asking the host to connect out");
    writer.send(payload::bwu_wifi_lan(ip.octets(), port));
    accept_client(listener, wait).await
}

async fn host_lan(peer_ip: Option<Ipv4Addr>, writer: &Writer, informed: &AtomicBool, t: &Timeouts) -> Result<Channel> {
    // The phone reports its IP when we receive; when we send it usually doesn't,
    // so fall back to our own address on the local network.
    let ip = match peer_ip {
        Some(p) => local_ip_towards(p)?,
        None => lan_ip().ok_or_else(|| anyhow!("no local network address to offer"))?,
    };
    tracing::info!(%ip, "offering LAN path");
    let listener = TcpListener::bind((ip, 0)).await?;
    let port = listener.local_addr()?.port();
    writer.send(payload::bwu_wifi_lan(ip.octets(), port));
    informed.store(true, Ordering::SeqCst);
    let (rd, wr) = accept_client(listener, t.lan).await?;
    Ok((rd, wr, true, None))
}

/// Our address on the shared Wi-Fi/Ethernet network: a private, up, non-tunnel
/// IPv4 that isn't one of our own hosted subnets. Wi-Fi adapters first.
pub fn lan_ip() -> Option<Ipv4Addr> {
    let mut c: Vec<(bool, Ipv4Addr)> = if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter(|i| !i.is_loopback() && !i.is_p2p && i.oper_status == if_addrs::IfOperStatus::Up)
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(ip) if lan_candidate(ip) => {
                let n = i.name.to_lowercase();
                Some((!(n.contains("wi-fi") || n.contains("wifi") || n.contains("wlan") || n.starts_with("wl")), ip))
            }
            _ => None,
        })
        .collect();
    c.sort_by_key(|(not_wifi, _)| *not_wifi);
    c.first().map(|(_, ip)| *ip)
}

/// Android's hotspot (SoftAP) interface names; the STA is wlan0, Wi-Fi Direct p2p*.
fn is_hotspot_iface(name: &str) -> bool {
    let n = name.to_lowercase();
    n.starts_with("ap") || n.starts_with("swlan") || n.starts_with("softap") || n == "wlan1" || n == "wlan2"
}

/// The address we tell a receiver in our connection request: on a phone with its hotspot on, the
/// hotspot's (a laptop on it sees that as its gateway); otherwise our LAN address.
pub fn advertised_ip() -> Option<Ipv4Addr> {
    if cfg!(target_os = "android") {
        let hs = if_addrs::get_if_addrs().ok()?.into_iter().find_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) if is_hotspot_iface(&i.name) && a.ip.is_private() => Some(a.ip),
            _ => None,
        });
        if hs.is_some() {
            return hs;
        }
    }
    lan_ip()
}

/// Phone side: is `peer` on our own hotspot (inside the subnet of a hotspot interface)?
pub fn on_my_hotspot(peer: Ipv4Addr) -> bool {
    if !cfg!(target_os = "android") {
        return false;
    }
    let Ok(ifs) = if_addrs::get_if_addrs() else { return false };
    ifs.into_iter().any(|i| match i.addr {
        if_addrs::IfAddr::V4(a) if is_hotspot_iface(&i.name) => {
            let m = u32::from(a.netmask);
            u32::from(a.ip) & m == u32::from(peer) & m
        }
        _ => false,
    })
}

fn lan_candidate(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() && !(o[0] == 192 && o[1] == 168 && (o[2] == 137 || o[2] == 49))
}

/// Non-DFS 5 GHz channels: a hint for the phone's GO channel (often ignored).
const NON_DFS_5G: [i32; 9] = [5180, 5200, 5220, 5240, 5745, 5765, 5785, 5805, 5825];

async fn join_phone_group(platform: &dyn Platform, writer: &Writer, path_rx: oneshot::Receiver<Upi>, eid: &str, t: &Timeouts, reverse: bool) -> Result<Channel> {
    writer.send(payload::bwu_path_request(&[UpiMedium::WifiDirect], &NON_DFS_5G));
    let info = tokio::time::timeout(t.role_switch, path_rx)
        .await
        .map_err(|_| anyhow!("phone did not offer to host a group"))?
        .map_err(|_| anyhow!("attempt cancelled"))?;
    let encrypted = !info.supports_disabling_encryption.unwrap_or(false);
    if reverse {
        let joined = join_group_only(platform, &info).await?;
        let (rd, wr) = offer_reverse(writer, &joined.local_ip, Duration::from_secs(20)).await?;
        return Ok((rd, wr, encrypted, Some(joined.guard)));
    }
    let (sock, guard) = join_direct(platform, &info).await?;
    let (mut rd, mut wr) = tokio::io::split(sock);
    write_frame(&mut wr, &payload::bwu_client_introduction(eid).encode_to_vec()).await?;
    let ack = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd)).await.map_err(|_| anyhow!("no intro ack"))??;
    let ack = nc::OfflineFrame::decode(ack.as_slice())?;
    let ok = ack
        .v1
        .and_then(|v| v.bandwidth_upgrade_negotiation)
        .and_then(|b| b.event_type)
        .is_some_and(|e| e == BwuEvent::ClientIntroductionAck as i32);
    if !ok {
        bail!("expected CLIENT_INTRODUCTION_ACK");
    }
    Ok((Box::new(rd), Box::new(wr), encrypted, Some(guard)))
}

/// Join a Wi-Fi Direct group the other side hosts (WIFI_DIRECT credentials) as a P2P client, and
/// connect to it (retrying while the address comes up). Our side keeps its own Wi-Fi.
pub(crate) async fn join_direct(platform: &dyn Platform, info: &Upi) -> Result<(TcpStream, Guard)> {
    let creds: WifiDirectCredentials = info.wifi_direct_credentials.clone().ok_or_else(|| anyhow!("no Wi-Fi Direct credentials"))?;
    let joined = join_group_only(platform, info).await?;
    let gw = creds.gateway.clone().filter(|g| !g.is_empty() && g != "0.0.0.0").unwrap_or_else(|| joined.remote_ip.clone());
    let port = creds.port.unwrap_or(0) as u16;
    let mut last_err = None;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect((gw.as_str(), port))).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true)?;
                crate::channel::tune(&s);
                return Ok((s, joined.guard));
            }
            Ok(Err(e)) => last_err = Some(e.to_string()),
            Err(_) => last_err = Some("timeout".into()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("connect {gw}:{port}: {}", last_err.unwrap_or_default())
}

/// Join the peer's Wi-Fi Direct group (no connection yet).
pub(crate) async fn join_group_only(platform: &dyn Platform, info: &Upi) -> Result<JoinedGroup> {
    let creds: WifiDirectCredentials = info.wifi_direct_credentials.clone().ok_or_else(|| anyhow!("no Wi-Fi Direct credentials"))?;
    let t0 = Instant::now();
    let joined = platform
        .join_group(creds.device_name.clone().unwrap_or_default(), creds.pin.clone().unwrap_or_default())
        .await?;
    tracing::info!(took = ?t0.elapsed(), remote = %joined.remote_ip, local = %joined.local_ip, "joined the peer's Wi-Fi Direct group");
    Ok(joined)
}

/// Join a hotspot the other side offered and connect to it (retrying while the address comes up).
pub(crate) async fn join_hotspot(platform: &dyn Platform, info: &Upi) -> Result<(TcpStream, Guard)> {
    let c = info.wifi_hotspot_credentials.clone().ok_or_else(|| anyhow!("no hotspot credentials"))?;
    let gw: Ipv4Addr = c.gateway.clone().unwrap_or_default().parse().map_err(|_| anyhow!("bad hotspot gateway {:?}", c.gateway))?;
    let addr = std::net::SocketAddrV4::new(gw, c.port.unwrap_or(0) as u16);
    let joined = platform
        .join_wifi(c.ssid.clone().unwrap_or_default(), c.password.clone().unwrap_or_default(), c.frequency.unwrap_or(-1))
        .await?;
    let JoinedWifi { connect, guard } = joined;
    let mut last_err = String::new();
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(4), connect_via(&connect, addr)).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true)?;
                crate::channel::tune(&s);
                return Ok((s, guard));
            }
            Ok(Err(e)) => last_err = format!("{e:#}"),
            Err(_) => last_err = "timeout".into(),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("connect {addr}: {last_err}")
}

/// Receiver side, Drift peers: ask the sender to open a LAN socket and connect to it. Quick or not at
/// all (networks that keep devices apart just fail it, silently, and Wi-Fi Direct carries on).
async fn join_peer_lan(writer: &Writer, path_rx: oneshot::Receiver<Upi>, eid: &str) -> Result<Channel> {
    writer.send(payload::bwu_path_request_from(&[UpiMedium::WifiLan], &NON_DFS_5G, lan_ip().map(|i| i.octets())));
    // over Bluetooth, behind whatever is queued there: give the offer time to arrive
    let info = tokio::time::timeout(Duration::from_secs(10), path_rx)
        .await
        .map_err(|_| anyhow!("the sender offered no LAN socket"))?
        .map_err(|_| anyhow!("attempt cancelled"))?;
    let encrypted = !info.supports_disabling_encryption.unwrap_or(false);
    let s = info.wifi_lan_socket.clone().ok_or_else(|| anyhow!("no LAN socket"))?;
    let ip: [u8; 4] = s.ip_address.as_deref().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow!("bad LAN ip"))?;
    let addr = (Ipv4Addr::from(ip), s.wifi_port.unwrap_or(0) as u16);
    let sock = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("LAN connect timeout (the network may keep devices apart)"))??;
    sock.set_nodelay(true)?;
    crate::channel::tune(&sock);
    let (mut rd, mut wr) = tokio::io::split(sock);
    write_frame(&mut wr, &payload::bwu_client_introduction(eid).encode_to_vec()).await?;
    let ack = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd)).await.map_err(|_| anyhow!("no intro ack"))??;
    let ack = nc::OfflineFrame::decode(ack.as_slice())?;
    let ok = ack.v1.and_then(|v| v.bandwidth_upgrade_negotiation).and_then(|b| b.event_type) == Some(BwuEvent::ClientIntroductionAck as i32);
    if !ok {
        bail!("expected CLIENT_INTRODUCTION_ACK");
    }
    Ok((Box::new(rd), Box::new(wr), encrypted, None))
}

/// Receiver side of the role switch for Drift peers: ask the sender to host a hotspot, join it.
async fn join_peer_hotspot(platform: &dyn Platform, writer: &Writer, path_rx: oneshot::Receiver<Upi>, eid: &str, t: &Timeouts) -> Result<Channel> {
    writer.send(payload::bwu_path_request(&[UpiMedium::WifiHotspot], &NON_DFS_5G));
    let info = tokio::time::timeout(t.role_switch, path_rx)
        .await
        .map_err(|_| anyhow!("sender did not offer a hotspot"))?
        .map_err(|_| anyhow!("attempt cancelled"))?;
    let encrypted = !info.supports_disabling_encryption.unwrap_or(false);
    let (sock, guard) = join_hotspot(platform, &info).await?;
    let (mut rd, mut wr) = tokio::io::split(sock);
    write_frame(&mut wr, &payload::bwu_client_introduction(eid).encode_to_vec()).await?;
    if info.supports_client_introduction_ack.unwrap_or(false) {
        let ack = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd)).await.map_err(|_| anyhow!("no intro ack"))??;
        let ack = nc::OfflineFrame::decode(ack.as_slice())?;
        let ok = ack.v1.and_then(|v| v.bandwidth_upgrade_negotiation).and_then(|b| b.event_type)
            == Some(BwuEvent::ClientIntroductionAck as i32);
        if !ok {
            bail!("expected CLIENT_INTRODUCTION_ACK");
        }
    }
    Ok((Box::new(rd), Box::new(wr), encrypted, Some(guard)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{spawn_reader, spawn_writer};
    use drift_core::secure::{self, Opener};
    use drift_core::ukey2::{ClientHandshake, Role, ServerHandshake};

    /// Hosts a "group" on loopback; nothing ever joins it.
    struct FakePlatform;
    impl Platform for FakePlatform {
        fn host_group(&self) -> BoxFuture<'_, Result<HostedGroup>> {
            Box::pin(async {
                Ok(HostedGroup { ssid: "DIRECT-xx".into(), password: "pw".into(), ip: Ipv4Addr::LOCALHOST, device_name: None, frequency: -1, guard: Box::new(()) })
            })
        }
    }

    fn fast() -> Timeouts {
        Timeouts {
            hosted: Duration::from_millis(80),
            lan: Duration::from_millis(80),
            role_switch: Duration::from_millis(80),
            peer_failure_wait: Duration::from_secs(30),
        }
    }

    struct Rig {
        up: Upgrader,
        events: mpsc::UnboundedReceiver<UpgradeEvent>,
        peer_rd: tokio::io::ReadHalf<tokio::io::DuplexStream>,
        peer_opener: Opener,
    }

    fn rig(ladder: Vec<Rung>, health: Arc<Health>) -> Rig {
        let (c, ci) = ClientHandshake::new();
        let (s, si) = ServerHandshake::handle_client_init(&ci).unwrap();
        let (cr, cf) = c.handle_server_init(&si).unwrap();
        let sr = s.handle_client_finish(&cf).unwrap();
        let (our_sealer, our_opener) = secure::split(&sr.session_keys(Role::Server));
        let (_, peer_opener) = secure::split(&cr.session_keys(Role::Client));
        let (ours, peer) = tokio::io::duplex(1 << 16);
        let (our_rd, our_wr) = tokio::io::split(ours);
        let (peer_rd, _peer_wr) = tokio::io::split(peer);
        let (writer, _) = spawn_writer(Box::new(our_wr), our_sealer);
        let (_frames, switch) = spawn_reader(Box::new(our_rd), our_opener, None);
        let (up, events) = Upgrader::new(
            ladder,
            Arc::new(FakePlatform),
            health,
            fast(),
            Some(Ipv4Addr::LOCALHOST),
            "TEST".into(),
            writer,
            switch,
        );
        Rig { up, events, peer_rd, peer_opener }
    }

    /// Next frame the phone would receive, or None if nothing arrives within `wait`.
    async fn next_path(r: &mut Rig, wait: Duration) -> Option<i32> {
        let raw = tokio::time::timeout(wait, read_frame(&mut r.peer_rd)).await.ok()?.ok()?;
        let f = nc::OfflineFrame::decode(r.peer_opener.open(&raw).unwrap().as_slice()).unwrap();
        f.v1?.bandwidth_upgrade_negotiation?.upgrade_path_info?.medium
    }

    #[tokio::test]
    async fn waits_for_phone_failure_before_next_rung() {
        let mut r = rig(vec![Rung::PcHostedHotspot, Rung::Lan], Arc::new(Health::default()));
        r.up.allow_disruptive();
        assert_eq!(next_path(&mut r, Duration::from_secs(2)).await, Some(UpiMedium::WifiHotspot as i32));

        // Our side times out; the phone was informed, so we must NOT offer LAN yet.
        let ev = r.events.recv().await.unwrap();
        assert!(matches!(ev, UpgradeEvent::Failed { informed: true, .. }));
        r.up.on_event(ev);
        assert!(r.up.busy());
        assert_eq!(next_path(&mut r, Duration::from_millis(300)).await, None, "LAN offered while phone still busy");

        // The phone finally gives up → LAN is offered.
        let failure = nc::BandwidthUpgradeNegotiationFrame { event_type: Some(BwuEvent::UpgradeFailure as i32), ..Default::default() };
        r.up.on_peer_frame(&failure);
        assert_eq!(next_path(&mut r, Duration::from_secs(2)).await, Some(UpiMedium::WifiLan as i32));
    }

    #[tokio::test]
    async fn a_different_request_replaces_the_attempt_in_progress() {
        // The phone hosts for a Drift receiver; the receiver's join fails and it asks for LAN
        // instead. The phone must follow (it used to ignore it and wait for a joiner forever).
        let mut r = rig(vec![], Arc::new(Health::default()));
        r.up.set_peer_drift(true);
        assert!(r.up.take_request(vec![Rung::PcHostedDirect]));
        assert_eq!(r.up.current.as_ref().map(|a| a.rung), Some(Rung::PcHostedDirect));
        assert!(r.up.take_request(vec![Rung::Lan]), "a different request was ignored");
        assert_eq!(r.up.current.as_ref().map(|a| a.rung), Some(Rung::Lan));
        // the same request again is a repeat: ignored
        assert!(!r.up.take_request(vec![Rung::Lan]));
    }

    #[tokio::test]
    async fn unhealthy_hosted_rung_is_skipped() {
        let health = Arc::new(Health::default());
        health.record_hosted_stall();
        let mut r = rig(vec![Rung::PcHostedHotspot, Rung::Lan], health);
        r.up.allow_disruptive();
        assert_eq!(next_path(&mut r, Duration::from_secs(2)).await, Some(UpiMedium::WifiLan as i32));
    }
}

#[cfg(test)]
mod lan_tests {
    use super::*;

    #[test]
    fn lan_candidates_skip_hosted_subnets() {
        assert!(lan_candidate(Ipv4Addr::new(10, 1, 2, 3)));
        assert!(!lan_candidate(Ipv4Addr::new(192, 168, 137, 1)));
        assert!(!lan_candidate(Ipv4Addr::new(192, 168, 49, 1)));
        assert!(!lan_candidate(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn this_machine_has_a_lan_ip() {
        println!("lan_ip = {:?}", lan_ip());
    }
}
