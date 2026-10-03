//! Clock anchoring and drift detection.
//!
//! Every segment anchors a pair of clocks at its first sample: wall clock
//! (`CLOCK_REALTIME`, as nanoseconds since the Unix epoch) and a monotonic
//! reading. From there, the UTC time of any sample is `anchor + samples /
//! rate`. If the wall clock and the sample clock drift apart beyond a
//! threshold — suspend/resume, an NTP step, an audio driver stall — the
//! current segment must be closed and a new one anchored. This module is
//! pure: the caller supplies all clock readings, so everything is testable
//! without sleeping or touching real clocks.

use std::time::Duration;

/// Nanoseconds since the Unix epoch (UTC).
pub type UtcNs = u64;

/// A pair of clock readings taken at the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockPair {
    /// CLOCK_REALTIME in nanoseconds since the Unix epoch.
    pub realtime_ns: UtcNs,
    /// CLOCK_MONOTONIC (or any steady clock) in nanoseconds.
    pub monotonic_ns: u64,
}

impl ClockPair {
    /// Read the system clocks now.
    pub fn now() -> Self {
        let realtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            realtime_ns: u64::try_from(realtime.as_nanos()).unwrap_or(u64::MAX),
            monotonic_ns: monotonic_now_ns(),
        }
    }
}

fn monotonic_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// What a new clock observation means for the running segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockCheck {
    /// Clocks agree; keep recording into the current segment.
    Continuous,
    /// Wall clock and sample clock diverged (suspend, clock step, stall):
    /// close the current segment and re-anchor. The payload is the observed
    /// drift.
    Discontinuity { drift: Duration },
}

/// Anchors a stream of samples to the wall clock.
#[derive(Debug, Clone, Copy)]
pub struct Timeline {
    anchor: ClockPair,
    sample_rate: u32,
    samples_seen: u64,
    drift_threshold: Duration,
}

impl Timeline {
    pub fn new(anchor: ClockPair, sample_rate: u32, drift_threshold: Duration) -> Self {
        Self {
            anchor,
            sample_rate,
            samples_seen: 0,
            drift_threshold,
        }
    }

    pub fn anchor(&self) -> ClockPair {
        self.anchor
    }

    pub fn samples_seen(&self) -> u64 {
        self.samples_seen
    }

    /// UTC timestamp of the segment's first sample.
    pub fn start_utc_ns(&self) -> UtcNs {
        self.anchor.realtime_ns
    }

    /// UTC timestamp of the next sample to arrive (= current end of audio).
    pub fn current_utc_ns(&self) -> UtcNs {
        self.anchor.realtime_ns + self.samples_to_ns(self.samples_seen)
    }

    /// UTC timestamp of a given sample index within this timeline.
    pub fn sample_utc_ns(&self, sample_index: u64) -> UtcNs {
        self.anchor.realtime_ns + self.samples_to_ns(sample_index)
    }

    /// Account for `frames` new frames (samples per channel) and check the
    /// clocks for a discontinuity. `now` must be read after the frames were
    /// received.
    pub fn advance(&mut self, frames: u64, now: ClockPair) -> ClockCheck {
        self.samples_seen += frames;
        self.check(now)
    }

    /// Compare expected elapsed time (from the sample count) to both real
    /// clocks. Monotonic elapsed catches driver stalls and suspend without
    /// wall-clock steps; realtime elapsed additionally catches clock steps.
    pub fn check(&self, now: ClockPair) -> ClockCheck {
        let expected_ns = self.samples_to_ns(self.samples_seen);

        let real_elapsed = now.realtime_ns.saturating_sub(self.anchor.realtime_ns);
        let mono_elapsed = now.monotonic_ns.saturating_sub(self.anchor.monotonic_ns);

        let drift_ns = real_elapsed
            .abs_diff(expected_ns)
            .max(mono_elapsed.abs_diff(expected_ns));

        if drift_ns > u64::try_from(self.drift_threshold.as_nanos()).unwrap_or(u64::MAX) {
            ClockCheck::Discontinuity {
                drift: Duration::from_nanos(drift_ns),
            }
        } else {
            ClockCheck::Continuous
        }
    }

