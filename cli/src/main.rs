//! `drift` — command-line front end until the native UIs exist.
//!
//!   drift receive [--name NAME] [--out DIR] [--ladder hotspot,lan,phone,direct|none] [--yes]
//!
//! Quit Google's Quick Share for Windows first (it registers the same Bluetooth service).

mod scan;
mod sendcmd;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use drift_app::{Config, ServiceEvent};
use drift_net::{Medium, ReceiveEvent, Rung};

fn downloads() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn parse_ladder(s: &str) -> Result<Vec<Rung>> {
    if s == "none" {
        return Ok(vec![]);
    }
    s.split(',')
        .map(|r| match r.trim() {
            "hotspot" => Ok(Rung::PcHostedHotspot),
            "direct" => Ok(Rung::PcHostedDirect),
            "phone" => Ok(Rung::PhoneHosted),
            "lan" => Ok(Rung::Lan),
            other => bail!("unknown rung {other:?} (hotspot, direct, phone, lan)"),
        })
        .collect()
}

fn human(bytes: f64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut v = bytes;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", units[u])
}

/// Wall-clock time (UTC) for lining progress up with phone/Windows logs.
fn now() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let s = t.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}Z", s / 3600, (s / 60) % 60, s % 60, t.subsec_millis())
}

fn ask(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("receive") => {}
        Some("send") => {
            // drift send FILE... [--to NAME] [--seconds N] [--ladder hotspot,direct,lan|none]
            let (mut paths, mut to, mut secs, mut ladder) = (vec![], None, 30u64, None);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--to" => to = args.next(),
                    "--seconds" => secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(30),
                    "--ladder" => ladder = Some(parse_ladder(&args.next().unwrap_or_default())?),
                    f => paths.push(PathBuf::from(f)),
                }
            }
            let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Drift".into());
            return sendcmd::run(paths, to, secs, name, ladder).await;
        }
        #[cfg(windows)]
        Some("wfd-join") => {
            // drift wfd-join [--name P2P-NAME] [--mb N] [--port P]: probe for a phone-hosted group. Join it as a
            // P2P client (pairs once with push-button, reuses the pairing after), then time an N MB send to
            // the phone's `toybox nc -L -p P wc -c` listener at the group owner's address.
            let (mut name, mut mb, mut port, mut hold) = (String::new(), 200usize, 47622u16, 0u64);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--name" => name = args.next().unwrap_or_default(),
                    "--mb" => mb = args.next().and_then(|v| v.parse().ok()).unwrap_or(200),
                    "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(47622),
                    "--hold" => hold = args.next().and_then(|v| v.parse().ok()).unwrap_or(60),
                    other => bail!("unknown argument {other}"),
                }
            }
            anyhow::ensure!(!name.is_empty(), "--name <phone's Wi-Fi Direct name> is required");
            let t0 = std::time::Instant::now();
            let link = drift_platform_win::wfd::join_group(&name, "", std::time::Duration::from_secs(30)).await?;
            println!("{:6.1}s joined: local {} remote {}", t0.elapsed().as_secs_f32(), link.local_ip, link.remote_ip);
            if mb == 0 {
                // --mb 0 --hold N: keep the link for N s (phone-to-PC tests)
                tokio::time::sleep(std::time::Duration::from_secs(hold)).await;
                println!("{:6.1}s released", t0.elapsed().as_secs_f32());
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect((link.remote_ip.as_str(), port)).await?;
            let buf = vec![0u8; 1 << 20];
            let t1 = std::time::Instant::now();
            for _ in 0..mb {
                s.write_all(&buf).await?;
            }
            s.shutdown().await?;
            let mut sink = Vec::new();
            let _ = s.read_to_end(&mut sink).await;
            let secs = t1.elapsed().as_secs_f64();
            println!("{:6.1}s sent {mb} MB in {secs:.2}s = {:.1} MB/s", t0.elapsed().as_secs_f32(), mb as f64 / secs);
            drop(link);
            return Ok(());
        }
        #[cfg(windows)]
        Some("wfd-accept") => {
            // drift wfd-accept [--seconds N] [--name PREFIX]: probe for the companion's Wi-Fi Direct
            // path (C1): host a discoverable group, admit joins from PREFIX in code, log each step.
            let (mut secs, mut name) = (120u64, String::new());
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--seconds" => secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(120),
                    "--name" => name = args.next().unwrap_or_default(),
                    other => bail!("unknown argument {other}"),
                }
            }
            let t0 = std::time::Instant::now();
            let log: std::sync::Arc<dyn Fn(String) + Send + Sync> =
                std::sync::Arc::new(move |m: String| println!("{:6.1}s {m}", t0.elapsed().as_secs_f32()));
            let allow: std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync> = std::sync::Arc::new(move |n: &str| n.starts_with(name.as_str()));
            let g = drift_platform_win::wfd::start_accepting_group(allow, log.clone()).await?;
            log(format!("group up: ssid={} pass={} ip={} (discoverable)", g.group.ssid, g.group.password, g.group.ip));
            // DRIFT_WFD_STOPADV=1: stop advertising once someone is admitted (link-speed probe)
            let stop_adv = std::env::var_os("DRIFT_WFD_STOPADV").is_some();
            let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            let mut stopped = false;
            while std::time::Instant::now() < end {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                if stop_adv && !stopped && g.clients() > 0 {
                    stopped = true;
                    log(format!("stopping advertisement: {:?}", g.group.stop_advertising()));
                }
            }
            log(format!("done; {} client(s) admitted", g.clients()));
            return Ok(());
        }
        Some("scan") => {
            // drift scan [--seconds N] [--notify] [--no-bt] [--no-wake]
            let (mut secs, mut kind, mut require_bt, mut wake) = (30, drift_core::ble::FastInitType::Silent, true, true);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--seconds" => secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(30),
                    "--notify" => kind = drift_core::ble::FastInitType::Notify,
                    "--no-bt" => require_bt = false,
                    "--no-wake" => wake = false,
                    other => bail!("unknown argument {other}"),
                }
            }
            return scan::run(secs, kind, require_bt, wake).await;
        }
        _ => bail!(
            "usage: drift receive [--name NAME] [--out DIR] [--ladder hotspot,lan,...|none] [--yes]\n       drift scan [--seconds N] [--notify] [--no-bt] [--no-wake]"
        ),
    }
    let mut cfg = Config {
        device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Drift".into()),
        download_dir: downloads().join("Drift"),
        ladder: Config::default_ladder(),
        device_key: None,
    };
    let mut yes = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => cfg.device_name = args.next().unwrap_or_default(),
            "--out" => cfg.download_dir = PathBuf::from(args.next().unwrap_or_default()),
            "--ladder" => cfg.ladder = parse_ladder(&args.next().unwrap_or_default())?,
            "--yes" => yes = true,
            other => bail!("unknown argument {other}"),
        }
    }

    let (_service, mut events) = drift_app::Service::start(cfg.clone()).await?;
    while let Some(ev) = events.recv().await {
        match ev {
            ServiceEvent::Started { endpoint_id, mdns, ble, bluetooth } => {
                println!("Drift is visible as \"{}\" (endpoint {endpoint_id})", cfg.device_name);
                println!("  mDNS: {}  BLE: {}  Bluetooth: {}", if mdns { "on" } else { "off" }, ble.unwrap_or_else(|| "off".into()), if bluetooth { "on" } else { "off" });
                println!("  ladder: {}", cfg.ladder.iter().map(|r| r.label()).collect::<Vec<_>>().join(" → "));
                println!("  saving to {}", cfg.download_dir.display());
            }
            ServiceEvent::Warning(w) => println!("! {w}"),
            ServiceEvent::Connection { id, bootstrap, peer_addr } => println!("[{id}] connection over {bootstrap:?} from {peer_addr}"),
            ServiceEvent::Share { id, event } => match event {
                ReceiveEvent::Request { peer, pin, files, texts, decision, .. } => {
                    println!("[{id}] {peer} wants to send (PIN {pin}):");
                    for f in &files {
                        println!("       {}  {}", f.name, human(f.size as f64));
                    }
                    for t in &texts {
                        println!("       [{:?}] {}", t.kind, t.title);
                    }
                    let ok = yes || tokio::task::spawn_blocking(|| ask("       accept?")).await.unwrap_or(false);
                    let _ = decision.send(ok);
                }
                ReceiveEvent::MediumChanged { medium, setup } => match medium {
                    Medium::Upgraded(r) => println!("[{id}] now on {} (set up in {:.1?})", r.label(), setup.unwrap_or_default()),
                    Medium::SameHotspot => println!("[{id}] now on the shared hotspot (set up in {:.1?})", setup.unwrap_or_default()),
                    Medium::Bootstrap => {}
                },
                ReceiveEvent::Progress { received, total, bytes_per_sec } => {
                    if bytes_per_sec > 0.0 {
                        println!("[{id}] {} {:>5.1}%  {}/s", now(), 100.0 * received as f64 / total.max(1) as f64, human(bytes_per_sec));
                    }
                }
                ReceiveEvent::Text { item, content } => println!("[{id}] {:?}: {content}", item.kind),
                ReceiveEvent::FileSaved { path, bytes } => println!("[{id}] saved {} ({})", path.display(), human(bytes as f64)),
                ReceiveEvent::Completed => println!("[{id}] done"),
                ReceiveEvent::Declined => println!("[{id}] declined"),
                ReceiveEvent::Cancelled => println!("[{id}] cancelled by sender"),
                ReceiveEvent::Stalled { medium, after } => {
                    println!("[{id}] link stalled on {medium:?} (no data for {after:.1?}); partial file kept, next try skips it")
                }
            },
            ServiceEvent::Finished { id, error } => {
                if let Some(e) = error {
                    println!("[{id}] ended with error: {e}");
                }
            }
        }
    }
    Ok(())
}
