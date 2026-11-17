//! `drift_ffi`: the Drift core behind a three-function C ABI for the native UIs.
//!
//! ```text
//! drift_start(config_json, callback, user) -> handle   (0 on failure)
//! drift_command(handle, command_json) -> 0 ok, -1 bad command, -2 no handle
//! drift_stop(handle)                                    (no callbacks after it returns)
//! ```
//!
//! Events are UTF-8 JSON objects (no terminator; the length is passed), delivered in order
//! on one core thread. Shapes match `ui/windows/Core/Protocol.cs`; `MockCore` emits the same.
//!
//! Commands: `{"c":"visibility","everyone":true}`, `{"c":"scan","on":true}`,
//! `{"c":"send","device":"ABCD","paths":[...]}`, `{"c":"respond","share":1,"accept":true}`,
//! `{"c":"cancel","share":1}`, `{"c":"settings","device_name":"...","download_dir":"..."}`.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use drift_app::nearby::{self, NearbyDevice, NearbyEvent, Route};
use drift_app::{Bootstrap, ServiceEvent};
use drift_core::endpoint::DeviceType;
use drift_net::send::SendEvent;
use drift_net::{Medium, ReceiveEvent, Rung};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

/// `void callback(const uint8_t* json, size_t len, intptr_t user)`
#[cfg(target_os = "android")]
mod android;

pub type Callback = unsafe extern "C" fn(*const u8, usize, isize);

#[derive(Deserialize)]
struct StartConfig {
    device_name: String,
    download_dir: String,
    /// Where Drift keeps its log, key and trusted devices (default %AppData%\Drift).
    #[serde(default)]
    data_dir: Option<String>,
    /// The device key's secret (hex), when the platform keeps it itself (Android: wrapped by
    /// a Keystore key). Without it, the key lives in data_dir, protected by the OS (DPAPI).
    #[serde(default)]
    device_secret: Option<String>,
}

/// A link or some text to send: kind is url | text | address | phone.
#[derive(Deserialize, Debug)]
struct TextItem {
    kind: String,
    body: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "c", rename_all = "snake_case")]
enum Cmd {
    Visibility { everyone: bool },
    Scan { on: bool },
    Send {
        device: String,
        paths: Vec<String>,
        #[serde(default)]
        texts: Vec<TextItem>,
    },
    Respond { share: u64, accept: bool },
    Cancel { share: u64 },
    /// New name and/or download folder; applies now (re-advertises if visible).
    Settings { device_name: Option<String>, download_dir: Option<String> },
    /// Trust the device on the other side of this share (it proved its Drift key in the session).
    Trust { share: u64 },
    /// Forget a trusted device (by key id).
    Untrust { id: String },
    /// BLE 0xFEF3 service data (hex) heard by the platform's scanner (Android).
    BleSeen { data: String },
    /// The app joined (net = Android network handle) or couldn't join (error) a `wifi_join`.
    WifiJoined {
        req: u64,
        #[serde(default)]
        net: Option<u64>,
        #[serde(default)]
        error: Option<String>,
    },
    /// The app started the Wi-Fi Direct group a `p2p_host` asked for, or couldn't (error).
    P2pHosted {
        req: u64,
        #[serde(default)]
        ssid: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        device_name: Option<String>,
        #[serde(default)]
        frequency: Option<i32>,
        #[serde(default)]
        error: Option<String>,
    },
    /// The app opened a `wifi_connect` socket (fd, now ours) or couldn't (error).
    WifiConnected {
        conn: u64,
        #[serde(default)]
        fd: Option<i32>,
        #[serde(default)]
        error: Option<String>,
    },
}

/// Ordered event delivery on one thread, so the UI sees events in the order they happened.
#[derive(Clone)]
pub(crate) struct Emit(std::sync::mpsc::Sender<Option<String>>);

impl Emit {
    pub(crate) fn send(&self, v: Value) {
        let _ = self.0.send(Some(v.to_string()));
    }
}

pub struct Core {
    rt: Option<tokio::runtime::Runtime>,
    cmd: mpsc::UnboundedSender<Cmd>,
    out: std::sync::mpsc::Sender<Option<String>>,
    emitter: Option<std::thread::JoinHandle<()>>,
}

// ------------------------------------------------------------------ C ABI

/// Start the core. `config` is `{"device_name": "...", "download_dir": "..."}`.
///
/// # Safety
/// `config` must be a NUL-terminated UTF-8 string; `callback` must stay callable until
/// `drift_stop` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drift_start(config: *const c_char, callback: Option<Callback>, user: isize) -> *mut Core {
    let Some(cb) = callback else { return std::ptr::null_mut() };
    if config.is_null() {
        return std::ptr::null_mut();
    }
    let raw = unsafe { CStr::from_ptr(config) }.to_string_lossy().into_owned();
    std::panic::catch_unwind(move || start(&raw, cb, user)).ok().flatten().map(Box::into_raw).unwrap_or(std::ptr::null_mut())
}

/// Send a command (JSON, NUL-terminated).
///
/// # Safety
/// `core` must come from `drift_start` and not be stopped; `json` must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drift_command(core: *mut Core, json: *const c_char) -> i32 {
    if core.is_null() || json.is_null() {
        return -2;
    }
    let core = unsafe { &*core };
    let text = unsafe { CStr::from_ptr(json) }.to_string_lossy();
    match serde_json::from_str::<Cmd>(&text) {
        Ok(c) => {
            tracing::debug!(?c, "command");
            let _ = core.cmd.send(c);
            0
        }
        Err(e) => {
            tracing::warn!("bad command {text}: {e}");
            -1
        }
    }
}

