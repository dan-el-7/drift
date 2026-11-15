//! Drift application core: the always-on receiver service the native UIs sit on.
//!
//! Advertises over mDNS (open networks) and BLE (everywhere), accepts the
//! phone's bootstrap connection over TCP or Bluetooth RFCOMM, then runs each
//! share through `drift_net` with the configured upgrade ladder.

use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use drift_core::endpoint::{self, DeviceType, EndpointInfo};
use drift_net::{LocalIdentity, Platform, ReceiveConfig, ReceiveEvent, Rung};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

pub use drift_net::Medium;

/// Bluetooth run by the app around the core (Android: only Java can use the radio). The core
/// decides what to advertise; the app accepts RFCOMM links and hands them in via `bt_incoming`.
pub struct ExternalBt {
    /// Some(0xFEF3 service data) to start advertising it, None to stop.
    pub advertise: Box<dyn Fn(Option<Vec<u8>>) + Send + Sync>,
}
static EXTERNAL_BT: std::sync::RwLock<Option<Arc<ExternalBt>>> = std::sync::RwLock::new(None);
type Incoming = mpsc::UnboundedSender<(tokio::net::TcpStream, String)>;
static BT_INCOMING: Mutex<Option<Incoming>> = Mutex::new(None);

pub fn set_external_bt(b: Option<Arc<ExternalBt>>) {
    *EXTERNAL_BT.write().unwrap() = b;
}

/// An accepted Bluetooth link (bridged to a stream), with the peer's address. Dropped if we
/// aren't visible.
pub fn bt_incoming(stream: tokio::net::TcpStream, peer: String) {
    if let Some(tx) = BT_INCOMING.lock().unwrap().as_ref() {
        let _ = tx.send((stream, peer));
    }
}

pub mod diagnostics;
pub mod nearby;

pub(crate) const SERVICE_ID: &str = "NearbySharing";

#[derive(Clone, Debug)]
pub struct Config {
    pub device_name: String,
    pub download_dir: PathBuf,
    /// Upgrade ladder for receives. See CLAUDE.md "Upgrade fallback & recovery".
    pub ladder: Vec<Rung>,
    /// Drift's device key (trust between Drift devices). None = act exactly like stock.
    pub device_key: Option<Arc<drift_core::trust::DeviceKey>>,
}

impl Config {
    pub fn default_ladder() -> Vec<Rung> {
        if cfg!(target_os = "android") {
            // The phone hosts a Wi-Fi Direct group through the app (offered as WIFI_DIRECT; senders
            // join it on their P2P adapter and keep their Wi-Fi), else ask a Drift sender to host a
            // hotspot, else LAN.
            vec![Rung::PcHostedDirect, Rung::PeerHostedHotspot, Rung::Lan]
        } else {
            vec![Rung::PcHostedHotspot, Rung::Lan]
        }
    }
}

/// How the phone first reached us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bootstrap {
    /// TCP on the shared network (found us via mDNS).
    Lan,
    Bluetooth,
}

pub enum ServiceEvent {
    Started { endpoint_id: String, mdns: bool, ble: Option<String>, bluetooth: bool },
    /// Something about advertising failed but the service keeps running.
    Warning(String),
    Connection { id: u64, bootstrap: Bootstrap, peer_addr: String },
    Share { id: u64, event: ReceiveEvent },
    Finished { id: u64, error: Option<String> },
}

/// Running shares by id, so the UI can cancel one.
type Shares = Arc<Mutex<HashMap<u64, (tokio::task::AbortHandle, Arc<tokio::sync::Notify>)>>>;

pub struct Service {
    /// Shared medium health (F6); the UI can show it and offer a reset.
    pub health: Arc<drift_net::Health>,
    shares: Shares,
    accept: tokio::task::JoinHandle<()>,
    _mdns: Option<mdns_sd::ServiceDaemon>,
    #[cfg(windows)]
    _bt: Option<(drift_platform_win::bt::RfcommServer, drift_platform_win::bt::BlePublisher)>,
}

impl Service {
    /// Something the OS stopped under us that only a restart brings back (Windows: the Bluetooth
    /// advertisement dies when the radio is turned off and on; phones then can't find this PC).
    pub fn needs_restart(&self) -> bool {
        #[cfg(windows)]
        if let Some((_, ble)) = &self._bt {
            return !ble.is_running();
        }
        false
    }

    /// A share is running (don't restart under it).
    pub fn busy(&self) -> bool {
        !self.shares.lock().unwrap().is_empty()
    }
}

static PLATFORM: std::sync::RwLock<Option<Arc<dyn Platform>>> = std::sync::RwLock::new(None);

