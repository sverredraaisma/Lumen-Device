//! Virtual time.
//!
//! The clock only moves when the harness moves it, which is what lets a
//! twenty-four hour drift scenario finish in milliseconds. It also lets every
//! node in a world hold a *different* idea of what time it is, which is the
//! only way to test time sync at all: a mesh where every clock already agrees
//! proves nothing.

use core::cell::Cell;
use lumen_hal::{Clock, ShowTimeUs, WallClock};

/// Parts per million, as a signed integer. No floats anywhere in the timebase:
/// two hosts must compute the same drift, and `f64` accumulation over 86 400
/// seconds is exactly where that stops being true.
pub type Ppm = i32;

/// How fast a slew is allowed to eat a correction, in parts per million of real
/// time. 500 ppm is roughly the fastest a strip can be re-timed without the
/// change being visible as a stutter in a moving effect.
pub const DEFAULT_SLEW_PPM: u32 = 500;

/// A per-node show clock over virtual time.
///
/// Three things separate it from "a counter":
///
/// - **Skew** — a fixed offset. A node that booted before the mesh existed, or
///   whose RTC is simply wrong.
/// - **Drift** — a rate error in ppm. Two crystals at ±20 ppm diverge by about
///   3.5 seconds a day, which is the failure time sync is there to prevent.
/// - **Slew** — corrections are absorbed gradually, never applied as a step.
///   A stepped render clock is a visible glitch and, worse, can run time
///   backwards, so [`Clock::now_us`] is clamped monotone regardless.
#[derive(Debug)]
pub struct SimClock {
    /// Virtual reference time the harness has advanced to.
    reference_us: u64,
    /// This node's own elapsed count, accumulated at its drifted rate. Kept
    /// separately from `reference_us` because drift is a *rate* error: it has
    /// to integrate over the advances, not be recomputed from the total, or
    /// changing the rate mid-run would retroactively rewrite the past.
    local_us: i128,
    skew_us: i64,
    drift_ppm: Ppm,
    /// Correction still owed to the mesh timebase, worked off at `slew_ppm`.
    pending_slew_us: i64,
    /// Correction already absorbed. Separate from `pending` so a test can see
    /// that a discipline call is being applied gradually rather than at once.
    applied_slew_us: i64,
    slew_ppm: u32,
    /// Unix epoch offset, once the node has a trusted wall-clock source.
    /// `None` is the normal state and stays supported: schedules degrade
    /// explicitly without wall time rather than guessing.
    unix_base_us: Option<u64>,
    /// Highest value ever returned. `now_us` takes `&self`, so the monotonicity
    /// clamp needs interior mutability; `Cell` is enough because the simulator
    /// is single-threaded by construction.
    last_reported_us: Cell<u64>,
}

impl SimClock {
    /// A clock with no skew and no drift, starting at zero.
    pub fn new() -> Self {
        Self::with_error(0, 0)
    }

    /// A clock that is `skew_us` out and runs `drift_ppm` fast (negative for
    /// slow).
    pub fn with_error(skew_us: i64, drift_ppm: Ppm) -> Self {
        Self {
            reference_us: 0,
            local_us: 0,
            skew_us,
            drift_ppm,
            pending_slew_us: 0,
            applied_slew_us: 0,
            slew_ppm: DEFAULT_SLEW_PPM,
            unix_base_us: None,
            last_reported_us: Cell::new(0),
        }
    }

    /// Change how aggressively corrections are absorbed. Zero freezes the slew,
    /// which is a useful way to write a test that a node stays wrong.
    pub fn set_slew_ppm(&mut self, ppm: u32) {
        self.slew_ppm = ppm;
    }

    /// Virtual reference time — what the harness thinks it is, before this
    /// node's errors.
    pub fn reference_us(&self) -> u64 {
        self.reference_us
    }

    /// The current skew.
    pub fn skew_us(&self) -> i64 {
        self.skew_us
    }

    /// Move the skew. Only the harness does this — it is a fault injection, the
    /// simulated equivalent of an RTC that was wrong all along.
    pub fn set_skew_us(&mut self, skew_us: i64) {
        self.skew_us = skew_us;
    }

    /// The current drift rate.
    pub fn drift_ppm(&self) -> Ppm {
        self.drift_ppm
    }

    /// Change the drift rate from here on. Past elapsed time keeps the rate it
    /// was accumulated at.
    pub fn set_drift_ppm(&mut self, ppm: Ppm) {
        self.drift_ppm = ppm;
    }

    /// Correction not yet absorbed.
    pub fn pending_slew_us(&self) -> i64 {
        self.pending_slew_us
    }