/// Stop the core. No callback runs after this returns.
///
/// # Safety
/// `core` must come from `drift_start`; it is freed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drift_stop(core: *mut Core) {
    if core.is_null() {
        return;
    }
    let mut core = unsafe { Box::from_raw(core) };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(rt) = core.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
        let _ = core.out.send(None);
        if let Some(t) = core.emitter.take() {
            let _ = t.join();
        }
    }));
}

// ------------------------------------------------------------------ setup

fn init_log(dir: &std::path::Path) {
    // %AppData%\Drift (Roaming): the install lives in %LocalAppData%\Drift, data never goes there.
    let _ = std::fs::create_dir_all(dir);
    if let Ok(f) = std::fs::File::create(dir.join("core.log")) {
        let filter = tracing_subscriber::EnvFilter::try_from_env("DRIFT_LOG").unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_target(false).with_writer(Mutex::new(f)).try_init();
    }
}

fn start(raw: &str, cb: Callback, user: isize) -> Option<Box<Core>> {
    let cfg: StartConfig = match serde_json::from_str(raw) {
        Ok(c) => c,
        Err(e) => {
            init_log(&trust::data_dir());
            tracing::error!("bad config: {e}");
            return None;
        }
    };
    let dir = cfg.data_dir.as_ref().map(PathBuf::from).unwrap_or_else(trust::data_dir);
    init_log(&dir);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("drift-core")
        .on_thread_start(|| {
            // WinRT (Bluetooth, Wi-Fi Direct) from our own threads: join the MTA.
            #[cfg(windows)]
            unsafe {
                let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
            }
        })
        .build()
        .ok()?;

    let (out, out_rx) = std::sync::mpsc::channel::<Option<String>>();
    let emitter = std::thread::Builder::new()
        .name("drift-events".into())
        .spawn(move || {
            while let Ok(Some(json)) = out_rx.recv() {
                let _ = std::panic::catch_unwind(|| unsafe { cb(json.as_ptr(), json.len(), user) });
            }
        })
        .ok()?;

    let (cmd, cmd_rx) = mpsc::unbounded_channel();
    let emit = Emit(out.clone());
    emit.send(json!({ "t": "ready", "name": cfg.device_name }));
    tracing::info!(name = %cfg.device_name, dir = %cfg.download_dir, "core started");
    let device_key = match &cfg.device_secret {
        Some(h) => trust::unhex(h).and_then(|s| drift_core::trust::DeviceKey::from_secret(&s)),
        None => trust::device_key(&dir),
    }
    .map(Arc::new);
    tracing::info!(device = ?device_key, "identity");
    let app_cfg = drift_app::Config {
        device_name: cfg.device_name,
        download_dir: PathBuf::from(cfg.download_dir),
        ladder: drift_app::Config::default_ladder(),
        device_key,
    };
    #[cfg(target_os = "android")]
    android::install_bt_connector(emit.clone());
    #[cfg(target_os = "android")]
    android::install_bt_port(emit.clone(), &rt);
    #[cfg(target_os = "android")]
    drift_app::set_platform(Some(std::sync::Arc::new(android::AndroidPlatform { emit: emit.clone() })));
    rt.spawn(run(app_cfg, cmd_rx, emit, trust::Store::load(&dir)));
    Some(Box::new(Core { rt: Some(rt), cmd, out, emitter: Some(emitter) }))
}

// ------------------------------------------------------------------ the core loop

/// Ids for outgoing shares; incoming ones come from the service (small numbers), so these
/// start high and never collide.
static NEXT_SEND: AtomicU64 = AtomicU64::new(1 << 40);

#[derive(Clone)]
struct Shared {
    emit: Emit,
    /// Incoming shares waiting for accept/decline.
    decisions: Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>,
    /// Outgoing shares in flight (task, cancel signal).
    sends: Arc<Mutex<HashMap<u64, (tokio::task::AbortHandle, Arc<tokio::sync::Notify>)>>>,
    /// Shares whose outcome the UI already has (exactly one `done` per share).
    done: Arc<Mutex<HashSet<u64>>>,
    /// Last seen nearby devices (kept after scanning stops: the UI stops the scan, then sends).
    devices: Arc<Mutex<HashMap<String, NearbyDevice>>>,
    health: Arc<drift_net::Health>,
    /// Our own endpoint id while visible: scanning must not list this PC itself.
    own_id: Arc<Mutex<Option<String>>>,
    /// Trusted Drift devices, and the device key each share's peer proved (for "Always accept").
    trust: Arc<Mutex<trust::Store>>,
    peer_keys: Arc<Mutex<HashMap<u64, (String, drift_core::trust::PeerKey, Option<[u8; 6]>)>>>,
}

impl Shared {
    fn finish(&self, share: u64, outcome: &str, error: Option<String>) {
        if !self.done.lock().unwrap().insert(share) {
            return;
        }
        tracing::info!(share, outcome, ?error, "share ended");
        let mut v = json!({ "t": "done", "share": share, "outcome": outcome });
        if let Some(e) = error {
            v["error"] = json!(e);
        }
        self.emit.send(v);
    }
}

