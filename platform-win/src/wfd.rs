//! Host a "legacy" Wi-Fi Direct group (soft AP on the Wi-Fi Direct virtual
//! adapter) that the phone joins like a normal Wi-Fi network. The PC's primary
//! Wi-Fi connection stays up (STA + GO concurrency).

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rand::Rng;
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
    WiFiDirectAdvertisementPublisherStatus,
};
use windows::Networking::Connectivity::NetworkInformation;
use windows::Networking::HostNameType;
use windows::Security::Credentials::PasswordCredential;
use windows::core::HSTRING;

/// A Wi-Fi Direct connection where the *phone* is group owner and we are a P2P
/// client (role switch). The PC's normal Wi-Fi stays up; so does the phone's.
pub struct ClientLink {
    device: windows::Devices::WiFiDirect::WiFiDirectDevice,
    pub local_ip: String,
    pub remote_ip: String,
}

impl Drop for ClientLink {
    fn drop(&mut self) {
        let _ = self.device.Close();
    }
}

/// Find the phone's P2P device by `device_name`, pair using `pin` (ProvidePin,
/// low GO intent, invitation procedure — as google/nearby's Windows GC does),
/// then open the connection.
pub async fn join_group(device_name: &str, pin: &str, timeout: Duration) -> Result<ClientLink> {
    use windows::Devices::Enumeration::{
        DeviceInformation, DeviceInformationCustomPairing, DevicePairingKinds, DevicePairingProtectionLevel,
        DevicePairingRequestedEventArgs, DevicePairingResultStatus, DeviceUnpairingResultStatus, DeviceWatcher,
    };
    use windows::Devices::WiFiDirect::{
        WiFiDirectConfigurationMethod, WiFiDirectConnectionParameters, WiFiDirectDevice, WiFiDirectDeviceSelectorType,
        WiFiDirectPairingProcedure,
    };
    use windows::Foundation::TypedEventHandler;
    let t_start = std::time::Instant::now();
    let mark = |what: &str| tracing::info!("[wfd] t+{:.2}s {what}", t_start.elapsed().as_secs_f32());

    // Known from a previous join: open it directly. Any Windows lookup of Wi-Fi Direct devices, even
    // paired ones, waits out a 10 s scan; opening a known id takes 0.02-2 s.
    if !device_name.is_empty() && std::env::var_os("DRIFT_WFD_UNPAIR").is_none() {
        if let Some(id) = known_id(device_name) {
            // FromIdAsync only succeeds once Windows' own scan has seen the phone (a miss costs 10 s).
            // So watch for it: open at once, and open again the moment the scan reports the phone
            // present; the first open that succeeds wins.
            match open_known_racing(&id, Duration::from_secs(25)).await {
                Ok((link, how)) => {
                    mark(&format!("reconnected by known id ({how})"));
                    return Ok(link);
                }
                Err(e) => mark(&format!("known id failed: {e:#}")),
            }
        }
    }
    // Paired before: connect straight away, no discovery (the phone re-admits its saved group's member
    // without asking). FindAllAsync runs a 10 s discovery scan even for paired entries, so use a
    // watcher: paired endpoints come from Windows' store as soon as it starts.
    if !device_name.is_empty() && std::env::var_os("DRIFT_WFD_UNPAIR").is_none() {
        let aep = HSTRING::from(format!(
            "{} AND System.Devices.Aep.IsPaired:=System.StructuredQueryType.Boolean#True",
            WiFiDirectDevice::GetDeviceSelector2(WiFiDirectDeviceSelectorType::AssociationEndpoint)?
        ));
        let pw = DeviceInformation::CreateWatcherAqsFilter(&aep)?;
        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<(String, HSTRING)>();
        let ptx2 = ptx.clone();
        pw.Added(&TypedEventHandler::new(move |_: windows::core::Ref<DeviceWatcher>, info: windows::core::Ref<DeviceInformation>| {
            if let Some(i) = info.as_ref() {
                let _ = ptx.send((i.Name()?.to_string(), i.Id()?));
            }
            Ok(())
        }))?;
        // enumeration of what's stored is done: no paired match means pair below
        pw.EnumerationCompleted(&TypedEventHandler::new(move |_: windows::core::Ref<DeviceWatcher>, _: windows::core::Ref<windows::core::IInspectable>| {
            let _ = ptx2.send((String::new(), HSTRING::new()));
            Ok(())
        }))?;
        pw.Start()?;
        let hit = tokio::time::timeout(Duration::from_secs(12), async {
            while let Some((name, id)) = prx.recv().await {
                if id.is_empty() {
                    return None;
                }
                tracing::info!("[wfd] paired P2P device {name:?}");
                if name.eq_ignore_ascii_case(device_name) {
                    return Some((name, id));
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        let _ = pw.Stop();
        mark(&format!("paired lookup: {}", if hit.is_some() { "found" } else { "none" }));
        if let Some((name, id)) = hit {
            let t = std::time::Instant::now();
            match tokio::time::timeout(Duration::from_secs(20), open_device(&id)).await {
                Ok(Ok(link)) => {
                    tracing::info!("[wfd] reconnected to paired {name:?} in {:?}", t.elapsed());
                    remember_id(device_name, &id);
                    return Ok(link);
                }
                Ok(Err(e)) => tracing::info!("[wfd] paired reconnect failed after {:?}: {e:#}", t.elapsed()),
                Err(_) => tracing::info!("[wfd] paired reconnect timed out"),
            }
        }
    }
    let selector = WiFiDirectDevice::GetDeviceSelector2(WiFiDirectDeviceSelectorType::AssociationEndpoint)?;
    let watcher = DeviceInformation::CreateWatcherAqsFilter(&selector)?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeviceInformation>();
    watcher.Added(&TypedEventHandler::new(move |_: windows::core::Ref<DeviceWatcher>, info: windows::core::Ref<DeviceInformation>| {
        if let Some(info) = info.as_ref() {
            let _ = tx.send(info.clone());
        }
        Ok(())
    }))?;
    watcher.Start()?;
    mark("searching");
    let found = tokio::time::timeout(timeout, async {
        while let Some(info) = rx.recv().await {
            let name = info.Name().map(|n| n.to_string()).unwrap_or_default();
            tracing::info!("[wfd] saw P2P device {name:?}");
            // Phone sent no device name (SSID/password auth): take the first P2P device seen.
            if device_name.is_empty() || name.eq_ignore_ascii_case(device_name) {
                return Some(info);
            }
        }
        None
    })
    .await;
    let _ = watcher.Stop();
    mark("search done");
    let mut info = found.ok().flatten().with_context(|| format!("phone P2P device {device_name:?} not found"))?;
    let id = info.Id()?;

    // Probe: DRIFT_WFD_UNPAIR=1 drops any pairing (Windows can report AlreadyPaired for a stale one it can't open).
    if std::env::var_os("DRIFT_WFD_UNPAIR").is_some() {
        let r = info.Pairing()?.UnpairAsync()?.await?;
        tracing::info!("[wfd] unpaired first: {:?}", r.Status()?);
        tokio::time::sleep(Duration::from_millis(500)).await;
        info = DeviceInformation::CreateFromIdAsync(&id)?.await?;
    }
    // Reuse an existing pairing first: if it works, the phone's prompt only appears once.
    if info.Pairing()?.IsPaired()? {
        tracing::info!("[wfd] already paired with {:?}, trying to connect without re-pairing", info.Name()?);
        match tokio::time::timeout(Duration::from_secs(12), open_device(&id)).await {
            Ok(Ok(link)) => return Ok(link),
            Ok(Err(e)) => tracing::info!("[wfd] reuse failed: {e:#}; re-pairing"),
            Err(_) => tracing::info!("[wfd] reuse timed out; re-pairing"),
        }
        let r = info.Pairing()?.UnpairAsync()?.await?;
        tracing::info!("[wfd] unpaired stale pairing: {:?}", r.Status()?);
        if r.Status()? != DeviceUnpairingResultStatus::Failed {
            tokio::time::sleep(Duration::from_millis(500)).await;
            info = DeviceInformation::CreateFromIdAsync(&id)?.await?;
        }
    }

    let params = WiFiDirectConnectionParameters::new()?;
    params.SetGroupOwnerIntent(1)?;
    // With a PIN from the phone use ProvidePin (upstream's path); without one, try push-button.
    let method = if pin.is_empty() { WiFiDirectConfigurationMethod::PushButton } else { WiFiDirectConfigurationMethod::ProvidePin };
    tracing::info!("[wfd] pairing with {:?} using {method:?}", info.Name()?);
    params.PreferenceOrderedConfigurationMethods()?.Append(method)?;
    params.SetPreferredPairingProcedure(WiFiDirectPairingProcedure::Invitation)?;
    let kinds = WiFiDirectConnectionParameters::GetDevicePairingKinds(method)?;

    let custom = info.Pairing()?.Custom()?;
    let pin_h = HSTRING::from(pin);
    custom.PairingRequested(&TypedEventHandler::new(
        move |_: windows::core::Ref<DeviceInformationCustomPairing>, args: windows::core::Ref<DevicePairingRequestedEventArgs>| {
            if let Some(a) = args.as_ref() {
                let kind = a.PairingKind()?;
                tracing::info!("[wfd] pairing requested: {kind:?}");
                if kind == DevicePairingKinds::ProvidePin { a.AcceptWithPin(&pin_h)? } else { a.Accept()? }
            }
            Ok(())
        },
    ))?;
    let started = std::time::Instant::now();
    let res = custom
        .PairWithProtectionLevelAndSettingsAsync(kinds, DevicePairingProtectionLevel::Default, &params)?
        .await?;
    let status = res.Status()?;
    tracing::info!("[wfd] pairing result {status:?} after {:?}", started.elapsed());
    if status != DevicePairingResultStatus::Paired && status != DevicePairingResultStatus::AlreadyPaired {
        bail!("Wi-Fi Direct pairing failed: {status:?}");
    }

    let link = open_device(&id).await?;
    remember_id(&info.Name()?.to_string(), &id);
    Ok(link)
}

/// Open a known device, racing a fresh open against Windows' scan: a watcher on that endpoint
/// reports when it is present (Added/Updated with System.Devices.Aep.IsPresent), and each time it
/// does, another FromIdAsync starts. Returns the link and which try won.
async fn open_known_racing(id: &HSTRING, limit: Duration) -> Result<(ClientLink, String)> {
    use windows::Devices::Enumeration::{DeviceInformation, DeviceInformationKind, DeviceInformationUpdate, DeviceWatcher};
    use windows::Foundation::TypedEventHandler;
    use windows_collections::IIterable;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let present = HSTRING::from("System.Devices.Aep.IsPresent");
    let is_present = |props: windows_collections::IMapView<HSTRING, windows::core::IInspectable>| -> bool {
        props
            .Lookup(&HSTRING::from("System.Devices.Aep.IsPresent"))
            .ok()
            .and_then(|v| windows::core::Interface::cast::<windows::Foundation::IReference<bool>>(&v).ok())
            .and_then(|r| r.Value().ok())
            .unwrap_or(false)
    };
    // Wi-Fi Direct association endpoints (the known id is one of them). The property list isn't Send:
    // keep it out of the awaits below.
    let watcher = {
        let props: IIterable<HSTRING> = IIterable::from(vec![present.clone()]);
        DeviceInformation::CreateWatcherWithKindAqsFilterAndAdditionalProperties(
            &windows::Devices::WiFiDirect::WiFiDirectDevice::GetDeviceSelector2(windows::Devices::WiFiDirect::WiFiDirectDeviceSelectorType::AssociationEndpoint)?,
            &props,
            DeviceInformationKind::AssociationEndpoint,
        )?
    };
    let want = id.clone();
    let (t1, t2) = (tx.clone(), tx.clone());
    let want2 = want.clone();
    watcher.Added(&TypedEventHandler::new(move |_: windows::core::Ref<DeviceWatcher>, info: windows::core::Ref<DeviceInformation>| {
        if let Some(i) = info.as_ref() {
            if i.Id()? == want && is_present(i.Properties()?) {
                let _ = t1.send(());
            }
        }
        Ok(())
    }))?;
    watcher.Updated(&TypedEventHandler::new(move |_: windows::core::Ref<DeviceWatcher>, u: windows::core::Ref<DeviceInformationUpdate>| {
        if let Some(u) = u.as_ref() {
            if u.Id()? == want2 && is_present(u.Properties()?) {
                let _ = t2.send(());
            }
        }
        Ok(())
    }))?;
    // The Drift phone also *invites* us (P2p.invite): Windows hands that to a connection listener as
    // ConnectionRequested, and the device it names is present right then, so open it at once.
    let listener = windows::Devices::WiFiDirect::WiFiDirectConnectionListener::new()?;
    let t3 = tx.clone();
    let (itx, mut irx) = tokio::sync::mpsc::unbounded_channel::<HSTRING>();
    listener.ConnectionRequested(&TypedEventHandler::new(
        move |_: windows::core::Ref<windows::Devices::WiFiDirect::WiFiDirectConnectionListener>,
              args: windows::core::Ref<windows::Devices::WiFiDirect::WiFiDirectConnectionRequestedEventArgs>| {
            if let Some(a) = args.as_ref() {
                let info = a.GetConnectionRequest()?.DeviceInformation()?;
                tracing::info!("[wfd] invitation from {:?}", info.Name()?.to_string());
                let _ = itx.send(info.Id()?);
                let _ = t3.send(());
            }
            Ok(())
        },
    ))?;
    drop(tx);
    watcher.Start()?;
    let stop = scopeguard_stop(watcher);

    let started = std::time::Instant::now();
    let mut tries: tokio::task::JoinSet<(u32, Result<ClientLink>)> = tokio::task::JoinSet::new();
    let mut n = 1u32;
    let first = id.clone();
    tries.spawn(async move { (1, open_device(&first).await) });
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut last_err = anyhow::anyhow!("not found");
    let mut seen = false;
    loop {
        tokio::select! {
            Some(inv) = irx.recv() => {
                n += 1;
                let k = n;
                tracing::info!("[wfd] invited after {:?}; opening the inviter", started.elapsed());
                tries.spawn(async move { (k, open_device(&inv).await) });
                let (k2, again) = (n + 1, id.clone());
                n += 1;
                tries.spawn(async move { (k2, open_device(&again).await) });
            }
            Some(()) = rx.recv(), if !seen => {
                // present: one more try (the scan keeps reporting; one extra open is enough)
                seen = true;
                n += 1;
                let (k, again) = (n, id.clone());
                tracing::info!("[wfd] phone seen by the scan after {:?}; opening again", started.elapsed());
                tries.spawn(async move { (k, open_device(&again).await) });
            }
            Some(done) = tries.join_next() => {
                match done {
                    Ok((k, Ok(link))) => {
                        drop(stop);
                        return Ok((link, format!("try {k} of {n}, {:.1} s", started.elapsed().as_secs_f32())));
                    }
                    Ok((k, Err(e))) => {
                        tracing::info!("[wfd] open try {k} failed after {:?}: {e:#}", started.elapsed());
                        last_err = e;
                        if tries.is_empty() && seen {
                            // the scan saw it and the open still failed: give up on the known id
                            break;
                        }
                        if tries.is_empty() {
                            // nothing running: try again (the scan may still report it)
                            n += 1;
                            let (k2, again) = (n, id.clone());
                            tries.spawn(async move { (k2, open_device(&again).await) });
                        }
                    }
                    Err(e) => last_err = anyhow::anyhow!("open task: {e}"),
                }
            }
            _ = &mut deadline => break,
        }
    }
    drop(stop);
    drop(listener);
    Err(last_err)
}

/// Stops the watcher when dropped.
struct WatcherStop(windows::Devices::Enumeration::DeviceWatcher);
impl Drop for WatcherStop {
    fn drop(&mut self) {
        let _ = self.0.Stop();
    }
}
fn scopeguard_stop(w: windows::Devices::Enumeration::DeviceWatcher) -> WatcherStop {
    WatcherStop(w)
}

/// Phone P2P device ids we've joined before, by name (%APPDATA%\Drift\p2p-devices.txt, "name\tid").
fn ids_file() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("Drift").join("p2p-devices.txt"))
}

fn known_id(name: &str) -> Option<HSTRING> {
    let text = std::fs::read_to_string(ids_file()?).ok()?;
    text.lines().filter_map(|l| l.split_once('\t')).find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, id)| HSTRING::from(id))
}