    /// Advance the reference to `now_us`.
    ///
    /// Going backwards is ignored rather than fatal. The harness never does it,
    /// but a bug that made it do so should show up as a stalled scenario in a
    /// test, not as a panic in the middle of someone's fault injection.
    pub fn advance_to(&mut self, now_us: u64) {
        if now_us <= self.reference_us {
            return;
        }
        let delta = (now_us - self.reference_us) as i128;
        self.reference_us = now_us;

        // Drift integrates over the advance, so the error grows with elapsed
        // time exactly as a crystal's does.
        let drifted = delta + (delta * self.drift_ppm as i128) / 1_000_000;
        self.local_us += drifted;

        self.absorb_slew(delta);
    }

    /// Work off part of the outstanding correction, proportional to how much
    /// real time passed. Never more than what is owed, so the clock converges
    /// instead of oscillating around the target.
    fn absorb_slew(&mut self, delta_us: i128) {
        if self.pending_slew_us == 0 || self.slew_ppm == 0 {
            return;
        }
        let budget = (delta_us * self.slew_ppm as i128) / 1_000_000;
        let budget = budget.clamp(0, i64::MAX as i128) as i64;
        let step = budget.min(self.pending_slew_us.abs());
        let step = if self.pending_slew_us < 0 {
            -step
        } else {
            step
        };
        self.pending_slew_us -= step;
        self.applied_slew_us += step;
    }

    /// Give the node a wall-clock source, expressed as the Unix time
    /// corresponding to show time zero.
    pub fn set_unix_base_us(&mut self, base_us: u64) {
        self.unix_base_us = Some(base_us);
    }

    /// Take the wall-clock source away — a node that has never seen one, or has
    /// decided its source is no longer trustworthy. Schedules must degrade
    /// visibly here rather than silently running on a guess.
    pub fn clear_unix_base(&mut self) {
        self.unix_base_us = None;
    }

