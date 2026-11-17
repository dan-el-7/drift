//! Sending side for the UI: a live list of nearby receivers and `send`.
//!
//! Discovery wakes Android receivers with the fast-init beacon, then merges
//! what they advertise over BLE (named adverts carry the Bluetooth address)
//! and mDNS (open networks). Each receiver gets one entry; it is reported lost
//! when nothing has been heard from it for a while.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use drift_core::ble;
use drift_core::endpoint::{DeviceType, EndpointInfo};
use drift_net::send::SendEvent;
use drift_net::{Health, LocalIdentity, Platform};
use tokio::sync::mpsc;

/// Platforms whose Bluetooth lives outside Rust (Android: the app scans and connects in Kotlin).
/// `feed_ble` takes 0xFEF3 service data heard while discovering; the connector opens an RFCOMM
/// link (address MSB first, service UUID) and hands back a stream bridged to it.
pub type BtConnector = Arc<
    dyn Fn([u8; 6], [u8; 16]) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<tokio::net::TcpStream>> + Send>> + Send + Sync,
>;
static BT_CONNECTOR: std::sync::RwLock<Option<BtConnector>> = std::sync::RwLock::new(None);
static BLE_FEED: std::sync::Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>> = std::sync::Mutex::new(None);

pub fn set_bt_connector(c: Option<BtConnector>) {
    *BT_CONNECTOR.write().unwrap() = c;
}

/// BLE service data (0xFEF3) seen by the platform's own scanner; used while `discover` runs.
pub fn feed_ble(data: Vec<u8>) {
    if let Some(tx) = BLE_FEED.lock().unwrap().as_ref() {
        let _ = tx.send(data);
    }
}

/// How to reach a receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Bluetooth Classic RFCOMM (address MSB first). Works anywhere.
    Bluetooth { mac: [u8; 6] },
    /// TCP on the shared network (found via mDNS).
    Lan { addr: SocketAddr },
    /// A trusted Drift phone, on Drift's own RFCOMM service. Android can't advertise its Bluetooth
    /// address, so we know it from when it last connected to us (trust::Store keeps it).
    DriftBluetooth { mac: [u8; 6] },
}

/// RFCOMM service of the Drift companion (not "NearbySharing": Google's Quick Share owns that one
/// on the phone).
pub const DRIFT_BT_SERVICE: &str = "DriftSharing";

/// Recognises trusted Drift devices from their advert's presence hint: (name, Bluetooth address).
pub type Resolver = Arc<dyn Fn(&EndpointInfo) -> Option<(String, [u8; 6])> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NearbyDevice {
    /// Nearby endpoint id; stable for one advertising session.
    pub endpoint_id: String,
    pub name: String,
    pub device_type: DeviceType,
    /// Preferred first (LAN when available: no Bluetooth bootstrap needed).
    pub routes: Vec<Route>,
    /// It runs Drift (its endpoint info carries the Drift tag): trust and Drift-only paths apply.
    pub drift: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NearbyEvent {
    Found(NearbyDevice),
    Updated(NearbyDevice),
    Lost { endpoint_id: String },
}

const LOST_AFTER: Duration = Duration::from_secs(15);

/// Merges sightings into device entries; pure, unit-tested.
#[derive(Default)]
pub struct Roster {
    devices: HashMap<String, (NearbyDevice, Instant)>,
}

impl Roster {
    pub fn sighting(&mut self, endpoint_id: String, info: &EndpointInfo, route: Route, now: Instant) -> Option<NearbyEvent> {
        let name = info.name.clone()?; // hidden (contacts-only) receivers can't be targeted
        match self.devices.get_mut(&endpoint_id) {
            Some((dev, seen)) => {
                *seen = now;
                if dev.routes.contains(&route) {
                    return None;
                }
                dev.routes.push(route);
                dev.routes.sort_by_key(|r| !matches!(r, Route::Lan { .. })); // LAN first
                Some(NearbyEvent::Updated(dev.clone()))
            }
            None => {
                let dev = NearbyDevice { endpoint_id: endpoint_id.clone(), name, device_type: info.device_type, routes: vec![route], drift: info.drift_version().is_some() };
                self.devices.insert(endpoint_id, (dev.clone(), now));
                Some(NearbyEvent::Found(dev))
            }
        }
    }

    pub fn expire(&mut self, now: Instant) -> Vec<NearbyEvent> {
        let gone: Vec<String> =
            self.devices.iter().filter(|(_, (_, seen))| now.duration_since(*seen) >= LOST_AFTER).map(|(k, _)| k.clone()).collect();
        gone.into_iter()
            .map(|k| {
                self.devices.remove(&k);
                NearbyEvent::Lost { endpoint_id: k }
            })
            .collect()
    }
}

/// Keeps discovery (beacon, scanners, mDNS browse) running while held.
pub struct Discovery {
    _task: tokio::task::JoinHandle<()>,
    _mdns: mdns_sd::ServiceDaemon,
    #[cfg(windows)]
    _win: (Option<drift_platform_win::bt::BlePublisher>, Option<drift_platform_win::scan::BleScanner>),
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self._task.abort();
        let _ = self._mdns.shutdown();
        #[cfg(not(windows))]
        BLE_FEED.lock().unwrap().take();
    }
}