/// Platforms whose OS services live outside Rust (Android: Kotlin joins Wi-Fi networks).
pub fn set_platform(p: Option<Arc<dyn Platform>>) {
    *PLATFORM.write().unwrap() = p;
}

pub fn platform() -> Arc<dyn Platform> {
    if let Some(p) = PLATFORM.read().unwrap().clone() {
        return p;
    }
    #[cfg(windows)]
    {
        Arc::new(drift_platform_win::WinPlatform)
    }
    #[cfg(not(windows))]
    {
        Arc::new(drift_net::NoPlatform)
    }
}

pub fn local_identity(endpoint_id: [u8; 4]) -> LocalIdentity {
    local_identity_with(endpoint_id, None)
}

/// Our identity, carrying Drift's device key.
pub fn local_identity_with(endpoint_id: [u8; 4], device_key: Option<Arc<drift_core::trust::DeviceKey>>) -> LocalIdentity {
    use drift_core::proto::connections::os_info::OsType;
    #[cfg(windows)]
    let (name, os) = (drift_platform_win::wifi_direct_device_name(), OsType::Windows);
    #[cfg(target_os = "android")]
    let (name, os) = (None, OsType::Android);
    #[cfg(not(any(windows, target_os = "android")))]
    let (name, os) = (None, OsType::Linux);
    LocalIdentity { endpoint_id, device_key, wifi_direct_device_name: name, os }
}

