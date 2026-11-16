//! Windows platform services for Drift: Bluetooth (BLE advertising, RFCOMM
//! bootstrap), Wi-Fi Direct (host a legacy group, or join a phone's group as a
//! P2P client), firewall rule management and Bluetooth radio recovery.
#![cfg(windows)]

pub mod bt;
pub mod firewall;
pub mod protect;
pub mod radio;
pub mod routes;
pub mod scan;
pub mod wfd;

use std::time::Duration;

use anyhow::Result;
use drift_net::upgrade::{BoxFuture, HostedGroup, JoinedGroup, Platform};

/// Our Wi-Fi Direct device name as Windows advertises it (the computer name).
pub fn wifi_direct_device_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

#[derive(Default)]
pub struct WinPlatform;

impl Platform for WinPlatform {
    fn host_group(&self) -> BoxFuture<'_, Result<HostedGroup>> {
        Box::pin(async {
            // Starting a group right after the previous one was torn down makes the
            // publisher abort (seen 2026-11-13); back off and retry a couple of times.
            let mut attempt = 0;
            let g = loop {
                match wfd::start_group().await {
                    Ok(g) => break g,
                    Err(e) if attempt < 2 => {
                        attempt += 1;
                        tracing::warn!("Wi-Fi Direct group start failed ({e:#}); retrying");
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                    }
                    Err(e) => return Err(e),
                }
            };
            Ok(HostedGroup { ssid: g.ssid.clone(), password: g.password.clone(), ip: g.ip, device_name: None, frequency: -1, guard: Box::new(g) })
        })
    }

    fn join_group(&self, device_name: String, pin: String) -> BoxFuture<'_, Result<JoinedGroup>> {
        Box::pin(async move {
            let link = wfd::join_group(&device_name, &pin, Duration::from_secs(30)).await?;
            // the group's gateway must never become our default route (F5): fix the port once, off
            // the transfer's path
            tokio::task::spawn_blocking(|| match routes::ensure_ignore_default_routes() {
                Ok(n) if n > 0 => tracing::info!(n, "Wi-Fi Direct ports now ignore group default routes"),
                Ok(_) => {}
                Err(e) => tracing::warn!("default-route guard: {e:#}"),
            });
            Ok(JoinedGroup { remote_ip: link.remote_ip.clone(), local_ip: link.local_ip.clone(), guard: Box::new(WarmLink(Some(link))) })
        })
    }

    fn ensure_firewall(&self) -> Result<()> {
        firewall::ensure()
    }

    fn cleanup_routes(&self) -> Result<usize> {
        routes::remove_stale()
    }

    fn direct_known(&self, peer_name: &str) -> bool {
        wfd::direct_known(peer_name)
    }

    fn direct_worked(&self, peer_name: &str) {
        wfd::remember_direct_peer(peer_name)
    }

    fn internet_gateway(&self) -> Option<std::net::Ipv4Addr> {
        routes::internet_gateway()
    }
}


/// Keeps a phone's Wi-Fi Direct link up for a minute after the share is done with it: the next share
/// to that phone reopens it at once (0.2 s) instead of waiting for Windows to find the phone (5–10 s).
struct WarmLink(Option<wfd::ClientLink>);

static WARM: std::sync::Mutex<Option<(u64, wfd::ClientLink)>> = std::sync::Mutex::new(None);
static WARM_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Drop for WarmLink {
    fn drop(&mut self) {
        let Some(link) = self.0.take() else { return };
        let generation = WARM_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        // replaces (and so closes) any older warm link
        *WARM.lock().unwrap() = Some((generation, link));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            let mut w = WARM.lock().unwrap();
            if w.as_ref().is_some_and(|(g, _)| *g == generation) {
                tracing::info!("closing the idle Wi-Fi Direct link");
                *w = None;
            }
        });
    }
}
