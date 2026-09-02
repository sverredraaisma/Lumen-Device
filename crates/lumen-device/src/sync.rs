//! Time sync.
//!
//! ```text
//! Unsynced | TICK seen                 | send SYNC_REQ        | Syncing
//! Syncing  | SYNC_RESP, RTT <= 1.5x min| add sample           | Syncing, until 8
//! Syncing  | 8 good samples            | set offset           | Synced
//! Synced   | 30 s elapsed              | send SYNC_REQ        | Synced
//! Synced   | 3 TICKs missed            | -                    | Unsynced
//! any      | correction needed         | SLEW, never step     | -
//! ```
//!
//! The whole architecture rests on this working: effects are pure functions of
//! position and time, so two devices that disagree about the time render
//! different frames of the same show. The target is a 95th-percentile offset
//! under ±500 µs on ordinary WiFi.
//!
//! Three rules earn their complexity.
//!
//! **Filter on RTT.** The offset estimate assumes the request and the response
//! took equally long. On a contended WiFi link that assumption fails badly and
//! asymmetrically, and a single bad sample can move the clock further than an
//! hour of drift would. Discarding anything slower than 1.5× the running
//! minimum throws away the samples where the assumption is least true.
//!
//! **Never step.** Corrections slew the rate. A step means a frame is rendered
//! twice or skipped, which is visible, and every effect is a function of this
//! clock.
//!
//! **Degrade explicitly.** Losing sync raises it rather than carrying on with a
//! clock nobody should trust — the layer above suppresses tightly-synced content
//! instead of rendering it wrong.

/// Samples needed before the offset is trusted.
pub const SAMPLES_REQUIRED: usize = 8;
/// How much slower than the best round trip a sample may be and still count.
///
/// As a ratio in eighths, so the filter is integer arithmetic: 12/8 = 1.5.
pub const RTT_TOLERANCE_EIGHTHS: u64 = 12;
/// How often a synced node re-checks.
pub const RESYNC_INTERVAL_US: u64 = 30_000_000;
/// Missed `TICK`s before the clock is no longer trusted.
///
/// Three rather than one: a single dropped multicast is ordinary on a consumer
/// AP, and dropping out of sync every time one goes missing would make the mesh
/// spend its life resyncing.
pub const MISSED_TICKS_ALLOWED: u32 = 3;
/// Expected `TICK` period, from the leader's send interval.
pub const TICK_PERIOD_US: u64 = 1_000_000;

/// How much the clock is trusted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncState {
    /// No usable timebase. Suppress tightly-synced content.
    Unsynced,
    /// Collecting samples.
    Syncing,
    /// Offset known and applied.
    Synced,
}

/// One completed round trip.
///
/// `t1` is when the request left, `t2` and `t3` are the peer's receive and send
/// times, `t4` is when the response arrived. `t4` never travels — it is recorded
/// locally, which is the point of the exchange.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sample {
    pub t1: u64,
    pub t2: u64,
    pub t3: u64,
    pub t4: u64,
}

impl Sample {
    /// `((t2 - t1) + (t3 - t4)) / 2`, in microseconds, signed.
    ///
    /// Computed in `i128` because the two halves are large unsigned times whose
    /// difference is small and may be negative; doing it in `u64` and hoping is
    /// how a clock ends up years out.
    pub fn offset_us(&self) -> i64 {
        let a = self.t2 as i128 - self.t1 as i128;
        let b = self.t3 as i128 - self.t4 as i128;
        ((a + b) / 2) as i64
    }

    /// `(t4 - t1) - (t3 - t2)`: elapsed here, minus time the peer held it.
    pub fn rtt_us(&self) -> u64 {
        let total = self.t4.saturating_sub(self.t1) as i128;
        let held = self.t3.saturating_sub(self.t2) as i128;
        (total - held).max(0) as u64
    }
}

/// What the sync machine wants next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Idle,
    /// Send a `SYNC_REQ` to the timebase master.
    Probe,
    /// Apply this correction by slewing.
    Discipline(i64),
    /// The clock is trustworthy now.
    Acquired,
    /// The clock is no longer trustworthy.
    Lost,
}

