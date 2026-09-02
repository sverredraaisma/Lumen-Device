//! Timebase election.
//!
//! ```text
//! Follower  | no TICK for 3 s                         | broadcast candidacy | Candidate
//! Candidate | a better candidate seen                 | -                   | Follower
//! Candidate | 2 s with nothing better                 | assume role, epoch++| Leader
//! Leader    | a strictly better TICK, 3 ticks running | yield               | Follower
//! Leader    | 1 s elapsed                             | send TICK           | Leader
//! ```
//!
//! Two details in that table do most of the work.
//!
//! **Capacity only, never load.** Load changes constantly, so a comparison that
//! included it would hand the role back and forth between two busy devices
//! forever. A role that flaps is worse than a slightly suboptimal one that
//! holds, and every device downstream has to cope with the handover.
//!
//! **Three consecutive ticks before yielding.** Without the hysteresis, a device
//! rebooting with a marginally different benchmark score triggers a needless
//! handover — and a handover is visible, because the show clock changes hands.

use lumen_proto::Uuid;

/// How long without a `TICK` before a follower stands for election.
pub const FOLLOWER_TIMEOUT_US: u64 = 3_000_000;
/// How long a candidate waits, unchallenged, before taking the role.
pub const CANDIDATE_SETTLE_US: u64 = 2_000_000;
/// How often a leader sends `TICK`.
pub const LEADER_TICK_INTERVAL_US: u64 = 1_000_000;
/// Consecutive better ticks a leader must see before yielding.
pub const YIELD_HYSTERESIS: u8 = 3;

/// What this node is doing about the timebase.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

/// A claim to the timebase, ordered by [`Candidacy::beats`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Candidacy {
    pub capacity: u32,
    pub uuid: Uuid,
}

impl Candidacy {
    /// Whether this candidacy wins against `other`.
    ///
    /// Higher capacity wins. Ties break on the **lower** UUID, which is what
    /// `~uuid` compared lexicographically means — the important property is not
    /// which direction it goes but that every device computes the same answer
    /// from the same two candidacies, with no reference to anything local.
    pub fn beats(&self, other: &Candidacy) -> bool {
        match self.capacity.cmp(&other.capacity) {
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Less => false,
            core::cmp::Ordering::Equal => self.uuid.0 < other.uuid.0,
        }
    }
}

/// What the election wants the caller to do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Nothing to do.
    Idle,
    /// Broadcast this node's candidacy as a `TICK`.
    Announce,
    /// Send the periodic `TICK`.
    SendTick,
    /// The role changed. Emit it and act on it.
    Became(Role),
}

/// The election state machine.
#[derive(Clone, Debug)]
pub struct Election {
    me: Candidacy,
    role: Role,
    epoch: u32,
    /// When the current state was entered, or the last `TICK` was seen.
    since_us: u64,
    /// When this leader last sent a `TICK`.
    last_tick_sent_us: u64,
    /// Consecutive better ticks seen while leading.
    better_ticks: u8,
    /// The best candidacy seen since becoming a candidate.
    best_seen: Option<Candidacy>,
}