async fn run(mut cfg: drift_app::Config, mut cmds: mpsc::UnboundedReceiver<Cmd>, emit: Emit, store: trust::Store) {
    let sh = Shared {
        emit,
        decisions: Arc::default(),
        sends: Arc::default(),
        done: Arc::default(),
        devices: Arc::default(),
        health: Arc::new(drift_net::Health::default()),
        own_id: Arc::default(),
        trust: Arc::new(Mutex::new(store)),
        peer_keys: Arc::default(),
    };
    sh.emit.send(sh.trust.lock().unwrap().event()); // the UI's Trusted devices list
    let mut service: Option<drift_app::Service> = None;
    let mut _discovery: Option<nearby::Discovery> = None; // held: dropping it stops the beacon and scanners

    // Watchdog: while visible, bring back what the OS stopped under us (a Bluetooth radio restart
    // kills Windows' advertisement for good; until now only hiding and showing again fixed it).
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let c = tokio::select! {
            c = cmds.recv() => match c { Some(c) => c, None => break },
            _ = watchdog.tick() => {
                if service.as_ref().is_some_and(|s| s.needs_restart() && !s.busy()) {
                    tracing::warn!("Bluetooth advertising stopped under us (radio restarted?); restarting the receiver");
                    service = None;
                    match drift_app::Service::start(cfg.clone()).await {
                        Ok((s, rx)) => {
                            tokio::spawn(forward_service(rx, sh.clone()));
                            service = Some(s);
                        }
                        Err(e) => tracing::warn!("receiver restart failed (will retry): {e:#}"),
                    }
                }
                continue;
            }
        };
        match c {
            Cmd::Visibility { everyone: true } if service.is_none() => match drift_app::Service::start(cfg.clone()).await {
                Ok((s, rx)) => {
                    tokio::spawn(forward_service(rx, sh.clone()));
                    service = Some(s);
                }
                Err(e) => {
                    tracing::error!("receiver failed to start: {e:#}");
                    sh.emit.send(json!({ "t": "warning", "text": "Phones can't find this PC right now", "detail": "Check that Bluetooth and Wi-Fi are on." }));
                }
            },
            Cmd::Visibility { everyone: true } => {}
            Cmd::Visibility { everyone: false } => {
                if service.take().is_some() {
                    tracing::info!("hidden: stopped advertising");
                }
            }
            Cmd::Scan { on: true } => {
                _discovery = None;
                sh.devices.lock().unwrap().clear();
                // trusted Drift phones advertise no name or address, just a hint only we recognise
                let store = sh.trust.clone();
                let resolver: nearby::Resolver = Arc::new(move |info| store.lock().unwrap().resolve(info));
                match nearby::discover_with(Some(resolver)) {
                    Ok((d, rx)) => {
                        tokio::spawn(forward_nearby(rx, sh.clone()));
                        _discovery = Some(d);
                    }
                    Err(e) => {
                        tracing::error!("discovery failed: {e:#}");
                        sh.emit.send(json!({ "t": "warning", "text": "Can't look for devices", "detail": "Check your network connection." }));
                    }
                }
            }
            Cmd::Scan { on: false } => _discovery = None,
            Cmd::Send { device, paths, texts } => {
                _discovery = None; // the beacon and scanners must be off before connecting
                send(&sh, &cfg.device_name, cfg.device_key.clone(), device, paths, texts);
            }
            Cmd::Respond { share, accept } => {
                if let Some(d) = sh.decisions.lock().unwrap().remove(&share) {
                    let _ = d.send(accept);
                    if accept {
                        sh.emit.send(json!({ "t": "accepted", "share": share, "direction": "incoming" }));
                    }
                }
            }
            Cmd::Settings { device_name, download_dir } => {
                let mut changed = false;
                if let Some(n) = device_name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty() && *n != cfg.device_name) {
                    cfg.device_name = n;
                    changed = true;
                }
                if let Some(d) = download_dir.map(PathBuf::from).filter(|d| *d != cfg.download_dir) {
                    cfg.download_dir = d;
                    changed = true;
                }
                tracing::info!(name = %cfg.device_name, dir = %cfg.download_dir.display(), changed, "settings");
                // The receiver advertises the name and owns the folder: restart it (running shares carry on).
                if changed && service.take().is_some() {
                    match drift_app::Service::start(cfg.clone()).await {
                        Ok((s, rx)) => {
                            tokio::spawn(forward_service(rx, sh.clone()));
                            service = Some(s);
                        }
                        Err(e) => {
                            tracing::error!("receiver failed to restart: {e:#}");
                            sh.emit.send(json!({ "t": "warning", "text": "Phones can't find this PC right now", "detail": "Check that Bluetooth and Wi-Fi are on." }))
                        }
                    }
                }
            }
            Cmd::Trust { share } => {
                let peer = sh.peer_keys.lock().unwrap().get(&share).cloned();
                if let Some((name, key, mac)) = peer {
                    tracing::info!(share, id = %key.id_hex(), %name, ?mac, "trusted");
                    sh.trust.lock().unwrap().add(&name, &key, mac);
                    sh.emit.send(sh.trust.lock().unwrap().event());
                }
            }
            Cmd::WifiJoined { req, net, error } => {
                #[cfg(target_os = "android")]
                android::wifi_joined(req, net, error);
                #[cfg(not(target_os = "android"))]
                let _ = (req, net, error);
            }
            Cmd::P2pHosted { req, ssid, password, device_name, frequency, error } => {
                #[cfg(target_os = "android")]
                android::p2p_hosted(req, ssid.zip(password), device_name, frequency.unwrap_or(-1), error);
                #[cfg(not(target_os = "android"))]
                let _ = (req, ssid, password, device_name, frequency, error);
            }
            Cmd::WifiConnected { conn, fd, error } => {
                #[cfg(target_os = "android")]
                android::wifi_connected(conn, fd, error);
                #[cfg(not(target_os = "android"))]
                let _ = (conn, fd, error);
            }
            Cmd::BleSeen { data } => {
                if let Some(b) = trust::unhex(&data) {
                    nearby::feed_ble(b);
                }
            }
            Cmd::Untrust { id } => {
                sh.trust.lock().unwrap().remove(&id);
                sh.emit.send(sh.trust.lock().unwrap().event());
            }
            Cmd::Cancel { share } => {
                let send = sh.sends.lock().unwrap().get(&share).cloned();
                if let Some((task, signal)) = send {
                    // Connected: the phone is told. Still connecting: there's no one to tell.
                    signal.notify_one();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        task.abort();
                    });
                } else if let Some(d) = sh.decisions.lock().unwrap().remove(&share) {
                    let _ = d.send(false);
                } else if let Some(s) = &service {
                    s.cancel(share);
                }
                sh.finish(share, "cancelled", None);
            }
        }
    }
}