fn remember_id(name: &str, id: &HSTRING) {
    let Some(path) = ids_file() else { return };
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().filter(|l| !l.split_once('\t').is_some_and(|(n, _)| n.eq_ignore_ascii_case(name))).map(String::from).collect();
    lines.push(format!("{name}\t{id}"));
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, lines.join("
") + "
");
}

async fn open_device(id: &HSTRING) -> Result<ClientLink> {
    use windows::Devices::WiFiDirect::WiFiDirectDevice;
    let t = std::time::Instant::now();
    let device = WiFiDirectDevice::FromIdAsync(id)?.await.context("WiFiDirectDevice::FromIdAsync")?;
    tracing::info!("[wfd] FromIdAsync took {:.2}s", t.elapsed().as_secs_f32());
    let pairs = device.GetConnectionEndpointPairs()?;
    if pairs.Size()? == 0 {
        bail!("connected but no endpoint pairs");
    }
    let p = pairs.GetAt(0)?;
    Ok(ClientLink {
        local_ip: p.LocalHostName()?.DisplayName()?.to_string(),
        remote_ip: p.RemoteHostName()?.DisplayName()?.to_string(),
        device,
    })
}

pub struct Group {
    publisher: WiFiDirectAdvertisementPublisher,
    pub ssid: String,
    pub password: String,
    pub ip: Ipv4Addr,
}

impl Drop for Group {
    fn drop(&mut self) {
        let _ = self.publisher.Stop();
    }
}

impl Group {
    /// Stop advertising. For a group formed by negotiation (C1, non-autonomous) the admitted
    /// clients' WiFiDirectDevice connections may keep the group up; a discoverable publisher keeps
    /// the radio hopping to the 2.4 GHz listen channels, which may be what starves the link.
    pub fn stop_advertising(&self) -> windows::core::Result<()> {
        self.publisher.Stop()
    }
}

fn ipv4_hosts() -> Result<HashSet<Ipv4Addr>> {
    let mut out = HashSet::new();
    for h in NetworkInformation::GetHostNames()? {
        if h.Type()? == HostNameType::Ipv4 {
            if let Ok(ip) = h.CanonicalName()?.to_string().parse() {
                out.insert(ip);
            }
        }
    }
    Ok(out)
}

fn random_alnum(n: usize) -> String {
    const A: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut r = rand::thread_rng();
    (0..n).map(|_| A[r.gen_range(0..A.len())] as char).collect()
}

/// Start an autonomous GO with legacy (WPA2-PSK) settings and wait for its IP.
pub async fn start_group() -> Result<Group> {
    start_group_with(false).await
}

/// Companion path C1 (NOTES.md): a group the Drift phone joins over Wi-Fi Direct (its p2p0, so it
/// keeps its own Wi-Fi). It must be discoverable so the phone can provision; joins are paired and
/// admitted in code, so nobody is asked.
pub struct AcceptingGroup {
    pub group: Group,
    _listener: windows::Devices::WiFiDirect::WiFiDirectConnectionListener,
    /// Admitted P2P clients; closing one drops it from the group.
    devices: std::sync::Arc<std::sync::Mutex<Vec<windows::Devices::WiFiDirect::WiFiDirectDevice>>>,
}

impl AcceptingGroup {
    pub fn clients(&self) -> usize {
        self.devices.lock().unwrap().len()
    }
}

impl Drop for AcceptingGroup {
    fn drop(&mut self) {
        for d in self.devices.lock().unwrap().drain(..) {
            let _ = d.Close();
        }
    }
}

/// Start a discoverable GO that pairs and admits P2P clients without asking. `allow` decides by
/// the requester's name (the Drift device we're talking to over Bluetooth); `log` reports steps.
pub async fn start_accepting_group(
    allow: std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>,
    log: std::sync::Arc<dyn Fn(String) + Send + Sync>,
) -> Result<AcceptingGroup> {
    use windows::Devices::Enumeration::{DeviceInformationCustomPairing, DevicePairingKinds, DevicePairingProtectionLevel, DevicePairingRequestedEventArgs, DevicePairingResultStatus};
    use windows::Devices::WiFiDirect::{
        WiFiDirectConfigurationMethod, WiFiDirectConnectionListener, WiFiDirectConnectionParameters, WiFiDirectConnectionRequestedEventArgs, WiFiDirectDevice,
    };
    use windows::Foundation::TypedEventHandler;

    let group = start_group_with(true).await?;
    let listener = WiFiDirectConnectionListener::new()?;
    let devices: std::sync::Arc<std::sync::Mutex<Vec<WiFiDirectDevice>>> = Default::default();
    let devs = devices.clone();
    listener.ConnectionRequested(&TypedEventHandler::new(
        move |_: windows::core::Ref<WiFiDirectConnectionListener>, args: windows::core::Ref<WiFiDirectConnectionRequestedEventArgs>| {
            let Some(args) = args.as_ref() else { return Ok(()) };
            let req = args.GetConnectionRequest()?;
            let info = req.DeviceInformation()?;
            let name = info.Name()?.to_string();
            let id = info.Id()?;
            if !allow(&name) {
                log(format!("join request from {name:?}: not ours, ignored"));
                return Ok(());
            }
            log(format!("join request from {name:?}"));
            let (log, devs) = (log.clone(), devs.clone());
            // pairing blocks: off the event thread
            std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                let r = (|| -> Result<WiFiDirectDevice> {
                    // Seen before: Windows keeps the pairing and won't pair again. Connect straight
                    // away; if that's refused, forget the old pairing and pair fresh.
                    if info.Pairing()?.IsPaired()? {
                        match WiFiDirectDevice::FromIdAsync(&id)?.join() {
                            Ok(d) => {
                                log(format!("already paired; connected after {:?}", t0.elapsed()));
                                return Ok(d);
                            }
                            Err(e) => {
                                log(format!("reconnect refused ({e}); re-pairing"));
                                let _ = info.Pairing()?.UnpairAsync()?.join();
                            }
                        }
                    }
                    let params = WiFiDirectConnectionParameters::new()?;
                    params.SetGroupOwnerIntent(15)?;
                    // C1 probe: DRIFT_WFD_INVITE=1 invites the phone into our running group
                    if std::env::var_os("DRIFT_WFD_INVITE").is_some() {
                        params.PreferenceOrderedConfigurationMethods()?.Append(WiFiDirectConfigurationMethod::PushButton)?;
                        params.SetPreferredPairingProcedure(windows::Devices::WiFiDirect::WiFiDirectPairingProcedure::Invitation)?;
                    }
                    let custom = info.Pairing()?.Custom()?;
                    let log2 = log.clone();
                    custom.PairingRequested(&TypedEventHandler::new(
                        move |_: windows::core::Ref<DeviceInformationCustomPairing>, a: windows::core::Ref<DevicePairingRequestedEventArgs>| {
                            if let Some(a) = a.as_ref() {
                                let kind = a.PairingKind()?;
                                log2(format!("pairing kind {kind:?}: accepting"));
                                a.Accept()?;
                            }
                            Ok(())
                        },
                    ))?;
                    let kinds = DevicePairingKinds::ConfirmOnly | DevicePairingKinds::DisplayPin | DevicePairingKinds::ProvidePin;
                    let _ = WiFiDirectConfigurationMethod::PushButton;
                    let mut st = custom.PairWithProtectionLevelAndSettingsAsync(kinds, DevicePairingProtectionLevel::Default, &params)?.join()?.Status()?;
                    // Refused at once: the phone opened with an invitation to re-use the group it saved
                    // last time; we answer "unavailable" and Android falls back to a normal
                    // negotiation on its own (AOSP WifiP2pServiceImpl, INFORMATION_IS_CURRENTLY_UNAVAILABLE).
                    // Pair again once that negotiation is under way.
                    let mut tries = 0;
                    while st == DevicePairingResultStatus::Failed && tries < 4 {
                        let quick = std::time::Instant::now();
                        tries += 1;
                        std::thread::sleep(Duration::from_millis(1500));
                        log(format!("pairing refused; trying again ({tries})"));
                        st = custom.PairWithProtectionLevelAndSettingsAsync(kinds, DevicePairingProtectionLevel::Default, &params)?.join()?.Status()?;
                        if quick.elapsed() > Duration::from_secs(5) {
                            break; // a real attempt that failed, not a refusal
                        }
                    }
                    log(format!("pairing {st:?} after {:?}", t0.elapsed()));
                    if st != DevicePairingResultStatus::Paired && st != DevicePairingResultStatus::AlreadyPaired {
                        // Windows may still hold this device's Wi-Fi Direct profile from a past
                        // session and need no pairing at all: try connecting before giving up.
                        log(format!("pairing {st:?}; trying to connect anyway"));
                        return Ok(WiFiDirectDevice::FromIdAsync(&id)?.join().map_err(|e| anyhow::anyhow!("pairing {st:?}, connect: {e}"))?);
                    }
                    Ok(WiFiDirectDevice::FromIdAsync(&id)?.join()?)
                })();
                match r {
                    Ok(d) => {
                        let ips = d
                            .GetConnectionEndpointPairs()
                            .ok()
                            .and_then(|p| p.GetAt(0).ok())
                            .and_then(|p| p.RemoteHostName().ok())
                            .and_then(|h| h.DisplayName().ok())
                            .map(|h| h.to_string())
                            .unwrap_or_default();
                        log(format!("admitted after {:?}; client ip {ips}", t0.elapsed()));
                        devs.lock().unwrap().push(d);
                    }
                    Err(e) => log(format!("join failed after {:?}: {e:#}", t0.elapsed())),
                }
            });
            Ok(())
        },
    ))?;
    Ok(AcceptingGroup { group, _listener: listener, devices })
}

