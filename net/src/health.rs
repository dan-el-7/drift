//! Medium health tracking shared across connections (failure catalogue F6/F4).
//!
//! The PC-hosted Wi-Fi Direct group can get into a state where Windows reports
//! it as started but it isn't usable (no beacon, or the link dies mid-transfer).
//! Retrying it on every share wastes ~15–45 s each time, so after repeated
//! failures we stop offering it for a cool-down period and go straight to the
//! next rung. Deterministic and clock-injectable for tests.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedState {
    Healthy,
    /// Skipped until the given instant.
    Unhealthy { until: Instant },
}

#[derive(Debug)]
struct Inner {
    consecutive_failures: u32,
    state: HostedState,
}

#[derive(Debug)]
pub struct Health {
    inner: Mutex<Inner>,
    /// Failures in a row before the hosted rungs are benched.
    threshold: u32,
    cooldown: Duration,
}

impl Default for Health {
    fn default() -> Self {
        Self::new(2, Duration::from_secs(600))
    }
}

impl Health {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self { inner: Mutex::new(Inner { consecutive_failures: 0, state: HostedState::Healthy }), threshold, cooldown }
    }

    pub fn hosted_state_at(&self, now: Instant) -> HostedState {
        let mut g = self.inner.lock().unwrap();
        if let HostedState::Unhealthy { until } = g.state {
            if now >= until {
                g.state = HostedState::Healthy;
                g.consecutive_failures = 0;
            }
        }
        g.state
    }

    pub fn hosted_ok(&self) -> bool {
        self.hosted_state_at(Instant::now()) == HostedState::Healthy
    }

    /// The phone was offered the hosted group but never connected / the attempt failed.
    pub fn record_hosted_failure_at(&self, now: Instant) {
        let mut g = self.inner.lock().unwrap();
        g.consecutive_failures += 1;
        if g.consecutive_failures >= self.threshold {
            g.state = HostedState::Unhealthy { until: now + self.cooldown };
        }
    }

    pub fn record_hosted_failure(&self) {
        self.record_hosted_failure_at(Instant::now());
    }

    /// A hosted link carried data and then stopped: bench immediately.
    pub fn record_hosted_stall_at(&self, now: Instant) {
        let mut g = self.inner.lock().unwrap();
        g.consecutive_failures = self.threshold;
        g.state = HostedState::Unhealthy { until: now + self.cooldown };
    }

    pub fn record_hosted_stall(&self) {
        self.record_hosted_stall_at(Instant::now());
    }

    pub fn record_hosted_success(&self) {
        let mut g = self.inner.lock().unwrap();
        g.consecutive_failures = 0;
        g.state = HostedState::Healthy;
    }

    /// User fixed something (e.g. reset Wi-Fi): try hosted again right away.
    pub fn reset(&self) {
        self.record_hosted_success();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benches_after_threshold_and_recovers_after_cooldown() {
        let h = Health::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        h.record_hosted_failure_at(t0);
        assert_eq!(h.hosted_state_at(t0), HostedState::Healthy, "one failure is not enough");
        h.record_hosted_failure_at(t0);
        assert!(matches!(h.hosted_state_at(t0), HostedState::Unhealthy { .. }));
        assert!(matches!(h.hosted_state_at(t0 + Duration::from_secs(59)), HostedState::Unhealthy { .. }));
        assert_eq!(h.hosted_state_at(t0 + Duration::from_secs(60)), HostedState::Healthy);
    }

    #[test]
    fn success_resets_count() {
        let h = Health::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        h.record_hosted_failure_at(t0);
        h.record_hosted_success();
        h.record_hosted_failure_at(t0);
        assert_eq!(h.hosted_state_at(t0), HostedState::Healthy);
    }

    #[test]
    fn stall_benches_immediately() {
        let h = Health::new(3, Duration::from_secs(60));
        let t0 = Instant::now();
        h.record_hosted_stall_at(t0);
        assert!(matches!(h.hosted_state_at(t0), HostedState::Unhealthy { .. }));
        h.reset();
        assert_eq!(h.hosted_state_at(t0), HostedState::Healthy);
    }
}