/// What this device advertises itself as.
pub const DEVICE_TYPE: DeviceType = if cfg!(target_os = "android") { DeviceType::Phone } else { DeviceType::Laptop };

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn spawn_share<S>(
    stream: S,
    bootstrap: Bootstrap,
    peer_addr: String,
    local: LocalIdentity,
    cfg: Config,
    platform: Arc<dyn Platform>,
    health: Arc<drift_net::Health>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    shares: Shares,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let _ = events.send(ServiceEvent::Connection { id, bootstrap, peer_addr });
    let registry = shares.clone();
    let cancel = Arc::new(tokio::sync::Notify::new());
    let cancel_in = cancel.clone();
    let task = tokio::spawn(async move {
        let (share_tx, mut share_rx) = mpsc::unbounded_channel();
        let fwd_events = events.clone();
        let forward = tokio::spawn(async move {
            let mut used_hosted = false;
            while let Some(event) = share_rx.recv().await {
                if let ReceiveEvent::MediumChanged { medium: Medium::Upgraded(Rung::PcHostedHotspot | Rung::PcHostedDirect), .. } = &event {
                    used_hosted = true;
                }
                let _ = fwd_events.send(ServiceEvent::Share { id, event });
            }
            used_hosted
        });
        // Already on LAN? Then the LAN rung is pointless.
        let ladder: Vec<Rung> =
            cfg.ladder.iter().copied().filter(|r| !(bootstrap == Bootstrap::Lan && *r == Rung::Lan)).collect();
        let hold = std::env::var("DRIFT_HOLD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0u64); // hold didn't help (NOTES.md); kept as a switch
        let rcfg = ReceiveConfig {
            download_dir: cfg.download_dir.clone(),
            ladder,
            health: health.clone(),
            timeouts: drift_net::Timeouts::default(),
            stall_timeout: std::time::Duration::from_secs(6),
            cancel: Some(cancel_in),
            hold_after_switch: (bootstrap == Bootstrap::Bluetooth && hold > 0).then(|| std::time::Duration::from_millis(hold)),
        };
        let result = async {
            let conn = drift_net::accept(stream, &local).await?;
            drift_net::receive::run(conn, &local, &rcfg, platform, share_tx).await
        }
        .await;
        let used_hosted = forward.await.unwrap_or(false);
        registry.lock().unwrap().remove(&id);
        let _ = events.send(ServiceEvent::Finished { id, error: result.err().map(|e| format!("{e:#}")) });
        // F15: hosting a group churns the network stack; some VPNs lose DNS afterwards.
        if used_hosted {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Ok(diagnostics::NetHealth::DnsBroken) = tokio::task::spawn_blocking(diagnostics::check_net).await {
                let _ = events.send(ServiceEvent::Warning(
                    "Internet names stopped resolving after the transfer (your VPN's DNS may be stuck). Reconnecting the VPN usually fixes it.".into(),
                ));
            }
        }
    });
    shares.lock().unwrap().insert(id, (task.abort_handle(), cancel));
}

impl Drop for Service {
    fn drop(&mut self) {
        self.accept.abort();
        if let Some(d) = &self._mdns {
            let _ = d.shutdown();
        }
        if let Some(x) = EXTERNAL_BT.read().unwrap().as_ref() {
            BT_INCOMING.lock().unwrap().take();
            (x.advertise)(None);
        }
    }
}

impl Service {
    /// Cancel a share: the phone is told and the connection closes cleanly; if that hasn't
    /// happened within 3 s the connection is dropped. Returns false if it already ended.
    pub fn cancel(&self, id: u64) -> bool {
        let Some((task, signal)) = self.shares.lock().unwrap().get(&id).cloned() else { return false };
        signal.notify_one();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            task.abort();
        });
        true
    }

    /// Start advertising and accepting shares. Events arrive on the returned receiver.
    pub async fn start(cfg: Config) -> Result<(Self, mpsc::UnboundedReceiver<ServiceEvent>)> {
        let (events, rx) = mpsc::unbounded_channel();
        let eid = endpoint::random_endpoint_id();
        let local = local_identity_with(eid, cfg.device_key.clone());
        // Drift's tag, so other Drift devices know us (stock Quick Share ignores it)
        let info = EndpointInfo::new_drift(&cfg.device_name, DEVICE_TYPE);
        let platform = platform();
        let health = Arc::new(drift_net::Health::default());

        // F11: Google's Quick Share uses the same Bluetooth service; both can't run.
        if tokio::task::spawn_blocking(diagnostics::quick_share_running).await.unwrap_or(false) {
            let _ = events.send(ServiceEvent::Warning(
                "Google's Quick Share is running. It uses the same Bluetooth service, so phones may not find Drift. Quit it from the tray.".into(),
            ));
        }

        // F5: a stale default route from an earlier session would hijack the
        // laptop's internet as soon as we host a group. Fix it before we start.
        {
            let p = platform.clone();
            match tokio::task::spawn_blocking(move || p.cleanup_routes()).await? {
                Ok(n) if n > 0 => { let _ = events.send(ServiceEvent::Warning(format!("removed {n} stale Wi-Fi Direct route(s)"))); }
                Ok(_) => {}
                Err(e) => { let _ = events.send(ServiceEvent::Warning(format!("stale Wi-Fi Direct route found but not removed: {e:#}"))); }
            }
        }

        // Hosted rungs need an inbound firewall rule; ask (UAC) now, not mid-transfer.
        if cfg.ladder.iter().any(|r| matches!(r, Rung::PcHostedHotspot | Rung::PcHostedDirect)) {
            let p = platform.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || p.ensure_firewall()).await? {
                let _ = events.send(ServiceEvent::Warning(format!("Direct mode may be blocked by the firewall: {e:#}")));
            }
        }

        // TCP listener for phones that find us over mDNS.
        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let port = listener.local_addr()?.port();
        let mdns = (|| -> Result<mdns_sd::ServiceDaemon> {
            let d = mdns_sd::ServiceDaemon::new()?;
            let host = format!("drift-{}.local.", String::from_utf8_lossy(&eid).to_lowercase());
            let txt = info.to_txt();
            let svc = mdns_sd::ServiceInfo::new(
                endpoint::SERVICE_TYPE,
                &endpoint::mdns_instance_name(&eid),
                &host,
                "",
                port,
                &[("n", txt.as_str())][..],
            )?
            .enable_addr_auto();
            d.register(svc.clone())?;
            // Re-announce while visible. Phones hear our announcements reliably but don't always get
            // answers to their own queries (seen on the phone's hotspot: found only after a re-announce),
            // so a phone that starts looking later would miss us until we announce again.
            let d2 = d.clone();
            tokio::spawn(async move {
                let start = tokio::time::Instant::now();
                loop {
                    let every = if start.elapsed() < std::time::Duration::from_secs(120) { 10 } else { 60 };
                    tokio::time::sleep(std::time::Duration::from_secs(every)).await;
                    if d2.register(svc.clone()).is_err() {
                        break; // the daemon was shut down (hidden)
                    }
                }
            });
            Ok(d)
        })();
        let mdns = match mdns {
            Ok(d) => Some(d),
            Err(e) => {
                let _ = events.send(ServiceEvent::Warning(format!("mDNS unavailable: {e:#}")));
                None
            }
        };

        // Bluetooth: RFCOMM bootstrap + BLE advertisement.
        #[cfg(windows)]
        let (bt, bt_rx, ble_desc) = {
            use drift_platform_win::bt;
            let (bt_tx, bt_rx) = mpsc::unbounded_channel();
            let r = (|| -> Result<_> {
                let adapter = bt::adapter_info()?;
                let rfcomm = bt::start_rfcomm(drift_core::ble::name_uuid(SERVICE_ID), SERVICE_ID, bt_tx)?;
                let (adv, extended, desc) = if adapter.extended_adv {
                    let adv = drift_core::ble::regular_advertisement(SERVICE_ID, &eid, &info.encode(), adapter.mac())?;
                    (adv, true, "extended (named)".to_string())
                } else {
                    // Legacy controllers: 17-byte endpoint info, no name.
                    let hidden = EndpointInfo { visible: false, name: None, ..info.clone() };
                    (drift_core::ble::fast_advertisement(&eid, &hidden.encode())?, false, "legacy (unnamed)".to_string())
                };
                let ble = bt::start_ble(drift_core::ble::COPRESENCE_UUID16, &adv, extended)?;
                Ok(((rfcomm, ble), desc))
            })();
            match r {
                Ok((pair, desc)) => (Some(pair), Some(bt_rx), Some(desc)),
                Err(e) => {
                    let _ = events.send(ServiceEvent::Warning(format!("Bluetooth unavailable: {e:#}")));
                    (None, None, None)
                }
            }
        };
        #[cfg(not(windows))]
        let (bt_rx, mut ble_desc): (Option<mpsc::UnboundedReceiver<()>>, Option<String>) = (None, None);

        // App-run Bluetooth (Android): a short, nameless advert. No address (Android won't give an
        // app its own), so it carries a presence hint that only devices trusting us recognise.
        let mut ext_rx = None;
        let ext = EXTERNAL_BT.read().unwrap().clone();
        if let Some(x) = ext {
            let (tx, rx) = mpsc::unbounded_channel();
            *BT_INCOMING.lock().unwrap() = Some(tx);
            ext_rx = Some(rx);
            let mut adv_info = EndpointInfo { visible: false, name: None, ..info.clone() };
            if let Some(k) = &cfg.device_key {
                let salt: [u8; 2] = rand::random();
                adv_info = adv_info.with_hint(salt, drift_core::trust::presence_hint(&k.public(), salt));
            }
            match drift_core::ble::fast_advertisement(&eid, &adv_info.encode()) {
                Ok(adv) => {
                    (x.advertise)(Some(adv));
                    #[cfg(not(windows))]
                    {
                        ble_desc = Some("fast (Drift hint)".into());
                    }
                }
                Err(e) => { let _ = events.send(ServiceEvent::Warning(format!("BLE advert: {e:#}"))); }
            }
        }

        let _ = events.send(ServiceEvent::Started {
            endpoint_id: local.endpoint_id_str(),
            mdns: mdns.is_some(),
            ble: ble_desc.clone(),
            bluetooth: bt_rx.is_some() || ext_rx.is_some(),
        });

        let health_out = health.clone();
        let shares: Shares = Arc::default();
        let shares_out = shares.clone();
        // Accept loop.
        let ev = events.clone();
        let accept = tokio::spawn(async move {
            let mut bt_rx = bt_rx;
            let mut ext_rx = ext_rx;
            loop {
                tokio::select! {
                    Some((stream, peer)) = async { match ext_rx.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => {
                        spawn_share(stream, Bootstrap::Bluetooth, peer, local.clone(), cfg.clone(), platform.clone(), health.clone(), ev.clone(), shares.clone());
                    }
                    r = listener.accept() => match r {
                        Ok((sock, peer)) => {
                            let _ = sock.set_nodelay(true);
                            drift_net::channel::tune(&sock);
                            spawn_share(sock, Bootstrap::Lan, peer.to_string(), local.clone(), cfg.clone(), platform.clone(), health.clone(), ev.clone(), shares.clone());
                        }
                        Err(e) => { let _ = ev.send(ServiceEvent::Warning(format!("TCP accept: {e}"))); }
                    },
                    Some(s) = async { match bt_rx.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => {
                        #[cfg(windows)]
                        {
                            let peer = s.Information().and_then(|i| i.RemoteHostName()).and_then(|h| h.DisplayName())
                                .map(|h| h.to_string()).unwrap_or_default();
                            match drift_platform_win::bt::bridge(s) {
                                Ok(stream) => spawn_share(stream, Bootstrap::Bluetooth, peer, local.clone(), cfg.clone(), platform.clone(), health.clone(), ev.clone(), shares.clone()),
                                Err(e) => { let _ = ev.send(ServiceEvent::Warning(format!("Bluetooth bridge: {e:#}"))); }
                            }
                        }
                        #[cfg(not(windows))]
                        let _: () = s;
                    }
                }
            }
        });

        Ok((
            Self {
                health: health_out,
                shares: shares_out,
                accept,
                _mdns: mdns,
                #[cfg(windows)]
                _bt: bt,
            },
            rx,
        ))
    }
}
