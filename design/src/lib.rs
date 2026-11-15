//! Drift design system in code: the motion spec (`design/motion.json`) and
//! tokens (`design/tokens.json`), conversions to each toolkit's spring
//! parameters, and a reference spring solver so the Linux UI's custom widgets
//! move exactly like Windows' Composition springs.

use std::collections::BTreeMap;
use std::f64::consts::PI;

use serde::Deserialize;

pub const MOTION_JSON: &str = include_str!("../motion.json");
pub const TOKENS_JSON: &str = include_str!("../tokens.json");

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Spring {
    /// 1 = critically damped (no overshoot), < 1 = bouncy.
    pub damping_ratio: f64,
    /// Undamped period in milliseconds ("how quick it feels").
    pub response_ms: f64,
}

#[derive(Debug, Deserialize)]
struct SpringEntry {
    damping_ratio: f64,
    response_ms: f64,
}

#[derive(Debug, Deserialize)]
struct MotionFile {
    springs: BTreeMap<String, SpringEntry>,
}

/// All named springs from the spec.
pub fn springs() -> BTreeMap<String, Spring> {
    let m: MotionFile = serde_json::from_str(MOTION_JSON).expect("motion.json is valid");
    m.springs.into_iter().map(|(k, v)| (k, Spring { damping_ratio: v.damping_ratio, response_ms: v.response_ms })).collect()
}

/// Windows.UI.Composition `SpringScalarNaturalMotionAnimation` parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositionSpring {
    pub damping_ratio: f32,
    /// `Period` (TimeSpan) in milliseconds.
    pub period_ms: f64,
}

/// GTK/libadwaita `AdwSpringParams` (damping ratio, mass, stiffness).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdwSpring {
    pub damping_ratio: f64,
    pub mass: f64,
    pub stiffness: f64,
}

impl Spring {
    /// Composition's `Period` behaves like 1/ω (its default, 48 ms, settles in ~0.3 s), not a
    /// full oscillation, so response (2π/ω) maps to response / 2π.
    pub fn composition(self) -> CompositionSpring {
        CompositionSpring { damping_ratio: self.damping_ratio as f32, period_ms: self.response_ms / (2.0 * PI) }
    }

    /// With mass 1: stiffness = (2π / period)², period in seconds.
    pub fn adw(self) -> AdwSpring {
        let omega = 2.0 * PI / (self.response_ms / 1000.0);
        AdwSpring { damping_ratio: self.damping_ratio, mass: 1.0, stiffness: omega * omega }
    }

    /// Reference solver: position at `t_ms` for a move from 0 to 1, starting at
    /// rest. Closed form for under-, critically- and over-damped springs.
    pub fn value_at(self, t_ms: f64) -> f64 {
        let t = t_ms / 1000.0;
        let w = 2.0 * PI / (self.response_ms / 1000.0);
        let z = self.damping_ratio;
        let x = if (z - 1.0).abs() < 1e-9 {
            (1.0 + w * t) * (-w * t).exp()
        } else if z < 1.0 {
            let wd = w * (1.0 - z * z).sqrt();
            (-z * w * t).exp() * ((wd * t).cos() + (z * w / wd) * (wd * t).sin())
        } else {
            let s = (z * z - 1.0).sqrt();
            let r1 = -w * (z - s);
            let r2 = -w * (z + s);
            (r2 * (r1 * t).exp() - r1 * (r2 * t).exp()) / (r2 - r1)
        };
        1.0 - x
    }

    /// Time until the motion stays within `epsilon` of the target.
    pub fn settle_ms(self, epsilon: f64) -> f64 {
        let step = 1.0;
        let mut last_out = 0.0;
        let mut t = 0.0;
        while t < 20_000.0 {
            if (1.0 - self.value_at(t)).abs() > epsilon {
                last_out = t;
            }
            t += step;
        }
        last_out + step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_and_has_core_springs() {
        let s = springs();
        for name in ["settle", "snappy", "drop", "breathe", "puff"] {
            assert!(s.contains_key(name), "missing spring {name}");
        }
        let _: serde_json::Value = serde_json::from_str(TOKENS_JSON).unwrap();
    }

    #[test]
    fn critically_damped_never_overshoots() {
        let s = springs()["settle"];
        assert!((0..3000).all(|t| s.value_at(t as f64) <= 1.0 + 1e-9));
        assert!(s.value_at(0.0).abs() < 1e-9);
    }

    #[test]
    fn bouncy_springs_overshoot_once_and_settle() {
        let s = springs()["drop"];
        let peak = (0..3000).map(|t| s.value_at(t as f64)).fold(0.0, f64::max);
        assert!(peak > 1.01 && peak < 1.2, "drop should overshoot softly, peak {peak}");
        assert!(s.settle_ms(0.005) < 1500.0);
    }

    #[test]
    fn adw_conversion_matches_period() {
        let s = Spring { damping_ratio: 1.0, response_ms: 500.0 };
        let a = s.adw();
        let period = 2.0 * PI / (a.stiffness / a.mass).sqrt();
        assert!((period * 1000.0 - 500.0).abs() < 1e-6);
        assert!((s.composition().period_ms - 500.0 / (2.0 * PI)).abs() < 1e-9, "Composition Period = 1/ω");
    }

    #[test]
    fn state_changes_feel_quick() {
        // Whole-UI state transitions should be done well under half a second.
        assert!(springs()["settle"].settle_ms(0.01) < 450.0);
        assert!(springs()["snappy"].settle_ms(0.01) < 300.0);
    }
}