    fn samples_to_ns(&self, samples: u64) -> u64 {
        // samples * 1e9 / rate, in u128 to avoid overflow.
        u64::try_from(u128::from(samples) * 1_000_000_000 / u128::from(self.sample_rate))
            .unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const THRESHOLD: Duration = Duration::from_millis(250);

    fn pair(realtime_ns: u64, monotonic_ns: u64) -> ClockPair {
        ClockPair {
            realtime_ns,
            monotonic_ns,
        }
    }

    fn timeline() -> Timeline {
        // Anchor at an arbitrary wall-clock instant.
        Timeline::new(pair(1_000_000_000_000, 500_000_000), RATE, THRESHOLD)
    }

    #[test]
    fn sample_times_follow_the_anchor() {
        let tl = timeline();
        assert_eq!(tl.start_utc_ns(), 1_000_000_000_000);
        // 48000 samples = exactly one second.
        assert_eq!(tl.sample_utc_ns(48_000), 1_000_000_000_000 + 1_000_000_000);
        assert_eq!(tl.sample_utc_ns(24_000), 1_000_000_000_000 + 500_000_000);
    }

    #[test]
    fn continuous_when_clocks_track_samples() {
        let mut tl = timeline();
        // One second of audio, clocks one second later (+10ms jitter).
        let now = pair(
            1_000_000_000_000 + 1_010_000_000,
            500_000_000 + 1_010_000_000,
        );
        assert_eq!(tl.advance(48_000, now), ClockCheck::Continuous);
        assert_eq!(tl.current_utc_ns(), 1_000_000_000_000 + 1_000_000_000);
    }

    #[test]
    fn suspend_is_detected_via_both_clocks() {
        let mut tl = timeline();
        // One second of audio arrived, but 61 seconds passed on both clocks
        // (a suspend where CLOCK_MONOTONIC kept counting, as on Linux with
        // suspend-aware monotonic, or a wall clock step).
        let now = pair(
            1_000_000_000_000 + 61_000_000_000,
            500_000_000 + 61_000_000_000,
        );
        match tl.advance(48_000, now) {
            ClockCheck::Discontinuity { drift } => {
                assert!(drift >= Duration::from_secs(59), "drift = {drift:?}");
            }
            ClockCheck::Continuous => panic!("suspend not detected"),
        }
    }

    #[test]
    fn wall_clock_step_alone_is_detected() {
        let mut tl = timeline();
        // Monotonic tracks samples, but the wall clock jumped 10s (NTP step).
        let now = pair(
            1_000_000_000_000 + 11_000_000_000,
            500_000_000 + 1_000_000_000,
        );
        assert!(matches!(
            tl.advance(48_000, now),
            ClockCheck::Discontinuity { .. }
        ));
    }

    #[test]
    fn stall_without_samples_is_detected() {
        let tl = timeline();
        // No samples at all, but 2 seconds passed: the device stalled.
        let now = pair(
            1_000_000_000_000 + 2_000_000_000,
            500_000_000 + 2_000_000_000,
        );
        assert!(matches!(tl.check(now), ClockCheck::Discontinuity { .. }));
    }

    #[test]
    fn drift_below_threshold_is_continuous() {
        let mut tl = timeline();
        // 200ms late — under the 250ms threshold.
        let now = pair(
            1_000_000_000_000 + 1_200_000_000,
            500_000_000 + 1_200_000_000,
        );
        assert_eq!(tl.advance(48_000, now), ClockCheck::Continuous);
    }

    #[test]
    fn backwards_wall_clock_is_detected() {
        let mut tl = timeline();
        // Wall clock went backwards past the anchor; monotonic is fine.
        let now = pair(
            1_000_000_000_000 - 5_000_000_000,
            500_000_000 + 1_000_000_000,
        );
        assert!(matches!(
            tl.advance(48_000, now),
            ClockCheck::Discontinuity { .. }
        ));
    }

    #[test]
    fn clock_pair_now_is_sane() {
        let a = ClockPair::now();
        let b = ClockPair::now();
        assert!(b.realtime_ns >= a.realtime_ns.saturating_sub(1_000_000_000));
        assert!(b.monotonic_ns >= a.monotonic_ns);
        // Realtime should be after 2020-01-01.
        assert!(a.realtime_ns > 1_577_836_800_000_000_000);
    }
}