/// Lasting problems become a card in the UI: a short title and a one-line reason. Known warnings
/// get plain words; housekeeping notes return None (log only). The full text stays in core.log.
fn short_warning(w: &str) -> Option<(String, String)> {
    let (t, d) = if w.starts_with("removed ") {
        return None;
    } else if w.contains("Quick Share is running") {
        ("Quit Google Quick Share", "Phones may reach it instead of Drift.")
    } else if w.contains("DNS") {
        ("Your VPN lost DNS", "Websites may not load. Reconnecting the VPN fixes it.")
    } else if w.contains("firewall") {
        ("The firewall blocked the fast link", "Transfers will use your Wi-Fi instead.")
    } else if w.starts_with("Bluetooth") {
        ("Bluetooth is off or busy", "Turn it on so phones can reach Drift.")
    } else if w.starts_with("mDNS") {
        ("Wi-Fi discovery is off", "Phones will find Drift over Bluetooth.")
    } else if w.contains("route") {
        ("A leftover network route needs a reset", "")
    } else {
        let first = w.split([':', '.', '(']).next().unwrap_or(w).trim();
        return Some((first.chars().take(44).collect(), String::new()));
    };
    Some((t.into(), d.into()))
}

fn text_kind(k: drift_core::sharing::TextKind) -> &'static str {
    use drift_core::sharing::TextKind as K;
    match k {
        K::Url => "url",
        K::Address => "address",
        K::PhoneNumber => "phone",
        _ => "text",
    }
}

fn kind(t: DeviceType) -> &'static str {
    match t {
        DeviceType::Phone => "phone",
        DeviceType::Tablet => "tablet",
        DeviceType::Laptop => "laptop",
        _ => "unknown",
    }
}

fn rung_name(r: Rung) -> &'static str {
    match r {
        Rung::PcHostedHotspot => "hotspot",
        Rung::PcHostedDirect => "direct",
        Rung::PhoneHosted => "phone_hosted",
        Rung::PeerHostedHotspot => "hotspot",
        Rung::Lan | Rung::PeerHostedLan => "lan",
    }
}

/// Send-side medium labels are `Rung::label()`s (or the phone's own LAN path).
fn label_name(label: &str) -> &'static str {
    if label == drift_net::send::SAME_HOTSPOT {
        return "same_hotspot";
    }
    [Rung::PcHostedHotspot, Rung::PcHostedDirect, Rung::PhoneHosted, Rung::PeerHostedHotspot, Rung::Lan, Rung::PeerHostedLan]
        .into_iter()
        .find(|r| r.label() == label)
        .map(rung_name)
        .unwrap_or(if label.contains("LAN") { "lan" } else if label.contains("Hotspot") { "hotspot" } else { "bluetooth" })
}

/// Progress at most ~12 times a second per share (the UI animates between updates).
#[derive(Default)]
struct Throttle(HashMap<u64, Instant>);

impl Throttle {
    fn ready(&mut self, share: u64, done: u64, total: u64) -> bool {
        let now = Instant::now();
        let last = self.0.entry(share).or_insert(now - Duration::from_secs(1));
        if done >= total || now.duration_since(*last) >= Duration::from_millis(80) {
            *last = now;
            true
        } else {
            false
        }
    }
}