impl Election {
    pub fn new(me: Candidacy, now_us: u64) -> Election {
        Election {
            me,
            role: Role::Follower,
            epoch: 0,
            since_us: now_us,
            last_tick_sent_us: 0,
            better_ticks: 0,
            best_seen: None,
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }

    /// A `TICK` arrived from `who`, claiming `epoch`.
    pub fn on_tick(&mut self, now_us: u64, who: Candidacy, epoch: u32) -> Outcome {
        // A tick from ourselves says nothing; a mesh where a device counted its
        // own multicast as a peer would never elect anyone.
        if who.uuid == self.me.uuid {
            return Outcome::Idle;
        }
        if epoch > self.epoch {
            self.epoch = epoch;
        }

        match self.role {
            Role::Follower => {
                // Any tick from a live leader resets the timeout, even a worse
                // one: a working timebase is better than a better timebase that
                // has to be elected first.
                self.since_us = now_us;
                Outcome::Idle
            }
            Role::Candidate => {
                self.note(who);
                if who.beats(&self.me) {
                    self.role = Role::Follower;
                    self.since_us = now_us;
                    self.best_seen = None;
                    Outcome::Became(Role::Follower)
                } else {
                    Outcome::Idle
                }
            }
            Role::Leader => {
                if who.beats(&self.me) {
                    self.better_ticks = self.better_ticks.saturating_add(1);
                    if self.better_ticks >= YIELD_HYSTERESIS {
                        self.role = Role::Follower;
                        self.since_us = now_us;
                        self.better_ticks = 0;
                        return Outcome::Became(Role::Follower);
                    }
                } else {
                    // Not consecutive any more. Resetting is the whole point of
                    // the hysteresis: one stray better tick must not count
                    // towards a handover.
                    self.better_ticks = 0;
                }
                Outcome::Idle
            }
        }
    }

    /// Time passed with nothing else happening.
    pub fn on_timer(&mut self, now_us: u64) -> Outcome {
        match self.role {
            Role::Follower => {
                if now_us.saturating_sub(self.since_us) >= FOLLOWER_TIMEOUT_US {
                    self.role = Role::Candidate;
                    self.since_us = now_us;
                    self.best_seen = None;
                    Outcome::Announce
                } else {
                    Outcome::Idle
                }
            }
            Role::Candidate => {
                if now_us.saturating_sub(self.since_us) >= CANDIDATE_SETTLE_US {
                    let beaten = self.best_seen.map(|b| b.beats(&self.me)).unwrap_or(false);
                    if beaten {
                        self.role = Role::Follower;
                        self.since_us = now_us;
                        return Outcome::Became(Role::Follower);
                    }
                    self.role = Role::Leader;
                    self.epoch = self.epoch.saturating_add(1);
                    self.since_us = now_us;
                    self.last_tick_sent_us = now_us;
                    self.better_ticks = 0;
                    Outcome::Became(Role::Leader)
                } else {
                    Outcome::Idle
                }
            }
            Role::Leader => {
                if now_us.saturating_sub(self.last_tick_sent_us) >= LEADER_TICK_INTERVAL_US {
                    self.last_tick_sent_us = now_us;
                    Outcome::SendTick
                } else {
                    Outcome::Idle
                }
            }
        }
    }

    /// When the caller should look in again.
    ///
    /// Returned rather than assumed so the shell can sleep instead of polling —
    /// which on a battery-powered sensor node is the difference between weeks
    /// and days.
    pub fn next_deadline_us(&self, now_us: u64) -> u64 {
        let (from, interval) = match self.role {
            Role::Follower => (self.since_us, FOLLOWER_TIMEOUT_US),
            Role::Candidate => (self.since_us, CANDIDATE_SETTLE_US),
            Role::Leader => (self.last_tick_sent_us, LEADER_TICK_INTERVAL_US),
        };
        let due = from.saturating_add(interval);
        due.saturating_sub(now_us)
    }

    fn note(&mut self, who: Candidacy) {
        self.best_seen = Some(match self.best_seen {
            Some(best) if best.beats(&who) => best,
            _ => who,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(capacity: u32, first: u8) -> Candidacy {
        let mut bytes = [0u8; 16];
        bytes[0] = first;
        Candidacy {
            capacity,
            uuid: Uuid(bytes),
        }
    }

    #[test]
    fn higher_capacity_wins() {
        assert!(id(200, 1).beats(&id(100, 1)));
        assert!(!id(100, 1).beats(&id(200, 1)));
    }

    #[test]
    fn equal_capacity_breaks_on_the_uuid_and_is_never_a_tie() {
        // Two identical devices flashed from the same binary are common. If the
        // comparison could return "neither", they would both stand forever.
        let a = id(100, 1);
        let b = id(100, 2);
        assert!(a.beats(&b));
        assert!(!b.beats(&a));
        // And it is antisymmetric for every pair, which is what stops two
        // devices both believing they won.
        for x in 0..8u8 {
            for y in 0..8u8 {
                if x == y {
                    continue;
                }
                let (p, q) = (id(50, x), id(50, y));
                assert_ne!(p.beats(&q), q.beats(&p), "{x} vs {y}");
            }
        }
    }

    #[test]
    fn a_candidacy_never_beats_itself() {
        let a = id(100, 1);
        assert!(!a.beats(&a));
    }

    #[test]
    fn a_lone_follower_stands_and_wins() {
        let mut e = Election::new(id(100, 1), 0);
        assert_eq!(e.role(), Role::Follower);

        // Nothing happens before the timeout.
        assert_eq!(e.on_timer(FOLLOWER_TIMEOUT_US - 1), Outcome::Idle);
        assert_eq!(e.on_timer(FOLLOWER_TIMEOUT_US), Outcome::Announce);
        assert_eq!(e.role(), Role::Candidate);

        let t = FOLLOWER_TIMEOUT_US;
        assert_eq!(e.on_timer(t + CANDIDATE_SETTLE_US - 1), Outcome::Idle);
        assert_eq!(
            e.on_timer(t + CANDIDATE_SETTLE_US),
            Outcome::Became(Role::Leader)
        );
        assert_eq!(e.epoch(), 1, "taking the role must advance the epoch");
        assert!(e.is_leader());
    }

    #[test]
    fn a_leader_ticks_on_schedule() {
        let mut e = leader_at(0);
        assert_eq!(e.on_timer(LEADER_TICK_INTERVAL_US - 1), Outcome::Idle);
        assert_eq!(e.on_timer(LEADER_TICK_INTERVAL_US), Outcome::SendTick);
        // And keeps doing so.
        assert_eq!(e.on_timer(2 * LEADER_TICK_INTERVAL_US), Outcome::SendTick);
    }

    /// An election already won at `now_us`.
    fn leader_at(now_us: u64) -> Election {
        let mut e = Election::new(id(100, 1), now_us);
        e.on_timer(now_us + FOLLOWER_TIMEOUT_US);
        e.on_timer(now_us + FOLLOWER_TIMEOUT_US + CANDIDATE_SETTLE_US);
        assert!(e.is_leader());
        // Re-base so tests can reason from zero.
        e.last_tick_sent_us = now_us;
        e.since_us = now_us;
        e
    }

    #[test]
    fn a_tick_from_a_live_leader_keeps_a_follower_following() {
        let mut e = Election::new(id(100, 1), 0);
        // Even a *worse* leader holds the role: a working timebase beats a
        // better one that has to be elected first.
        for step in 1..10u64 {
            let now = step * (FOLLOWER_TIMEOUT_US - 1);
            assert_eq!(e.on_tick(now, id(1, 9), 1), Outcome::Idle);
            assert_eq!(e.on_timer(now), Outcome::Idle);
        }
        assert_eq!(e.role(), Role::Follower);
    }

    #[test]
    fn a_candidate_stands_down_for_something_better() {
        let mut e = Election::new(id(100, 1), 0);
        e.on_timer(FOLLOWER_TIMEOUT_US);
        assert_eq!(e.role(), Role::Candidate);
        assert_eq!(
            e.on_tick(FOLLOWER_TIMEOUT_US, id(200, 2), 1),
            Outcome::Became(Role::Follower)
        );
        assert_eq!(e.role(), Role::Follower);
    }

    #[test]
    fn a_candidate_ignores_something_worse_and_wins_anyway() {
        let mut e = Election::new(id(100, 1), 0);
        e.on_timer(FOLLOWER_TIMEOUT_US);
        assert_eq!(e.on_tick(FOLLOWER_TIMEOUT_US, id(1, 9), 1), Outcome::Idle);
        assert_eq!(e.role(), Role::Candidate);
        assert_eq!(
            e.on_timer(FOLLOWER_TIMEOUT_US + CANDIDATE_SETTLE_US),
            Outcome::Became(Role::Leader)
        );
    }

    #[test]
    fn a_candidate_that_was_beaten_during_its_wait_stands_down_at_the_end() {
        // The better candidate might not have been better at the moment it was
        // seen if only the last one counted. Remembering the best seen is what
        // makes the outcome independent of arrival order.
        let mut e = Election::new(id(100, 1), 0);
        e.on_timer(FOLLOWER_TIMEOUT_US);
        e.on_tick(FOLLOWER_TIMEOUT_US, id(200, 5), 1);
        // Standing down happens immediately on seeing better.
        assert_eq!(e.role(), Role::Follower);
    }

    #[test]
    fn a_leader_needs_three_consecutive_better_ticks_before_yielding() {
        // Without the hysteresis a device rebooting with a marginally different
        // benchmark triggers a needless handover, and a handover is visible.
        let mut e = leader_at(0);
        let better = id(500, 2);
        assert_eq!(e.on_tick(1, better, 1), Outcome::Idle);
        assert_eq!(e.on_tick(2, better, 1), Outcome::Idle);
        assert!(e.is_leader(), "two ticks must not be enough");
        assert_eq!(e.on_tick(3, better, 1), Outcome::Became(Role::Follower));
        assert_eq!(e.role(), Role::Follower);
    }

    #[test]
    fn a_worse_tick_resets_the_yield_count() {
        // One stray better tick between worse ones must not accumulate towards a
        // handover, or a flapping peer eventually wins by attrition.
        let mut e = leader_at(0);
        let better = id(500, 2);
        let worse = id(1, 9);
        e.on_tick(1, better, 1);
        e.on_tick(2, better, 1);
        e.on_tick(3, worse, 1);
        assert!(e.is_leader());
        e.on_tick(4, better, 1);
        e.on_tick(5, better, 1);
        assert!(e.is_leader(), "the count should have restarted");
        assert_eq!(e.on_tick(6, better, 1), Outcome::Became(Role::Follower));
    }

    #[test]
    fn a_leader_ignores_worse_ticks_forever() {
        let mut e = leader_at(0);
        for step in 0..50 {
            assert_eq!(e.on_tick(step, id(1, 9), 1), Outcome::Idle);
        }
        assert!(e.is_leader());
    }

    #[test]
    fn a_node_ignores_its_own_tick() {
        // A device that counted its own multicast as a peer would never elect
        // anyone: it would keep resetting its own follower timeout.
        let me = id(100, 1);
        let mut e = Election::new(me, 0);
        assert_eq!(e.on_tick(1, me, 5), Outcome::Idle);
        assert_eq!(e.on_timer(FOLLOWER_TIMEOUT_US), Outcome::Announce);
    }

    #[test]
    fn the_epoch_only_moves_forward() {
        let mut e = Election::new(id(100, 1), 0);
        e.on_tick(1, id(50, 2), 7);
        assert_eq!(e.epoch(), 7);
        e.on_tick(2, id(50, 2), 3);
        assert_eq!(e.epoch(), 7, "an older epoch must not roll the clock back");
    }

    #[test]
    fn the_deadline_shrinks_as_time_passes_and_never_underflows() {
        let e = Election::new(id(100, 1), 0);
        assert_eq!(e.next_deadline_us(0), FOLLOWER_TIMEOUT_US);
        assert_eq!(
            e.next_deadline_us(FOLLOWER_TIMEOUT_US / 2),
            FOLLOWER_TIMEOUT_US / 2
        );
        // Past due is zero, not a huge number from wrapping.
        assert_eq!(e.next_deadline_us(FOLLOWER_TIMEOUT_US * 10), 0);
    }

    #[test]
    fn every_role_reports_a_deadline() {
        let mut e = Election::new(id(100, 1), 0);
        assert!(e.next_deadline_us(0) > 0);
        e.on_timer(FOLLOWER_TIMEOUT_US);
        assert!(e.next_deadline_us(FOLLOWER_TIMEOUT_US) > 0);
        e.on_timer(FOLLOWER_TIMEOUT_US + CANDIDATE_SETTLE_US);
        assert!(e.next_deadline_us(FOLLOWER_TIMEOUT_US + CANDIDATE_SETTLE_US) > 0);
    }

    #[test]
    fn two_nodes_converge_on_one_leader() {
        // The scenario that matters: both start cold, both stand, and exactly
        // one wins - with no coordination beyond the ticks they exchange.
        let (strong, weak) = (id(200, 1), id(100, 2));
        let mut a = Election::new(strong, 0);
        let mut b = Election::new(weak, 0);

        let mut now = FOLLOWER_TIMEOUT_US;
        assert_eq!(a.on_timer(now), Outcome::Announce);
        assert_eq!(b.on_timer(now), Outcome::Announce);
        // They hear each other's candidacy.
        b.on_tick(now, strong, 0);
        a.on_tick(now, weak, 0);
        assert_eq!(b.role(), Role::Follower, "the weaker node must stand down");

        now += CANDIDATE_SETTLE_US;
        assert_eq!(a.on_timer(now), Outcome::Became(Role::Leader));
        assert!(a.is_leader());
        assert!(!b.is_leader());
    }
}