/// The time-sync state machine.
#[derive(Clone, Debug)]
pub struct Sync {
    state: SyncState,
    /// Accepted offsets, oldest first.
    samples: [i64; SAMPLES_REQUIRED],
    count: usize,
    /// Smallest round trip seen, which is the best estimate of a symmetric path.
    min_rtt_us: Option<u64>,
    last_tick_us: Option<u64>,
    last_probe_us: u64,
    applied_offset_us: i64,
}

impl Default for Sync {
    fn default() -> Self {
        Self::new()
    }
}

impl Sync {
    pub fn new() -> Sync {
        Sync {
            state: SyncState::Unsynced,
            samples: [0; SAMPLES_REQUIRED],
            count: 0,
            min_rtt_us: None,
            last_tick_us: None,
            last_probe_us: 0,
            applied_offset_us: 0,
        }
    }

    pub fn state(&self) -> SyncState {
        self.state
    }

    pub fn is_synced(&self) -> bool {
        self.state == SyncState::Synced
    }

    /// Total correction applied so far.
    pub fn applied_offset_us(&self) -> i64 {
        self.applied_offset_us
    }

    /// Best round trip seen, or `None` before the first response.
    pub fn min_rtt_us(&self) -> Option<u64> {
        self.min_rtt_us
    }

    /// A `TICK` from the timebase master.
    pub fn on_tick(&mut self, now_us: u64) -> Outcome {
        self.last_tick_us = Some(now_us);
        match self.state {
            SyncState::Unsynced => {
                self.state = SyncState::Syncing;
                self.last_probe_us = now_us;
                Outcome::Probe
            }
            _ => Outcome::Idle,
        }
    }

    /// A `SYNC_RESP` completing a round trip.
    pub fn on_sample(&mut self, now_us: u64, sample: Sample) -> Outcome {
        let rtt = sample.rtt_us();
        let best = self.min_rtt_us.map_or(rtt, |m| m.min(rtt));
        self.min_rtt_us = Some(best);

        // Filter against the *updated* minimum, so the fastest sample seen is
        // never itself discarded for being slower than an older, slower one.
        if rtt.saturating_mul(8) > best.saturating_mul(RTT_TOLERANCE_EIGHTHS) {
            // Too slow to trust the symmetry assumption. Ask again rather than
            // give up: on a busy link most samples are bad and a few are fine.
            self.last_probe_us = now_us;
            return Outcome::Probe;
        }

        self.push(sample.offset_us());

        if self.count < SAMPLES_REQUIRED {
            self.last_probe_us = now_us;
            return Outcome::Probe;
        }

        let correction = self.consensus_offset();
        self.applied_offset_us = self.applied_offset_us.saturating_add(correction);
        let was = self.state;
        self.state = SyncState::Synced;
        self.last_probe_us = now_us;
        // Starting again from empty after each convergence keeps a stale sample
        // from an earlier network condition out of the next estimate.
        self.count = 0;

        if was != SyncState::Synced {
            // Two things need saying, and only one can be returned. The
            // correction is the one that must not be dropped; the caller learns
            // about acquisition from `state()`, which it has to consult anyway.
            if correction != 0 {
                return Outcome::Discipline(correction);
            }
            return Outcome::Acquired;
        }
        if correction != 0 {
            Outcome::Discipline(correction)
        } else {
            Outcome::Idle
        }
    }

    /// Time passed.
    pub fn on_timer(&mut self, now_us: u64) -> Outcome {
        // Losing the master matters more than re-probing, so check it first.
        if let Some(last) = self.last_tick_us {
            let missed = now_us.saturating_sub(last);
            if missed >= TICK_PERIOD_US * MISSED_TICKS_ALLOWED as u64 {
                if self.state != SyncState::Unsynced {
                    self.state = SyncState::Unsynced;
                    self.count = 0;
                    // The minimum RTT is deliberately kept: it describes the
                    // network, not the master, and throwing it away would make
                    // the next convergence accept samples it should not.
                    return Outcome::Lost;
                }
                return Outcome::Idle;
            }
        }

        match self.state {
            SyncState::Unsynced => Outcome::Idle,
            SyncState::Syncing => {
                self.last_probe_us = now_us;
                Outcome::Probe
            }
            SyncState::Synced => {
                if now_us.saturating_sub(self.last_probe_us) >= RESYNC_INTERVAL_US {
                    self.last_probe_us = now_us;
                    Outcome::Probe
                } else {
                    Outcome::Idle
                }
            }
        }
    }

