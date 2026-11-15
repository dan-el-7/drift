//! Crypto throughput of the secure channel (AES-256-CBC + HMAC-SHA256) on 512 KiB frames, as the
//! file transfer uses it. `cargo run --release -p drift-core --example seal_bench` (or build for
//! Android and run it with adb) to see whether encryption caps a link.
use drift_core::secure::split;
use drift_core::ukey2::SessionKeys;
use std::time::Instant;

fn main() {
    let keys = SessionKeys { encrypt: [1; 32], send_hmac: [2; 32], decrypt: [1; 32], recv_hmac: [2; 32] };
    let (mut sealer, mut opener) = split(&keys);
    let frame = vec![7u8; 512 * 1024];
    let n = 200;
    let t = Instant::now();
    let mut sealed = Vec::new();
    for _ in 0..n {
        sealed = sealer.seal(&frame);
    }
    let s = t.elapsed().as_secs_f64();
    println!("seal: {:.0} MB/s", (n * frame.len()) as f64 / s / 1e6);
    let t = Instant::now();
    for _ in 0..n {
        let _ = opener.open(&sealed);
    }
    let s = t.elapsed().as_secs_f64();
    println!("open: {:.0} MB/s (seq checks may fail after the first; timing only)", (n * frame.len()) as f64 / s / 1e6);
}
