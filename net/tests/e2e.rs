//! Drift sender ↔ Drift receiver over an in-memory pipe: handshake, PIN,
//! PairedKey exchange, introduction, accept, file transfer, completion.
//! No radios, no phone — protocol regressions show up here first.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use drift_core::endpoint::{DeviceType, EndpointInfo};
use drift_core::proto::connections::os_info::OsType;
use drift_net::send::SendEvent;
use drift_net::{Health, LocalIdentity, NoPlatform, ReceiveConfig, ReceiveEvent, Timeouts};
use tokio::sync::mpsc;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("drift-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ident(id: &[u8; 4]) -> LocalIdentity {
    LocalIdentity { endpoint_id: *id, device_key: None, wifi_direct_device_name: None, os: OsType::Windows }
}

fn recv_cfg(dir: PathBuf) -> ReceiveConfig {
    ReceiveConfig {
        download_dir: dir,
        ladder: vec![],
        hold_after_switch: None,
        health: Arc::new(Health::default()),
        timeouts: Timeouts::default(),
        stall_timeout: Duration::from_secs(6),
        cancel: None,
        bootstrap_lan: false,
    }
}

/// Run one share; `accept` decides on the receiver. Returns (sender events, receiver events).
async fn share(files: Vec<PathBuf>, out: PathBuf, accept: bool) -> (Vec<SendEvent>, Vec<ReceiveEvent>) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let rx_local = ident(b"RECV");
    let tx_local = ident(b"SEND");

    let (rev_tx, mut rev_rx) = mpsc::unbounded_channel();
    let receiver = tokio::spawn(async move {
        let conn = drift_net::accept(b, &rx_local).await.expect("accept");
        drift_net::receive::run(conn, &rx_local, &recv_cfg(out), Arc::new(NoPlatform), rev_tx).await
    });

    let (sev_tx, mut sev_rx) = mpsc::unbounded_channel();
    let sender = tokio::spawn(async move {
        let info = EndpointInfo::new_visible("Test PC", DeviceType::Laptop).encode();
        let offer = drift_net::send::default_offer();
        let conn = drift_net::send::connect(a, &tx_local, &info, &offer).await.expect("connect");
        drift_net::send::run(conn, &tx_local, files, Arc::new(NoPlatform), Arc::new(Health::default()), None, sev_tx, None).await
    });

    let mut r_events = vec![];
    let mut s_events = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let (mut r_open, mut s_open) = (true, true);
    while r_open || s_open {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => panic!("share timed out; sender {s_events:?}"),
            ev = rev_rx.recv(), if r_open => match ev {
                Some(ReceiveEvent::Request { decision, .. }) => { let _ = decision.send(accept); }
                Some(ev) => r_events.push(ev),
                None => r_open = false,
            },
            ev = sev_rx.recv(), if s_open => match ev {
                Some(ev) => s_events.push(ev),
                None => s_open = false,
            },
        }
    }
    let _ = sender.await.unwrap();
    let _ = receiver.await.unwrap();
    (s_events, r_events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drift_to_drift_files_arrive_intact() {
    let src = tmpdir("src");
    let out = tmpdir("out");
    let small = src.join("note.txt");
    std::fs::write(&small, b"hello drift").unwrap();
    let big = src.join("blob.bin");
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    std::fs::write(&big, &data).unwrap();

    let (s, r) = share(vec![small, big], out.clone(), true).await;

    assert!(s.iter().any(|e| matches!(e, SendEvent::Accepted)), "sender events: {s:?}");
    assert!(s.iter().any(|e| matches!(e, SendEvent::Completed)), "sender events: {s:?}");
    assert!(r.iter().any(|e| matches!(e, ReceiveEvent::Completed)));
    assert_eq!(std::fs::read(out.join("note.txt")).unwrap(), b"hello drift");
    assert_eq!(std::fs::read(out.join("blob.bin")).unwrap(), data);
    // No leftover partial files.
    assert!(std::fs::read_dir(&out).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".part")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decline_is_reported_and_nothing_is_written() {
    let src = tmpdir("src2");
    let out = tmpdir("out2");
    let f = src.join("x.bin");
    std::fs::write(&f, vec![1u8; 1000]).unwrap();

    let (s, r) = share(vec![f], out.clone(), false).await;

    assert!(s.iter().any(|e| matches!(e, SendEvent::Declined(_))), "sender events: {s:?}");
    assert!(r.iter().any(|e| matches!(e, ReceiveEvent::Declined)));
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
}

/// Fault injection: forward the first `budget` bytes sender→receiver, then go
/// silent (receiver→sender keeps flowing). Models a link that dies mid-transfer.
async fn stalling_proxy(
    sender_side: tokio::io::DuplexStream,
    receiver_side: tokio::io::DuplexStream,
    budget: usize,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut sr, mut sw) = tokio::io::split(sender_side);
    let (mut rr, mut rw) = tokio::io::split(receiver_side);
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut rr, &mut sw).await;
    });
    let mut left = budget;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match sr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if left == 0 {
            continue; // swallow: the link is "dead"
        }
        let k = n.min(left);
        if rw.write_all(&buf[..k]).await.is_err() {
            break;
        }
        left -= k;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_link_is_detected_and_part_file_kept() {
    let src = tmpdir("src3");
    let out = tmpdir("out3");
    let f = src.join("big.bin");
    std::fs::write(&f, vec![7u8; 4_000_000]).unwrap();

    let (a, a_far) = tokio::io::duplex(1 << 20);
    let (b_far, b) = tokio::io::duplex(1 << 20);
    tokio::spawn(stalling_proxy(a_far, b_far, 600_000));

    let rx_local = ident(b"RECV");
    let tx_local = ident(b"SEND");
    let (rev_tx, mut rev_rx) = mpsc::unbounded_channel();
    let mut cfg = recv_cfg(out.clone());
    cfg.stall_timeout = Duration::from_millis(250); // bootstrap limit = 4x = 1 s
    let receiver = tokio::spawn(async move {
        let conn = drift_net::accept(b, &rx_local).await.expect("accept");
        drift_net::receive::run(conn, &rx_local, &cfg, Arc::new(NoPlatform), rev_tx).await
    });
    let (sev_tx, _sev_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let info = EndpointInfo::new_visible("Test PC", DeviceType::Laptop).encode();
        let conn = drift_net::send::connect(a, &tx_local, &info, &drift_net::send::default_offer()).await.expect("connect");
        drift_net::send::run(conn, &tx_local, vec![f], Arc::new(NoPlatform), Arc::new(Health::default()), None, sev_tx, None).await
    });

    let mut stalled = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, rev_rx.recv()).await {
        match ev {
            ReceiveEvent::Request { decision, .. } => {
                let _ = decision.send(true);
            }
            ReceiveEvent::Stalled { .. } => stalled = true,
            _ => {}
        }
    }
    assert!(stalled, "receiver never reported the stall");
    assert!(receiver.await.unwrap().is_err(), "a stalled share should end with an error");
    let parts: Vec<_> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().path()).collect();
    let part = parts.iter().find(|p| p.to_string_lossy().ends_with(".part")).unwrap_or_else(|| panic!("partial file should be kept for resume: {parts:?}"));
    // what arrived before the stall is on disk (not lost in the write buffer)
    let len = std::fs::metadata(part).unwrap().len();
    assert!(len > 0 && len < 4_000_000, "partial file should hold the bytes that arrived, has {len}");
}