async fn start_group_with(discoverable: bool) -> Result<Group> {
    let before = ipv4_hosts()?;
    let ssid = format!("DIRECT-{}-Drift", random_alnum(2));
    let password = random_alnum(16);

    let publisher = WiFiDirectAdvertisementPublisher::new()?;
    let adv = publisher.Advertisement()?;
    // NOT discoverable. `Normal` makes the radio hop to the 2.4 GHz P2P social
    // channels to answer discovery; every PC-hosted transfer with `Normal` stalled
    // ~2–3 s in on the Realtek 8852BE, every one with `None` completed (NOTES.md).
    // Hotspot-mode phones join by SSID and don't need P2P discovery.
    adv.SetListenStateDiscoverability(if discoverable {
        // C1 only: the phone must find us to provision. (F12: watch for stalls with this on.)
        WiFiDirectAdvertisementListenStateDiscoverability::Normal
    } else {
        WiFiDirectAdvertisementListenStateDiscoverability::None
    })?;
    if discoverable {
        adv.SupportedConfigurationMethods()?.Append(windows::Devices::WiFiDirect::WiFiDirectConfigurationMethod::PushButton)?;
    }
    // C1 probe: DRIFT_WFD_NOAUTO=1 forms the group by negotiation on the join request (MS sample flow)
    adv.SetIsAutonomousGroupOwnerEnabled(!(discoverable && std::env::var_os("DRIFT_WFD_NOAUTO").is_some()))?;
    let legacy = adv.LegacySettings()?;
    // C1 probe: DRIFT_WFD_PURE=1 hosts a pure Wi-Fi Direct group (no legacy soft-AP side)
    let pure = discoverable && std::env::var_os("DRIFT_WFD_PURE").is_some();
    legacy.SetIsEnabled(!pure)?;
    legacy.SetSsid(&HSTRING::from(&ssid))?;
    let cred = PasswordCredential::new()?;
    cred.SetPassword(&HSTRING::from(&password))?;
    legacy.SetPassphrase(&cred)?;
    publisher.Start().context("WiFiDirectAdvertisementPublisher::Start")?;

    for _ in 0..100 {
        match publisher.Status()? {
            WiFiDirectAdvertisementPublisherStatus::Started => break,
            WiFiDirectAdvertisementPublisherStatus::Aborted => bail!("Wi-Fi Direct GO aborted"),
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    if publisher.Status()? != WiFiDirectAdvertisementPublisherStatus::Started {
        bail!("Wi-Fi Direct GO did not start: {:?}", publisher.Status()?);
    }

    // The GO's address appears on the Wi-Fi Direct virtual adapter a moment later.
    // Some drivers only assign it once a client associates, so fall back to the
    // usual ICS gateway address and listen on all interfaces.
    for _ in 0..50 {
        let now = ipv4_hosts()?;
        if let Some(ip) = now.difference(&before).find(|ip| !ip.is_loopback() && !ip.is_link_local()) {
            return Ok(Group { publisher, ssid, password, ip: *ip });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::info!("[wfd] no new IPv4 address yet; assuming 192.168.137.1");
    Ok(Group { publisher, ssid, password, ip: Ipv4Addr::new(192, 168, 137, 1) })
}
