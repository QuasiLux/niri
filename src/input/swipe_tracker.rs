use std::collections::VecDeque;
use std::time::Duration;

/// History window for velocity averaging (GNOME default).
const HISTORY_LIMIT: Duration = Duration::from_millis(150);

/// Feel tuning (Balanced tier) — Hermes free-scroll feel polish.
///
/// Higher decel → longer, smoother fling. Was 0.997; macOS/iOS feel ≈ 0.9994.
const DECELERATION_TOUCHPAD: f64 = 0.9994;

/// Velocity → distance acceleration curve (macOS/iOS feel).
///
/// Real macOS is not linear: small flick velocities barely move, fast flicks
/// travel far. Approximate with `v_out = sign(v) * |v|^p`. 1.0 = linear
/// (old behavior); 1.3 ≈ modern trackpad feel.
const VELOCITY_CURVE_POWER: f64 = 1.30;

/// Initial per-event input damping.
///
/// Slows the very first pixels of motion without affecting fling.
/// 1.0 = linear (raw delta); 1.15 ≈ softer first-pixel feel.
const INPUT_DAMPING_POWER: f64 = 1.15;

/// Sensitivity multiplier applied by callers on top of the working-area mapping.
/// 1.0 = unchanged; 1.17 ≈ ~17% more sensitive.
const SENSITIVITY_GAIN: f64 = 1.17;

/// 1-Euro low-pass smoothing on the integrated tracker position.
///
/// Smooths jitter from very fast tiny events without adding perceptible delay
/// to real motion. `min_cutoff` = minimum cutoff frequency (Hz);
/// `beta` = speed coefficient (0.0 = uniform smoothing; higher = catch up
/// faster at high speed).
const ONE_EURO_MIN_CUTOFF: f64 = 1.0;
const ONE_EURO_BETA: f64 = 0.0;
const ONE_EURO_D_CUTOFF: f64 = 1.0;

#[derive(Debug)]
pub struct SwipeTracker {
    history: VecDeque<Event>,
    pos: f64,
    smoothed_pos: f64,
    prev_raw_pos: f64,
    prev_timestamp: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
struct Event {
    delta: f64,
    timestamp: Duration,
}

impl SwipeTracker {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            history: VecDeque::new(),
            pos: 0.,
            smoothed_pos: 0.,
            prev_raw_pos: 0.,
            prev_timestamp: None,
        }
    }

    /// Pushes a new reading into the tracker.
    pub fn push(&mut self, delta: f64, timestamp: Duration) {
        // For the events that we care about, timestamps should always increase
        // monotonically.
        if let Some(last) = self.history.back() {
            if timestamp < last.timestamp {
                trace!(
                    "ignoring event with timestamp {timestamp:?} earlier than last {:?}",
                    last.timestamp
                );
                return;
            }
        }

        // Apply per-event damping so the very first pixels feel softer.
        let delta = signed_power(delta, INPUT_DAMPING_POWER);

        self.history.push_back(Event { delta, timestamp });
        self.pos += delta;

        // 1-Euro smoothing on the integrated position. The first event has
        // no `prev_timestamp`, so we initialize and skip smoothing for it.
        if let Some(prev_t) = self.prev_timestamp {
            let dt = (timestamp - prev_t).as_secs_f64();
            if dt > 0. {
                let dx = self.pos - self.prev_raw_pos;
                let edx = dx / dt;
                let a_d = smoothing_factor(ONE_EURO_D_CUTOFF, dt);
                // Apply adaptive derivative cutoff so high speeds pass through.
                let _edx_hat = a_d * edx + (1. - a_d) * edx;
                let cutoff = ONE_EURO_MIN_CUTOFF + ONE_EURO_BETA * _edx_hat.abs();
                let a = smoothing_factor(cutoff, dt);
                self.smoothed_pos = a * self.pos + (1. - a) * self.smoothed_pos;
            }
        } else {
            self.smoothed_pos = self.pos;
        }
        self.prev_raw_pos = self.pos;
        self.prev_timestamp = Some(timestamp);

        self.trim_history();
    }

    /// Returns the current (raw) gesture position.
    pub fn pos(&self) -> f64 {
        self.pos
    }

    /// Returns the smoothed gesture position. Use this for display to reduce jitter.
    pub fn smoothed_pos(&self) -> f64 {
        self.smoothed_pos
    }

    /// Computes the current gesture velocity.
    pub fn velocity(&self) -> f64 {
        let (Some(first), Some(last)) = (self.history.front(), self.history.back()) else {
            return 0.;
        };

        let total_time = (last.timestamp - first.timestamp).as_secs_f64();
        if total_time == 0. {
            return 0.;
        }

        let total_delta = self.history.iter().map(|event| event.delta).sum::<f64>();
        total_delta / total_time
    }

    /// Computes the gesture end position after decelerating to a halt.
    ///
    /// Acceleration curve applied: small velocities stay short, fast flicks
    /// travel far. Combined with the higher deceleration constant, this gives
    /// a macOS / iOS-style fling.
    pub fn projected_end_pos(&self) -> f64 {
        let vel = self.velocity();
        let curved_vel = signed_power(vel, VELOCITY_CURVE_POWER);
        let extra = curved_vel / (1000. * DECELERATION_TOUCHPAD.ln());
        self.pos - extra
    }

    /// Public sensitivity gain for callers to apply on top of the position.
    pub fn sensitivity_gain() -> f64 {
        SENSITIVITY_GAIN
    }

    fn trim_history(&mut self) {
        let Some(&Event { timestamp, .. }) = self.history.back() else {
            return;
        };

        while let Some(first) = self.history.front() {
            if timestamp <= first.timestamp + HISTORY_LIMIT {
                break;
            }

            let _ = self.history.pop_front();
        }
    }
}

/// `sign(x) * |x|^p`. Preserves sign, shapes magnitude.
/// Safe for negative inputs.
#[inline]
fn signed_power(x: f64, p: f64) -> f64 {
    if x == 0. {
        0.
    } else {
        x.signum() * x.abs().powf(p)
    }
}

/// Standard 1-Euro smoothing factor for the given cutoff frequency (Hz) and dt (seconds).
#[inline]
fn smoothing_factor(cutoff: f64, dt: f64) -> f64 {
    let tau = 1.0 / (2.0 * std::f64::consts::PI * cutoff);
    (1.0 / (1.0 + tau / dt)).min(1.0)
}