fn mdns_endpoint_id(instance: &str) -> Option<String> {
    use base64::Engine;
    let label = instance.split('.').next()?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(label).ok()?;
    (raw.len() >= 5 && raw[0] == 0x23).then(|| String::from_utf8_lossy(&raw[1..5]).into_owned())
}

pub fn discover() -> Result<(Discovery, mpsc::UnboundedReceiver<NearbyEvent>)> {
    discover_with(None)
}

#[cfg(windows)]
fn start_bluetooth<S, B>(scanner: impl FnOnce() -> Result<S>, beacon: impl FnOnce() -> Result<B>) -> (Option<B>, Option<S>) {
    let scanner = scanner().inspect_err(|e| tracing::warn!("BLE scanning unavailable; mDNS discovery continues: {e:#}")).ok();
    let beacon = beacon().inspect_err(|e| tracing::warn!("BLE wake beacon unavailable; discovery continues: {e:#}")).ok();
    (beacon, scanner)
}

/// `discover`, also finding trusted Drift phones (no address in their advert) via `resolver`.
pub fn discover_with(resolver: Option<Resolver>) -> Result<(Discovery, mpsc::UnboundedReceiver<NearbyEvent>)> {
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let mdns = mdns_sd::ServiceDaemon::new()?;
    let mdns_rx = mdns.browse(drift_core::endpoint::SERVICE_TYPE)?;

    #[cfg(windows)]
    let (win, mut ble_rx) = {
        use drift_platform_win::{bt, scan};
        let (tx, rx) = mpsc::unbounded_channel();
        static UUIDS: [u16; 1] = [ble::COPRESENCE_UUID16];
        let win = start_bluetooth(
            || scan::scan_ble(&UUIDS, tx),
            || bt::start_ble(ble::FAST_INIT_UUID16, &ble::fast_init_beacon(ble::FastInitType::Notify, true), false),
        );
        (win, rx)
    };
    #[cfg(not(windows))]
    let mut ble_rx = {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        *BLE_FEED.lock().unwrap() = Some(tx);
        rx
    };

    let task = tokio::spawn(async move {
        let mut roster = Roster::default();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    while let Ok(ev) = mdns_rx.try_recv() {
                        if let mdns_sd::ServiceEvent::ServiceResolved(i) = ev {
                            let Some(eid) = mdns_endpoint_id(i.get_fullname()) else { continue };
                            let Some(info) = i.get_property_val_str("n").and_then(|n| {
                                use base64::Engine;
                                base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(n).ok()
                            }).and_then(|b| EndpointInfo::decode(&b).ok()) else { continue };
                            let Some(ip) = i.get_addresses_v4().into_iter().next() else { continue };
                            let route = Route::Lan { addr: SocketAddr::new(ip.into(), i.get_port()) };
                            if let Some(e) = roster.sighting(eid, &info, route, Instant::now()) { let _ = out_tx.send(e); }
                        }
                    }
                    for e in roster.expire(Instant::now()) { let _ = out_tx.send(e); }
                }
                Some(seen) = ble_rx.recv() => {
                    #[cfg(windows)]
                    let drift_platform_win::scan::Seen::Ble { data, .. } = seen else { continue };
                    #[cfg(not(windows))]
                    let data = seen;
                    let Ok(d) = ble::parse_advertisement(&data) else { continue };
                    let Ok(mut info) = EndpointInfo::decode(&d.endpoint_info) else { continue };
                    let eid = String::from_utf8_lossy(&d.endpoint_id).into_owned();
                    let route = match d.bt_mac {
                        Some(mac) => Route::Bluetooth { mac },
                        None => {
                            // a trusted Drift phone: nameless, addressless advert with our hint
                            let Some((name, mac)) = resolver.as_ref().and_then(|r| r(&info)) else { continue };
                            info.name = Some(name);
                            Route::DriftBluetooth { mac }
                        }
                    };
                    if let Some(e) = roster.sighting(eid, &info, route, Instant::now()) { let _ = out_tx.send(e); }
                }
            }
        }
    });

    Ok((
        Discovery {
            _task: task,
            _mdns: mdns,
            #[cfg(windows)]
            _win: win,
        },
        out_rx,
    ))
}