/// Cancel mid-transfer from either side: the other side is told (sharing CANCEL), both end
/// cleanly, and no partial file is left behind.
async fn cancel_mid_transfer(receiver_cancels: bool, tag: &str) -> (Vec<SendEvent>, Vec<ReceiveEvent>, PathBuf) {
    let src = tmpdir(&format!("src-{tag}"));
    let out = tmpdir(&format!("out-{tag}"));
    let f = src.join("big.bin");
    std::fs::write(&f, vec![9u8; 40_000_000]).unwrap();
    let (a, b) = tokio::io::duplex(1 << 20);
    let rx_local = ident(b"RECV");
    let tx_local = ident(b"SEND");
    let r_cancel = Arc::new(tokio::sync::Notify::new());
    let s_cancel = Arc::new(tokio::sync::Notify::new());
    let mut cfg = recv_cfg(out.clone());
    cfg.cancel = Some(r_cancel.clone());
    let (rev_tx, mut rev_rx) = mpsc::unbounded_channel();
    let receiver = tokio::spawn(async move {
        let conn = drift_net::accept(b, &rx_local).await.expect("accept");
        drift_net::receive::run(conn, &rx_local, &cfg, Arc::new(NoPlatform), rev_tx).await
    });
    let (sev_tx, mut sev_rx) = mpsc::unbounded_channel();
    let sc = s_cancel.clone();
    let sender = tokio::spawn(async move {
        let info = EndpointInfo::new_visible("Test PC", DeviceType::Laptop).encode();
        let conn = drift_net::send::connect(a, &tx_local, &info, &drift_net::send::default_offer()).await.expect("connect");
        drift_net::send::run(conn, &tx_local, vec![f], Arc::new(NoPlatform), Arc::new(Health::default()), None, sev_tx, Some(sc)).await
    });
    let (mut r_events, mut s_events) = (vec![], vec![]);
    let (mut r_open, mut s_open, mut fired) = (true, true, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while r_open || s_open {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => panic!("timed out; sender {s_events:?} receiver {} events", r_events.len()),
            ev = rev_rx.recv(), if r_open => match ev {
                Some(ReceiveEvent::Request { decision, .. }) => { let _ = decision.send(true); }
                Some(ev) => {
                    if !fired && matches!(ev, ReceiveEvent::Progress { received, .. } if received > 0) {
                        fired = true;
                        if receiver_cancels { r_cancel.notify_one() } else { s_cancel.notify_one() }
                    }
                    r_events.push(ev)
                }
                None => r_open = false,
            },
            ev = sev_rx.recv(), if s_open => match ev { Some(ev) => s_events.push(ev), None => s_open = false },
        }
    }
    assert!(receiver.await.unwrap().is_ok());
    assert!(sender.await.unwrap().is_ok());
    (s_events, r_events, out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receiver_cancel_tells_the_sender() {
    let (s, r, out) = cancel_mid_transfer(true, "rc").await;
    assert!(s.iter().any(|e| matches!(e, SendEvent::Cancelled)), "sender should hear the cancel: {s:?}");
    assert!(r.iter().any(|e| matches!(e, ReceiveEvent::Cancelled)));
    assert!(!s.iter().any(|e| matches!(e, SendEvent::Completed)));
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "partial file removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sender_cancel_tells_the_receiver() {
    let (s, r, out) = cancel_mid_transfer(false, "sc").await;
    assert!(s.iter().any(|e| matches!(e, SendEvent::Cancelled)));
    assert!(r.iter().any(|e| matches!(e, ReceiveEvent::Cancelled)), "receiver should hear the cancel ({} events)", r.len());
    assert!(!r.iter().any(|e| matches!(e, ReceiveEvent::Completed)));
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "partial file removed");
}

/// A link (and a file with it) arrives as text with the right kind and content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn links_and_text_arrive() {
    use drift_core::proto::sharing::text_metadata::Type as K;
    let src = tmpdir("src-text");
    let out = tmpdir("out-text");
    let f = src.join("note.txt");
    std::fs::write(&f, b"with a link").unwrap();
    let (a, b) = tokio::io::duplex(1 << 20);
    let rx_local = ident(b"RECV");
    let tx_local = ident(b"SEND");
    let (rev_tx, mut rev_rx) = mpsc::unbounded_channel();
    let receiver = tokio::spawn(async move {
        let conn = drift_net::accept(b, &rx_local).await.expect("accept");
        drift_net::receive::run(conn, &rx_local, &recv_cfg(out), Arc::new(NoPlatform), rev_tx).await
    });
    let (sev_tx, _sev_rx) = mpsc::unbounded_channel();
    let texts = vec![drift_core::send::OutgoingText::new(K::Url, "https://example.com/a?b=c")];
    let sender = tokio::spawn(async move {
        let info = EndpointInfo::new_visible("Test PC", DeviceType::Laptop).encode();
        let conn = drift_net::send::connect(a, &tx_local, &info, &drift_net::send::default_offer()).await.expect("connect");
        drift_net::send::run_with(conn, &tx_local, vec![f], texts, Arc::new(NoPlatform), Arc::new(Health::default()), None, sev_tx, None).await
    });
    let (mut got, mut completed, mut announced) = (None, false, 0);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, rev_rx.recv()).await {
        match ev {
            ReceiveEvent::Request { texts, decision, .. } => {
                announced = texts.len();
                let _ = decision.send(true);
            }
            ReceiveEvent::Text { content, .. } => got = Some(content),
            ReceiveEvent::Completed => completed = true,
            _ => {}
        }
    }
    let _ = sender.await;
    let _ = receiver.await;
    assert_eq!(announced, 1, "the introduction lists the link");
    assert_eq!(got.as_deref(), Some("https://example.com/a?b=c"));
    assert!(completed);
}