    /// When the caller should look in again.
    pub fn next_deadline_us(&self, now_us: u64) -> u64 {
        let resync = match self.state {
            SyncState::Synced => self
                .last_probe_us
                .saturating_add(RESYNC_INTERVAL_US)
                .saturating_sub(now_us),
            // While syncing, probe at the tick rate: the exchange is cheap and
            // convergence time is what a cold start is judged on.
            SyncState::Syncing => TICK_PERIOD_US,
            SyncState::Unsynced => TICK_PERIOD_US,
        };
        // The tick-loss deadline only matters while there is sync to lose. Once
        // Unsynced it is permanently in the past, so folding it in would return
        // zero for ever - and a shell honouring that busy-polls at whatever rate
        // its loop allows, which on a battery-powered sensor node is the
        // difference between weeks and days. The way out of Unsynced is a TICK
        // arriving as an event, not a timer firing.
        if self.state == SyncState::Unsynced {
            return resync;
        }
        match self.last_tick_us {
            Some(last) => {
                let deadline = last.saturating_add(TICK_PERIOD_US * MISSED_TICKS_ALLOWED as u64);
                resync.min(deadline.saturating_sub(now_us))
            }
            None => resync,
        }
    }

    fn push(&mut self, offset: i64) {
        if self.count < SAMPLES_REQUIRED {
            self.samples[self.count] = offset;
            self.count += 1;
        } else {
            self.samples.rotate_left(1);
            self.samples[SAMPLES_REQUIRED - 1] = offset;
        }
    }