async fn forward_service(mut rx: mpsc::UnboundedReceiver<ServiceEvent>, sh: Shared) {
    let mut boot: HashMap<u64, Bootstrap> = HashMap::new();
    // each share's current link (name, setup ms), to repeat once the UI knows the share
    let mut links: HashMap<u64, (&'static str, u64)> = HashMap::new();
    let mut bt_addrs: HashMap<u64, Option<[u8; 6]>> = HashMap::new();
    let mut throttle = Throttle::default();
    while let Some(ev) = rx.recv().await {
        match ev {
            ServiceEvent::Started { endpoint_id, mdns, ble, bluetooth } => {
                tracing::info!(%endpoint_id, mdns, ?ble, bluetooth, "visible");
                *sh.own_id.lock().unwrap() = Some(endpoint_id);
            }
            ServiceEvent::Warning(w) => {
                tracing::warn!("{w}");
                // Housekeeping notes stay in the log; the UI only shows problems, in a few words.
                if let Some((title, detail)) = short_warning(&w) {
                    // a code the UI can offer a fix for (F11: quit Google's Quick Share)
                    let code = w.contains("Quick Share is running").then_some("quick_share");
                    sh.emit.send(json!({ "t": "warning", "text": title, "detail": detail, "code": code }));
                }
            }
            ServiceEvent::Connection { id, bootstrap, peer_addr } => {
                tracing::info!(id, ?bootstrap, %peer_addr, "connection");
                if matches!(bootstrap, Bootstrap::Bluetooth) {
                    bt_addrs.insert(id, trust::parse_mac(&peer_addr));
                }
                boot.insert(id, bootstrap);
            }
            ServiceEvent::Share { id, event } => match event {
                ReceiveEvent::Request { peer, pin, files, texts, decision, drift, peer_key } => {
                    // A trusted Drift device (its key verified in this session) skips the prompt.
                    let auto = peer_key.as_ref().map(|k| sh.trust.lock().unwrap().auto_accepts(k)).unwrap_or(false);
                    let trusted = peer_key.as_ref().is_some_and(|k| sh.trust.lock().unwrap().find(k).is_some());
                    tracing::info!(id, %peer, files = files.len(), texts = texts.len(), drift, trusted, auto, "incoming");
                    // remember how to reach it: a phone can't advertise its own Bluetooth address
                    if let (Some(k), true, Some(Some(mac))) = (peer_key.as_ref(), trusted, bt_addrs.get(&id)) {
                        sh.trust.lock().unwrap().seen_at(k, *mac);
                    }
                    if let Some(k) = peer_key { sh.peer_keys.lock().unwrap().insert(id, (peer.clone(), k, bt_addrs.get(&id).copied().flatten())); }
                    let files: Vec<Value> = files.iter().map(|f| json!({ "name": f.name, "size": f.size.max(0) })).collect();
                    sh.emit.send(json!({
                        "t": "incoming", "share": id, "from": peer, "kind": "phone", "pin": pin,
                        "files": files, "texts": texts.len(), "text_kind": texts.first().map(|t| text_kind(t.kind.clone())),
                        "direction": "incoming", "drift": drift, "trusted": trusted, "auto": auto,
                    }));
                    if let Some((name, ms)) = links.get(&id) {
                        sh.emit.send(json!({ "t": "medium", "share": id, "medium": name, "setup_ms": ms }));
                    }
                    if auto {
                        let _ = decision.send(true);
                        sh.emit.send(json!({ "t": "accepted", "share": id, "direction": "incoming" }));
                    } else {
                        sh.decisions.lock().unwrap().insert(id, decision);
                    }
                }
                ReceiveEvent::MediumChanged { medium, setup } => {
                    let name = match medium {
                        Medium::Bootstrap => match boot.get(&id) {
                            Some(Bootstrap::Lan) => "lan",
                            _ => "bluetooth",
                        },
                        Medium::Upgraded(r) => rung_name(r),
                        Medium::SameHotspot => "same_hotspot",
                    };
                    let ms = setup.map(|d| d.as_millis() as u64).unwrap_or(0);
                    // the link can be up before the share is (12 ms over a hotspot): the UI may not know
                    // the share yet, so it's repeated right after "incoming" too
                    links.insert(id, (name, ms));
                    sh.emit.send(json!({ "t": "medium", "share": id, "medium": name, "setup_ms": ms }));
                }
                ReceiveEvent::Progress { received, total, bytes_per_sec } => {
                    if throttle.ready(id, received, total) {
                        sh.emit.send(json!({ "t": "progress", "share": id, "done": received, "total": total, "bps": bytes_per_sec }));
                    }
                }
                ReceiveEvent::Text { item, content } => {
                    sh.emit.send(json!({ "t": "text", "share": id, "text": content, "text_kind": text_kind(item.kind) }))
                }
                ReceiveEvent::FileSaved { path, bytes } => {
                    sh.emit.send(json!({ "t": "saved", "share": id, "path": path.display().to_string(), "done": bytes }));
                }
                ReceiveEvent::Completed => sh.finish(id, "completed", None),
                ReceiveEvent::Declined => sh.finish(id, "declined", None),
                ReceiveEvent::Cancelled => sh.finish(id, "cancelled", None),
                ReceiveEvent::Stalled { medium, after } => {
                    tracing::warn!(id, ?after, stalled_on = ?matches!(medium, Medium::Bootstrap), "stalled");
                    sh.finish(id, "stalled", None);
                }
            },
            ServiceEvent::Finished { id, error } => {
                sh.decisions.lock().unwrap().remove(&id);
                boot.remove(&id);
                throttle.0.remove(&id);
                match error {
                    Some(e) => {
                        tracing::warn!(id, "receive failed: {e}");
                        sh.finish(id, "failed", Some("The transfer stopped".into()))
                    }
                    None => sh.finish(id, "cancelled", None), // ended before anything was sent
                }
            }
        }
    }
}

async fn forward_nearby(mut rx: mpsc::UnboundedReceiver<NearbyEvent>, sh: Shared) {
    while let Some(ev) = rx.recv().await {
        let own = sh.own_id.lock().unwrap().clone();
        let id = match &ev {
            NearbyEvent::Found(d) | NearbyEvent::Updated(d) => &d.endpoint_id,
            NearbyEvent::Lost { endpoint_id } => endpoint_id,
        };
        if own.as_deref() == Some(id.as_str()) {
            continue; // that's us
        }
        match ev {
            NearbyEvent::Found(d) => {
                tracing::info!(id = %d.endpoint_id, name = %d.name, routes = ?d.routes, "found");
                sh.emit.send(json!({ "t": "device_found", "id": d.endpoint_id, "name": d.name, "kind": kind(d.device_type), "drift": d.drift }));
                sh.devices.lock().unwrap().insert(d.endpoint_id.clone(), d);
            }
            NearbyEvent::Updated(d) => {
                sh.devices.lock().unwrap().insert(d.endpoint_id.clone(), d);
            }
            NearbyEvent::Lost { endpoint_id } => {
                sh.devices.lock().unwrap().remove(&endpoint_id);
                sh.emit.send(json!({ "t": "device_lost", "id": endpoint_id }));
            }
        }
    }
}

fn send(sh: &Shared, our_name: &str, device_key: Option<Arc<drift_core::trust::DeviceKey>>, device: String, paths: Vec<String>, texts: Vec<TextItem>) {
    let id = NEXT_SEND.fetch_add(1, Ordering::Relaxed);
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let files: Vec<Value> = paths
        .iter()
        .map(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            json!({ "name": name, "size": std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) })
        })
        .collect();
    use drift_core::proto::sharing::text_metadata::Type as K;
    let texts: Vec<drift_core::send::OutgoingText> = texts
        .iter()
        .filter(|t| !t.body.is_empty())
        .map(|t| {
            let kind = match t.kind.as_str() { "url" => K::Url, "address" => K::Address, "phone" => K::PhoneNumber, _ => K::Text };
            drift_core::send::OutgoingText::new(kind, &t.body)
        })
        .collect();
    let dev = sh.devices.lock().unwrap().get(&device).cloned();
    let name = dev.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| "Phone".into());
    // The share exists from now on (connecting: no code yet).
    sh.emit.send(json!({ "t": "waiting", "share": id, "files": files, "direction": "outgoing", "name": name }));
    let Some(dev) = dev else {
        sh.finish(id, "failed", Some(format!("{name} isn't nearby any more")));
        return;
    };
    tracing::info!(id, to = %dev.name, routes = ?dev.routes, files = paths.len(), "send");

    let sh2 = sh.clone();
    let our_name = our_name.to_string();
    let signal = Arc::new(tokio::sync::Notify::new());
    let signal_out = signal.clone();
    let task = tokio::spawn(async move {
        let sh = sh2;
        // A Drift receiver: if the link dies mid-share, start it again once, on a fresh connection, and
        // skip the fast link that just died (we won't host Wi-Fi Direct again: LAN or Bluetooth then).
        // A trusted receiver takes it without asking; the transfer stays the same on screen.
        let mut allowed: Option<Vec<drift_net::Rung>> = None;
        for attempt in 0..2u32 {
            let (pin_tx, mut pin_rx) = oneshot::channel::<(String, Route)>();
            let cancel = signal.clone();
            let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
            // with our device key: a Drift receiver can then trust this device ("Always accept")
            let local = drift_app::local_identity_with(drift_core::endpoint::random_endpoint_id(), device_key.clone());
            let health = sh.health.clone();
            let target = dev.clone();
            let (paths, texts, our_name, allow) = (paths.clone(), texts.clone(), our_name.clone(), allowed.clone());
            let fut = async move { nearby::send_items(&target, paths, texts, local, &our_name, drift_app::platform(), health, allow, pin_tx, ev_tx, Some(cancel)).await };
            tokio::pin!(fut);
            let mut boot = "bluetooth";
            let mut throttle = Throttle::default();
            let mut pin_open = true;
            let mut connected = false;
            let mut moved = false; // file data flowed this attempt
            let mut ended = false; // declined / cancelled / completed were reported
            // the link we're on now: an upgrade can land before the receiver accepts (24 ms on a hotspot)
            let link: std::sync::Mutex<Option<(&'static str, u64)>> = std::sync::Mutex::new(None);
            let mut handle = |ev: SendEvent, boot: &str, throttle: &mut Throttle| match ev {
                SendEvent::PeerKey(k) => { sh.peer_keys.lock().unwrap().insert(id, (dev.name.clone(), k, None)); }
                SendEvent::WaitingForAccept => {}
                SendEvent::Accepted => {
                    sh.emit.send(json!({ "t": "accepted", "share": id, "direction": "outgoing" }));
                    match *link.lock().unwrap() {
                        Some((name, ms)) => sh.emit.send(json!({ "t": "medium", "share": id, "medium": name, "setup_ms": ms })),
                        None => sh.emit.send(json!({ "t": "medium", "share": id, "medium": boot })),
                    }
                }
                SendEvent::MediumChanged { medium, setup } => {
                    *link.lock().unwrap() = Some((label_name(medium), setup.as_millis() as u64));
                    sh.emit.send(json!({ "t": "medium", "share": id, "medium": label_name(medium), "setup_ms": setup.as_millis() as u64 }));
                }
                SendEvent::Progress { sent, total, bytes_per_sec } => {
                    if sent > 0 {
                        moved = true;
                    }
                    if throttle.ready(id, sent, total) {
                        sh.emit.send(json!({ "t": "progress", "share": id, "done": sent, "total": total, "bps": bytes_per_sec }));
                    }
                }
                SendEvent::Declined(why) => {
                    tracing::info!(id, %why, "declined");
                    ended = true;
                    sh.finish(id, "declined", None);
                }
                SendEvent::Cancelled => { ended = true; sh.finish(id, "cancelled", None) }
                SendEvent::Completed => { ended = true; sh.finish(id, "completed", None) }
            };
            let result = loop {
                tokio::select! {
                    r = &mut fut => break r,
                    p = &mut pin_rx, if pin_open => {
                        pin_open = false;
                        if let Ok((pin, route)) = p {
                            connected = true;
                            boot = if matches!(route, Route::Lan { .. }) { "lan" } else { "bluetooth" };
                            sh.emit.send(json!({ "t": "waiting", "share": id, "pin": pin, "direction": "outgoing", "name": dev.name }));
                        }
                    }
                    Some(ev) = ev_rx.recv() => handle(ev, boot, &mut throttle),
                }
            };
            while let Ok(ev) = ev_rx.try_recv() {
                handle(ev, boot, &mut throttle);
            }
            let last_link = link.lock().unwrap().map(|(n, _)| n);
            drop(handle);
            match result {
                Ok(()) => {
                    sh.sends.lock().unwrap().remove(&id);
                    sh.finish(id, "completed", None);
                    return;
                }
                Err(e) if attempt == 0 && dev.drift && connected && moved && !ended => {
                    tracing::warn!(id, ?last_link, "link lost mid-share: {e:#}; starting it again once");
                    if matches!(last_link, Some("phone_hosted" | "direct" | "hotspot")) {
                        allowed = Some(vec![drift_net::Rung::Lan]);
                    }
                    sh.emit.send(json!({ "t": "waiting", "share": id, "direction": "outgoing", "name": dev.name }));
                    continue;
                }
                Err(e) => {
                    sh.sends.lock().unwrap().remove(&id);
                    tracing::warn!(id, "send failed: {e:#}");
                    let why = if connected {
                        format!("Lost the link to {}", dev.name)
                    } else if format!("{e:#}").contains("Bluetooth connect failed") && dev.routes.iter().all(|r| !matches!(r, Route::Lan { .. })) {
                        // F8: a PC's Bluetooth can stop taking connections after Wi-Fi Direct sessions while
                        // still advertising; only a radio restart on the PC fixes it, so say where that is
                        format!("Couldn't reach {} over Bluetooth. On the PC: Drift Settings › Restart Bluetooth.", dev.name)
                    } else {
                        format!("Couldn't reach {}", dev.name)
                    };
                    sh.finish(id, "failed", Some(why));
                    return;
                }
            }
        }
    });
    sh.sends.lock().unwrap().insert(id, (task.abort_handle(), signal_out));
}