/// Trust between Drift devices: each side proves its device key in the session; a peer without
/// the Drift tag (stock Quick Share) never gets ours and isn't trusted.
async fn trust_session(sender_drift: bool) -> (Option<drift_core::trust::PeerKey>, bool, Option<drift_core::trust::PeerKey>, [u8; 6], [u8; 6]) {
    use drift_core::trust::DeviceKey;
    let src = tmpdir(&format!("src-trust-{sender_drift}"));
    let out = tmpdir(&format!("out-trust-{sender_drift}"));
    let f = src.join("hi.txt");
    std::fs::write(&f, b"hello").unwrap();
    let rk = Arc::new(DeviceKey::generate());
    let sk = Arc::new(DeviceKey::generate());
    let (rid, sid) = (rk.key_id(), sk.key_id());
    let (a, b) = tokio::io::duplex(1 << 20);
    let mut rx_local = ident(b"RECV");
    rx_local.device_key = Some(rk);
    let mut tx_local = ident(b"SEND");
    if sender_drift { tx_local.device_key = Some(sk); }
    let (rev_tx, mut rev_rx) = mpsc::unbounded_channel();
    let receiver = tokio::spawn(async move {
        let conn = drift_net::accept(b, &rx_local).await.expect("accept");
        drift_net::receive::run(conn, &rx_local, &recv_cfg(out), Arc::new(NoPlatform), rev_tx).await
    });
    let (sev_tx, mut sev_rx) = mpsc::unbounded_channel();
    let sender = tokio::spawn(async move {
        let info = if sender_drift { EndpointInfo::new_drift("Drift PC", DeviceType::Laptop) } else { EndpointInfo::new_visible("Phone", DeviceType::Phone) };
        let mut conn = drift_net::send::connect(a, &tx_local, &info.encode(), &drift_net::send::default_offer()).await.expect("connect");
        conn.peer_drift = true; // the receiver advertises the Drift tag
        drift_net::send::run(conn, &tx_local, vec![f], Arc::new(NoPlatform), Arc::new(Health::default()), None, sev_tx, None).await
    });
    let (mut r_key, mut r_drift, mut s_key) = (None, false, None);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (mut r_open, mut s_open) = (true, true);
    while r_open || s_open {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => panic!("timed out"),
            ev = rev_rx.recv(), if r_open => match ev {
                Some(ReceiveEvent::Request { drift, peer_key, decision, .. }) => { r_drift = drift; r_key = peer_key; let _ = decision.send(true); }
                Some(_) => {}
                None => r_open = false,
            },
            ev = sev_rx.recv(), if s_open => match ev {
                Some(SendEvent::PeerKey(k)) => s_key = Some(k),
                Some(_) => {}
                None => s_open = false,
            },
        }
    }
    let _ = sender.await;
    let _ = receiver.await;
    (r_key, r_drift, s_key, rid, sid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drift_devices_prove_their_keys_to_each_other() {
    let (r_key, r_drift, s_key, rid, sid) = trust_session(true).await;
    assert!(r_drift, "receiver sees the Drift tag");
    assert_eq!(r_key.map(|k| k.id), Some(sid), "receiver verified the sender's key");
    assert_eq!(s_key.map(|k| k.id), Some(rid), "sender verified the receiver's key");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stock_senders_are_not_trusted() {
    let (r_key, r_drift, _s_key, _rid, _sid) = trust_session(false).await;
    assert!(!r_drift, "no Drift tag");
    assert_eq!(r_key, None, "no device key from a stock sender");
}