    /// The median of the collected offsets.
    ///
    /// Median, not mean: one surviving outlier drags a mean by an eighth of its
    /// error and a median not at all, and the RTT filter does not catch a path
    /// that is *consistently* asymmetric.
    fn consensus_offset(&self) -> i64 {
        let mut sorted = self.samples;
        sorted.sort_unstable();
        let n = SAMPLES_REQUIRED;
        // Even count, so average the middle two - in i128, because two large
        // opposite-signed offsets would otherwise overflow on the way to a
        // perfectly representable answer.
        ((sorted[n / 2 - 1] as i128 + sorted[n / 2] as i128) / 2) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A round trip that took `rtt` with the peer's clock `offset` ahead.
    fn sample(t1: u64, rtt: u64, offset: i64) -> Sample {
        let half = rtt / 2;
        Sample {
            t1,
            t2: (t1 as i64 + half as i64 + offset) as u64,
            t3: (t1 as i64 + half as i64 + offset) as u64,
            t4: t1 + rtt,
        }
    }

    #[test]
    fn offset_and_rtt_come_out_of_a_symmetric_exchange() {
        let s = sample(1_000, 400, 5_000);
        assert_eq!(s.rtt_us(), 400);
        assert_eq!(s.offset_us(), 5_000);
    }

    #[test]
    fn a_negative_offset_survives_the_arithmetic() {
        // The case that breaks a u64 implementation: the peer is behind us.
        let s = sample(1_000_000, 200, -7_500);
        assert_eq!(s.offset_us(), -7_500);
        assert_eq!(s.rtt_us(), 200);
    }

    #[test]
    fn a_peer_that_held_the_request_does_not_inflate_the_rtt() {
        let s = Sample {
            t1: 0,
            t2: 100,
            t3: 900,
            t4: 1_000,
        };
        assert_eq!(s.rtt_us(), 200, "the 800us the peer held it must not count");
    }

    #[test]
    fn a_cold_node_starts_unsynced_and_probes_on_the_first_tick() {
        let mut s = Sync::new();
        assert_eq!(s.state(), SyncState::Unsynced);
        assert!(!s.is_synced());
        assert_eq!(s.on_tick(1_000), Outcome::Probe);
        assert_eq!(s.state(), SyncState::Syncing);
        // A second tick does not restart the exchange.
        assert_eq!(s.on_tick(2_000), Outcome::Idle);
    }

    #[test]
    fn eight_good_samples_converge() {
        let mut s = Sync::new();
        s.on_tick(0);
        for i in 0..SAMPLES_REQUIRED - 1 {
            assert_eq!(
                s.on_sample(i as u64 * 1_000, sample(i as u64 * 1_000, 400, 5_000)),
                Outcome::Probe,
                "sample {i} should ask for another"
            );
            assert_eq!(s.state(), SyncState::Syncing);
        }
        let last = s.on_sample(9_000, sample(9_000, 400, 5_000));
        assert_eq!(last, Outcome::Discipline(5_000));
        assert!(s.is_synced());
        assert_eq!(s.applied_offset_us(), 5_000);
    }

    #[test]
    fn a_slow_sample_is_discarded_rather_than_averaged_in() {
        // A single bad sample can move the clock further than an hour of drift.
        let mut s = Sync::new();
        s.on_tick(0);
        // Establish a fast path.
        s.on_sample(0, sample(0, 200, 1_000));
        // Now one that took five times as long, claiming a wildly different
        // offset. It must not reach the estimate.
        assert_eq!(
            s.on_sample(1_000, sample(1_000, 1_000, 900_000)),
            Outcome::Probe
        );
        for i in 2..=SAMPLES_REQUIRED {
            s.on_sample(i as u64 * 1_000, sample(i as u64 * 1_000, 200, 1_000));
        }
        assert!(s.is_synced());
        assert_eq!(
            s.applied_offset_us(),
            1_000,
            "the slow sample leaked into the estimate"
        );
    }

    #[test]
    fn the_fastest_sample_is_never_discarded_for_beating_the_old_minimum() {
        // Filtering against the previous minimum instead of the updated one
        // throws away exactly the best measurement available.
        let mut s = Sync::new();
        s.on_tick(0);
        s.on_sample(0, sample(0, 1_000, 100));
        assert_eq!(s.min_rtt_us(), Some(1_000));
        // Much faster, so a better sample - it must be accepted.
        let out = s.on_sample(1_000, sample(1_000, 100, 100));
        assert_eq!(out, Outcome::Probe);
        assert_eq!(s.min_rtt_us(), Some(100));
    }

    #[test]
    fn the_estimate_is_a_median_so_one_outlier_cannot_drag_it() {
        // The RTT filter does not catch a path that is consistently asymmetric,
        // so the estimator has to be robust on its own.
        let mut s = Sync::new();
        s.on_tick(0);
        for i in 0..SAMPLES_REQUIRED - 1 {
            s.on_sample(i as u64, sample(i as u64, 200, 1_000));
        }
        // One last sample with the same RTT but a nonsense offset.
        let bad = Sample {
            t1: 100,
            t2: 100 + 100 + 500_000,
            t3: 100 + 100 + 500_000,
            t4: 300,
        };
        s.on_sample(300, bad);
        assert!(s.is_synced());
        assert_eq!(s.applied_offset_us(), 1_000, "the median moved");
    }

    #[test]
    fn corrections_accumulate_across_resyncs() {
        let mut s = Sync::new();
        s.on_tick(0);
        for i in 0..SAMPLES_REQUIRED {
            s.on_sample(i as u64, sample(i as u64, 200, 1_000));
        }
        assert_eq!(s.applied_offset_us(), 1_000);
        // A second convergence, now only 50us out.
        for i in 0..SAMPLES_REQUIRED {
            s.on_sample(100 + i as u64, sample(100 + i as u64, 200, 50));
        }
        assert_eq!(s.applied_offset_us(), 1_050);
    }

    #[test]
    fn a_synced_node_reprobes_after_the_interval_and_not_before() {
        // Ticks have to keep arriving throughout, or the node correctly falls
        // out of sync long before the resync interval elapses - thirty seconds
        // of silence is ten times the tick timeout.
        let mut s = synced();
        let last_quiet = RESYNC_INTERVAL_US / TICK_PERIOD_US - 1;
        for k in 1..=last_quiet {
            let now = k * TICK_PERIOD_US;
            s.on_tick(now);
            assert_eq!(s.on_timer(now), Outcome::Idle, "reprobed early at {now}");
        }
        // A second past the interval, not exactly on it: the interval is
        // measured from the last sample, which landed a few microseconds after
        // the machine was built. Asserting the exact microsecond would be
        // testing the fixture, not the behaviour.
        let after = RESYNC_INTERVAL_US + TICK_PERIOD_US;
        s.on_tick(after);
        assert_eq!(s.on_timer(after), Outcome::Probe);
        assert!(s.is_synced(), "reprobing must not drop the current offset");
    }

    /// A machine that has converged, with the last tick at time zero.
    fn synced() -> Sync {
        let mut s = Sync::new();
        s.on_tick(0);
        for i in 0..SAMPLES_REQUIRED {
            s.on_sample(i as u64, sample(i as u64, 200, 0));
        }
        assert!(s.is_synced());
        s
    }

    #[test]
    fn three_missed_ticks_lose_sync_and_one_does_not() {
        // A single dropped multicast is ordinary on a consumer AP. Dropping out
        // of sync each time would make the mesh spend its life resyncing.
        let mut s = synced();
        assert_eq!(s.on_timer(TICK_PERIOD_US), Outcome::Idle);
        assert_eq!(s.on_timer(2 * TICK_PERIOD_US), Outcome::Idle);
        assert!(s.is_synced());
        assert_eq!(s.on_timer(3 * TICK_PERIOD_US), Outcome::Lost);
        assert_eq!(s.state(), SyncState::Unsynced);
        // And it does not keep announcing the loss.
        assert_eq!(s.on_timer(4 * TICK_PERIOD_US), Outcome::Idle);
    }

    #[test]
    fn losing_sync_keeps_the_network_measurement() {
        // The minimum RTT describes the network, not the master. Throwing it
        // away would make the next convergence accept samples it should not.
        let mut s = synced();
        let rtt = s.min_rtt_us();
        s.on_timer(3 * TICK_PERIOD_US);
        assert_eq!(s.min_rtt_us(), rtt);
    }

    #[test]
    fn a_recovered_master_resyncs_from_scratch() {
        let mut s = synced();
        s.on_timer(3 * TICK_PERIOD_US);
        assert_eq!(s.state(), SyncState::Unsynced);
        assert_eq!(s.on_tick(10 * TICK_PERIOD_US), Outcome::Probe);
        assert_eq!(s.state(), SyncState::Syncing);
    }

    #[test]
    fn a_zero_correction_reports_acquisition_rather_than_a_pointless_discipline() {
        let mut s = Sync::new();
        s.on_tick(0);
        for i in 0..SAMPLES_REQUIRED - 1 {
            s.on_sample(i as u64, sample(i as u64, 200, 0));
        }
        assert_eq!(s.on_sample(99, sample(99, 200, 0)), Outcome::Acquired);
        assert!(s.is_synced());
    }

    #[test]
    fn an_unsynced_node_does_not_busy_poll() {
        // Found by the conformance work. The tick-loss deadline is permanently
        // in the past once sync is gone, so folding it in returned zero for
        // ever and the node asked to be woken as fast as its shell allowed -
        // which is exactly what `next_deadline_us` exists to prevent.
        let mut s = synced();
        s.on_timer(3 * TICK_PERIOD_US);
        assert_eq!(s.state(), SyncState::Unsynced);
        for step in 3..60u64 {
            let now = step * TICK_PERIOD_US;
            s.on_timer(now);
            assert!(
                s.next_deadline_us(now) >= TICK_PERIOD_US / 2,
                "asked to be woken in {}us at {now}",
                s.next_deadline_us(now)
            );
        }
    }

    #[test]
    fn the_deadline_never_underflows_and_shrinks_toward_the_tick_timeout() {
        let s = synced();
        let early = s.next_deadline_us(0);
        let later = s.next_deadline_us(TICK_PERIOD_US);
        assert!(later < early, "{later} should be less than {early}");
        assert_eq!(s.next_deadline_us(u64::MAX / 2), 0);

        // A machine that has never seen a tick still asks to be woken.
        let cold = Sync::new();
        assert!(cold.next_deadline_us(0) > 0);
    }

    #[test]
    fn the_sample_window_slides_rather_than_overflowing() {
        let mut s = Sync::new();
        s.on_tick(0);
        // Feed far more than the window holds; the machine must keep working.
        for i in 0..100u64 {
            s.on_sample(i, sample(i, 200, 42));
        }
        assert!(s.is_synced());
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(Sync::default().state(), Sync::new().state());
    }
}
