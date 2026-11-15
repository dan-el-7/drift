//! Where a transfer's time goes, stage by stage (profiling; cheap enough to leave on). Each stage adds
//! up the time spent in it; the sender reports when its files are sent, the receiver when a file is
//! closed. One transfer at a time is what we measure, so process-wide counters are enough.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

pub struct Stage(&'static str, AtomicU64);

impl Stage {
    pub fn add(&self, since: Instant) {
        self.1.fetch_add(since.elapsed().as_nanos() as u64, Relaxed);
    }
    fn take(&self) -> (&'static str, f64) {
        (self.0, self.1.swap(0, Relaxed) as f64 / 1e6)
    }
}

// sender
pub static FILE_READ: Stage = Stage("file read", AtomicU64::new(0));
pub static PERMIT_WAIT: Stage = Stage("waiting for flow-control permits", AtomicU64::new(0));
pub static SEAL: Stage = Stage("encrypting (seal)", AtomicU64::new(0));
pub static QUEUE_WAIT: Stage = Stage("waiting for socket queue space", AtomicU64::new(0));
pub static SOCKET_WRITE: Stage = Stage("socket writes", AtomicU64::new(0));
// receiver
pub static SOCKET_READ: Stage = Stage("socket reads", AtomicU64::new(0));
pub static OPEN: Stage = Stage("decrypting (open) + decode", AtomicU64::new(0));
pub static DISPATCH_WAIT: Stage = Stage("waiting for the session loop", AtomicU64::new(0));
pub static DISK_WRITE: Stage = Stage("disk writes", AtomicU64::new(0));

pub fn reset() {
    for s in all() {
        s.take();
    }
}

fn all() -> [&'static Stage; 9] {
    [&FILE_READ, &PERMIT_WAIT, &SEAL, &QUEUE_WAIT, &SOCKET_WRITE, &SOCKET_READ, &OPEN, &DISPATCH_WAIT, &DISK_WRITE]
}

/// Log every stage that took any time over `elapsed` (the wall time of the transfer), then reset.
pub fn report(what: &str, bytes: u64, elapsed: std::time::Duration) {
    let secs = elapsed.as_secs_f64().max(1e-6);
    let parts: Vec<String> = all()
        .iter()
        .map(|s| s.take())
        .filter(|(_, ms)| *ms > 0.5)
        .map(|(n, ms)| format!("{n} {ms:.0} ms ({:.0}%)", ms / 10.0 / secs))
        .collect();
    tracing::info!("{what}: {:.1} MB in {secs:.2} s = {:.1} MB/s; {}", bytes as f64 / 1e6, bytes as f64 / secs / 1048576.0, parts.join(", "));
}
