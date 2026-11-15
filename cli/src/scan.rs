//! `drift scan`: wake nearby Android receivers and report how they advertise.
//! Exploration tool for milestone 2 (sending).

use std::time::{Duration, Instant};

use anyhow::Result;
use drift_core::ble::{self, FastInitType};
use drift_core::endpoint::EndpointInfo;

fn describe(info: &[u8]) -> String {
    match EndpointInfo::decode(info) {
        Ok(i) => format!("{:?} {}", i.device_type, i.name.unwrap_or_else(|| "(hidden)".into())),
        Err(_) => format!("info {} bytes", info.len()),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub async fn run(seconds: u64, kind: FastInitType, require_bt: bool, wake: bool) -> Result<()> {
    let start = Instant::now();
    let t = || format!("{:>5.1}s", start.elapsed().as_secs_f64());

    // mDNS: receivers on open networks advertise _FC9F5ED42C8A._tcp.
    let mdns = mdns_sd::ServiceDaemon::new()?;
    let mdns_rx = mdns.browse(drift_core::endpoint::SERVICE_TYPE)?;

    #[cfg(windows)]
    {
        use drift_platform_win::{bt, scan};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        static UUIDS: [u16; 2] = [ble::COPRESENCE_UUID16, ble::FAST_INIT_UUID16];
        let _ble = scan::scan_ble(&UUIDS, tx.clone())?;
        let _classic = scan::scan_classic(tx)?;
        let _wake = if wake {
            let beacon = ble::fast_init_beacon(kind, require_bt);
            println!("{} sending wake beacon {:?} require_bt={require_bt}: fe2c {}", t(), kind, hex(&beacon));
            Some(bt::start_ble(ble::FAST_INIT_UUID16, &beacon, false)?)
        } else {
            None
        };
        println!("{} scanning BLE (fef3, fe2c), Bluetooth Classic and mDNS for {seconds}s...", t());
        let mut seen_ble = std::collections::HashSet::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            while let Ok(ev) = mdns_rx.try_recv() {
                if let mdns_sd::ServiceEvent::ServiceResolved(i) = ev {
                    let info = i.get_property_val_str("n").unwrap_or_default().to_string();
                    println!("{} mDNS  {} @ {:?}:{} n={info}", t(), i.get_fullname(), i.get_addresses(), i.get_port());
                }
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                Some(ev) = rx.recv() => match ev {
                    scan::Seen::Ble { address, uuid16, data, rssi } => {
                        if !seen_ble.insert((address, uuid16, data.clone())) { continue; }
                        let addr = hex(&address.to_be_bytes()[2..]);
                        if uuid16 == ble::COPRESENCE_UUID16 {
                            match ble::parse_advertisement(&data) {
                                Ok(d) => println!("{} BLE   fef3 from {addr} rssi {rssi}: endpoint {} {} bt_mac={:?} fast={}",
                                    t(), String::from_utf8_lossy(&d.endpoint_id), describe(&d.endpoint_info),
                                    d.bt_mac.map(|m| hex(&m)), data.first().is_some_and(|v| v & 2 != 0)),
                                Err(e) => println!("{} BLE   fef3 from {addr} rssi {rssi}: {} ({e})", t(), hex(&data)),
                            }
                        } else {
                            println!("{} BLE   fe2c (wake) from {addr} rssi {rssi}: {}", t(), hex(&data));
                        }
                    }
                    scan::Seen::Classic { name, address, .. } => {
                        match ble::parse_bluetooth_name(&name) {
                            Ok(d) => println!("{} BT    NEARBY {name:?} addr={address:?}: endpoint {} {}",
                                t(), String::from_utf8_lossy(&d.endpoint_id), describe(&d.endpoint_info)),
                            Err(_) => println!("{} BT    {name:?} addr={address:?}", t()),
                        }
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }
    let _ = mdns.shutdown();
    Ok(())
}
