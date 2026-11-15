//! `drift send FILE... [--to NAME] [--seconds N]`: send files to a nearby phone.
//! Thin wrapper over `drift_app::nearby` (the same API the UI will use).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use drift_app::nearby::{self, NearbyEvent};
use drift_net::send::SendEvent;

fn human(b: f64) -> String {
    let u = ["B", "KB", "MB", "GB"];
    let (mut v, mut i) = (b, 0);
    while v >= 1000.0 && i < 3 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.1} {}", u[i])
}

fn platform() -> Arc<dyn drift_net::Platform> {
    #[cfg(windows)]
    {
        if let Err(e) = drift_platform_win::firewall::ensure() {
            println!("! firewall rule missing ({e:#}); the phone may not reach our network");
        }
        Arc::new(drift_platform_win::WinPlatform)
    }
    #[cfg(not(windows))]
    {
        Arc::new(drift_net::NoPlatform)
    }
}

/// `ladder`: which fast links we may host when the phone asks (None = all).
pub async fn run(paths: Vec<PathBuf>, to: Option<String>, seconds: u64, device_name: String, ladder: Option<Vec<drift_net::Rung>>) -> Result<()> {
    if paths.is_empty() {
        bail!("nothing to send");
    }
    let (discovery, mut found) = nearby::discover()?;
    println!("Looking for receivers (Quick Share visible to Everyone)...");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let target = loop {
        let ev = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => bail!("no matching receiver found in {seconds}s"),
            Some(ev) = found.recv() => ev,
        };
        if let NearbyEvent::Found(d) | NearbyEvent::Updated(d) = ev {
            if to.as_deref().is_none_or(|t| d.name.to_lowercase().contains(&t.to_lowercase())) {
                println!("Found {:?} ({:?}) via {:?}", d.name, d.device_type, d.routes);
                break d;
            }
        }
    };
    drop(discovery); // stop the beacon and scanners before connecting

    let local = drift_net::LocalIdentity {
        device_key: None,
        endpoint_id: drift_core::endpoint::random_endpoint_id(),
        #[cfg(windows)]
        wifi_direct_device_name: drift_platform_win::wifi_direct_device_name(),
        #[cfg(not(windows))]
        wifi_direct_device_name: None,
        os: drift_core::proto::connections::os_info::OsType::Windows,
    };
    let (pin_tx, pin_rx) = tokio::sync::oneshot::channel();
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let health = Arc::new(drift_net::Health::default());
    let name = target.name.clone();
    let task = tokio::spawn(async move {
        nearby::send(&target, paths, local, &device_name, platform(), health, ladder, pin_tx, ev_tx, None).await
    });
    if let Ok((pin, _route)) = pin_rx.await {
        println!("Connected to {name:?}. PIN {pin}: check it matches the phone.");
    }
    let mut last = std::time::Instant::now();
    while let Some(ev) = ev_rx.recv().await {
        match ev {
            SendEvent::PeerKey(k) => println!("Receiver is Drift (device {}).", k.id_hex()),
            SendEvent::WaitingForAccept => println!("Waiting for the phone to accept..."),
            SendEvent::Accepted => println!("Accepted, sending."),
            SendEvent::MediumChanged { medium, setup } => println!("Now on {medium} (set up in {setup:.1?})"),
            SendEvent::Progress { sent, total, bytes_per_sec } => {
                if last.elapsed() >= Duration::from_millis(500) || sent == total {
                    last = std::time::Instant::now();
                    println!("  {:>5.1}%  {}/s", 100.0 * sent as f64 / total.max(1) as f64, human(bytes_per_sec));
                }
            }
            SendEvent::Declined(why) => println!("Declined by the phone ({why})."),
            SendEvent::Cancelled => println!("Cancelled."),
            SendEvent::Completed => println!("Done."),
        }
    }
    task.await??;
    Ok(())
}