/// Drift's device key and the trusted-devices list, in %AppData%\Drift.
mod trust {
    use std::path::{Path, PathBuf};

    use drift_core::trust::{DeviceKey, PeerKey};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    pub fn data_dir() -> PathBuf {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Drift")).unwrap_or_else(std::env::temp_dir)
    }

    /// "(12:34:56:78:9A:BC)" (Windows' RFCOMM host name) or "12:34:..." → bytes, MSB first.
    pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
        let h: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        unhex(&h)?.try_into().ok()
    }

    pub fn unhex(s: &str) -> Option<Vec<u8>> {
        if s.len() % 2 != 0 {
            return None;
        }
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Load this install's key, or make one. The secret is stored encrypted to the Windows user
    /// (DPAPI); if it can't be read it's replaced (trusted peers then need to re-trust us).
    pub fn device_key(dir: &Path) -> Option<DeviceKey> {
        let path = dir.join("device.key");
        if let Ok(stored) = std::fs::read(&path)
            && let Some(k) = unprotect(&stored).and_then(|s| DeviceKey::from_secret(&s))
        {
            return Some(k);
        }
        let k = DeviceKey::generate();
        let _ = std::fs::create_dir_all(dir);
        match protect(&k.secret()) {
            Some(p) if std::fs::write(&path, &p).is_ok() => Some(k),
            _ => {
                tracing::warn!("couldn't store the device key; trust is off this session");
                None
            }
        }
    }

    #[cfg(windows)]
    fn protect(b: &[u8]) -> Option<Vec<u8>> {
        drift_platform_win::protect::protect(b).ok()
    }
    #[cfg(windows)]
    fn unprotect(b: &[u8]) -> Option<Vec<u8>> {
        drift_platform_win::protect::unprotect(b).ok()
    }
    #[cfg(not(windows))]
    fn protect(_: &[u8]) -> Option<Vec<u8>> {
        None // Linux: Secret Service (milestone 4)
    }
    #[cfg(not(windows))]
    fn unprotect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }

    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct Trusted {
        /// key id (hex), as Drift shows it
        pub id: String,
        pub name: String,
        /// the device's public key (hex, SEC1): a matching id alone is never enough
        pub public: String,
        pub auto_accept: bool,
        /// Bluetooth address (hex, MSB first) seen when it connected to us: how we reach a phone,
        /// since Android doesn't advertise it.
        #[serde(default)]
        pub mac: Option<String>,
    }

    #[derive(Default)]
    pub struct Store {
        path: PathBuf,
        devices: Vec<Trusted>,
    }

    impl Store {
        pub fn load(dir: &Path) -> Self {
            let path = dir.join("trusted.json");
            let devices = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            Self { path, devices }
        }

        fn save(&self) {
            if let Ok(b) = serde_json::to_vec_pretty(&self.devices) {
                let _ = std::fs::write(&self.path, b);
            }
        }

        pub fn find(&self, k: &PeerKey) -> Option<&Trusted> {
            let (id, public) = (k.id_hex(), hex(&k.public));
            self.devices.iter().find(|d| d.id == id && d.public == public)
        }

        pub fn auto_accepts(&self, k: &PeerKey) -> bool {
            self.find(k).is_some_and(|d| d.auto_accept)
        }

        pub fn add(&mut self, name: &str, k: &PeerKey, mac: Option<[u8; 6]>) {
            let id = k.id_hex();
            let old_mac = self.devices.iter().find(|d| d.id == id).and_then(|d| d.mac.clone());
            self.devices.retain(|d| d.id != id);
            let mac = mac.map(|m| hex(&m)).or(old_mac);
            self.devices.push(Trusted { id, name: name.into(), public: hex(&k.public), auto_accept: true, mac });
            self.save();
        }

        /// A trusted device connected from this Bluetooth address: keep it current.
        pub fn seen_at(&mut self, k: &PeerKey, mac: [u8; 6]) {
            let (id, mac) = (k.id_hex(), hex(&mac));
            if let Some(d) = self.devices.iter_mut().find(|d| d.id == id && d.mac.as_deref() != Some(&mac)) {
                d.mac = Some(mac);
                self.save();
            }
        }

        /// A trusted device's advert (Drift tag + presence hint): its name and Bluetooth address.
        pub fn resolve(&self, info: &drift_core::endpoint::EndpointInfo) -> Option<(String, [u8; 6])> {
            let (salt, hint) = info.drift_hint()?;
            self.devices.iter().find_map(|d| {
                let public = unhex(&d.public)?;
                let mac: [u8; 6] = unhex(d.mac.as_ref()?)?.try_into().ok()?;
                (drift_core::trust::presence_hint(&public, salt) == hint).then(|| (d.name.clone(), mac))
            })
        }

        pub fn remove(&mut self, id: &str) {
            self.devices.retain(|d| d.id != id);
            self.save();
        }

        /// {"t":"trusted","devices":[{id,name,auto}]}
        pub fn event(&self) -> Value {
            json!({ "t": "trusted", "devices": self.devices.iter().map(|d| json!({ "id": d.id, "name": d.name, "auto": d.auto_accept })).collect::<Vec<_>>() })
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn trusted_phone_is_recognised_from_its_hinted_advert() {
        use drift_core::endpoint::{DeviceType, EndpointInfo};
        use drift_core::trust::{DeviceKey, PeerKey, presence_hint};
        let dir = std::env::temp_dir().join(format!("drift-trust-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut store = super::trust::Store::load(&dir);
        let phone = DeviceKey::generate();
        let peer = PeerKey { id: phone.key_id(), public: phone.public() };
        store.add("Asha's phone", &peer, Some([0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]));
        let salt = [9, 4];
        let info = EndpointInfo::new_drift("x", DeviceType::Phone).with_hint(salt, presence_hint(&phone.public(), salt));
        assert_eq!(store.resolve(&info), Some(("Asha's phone".into(), [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC])));
        let stranger = DeviceKey::generate();
        let other = EndpointInfo::new_drift("x", DeviceType::Phone).with_hint(salt, presence_hint(&stranger.public(), salt));
        assert_eq!(store.resolve(&other), None);
        assert_eq!(super::trust::parse_mac("(12:34:56:78:9A:BC)"), Some([0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn commands_parse() {
        assert!(matches!(serde_json::from_str::<Cmd>(r#"{"c":"visibility","everyone":true}"#).unwrap(), Cmd::Visibility { everyone: true }));
        assert!(matches!(serde_json::from_str::<Cmd>(r#"{"c":"respond","share":7,"accept":false}"#).unwrap(), Cmd::Respond { share: 7, accept: false }));
        let Cmd::Send { device, paths, .. } = serde_json::from_str(r#"{"c":"send","device":"ABCD","paths":["C:\\a.txt"]}"#).unwrap() else { panic!() };
        assert_eq!((device.as_str(), paths.len()), ("ABCD", 1));
        assert!(serde_json::from_str::<Cmd>(r#"{"c":"nope"}"#).is_err());
        let Cmd::Settings { device_name, download_dir } = serde_json::from_str(r#"{"c":"settings","device_name":"Desk"}"#).unwrap() else { panic!() };
        assert_eq!((device_name.as_deref(), download_dir), (Some("Desk"), None));
    }

    #[test]
    fn medium_labels_map_to_protocol_names() {
        assert_eq!(label_name(Rung::PcHostedHotspot.label()), "hotspot");
        assert_eq!(label_name(Rung::Lan.label()), "lan");
        assert_eq!(label_name("Wi-Fi LAN"), "lan");
        assert_eq!(label_name("Bluetooth"), "bluetooth");
    }

    #[test]
    fn warnings_fit_the_notice() {
        assert_eq!(short_warning("removed 1 stale Wi-Fi Direct route(s)"), None);
        let long = "Google's Quick Share is running. It uses the same Bluetooth service, so phones may not find Drift. Quit it from the tray.";
        assert_eq!(short_warning(long).unwrap().0, "Quit Google Quick Share");
        assert!(short_warning("TCP accept: some very long operating system error text that goes on and on").unwrap().0.len() <= 44);
    }

    #[test]
    fn throttle_lets_the_last_update_through() {
        let mut t = Throttle::default();
        assert!(t.ready(1, 10, 100));
        assert!(!t.ready(1, 20, 100));
        assert!(t.ready(1, 100, 100), "completion always reported");
    }

    /// The C ABI end to end without radios: start, a command, stop; events arrive in order.
    #[test]
    fn abi_round_trip() {
        static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
        unsafe extern "C" fn cb(p: *const u8, n: usize, user: isize) {
            assert_eq!(user, 42);
            let s = std::str::from_utf8(unsafe { std::slice::from_raw_parts(p, n) }).unwrap().to_string();
            SEEN.lock().unwrap().push(s);
        }
        let dir = std::env::temp_dir();
        let cfg = std::ffi::CString::new(json!({ "device_name": "Test", "download_dir": dir, "data_dir": dir }).to_string()).unwrap();
        let h = unsafe { drift_start(cfg.as_ptr(), Some(cb), 42) };
        assert!(!h.is_null());
        let bad = std::ffi::CString::new("{}").unwrap();
        assert_eq!(unsafe { drift_command(h, bad.as_ptr()) }, -1);
        let c = std::ffi::CString::new(r#"{"c":"send","device":"GONE","paths":[]}"#).unwrap();
        assert_eq!(unsafe { drift_command(h, c.as_ptr()) }, 0);
        std::thread::sleep(Duration::from_millis(300));
        unsafe { drift_stop(h) };
        let seen = SEEN.lock().unwrap().clone();
        assert!(seen[0].contains(r#""t":"ready""#), "{seen:?}");
        assert!(seen.iter().any(|s| s.contains(r#""t":"waiting""#)), "{seen:?}");
        assert!(seen.iter().any(|s| s.contains(r#""outcome":"failed""#)), "{seen:?}");
    }
}