/// Send `paths` to `device`, trying its routes in order. `pin_tx` gets the PIN and the
/// route that connected; events report acceptance, medium changes, progress and the outcome.
pub async fn send(
    device: &NearbyDevice,
    paths: Vec<PathBuf>,
    local: LocalIdentity,
    our_name: &str,
    platform: Arc<dyn Platform>,
    health: Arc<Health>,
    allowed: Option<Vec<drift_net::Rung>>,
    pin_tx: tokio::sync::oneshot::Sender<(String, Route)>,
    events: mpsc::UnboundedSender<SendEvent>,
    cancel: Option<Arc<tokio::sync::Notify>>,
) -> Result<()> {
    send_items(device, paths, vec![], local, our_name, platform, health, allowed, pin_tx, events, cancel).await
}

/// `send`, plus links/text.
pub async fn send_items(
    device: &NearbyDevice,
    paths: Vec<PathBuf>,
    texts: Vec<drift_core::send::OutgoingText>,
    local: LocalIdentity,
    our_name: &str,
    platform: Arc<dyn Platform>,
    health: Arc<Health>,
    allowed: Option<Vec<drift_net::Rung>>,
    pin_tx: tokio::sync::oneshot::Sender<(String, Route)>,
    events: mpsc::UnboundedSender<SendEvent>,
    cancel: Option<Arc<tokio::sync::Notify>>,
) -> Result<()> {
    let info = EndpointInfo::new_drift(our_name, crate::DEVICE_TYPE).encode();
    let offer = drift_net::send::default_offer();
    let mut last_err = None;
    for route in &device.routes {
        let conn = match route {
            Route::Lan { addr } => match tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr)).await {
                Ok(Ok(sock)) => {
                    let _ = sock.set_nodelay(true);
                    drift_net::channel::tune(&sock);
                    drift_net::send::connect(sock, &local, &info, &offer).await
                }
                Ok(Err(e)) => Err(e.into()),
                Err(_) => Err(anyhow::anyhow!("LAN connect timed out")),
            },
            #[cfg(windows)]
            Route::Bluetooth { mac } | Route::DriftBluetooth { mac } => {
                use drift_platform_win::bt;
                let service = if matches!(route, Route::DriftBluetooth { .. }) { DRIFT_BT_SERVICE } else { crate::SERVICE_ID };
                let mut a = [0u8; 8];
                a[2..].copy_from_slice(mac);
                let addr = u64::from_be_bytes(a);
                let sock = tokio::task::spawn_blocking(move || bt::connect_rfcomm_blocking(addr, ble::name_uuid(service)))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r);
                match sock {
                    Ok(sock) => match bt::bridge(sock) {
                        Ok(stream) => drift_net::send::connect(stream, &local, &info, &offer).await,
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                }
            }
            #[cfg(not(windows))]
            Route::Bluetooth { mac } | Route::DriftBluetooth { mac } => {
                let service = if matches!(route, Route::DriftBluetooth { .. }) { DRIFT_BT_SERVICE } else { crate::SERVICE_ID };
                let connector = BT_CONNECTOR.read().unwrap().clone();
                match connector {
                    Some(c) => match c(*mac, ble::name_uuid(service)).await {
                        Ok(stream) => drift_net::send::connect(stream, &local, &info, &offer).await,
                        Err(e) => Err(e),
                    },
                    None => Err(anyhow::anyhow!("Bluetooth sending not implemented on this platform")),
                }
            }
        };
        match conn {
            Ok(mut c) => {
                c.peer_drift = device.drift;
                c.bootstrap_lan = matches!(route, Route::Lan { .. });
                let _ = pin_tx.send((c.pin(), route.clone()));
                return drift_net::send::run_with(c, &local, paths, texts, platform, health, allowed, events, cancel).await;
            }
            Err(e) => {
                tracing::warn!(?route, "route failed: {e:#}");
                last_err = Some(e);
            }
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => bail!("{} has no reachable route", device.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn bluetooth_startup_failures_are_optional() {
        let unavailable = || Err::<u8, _>(anyhow::anyhow!("Bluetooth unavailable"));
        assert_eq!(start_bluetooth(unavailable, || Ok(2)), (Some(2), None));
        assert_eq!(start_bluetooth(|| Ok(1), unavailable), (None, Some(1)));
        assert_eq!(start_bluetooth(unavailable, unavailable), (None, None));
        assert_eq!(start_bluetooth(|| Ok(1), || Ok(2)), (Some(2), Some(1)));
    }

    fn info(name: Option<&str>) -> EndpointInfo {
        let mut i = EndpointInfo::new_visible(name.unwrap_or("x"), DeviceType::Phone);
        if name.is_none() {
            i.visible = false;
            i.name = None;
        }
        i
    }

    #[test]
    fn found_updated_lost_and_lan_first() {
        let mut r = Roster::default();
        let t0 = Instant::now();
        let bt = Route::Bluetooth { mac: [1, 2, 3, 4, 5, 6] };
        let lan = Route::Lan { addr: "10.0.0.2:1234".parse().unwrap() };
        assert!(matches!(r.sighting("ABCD".into(), &info(Some("Phone")), bt.clone(), t0), Some(NearbyEvent::Found(_))));
        assert_eq!(r.sighting("ABCD".into(), &info(Some("Phone")), bt.clone(), t0), None, "same route again is not news");
        let Some(NearbyEvent::Updated(d)) = r.sighting("ABCD".into(), &info(Some("Phone")), lan.clone(), t0) else { panic!() };
        assert_eq!(d.routes, vec![lan, bt], "LAN preferred");
        assert!(r.expire(t0 + Duration::from_secs(5)).is_empty());
        assert_eq!(r.expire(t0 + LOST_AFTER), vec![NearbyEvent::Lost { endpoint_id: "ABCD".into() }]);
    }

    #[test]
    fn hidden_receivers_are_not_listed() {
        let mut r = Roster::default();
        assert_eq!(r.sighting("HIDE".into(), &info(None), Route::Bluetooth { mac: [0; 6] }, Instant::now()), None);
    }

    #[test]
    fn mdns_instance_decodes_endpoint_id() {
        let name = drift_core::endpoint::mdns_instance_name(b"WXYZ");
        assert_eq!(mdns_endpoint_id(&format!("{name}._FC9F5ED42C8A._tcp.local.")).as_deref(), Some("WXYZ"));
    }
}