    /// The clock's value before the monotonicity clamp, as a signed number.
    /// Used by `now_us`, and by tests that want to see a negative intermediate.
    fn raw_us(&self) -> i128 {
        self.local_us + self.skew_us as i128 + self.applied_slew_us as i128
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SimClock {
    fn now_us(&self) -> ShowTimeUs {
        let raw = self.raw_us();
        // Two clamps, both load-bearing. The lower one keeps a badly skewed
        // node from reporting a time before the epoch; the monotone one is the
        // contract `Clock` states outright, and a core that sees time go
        // backwards computes negative durations and behaves in ways no vector
        // will ever describe.
        let floored = raw.max(0) as u64;
        let reported = floored.max(self.last_reported_us.get());
        self.last_reported_us.set(reported);
        reported
    }

    fn discipline(&mut self, offset_us: i64) {
        // Accumulates rather than replaces: two corrections arriving between
        // advances both have to be honoured, and dropping the first would make
        // convergence depend on how often the harness happens to step.
        self.pending_slew_us = self.pending_slew_us.saturating_add(offset_us);
    }
}

impl WallClock for SimClock {
    fn unix_us(&self) -> Option<u64> {
        self.unix_base_us
            .map(|base| base.saturating_add(self.now_us()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_clock_tracks_the_reference() {
        let mut c = SimClock::new();
        assert_eq!(c.now_us(), 0);
        c.advance_to(1_000_000);
        assert_eq!(c.now_us(), 1_000_000);
        assert_eq!(c.reference_us(), 1_000_000);
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(SimClock::default().now_us(), SimClock::new().now_us());
    }

    #[test]
    fn skew_is_a_constant_offset() {
        let mut c = SimClock::with_error(500, 0);
        assert_eq!(c.now_us(), 500);
        c.advance_to(1_000);
        assert_eq!(c.now_us(), 1_500);
        assert_eq!(c.skew_us(), 500);
    }

    #[test]
    fn negative_skew_floors_at_zero_and_stays_monotone() {
        let mut c = SimClock::with_error(-1_000, 0);
        assert_eq!(c.now_us(), 0);
        c.advance_to(400);
        assert_eq!(c.now_us(), 0, "still behind the epoch");
        c.advance_to(1_500);
        assert_eq!(c.now_us(), 500);
    }

    #[test]
    fn skew_can_be_moved_mid_run() {
        let mut c = SimClock::new();
        c.advance_to(1_000);
        c.set_skew_us(250);
        assert_eq!(c.now_us(), 1_250);
    }

    #[test]
    fn a_backwards_skew_cannot_run_the_clock_backwards() {
        let mut c = SimClock::with_error(1_000, 0);
        c.advance_to(1_000);
        assert_eq!(c.now_us(), 2_000);
        c.set_skew_us(-1_000);
        assert_eq!(c.now_us(), 2_000, "monotonicity beats the new skew");
        c.advance_to(10_000);
        assert_eq!(c.now_us(), 9_000);
    }

    /// The number that motivates the whole time-sync workstream: two crystals
    /// twenty ppm apart are seconds out after a day.
    #[test]
    fn drift_over_a_day_is_seconds() {
        let day_us = 86_400 * 1_000_000u64;
        let mut fast = SimClock::with_error(0, 20);
        let mut slow = SimClock::with_error(0, -20);
        fast.advance_to(day_us);
        slow.advance_to(day_us);
        let spread = fast.now_us() - slow.now_us();
        assert_eq!(spread, 3_456_000, "±20 ppm over 24 h");
    }

    #[test]
    fn drift_integrates_rather_than_recomputing() {
        let mut c = SimClock::with_error(0, 1_000_000); // runs at double speed
        c.advance_to(1_000);
        assert_eq!(c.now_us(), 2_000);
        c.set_drift_ppm(0);
        assert_eq!(c.drift_ppm(), 0);
        c.advance_to(2_000);
        assert_eq!(c.now_us(), 3_000, "the first second keeps its old rate");
    }

    #[test]
    fn advancing_backwards_or_nowhere_is_ignored() {
        let mut c = SimClock::new();
        c.advance_to(5_000);
        c.advance_to(5_000);
        c.advance_to(1_000);
        assert_eq!(c.now_us(), 5_000);
        assert_eq!(c.reference_us(), 5_000);
    }

    #[test]
    fn discipline_slews_and_never_steps() {
        let mut c = SimClock::new();
        c.advance_to(1_000_000);
        c.discipline(400);
        assert_eq!(c.pending_slew_us(), 400);
        assert_eq!(c.now_us(), 1_000_000, "not applied until time passes");

        // 500 ppm of one second is 500 µs of budget, so 400 µs clears in one go.
        c.advance_to(2_000_000);
        assert_eq!(c.pending_slew_us(), 0);
        assert_eq!(c.now_us(), 2_000_400);
    }

    #[test]
    fn a_large_correction_takes_several_advances() {
        let mut c = SimClock::new();
        c.discipline(10_000);
        c.advance_to(1_000_000);
        assert_eq!(c.pending_slew_us(), 9_500, "only 500 µs of budget");
        c.advance_to(2_000_000);
        assert_eq!(c.pending_slew_us(), 9_000);
    }

    #[test]
    fn corrections_accumulate_between_advances() {
        let mut c = SimClock::new();
        c.discipline(100);
        c.discipline(-40);
        assert_eq!(c.pending_slew_us(), 60);
    }

    #[test]
    fn a_negative_correction_slews_the_other_way_without_stepping() {
        let mut c = SimClock::with_error(1_000_000, 0);
        c.advance_to(1_000_000);
        c.discipline(-300);
        c.advance_to(2_000_000);
        assert_eq!(c.pending_slew_us(), 0);
        assert_eq!(c.now_us(), 2_999_700);
    }

    #[test]
    fn a_frozen_slew_never_converges() {
        let mut c = SimClock::new();
        c.set_slew_ppm(0);
        c.discipline(1_000);
        c.advance_to(10_000_000);
        assert_eq!(c.pending_slew_us(), 1_000);
        assert_eq!(c.now_us(), 10_000_000);
    }

    #[test]
    fn discipline_saturates_instead_of_overflowing() {
        let mut c = SimClock::new();
        c.discipline(i64::MAX);
        c.discipline(i64::MAX);
        assert_eq!(c.pending_slew_us(), i64::MAX);
    }

    #[test]
    fn slew_is_absorbed_but_time_still_only_goes_forward() {
        let mut c = SimClock::new();
        c.advance_to(1_000_000);
        c.discipline(-1_000_000);
        for step in 2..12 {
            c.advance_to(step * 1_000_000);
            let _ = c.now_us();
        }
        let mut last = 0;
        for step in 12..20 {
            c.advance_to(step * 1_000_000);
            let now = c.now_us();
            assert!(now >= last);
            last = now;
        }
    }

    #[test]
    fn wall_clock_is_absent_until_a_source_arrives() {
        let mut c = SimClock::new();
        assert_eq!(c.unix_us(), None);
        c.advance_to(1_000);
        c.set_unix_base_us(1_700_000_000_000_000);
        assert_eq!(c.unix_us(), Some(1_700_000_000_001_000));
        c.clear_unix_base();
        assert_eq!(c.unix_us(), None);
    }

    #[test]
    fn wall_clock_saturates_rather_than_wrapping() {
        let mut c = SimClock::new();
        c.set_unix_base_us(u64::MAX);
        c.advance_to(1_000);
        assert_eq!(c.unix_us(), Some(u64::MAX));
    }

    #[test]
    fn a_full_day_of_virtual_time_costs_one_call() {
        // Not a timing assertion, a shape assertion: advancing a day is a
        // single arithmetic step, which is why the harness can afford it.
        let mut c = SimClock::with_error(0, 12);
        c.advance_to(86_400_000_000);
        assert_eq!(c.now_us(), 86_401_036_800);
    }
}
