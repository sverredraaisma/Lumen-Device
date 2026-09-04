//! The source stack.
//!
//! **There is exactly one mechanism.** Every zone holds a stack of active
//! sources; anything that wants to affect lights pushes one. Shows, schedules,
//! alerts, the app, Home Assistant and Art-Net are all just things that push.
//!
//! The payoff is that the hard question — "an alert fires during a scheduled
//! show while I am manually controlling the lights, what happens?" — has no
//! special case. Three sources sit at three priorities, the alert renders, and
//! when it expires the lights fall back to manual, then to the show. Nothing had
//! to be coordinated, and nothing gets permanently stuck.
//!
//! # Two rules that are not negotiable
//!
//! **Every source above the ambient floor must expire.** A source at priority
//! above zero with no expiry is how a room ends up stuck red at 3am with nobody
//! knowing why. [`SourceStack::push`] refuses it, the same way the wire codec
//! does — a rule enforced in one place is a rule someone routes around.
//!
//! **A source that cannot be admitted is not rendered at all**, never at reduced
//! quality. Degrading quietly looks like a bug; not appearing at least matches
//! what the admission report says happened.
//!
//! # Admission
//!
//! A device renders as many concurrent sources as its budget allows, admitting
//! **highest priority first**, so what gets dropped under pressure is always the
//! least important thing. Admission is re-evaluated whenever the stack changes,
//! so a source drops back in the moment something above it expires.

use alloc::vec::Vec;

use lumen_proto::Uuid;
use lumen_vm::q16::Q16;

/// The highest priority that may omit an expiry.
///
/// Band 0–63 is the ambient floor. Anything above it is a show, a manual
/// override, a system-health indicator or an alert, and every one of those has
/// something that ends it.
pub const AMBIENT_FLOOR_MAX: u8 = 63;

/// Concurrent sources every `render` device must support, plus one fading out.
///
/// Not negotiable: below it an alert cannot appear over an ambient scene, and
/// that is the whole point of the stack. A device that cannot meet the floor at
/// its configured LED count and frame rate reduces its frame rate until it can.
pub const CONCURRENCY_FLOOR: usize = 2;

/// Why a push was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushError {
    /// Above the ambient floor with no expiry — the "stuck red at 3am" rule.
    NoExpiry { priority: u8 },
    /// Already expired when it arrived.
    ///
    /// Not silently accepted-and-immediately-dropped: a source pushed after its
    /// own expiry must never render, not even for the frame it took to notice.
    AlreadyExpired { expires_at_us: u64 },
    /// The stack is full of things this node cannot drop.
    NoRoom,
}

/// Why a source left the stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Removal {
    Expired,
    Popped,
    /// Replaced by a newer push with the same id.
    Superseded,
}

/// One claim on a zone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Source {
    pub id: Uuid,
    pub zone: Uuid,
    pub scene: Uuid,
    pub priority: u8,
    /// Absolute show time. `None` is legal only at the ambient floor.
    ///
    /// Absolute rather than a duration because every device shares the clock, so
    /// a source expires at the same instant everywhere regardless of when each
    /// device received the push.
    pub expires_at_us: Option<u64>,
    pub fade_in_ms: u16,
    pub fade_out_ms: u16,
    /// When this source was pushed. Breaks ties at equal priority.
    pub pushed_at_us: u64,
    /// Cost per pixel, from the compiler's budget report.
    pub cost: u32,
}

impl Source {
    /// How much of this source is showing at `now_us`, as a 0..=1 fraction.
    ///
    /// `fade_in_ms` was decoded from the wire and then ignored: a source asking
    /// to arrive over half a second snapped on instantly, and only the fade
    /// *out* was ever honoured. This is the other half.
    ///
    /// Measured from `pushed_at_us`, which is on the **show clock** like every
    /// other instant the core sees. That is what synchronises a fade across
    /// devices to the millisecond rather than to whenever each of them happened
    /// to process the message - as long as the caller sets `pushed_at_us` from
    /// the pushing message's `show_time_us` and not from its own arrival time.
    /// A device that anchors it locally still fades correctly; it just fades a
    /// few milliseconds out of step with its neighbours, which on a wave
    /// crossing several strips is exactly what is visible.
    pub fn fade_in_alpha(&self, now_us: u64) -> Q16 {
        let span = (self.fade_in_ms as u64).saturating_mul(1_000);
        if span == 0 {
            return Q16::ONE;
        }
        let elapsed = now_us.saturating_sub(self.pushed_at_us);
        if elapsed >= span {
            return Q16::ONE;
        }
        // The same fixed-point fraction the fade out uses, so the two agree bit
        // for bit about what half-faded means.
        Q16(((elapsed * 65_536) / span) as i32)
    }

    /// Whether this source outranks `other` for the same pixel.
    ///
    /// Higher priority first; on a tie the **most recently pushed** wins, which
    /// is what makes "push again to take over" work without a separate protocol
    /// for replacing something.
    pub fn outranks(&self, other: &Source) -> bool {
        match self.priority.cmp(&other.priority) {
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Less => false,
            core::cmp::Ordering::Equal => self.pushed_at_us > other.pushed_at_us,
        }
    }

    pub fn is_ambient(&self) -> bool {
        self.priority <= AMBIENT_FLOOR_MAX
    }

    /// Whether this source has lapsed at `now_us`.
    pub fn has_expired(&self, now_us: u64) -> bool {
        matches!(self.expires_at_us, Some(t) if now_us >= t)
    }
}

/// A source that is on its way out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fading {
    pub source: Source,
    pub started_at_us: u64,
    pub reason: Removal,
}

impl Fading {
    /// How far through the fade, 0..=65536, at `now_us`.
    ///
    /// Returned as a fixed-point fraction rather than a float so the compositor
    /// and the VM agree bit for bit about what half-faded means.
    pub fn progress(&self, now_us: u64) -> u32 {
        let span = (self.source.fade_out_ms as u64).saturating_mul(1_000);
        if span == 0 {
            return 65_536;
        }
        let elapsed = now_us.saturating_sub(self.started_at_us);
        if elapsed >= span {
            return 65_536;
        }
        ((elapsed * 65_536) / span) as u32
    }

    pub fn is_done(&self, now_us: u64) -> bool {
        self.progress(now_us) >= 65_536
    }
}

/// What changed, so the caller can act without diffing the stack itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Admitted(Uuid),
    /// Rejected for budget, and by which margin.
    ///
    /// Reported rather than silent, so an app can say "the ceiling strip is
    /// ignoring the ambient scene because the alert and the show are using its
    /// budget" instead of leaving someone to notice a dark strip.
    Rejected {
        id: Uuid,
        cost: u32,
        spare: u32,
    },
    Removed {
        id: Uuid,
        reason: Removal,
    },
    FadeFinished(Uuid),
}

/// One zone's stack of sources, with admission control.
#[derive(Clone, Debug)]
pub struct SourceStack {
    active: Vec<Source>,
    fading: Vec<Fading>,
    /// Per-pixel budget this device can spend across all admitted sources.
    budget: u32,
    /// Most concurrent sources this device will admit.
    max_concurrent: usize,
    /// Ids currently admitted, highest priority first.
    admitted: Vec<Uuid>,
    /// Ids currently rejected.
    ///
    /// Kept so a rejection is reported when it *starts*, not on every frame it
    /// continues. Repeating it would flood the event stream and make the app's
    /// explanation flicker, which is worse than not explaining at all.
    rejected: Vec<Uuid>,
}

impl SourceStack {
    /// A stack for a device with `budget` per-pixel units and room for
    /// `max_concurrent` sources.
    ///
    /// `max_concurrent` is clamped up to [`CONCURRENCY_FLOOR`]: a device that
    /// cannot carry two sources cannot show an alert over an ambient scene, and
    /// the answer to that is a lower frame rate, not a smaller stack.
    pub fn new(budget: u32, max_concurrent: usize) -> SourceStack {
        SourceStack {
            active: Vec::new(),
            fading: Vec::new(),
            budget,
            max_concurrent: max_concurrent.max(CONCURRENCY_FLOOR),
            admitted: Vec::new(),
            rejected: Vec::new(),
        }
    }

    pub fn budget(&self) -> u32 {
        self.budget
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Sources on the stack, highest priority first.
    pub fn active(&self) -> &[Source] {
        &self.active
    }

    /// Sources currently fading out.
    pub fn fading(&self) -> &[Fading] {
        &self.fading
    }

    /// Ids that will actually render, highest priority first.
    pub fn admitted(&self) -> &[Uuid] {
        &self.admitted
    }

    pub fn is_admitted(&self, id: Uuid) -> bool {
        self.admitted.contains(&id)
    }

    /// The source that renders for a pixel this zone covers, if any.
    pub fn top(&self) -> Option<&Source> {
        self.admitted
            .first()
            .and_then(|id| self.active.iter().find(|s| s.id == *id))
    }

    /// Push a source, or say why not.
    pub fn push(
        &mut self,
        now_us: u64,
        source: Source,
        changes: &mut Vec<Change>,
    ) -> Result<(), PushError> {
        if !source.is_ambient() && source.expires_at_us.is_none() {
            return Err(PushError::NoExpiry {
                priority: source.priority,
            });
        }
        if let Some(at) = source.expires_at_us {
            if now_us >= at {
                // Accepting it and dropping it next frame would render it once,
                // which is exactly the visible artefact the rule exists to stop.
                return Err(PushError::AlreadyExpired { expires_at_us: at });
            }
        }
        if source.cost > self.budget {
            // Nothing this device can drop would make room, so say so now rather
            // than after evicting everything else.
            return Err(PushError::NoRoom);
        }

        // Pushing an id that is already present replaces it. That is what makes
        // "renew by pushing again" work with no separate message.
        if let Some(existing) = self.active.iter().position(|s| s.id == source.id) {
            self.active.remove(existing);
            changes.push(Change::Removed {
                id: source.id,
                reason: Removal::Superseded,
            });
        }

        self.active.push(source);
        self.sort();
        self.readmit(changes);
        Ok(())
    }

    /// Remove a source, fading it out.
    ///
    /// Returns whether anything was there. A pop for a source that already
    /// expired is normal, not an error: the pusher and the expiry race, and both
    /// orders have to be fine.
    pub fn pop(&mut self, now_us: u64, id: Uuid, changes: &mut Vec<Change>) -> bool {
        let Some(at) = self.active.iter().position(|s| s.id == id) else {
            return false;
        };
        let source = self.active.remove(at);
        self.begin_fade(now_us, source, Removal::Popped, changes);
        self.readmit(changes);
        true
    }

    /// Extend a source's expiry.
    ///
    /// A renewal for something already gone is refused rather than resurrecting
    /// it: a source that lapsed has faded out, and bringing it back would be a
    /// visible flash from a message that arrived late.
    pub fn renew(&mut self, id: Uuid, expires_at_us: u64) -> bool {
        match self.active.iter_mut().find(|s| s.id == id) {
            Some(s) => {
                s.expires_at_us = Some(expires_at_us);
                true
            }
            None => false,
        }
    }

    /// Advance to `now_us`, expiring and finishing fades.
    ///
    /// Called every frame. Everything time-dependent happens here, so the rest
    /// of the stack has no notion of time passing on its own.
    pub fn advance(&mut self, now_us: u64, changes: &mut Vec<Change>) {
        let mut expired = Vec::new();
        let mut i = 0;
        while i < self.active.len() {
            if self.active[i].has_expired(now_us) {
                expired.push(self.active.remove(i));
            } else {
                i += 1;
            }
        }
        for source in expired {
            self.begin_fade(now_us, source, Removal::Expired, changes);
        }

        let mut j = 0;
        while j < self.fading.len() {
            if self.fading[j].is_done(now_us) {
                let done = self.fading.remove(j);
                changes.push(Change::FadeFinished(done.source.id));
            } else {
                j += 1;
            }
        }

        self.readmit(changes);
    }

    /// Budget not currently claimed by an admitted source.
    pub fn spare_budget(&self) -> u32 {
        let used: u32 = self
            .active
            .iter()
            .filter(|s| self.admitted.contains(&s.id))
            .map(|s| s.cost)
            .sum();
        self.budget.saturating_sub(used)
    }

    fn begin_fade(
        &mut self,
        now_us: u64,
        source: Source,
        reason: Removal,
        changes: &mut Vec<Change>,
    ) {
        changes.push(Change::Removed {
            id: source.id,
            reason,
        });
        if source.fade_out_ms == 0 {
            changes.push(Change::FadeFinished(source.id));
            return;
        }
        self.fading.push(Fading {
            source,
            started_at_us: now_us,
            reason,
        });
    }

    fn sort(&mut self) {
        // Highest priority first, most recent first within a priority. Sorting
        // rather than inserting in place keeps the ordering rule in one
        // expression, where it can be read against the spec.
        self.active.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then(b.pushed_at_us.cmp(&a.pushed_at_us))
                .then(b.id.0.cmp(&a.id.0))
        });
    }

    /// Re-run admission from scratch.
    ///
    /// From scratch rather than incrementally, because "a source drops back in
    /// as soon as something above it expires" is only true if the whole stack is
    /// reconsidered — an incremental version would leave a low-priority source
    /// out after the thing that displaced it had gone.
    fn readmit(&mut self, changes: &mut Vec<Change>) {
        let was_admitted = core::mem::take(&mut self.admitted);
        let was_rejected = core::mem::take(&mut self.rejected);
        let mut spent = 0u32;
        let mut admitted = Vec::new();
        let mut rejected = Vec::new();

        for s in &self.active {
            let fits_budget = spent.saturating_add(s.cost) <= self.budget;
            let fits_count = admitted.len() < self.max_concurrent;
            if fits_budget && fits_count {
                spent += s.cost;
                admitted.push(s.id);
                if !was_admitted.contains(&s.id) {
                    changes.push(Change::Admitted(s.id));
                }
            } else {
                rejected.push(s.id);
                if !was_rejected.contains(&s.id) {
                    changes.push(Change::Rejected {
                        id: s.id,
                        cost: s.cost,
                        spare: self.budget.saturating_sub(spent),
                    });
                }
            }
        }
        self.admitted = admitted;
        self.rejected = rejected;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u8) -> Uuid {
        Uuid([n; 16])
    }

    /// A source at `priority`, expiring at `expires`, costing `cost`.
    fn src(n: u8, priority: u8, expires: Option<u64>, cost: u32) -> Source {
        Source {
            id: uuid(n),
            zone: uuid(0),
            scene: uuid(n),
            priority,
            expires_at_us: expires,
            fade_in_ms: 0,
            fade_out_ms: 0,
            pushed_at_us: 0,
            cost,
        }
    }

    fn stack() -> (SourceStack, Vec<Change>) {
        (SourceStack::new(1_000, 4), Vec::new())
    }

    #[test]
    fn the_highest_priority_source_renders() {
        let (mut s, mut c) = stack();
        s.push(0, src(1, 10, None, 100), &mut c).unwrap();
        s.push(0, src(2, 200, Some(5_000), 100), &mut c).unwrap();
        s.push(0, src(3, 100, Some(5_000), 100), &mut c).unwrap();
        assert_eq!(s.top().map(|t| t.id), Some(uuid(2)));
        assert_eq!(s.admitted(), &[uuid(2), uuid(3), uuid(1)]);
    }

    #[test]
    fn equal_priority_gives_way_to_the_most_recent_push() {
        // What makes "push again to take over" work with no separate protocol.
        let (mut s, mut c) = stack();
        let mut older = src(1, 100, Some(9_000), 10);
        older.pushed_at_us = 1_000;
        let mut newer = src(2, 100, Some(9_000), 10);
        newer.pushed_at_us = 2_000;
        s.push(0, older, &mut c).unwrap();
        s.push(0, newer, &mut c).unwrap();
        assert_eq!(s.top().map(|t| t.id), Some(uuid(2)));
    }

    #[test]
    fn a_source_above_the_floor_with_no_expiry_is_refused() {
        // The "stuck red at 3am" rule. Enforced here as well as at the wire, so
        // a local pusher cannot route around it.
        let (mut s, mut c) = stack();
        assert_eq!(
            s.push(0, src(1, 200, None, 10), &mut c),
            Err(PushError::NoExpiry { priority: 200 })
        );
        // The boundary, not just a comfortably high number.
        assert_eq!(
            s.push(0, src(2, AMBIENT_FLOOR_MAX + 1, None, 10), &mut c),
            Err(PushError::NoExpiry {
                priority: AMBIENT_FLOOR_MAX + 1
            })
        );
        // And the floor itself is fine without one.
        assert!(s
            .push(0, src(3, AMBIENT_FLOOR_MAX, None, 10), &mut c)
            .is_ok());
    }

    #[test]
    fn a_source_pushed_after_its_own_expiry_never_renders() {
        // Accepting it and dropping it next frame would render it once, which is
        // exactly the visible artefact the rule exists to prevent.
        let (mut s, mut c) = stack();
        assert_eq!(
            s.push(5_000, src(1, 200, Some(4_000), 10), &mut c),
            Err(PushError::AlreadyExpired {
                expires_at_us: 4_000
            })
        );
        assert!(s.active().is_empty());
        assert!(s.admitted().is_empty());
    }

    #[test]
    fn an_expired_source_falls_back_to_what_was_underneath() {
        // The whole argument for the stack: an alert over a show over an ambient
        // scene, resolving with no special case anywhere.
        let (mut s, mut c) = stack();
        s.push(0, src(1, 10, None, 10), &mut c).unwrap();
        s.push(0, src(2, 100, Some(5_000), 10), &mut c).unwrap();
        s.push(0, src(3, 240, Some(2_000), 10), &mut c).unwrap();
        assert_eq!(s.top().map(|t| t.id), Some(uuid(3)), "the alert renders");

        s.advance(2_000, &mut c);
        assert_eq!(s.top().map(|t| t.id), Some(uuid(2)), "then the show");

        s.advance(5_000, &mut c);
        assert_eq!(
            s.top().map(|t| t.id),
            Some(uuid(1)),
            "then the ambient floor"
        );

        // And the floor never goes away.
        s.advance(1_000_000, &mut c);
        assert_eq!(s.top().map(|t| t.id), Some(uuid(1)));
    }

    #[test]
    fn pushing_the_same_id_again_replaces_it() {
        let (mut s, mut c) = stack();
        s.push(0, src(1, 100, Some(5_000), 10), &mut c).unwrap();
        let mut again = src(1, 100, Some(9_000), 10);
        again.pushed_at_us = 100;
        s.push(0, again, &mut c).unwrap();
        assert_eq!(s.active().len(), 1);
        assert_eq!(s.active()[0].expires_at_us, Some(9_000));
        assert!(c.contains(&Change::Removed {
            id: uuid(1),
            reason: Removal::Superseded
        }));
    }

    #[test]
    fn popping_removes_and_reports() {
        let (mut s, mut c) = stack();
        s.push(0, src(1, 100, Some(5_000), 10), &mut c).unwrap();
        c.clear();
        assert!(s.pop(1_000, uuid(1), &mut c));
        assert!(s.active().is_empty());
        assert!(c.contains(&Change::Removed {
            id: uuid(1),
            reason: Removal::Popped
        }));
        // Popping again says nothing was there, rather than erroring: the pusher
        // and the expiry race, and both orders have to be fine.
        assert!(!s.pop(2_000, uuid(1), &mut c));
    }

    #[test]
    fn renewing_extends_a_live_source_and_refuses_a_dead_one() {
        let (mut s, mut c) = stack();
        s.push(0, src(1, 100, Some(5_000), 10), &mut c).unwrap();
        assert!(s.renew(uuid(1), 20_000));
        s.advance(6_000, &mut c);
        assert_eq!(s.top().map(|t| t.id), Some(uuid(1)), "the renewal held");

        s.advance(21_000, &mut c);
        assert!(
            !s.renew(uuid(1), 30_000),
            "resurrecting a lapsed source would be a visible flash"
        );
    }

    #[test]
    fn a_fade_runs_to_completion_and_then_the_source_is_gone() {
        let (mut s, mut c) = stack();
        let mut fading = src(1, 100, Some(1_000), 10);
        fading.fade_out_ms = 500;
        s.push(0, fading, &mut c).unwrap();
        c.clear();

        s.advance(1_000, &mut c);
        assert_eq!(s.fading().len(), 1, "it should be on its way out, not gone");
        assert_eq!(s.fading()[0].progress(1_000), 0);
        assert_eq!(s.fading()[0].progress(1_000 + 250_000), 65_536 / 2);

        s.advance(1_000 + 500_000, &mut c);
        assert!(s.fading().is_empty());
        assert!(c.contains(&Change::FadeFinished(uuid(1))));
    }

    #[test]
    fn a_zero_length_fade_finishes_immediately_rather_than_never() {
        // Dividing by the span would hang on zero; treating it as complete is
        // what "no fade" means.
        let (mut s, mut c) = stack();
        s.push(0, src(1, 100, Some(1_000), 10), &mut c).unwrap();
        c.clear();
        s.advance(1_000, &mut c);
        assert!(s.fading().is_empty());
        assert!(c.contains(&Change::FadeFinished(uuid(1))));
    }

    #[test]
    fn admission_drops_the_least_important_thing_first() {
        // Under pressure, what goes is always the least important source - never
        // whatever happened to arrive last.
        let mut s = SourceStack::new(100, 8);
        let mut c = Vec::new();
        s.push(0, src(1, 240, Some(9_000), 60), &mut c).unwrap();
        s.push(0, src(2, 100, Some(9_000), 60), &mut c).unwrap();
        assert_eq!(s.admitted(), &[uuid(1)], "the alert must survive");
        assert!(c
            .iter()
            .any(|x| matches!(x, Change::Rejected { id, .. } if *id == uuid(2))));
    }

    #[test]
    fn a_rejected_source_returns_when_the_budget_frees_up() {
        // Admission is re-evaluated from scratch, so a source drops back in the
        // moment what displaced it expires.
        let mut s = SourceStack::new(100, 8);
        let mut c = Vec::new();
        s.push(0, src(1, 240, Some(2_000), 60), &mut c).unwrap();
        s.push(0, src(2, 100, Some(9_000), 60), &mut c).unwrap();
        assert!(!s.is_admitted(uuid(2)));

        c.clear();
        s.advance(2_000, &mut c);
        assert!(s.is_admitted(uuid(2)), "it should have dropped back in");
        assert!(c.contains(&Change::Admitted(uuid(2))));
    }

    #[test]
    fn rejection_reports_the_shortfall_so_an_app_can_explain_it() {
        // "The ceiling strip is ignoring the ambient scene because the alert and
        // the show are using its budget" beats leaving someone to notice a dark
        // strip.
        let mut s = SourceStack::new(100, 8);
        let mut c = Vec::new();
        s.push(0, src(1, 240, Some(9_000), 80), &mut c).unwrap();
        c.clear();
        s.push(0, src(2, 100, Some(9_000), 50), &mut c).unwrap();
        let rejection = c
            .iter()
            .find_map(|x| match x {
                Change::Rejected { id, cost, spare } if *id == uuid(2) => Some((*cost, *spare)),
                _ => None,
            })
            .expect("expected a rejection");
        assert_eq!(rejection, (50, 20), "cost 50, only 20 spare");
    }

    #[test]
    fn a_source_costing_more_than_the_whole_budget_is_refused_outright() {
        // Nothing this device could drop would make room, so evicting everything
        // else first would be pure damage.
        let mut s = SourceStack::new(100, 8);
        let mut c = Vec::new();
        s.push(0, src(1, 10, None, 40), &mut c).unwrap();
        assert_eq!(
            s.push(0, src(2, 240, Some(9_000), 500), &mut c),
            Err(PushError::NoRoom)
        );
        assert!(s.is_admitted(uuid(1)), "the existing source must survive");
    }

    #[test]
    fn the_concurrency_floor_cannot_be_configured_away() {
        // Below two, an alert cannot appear over an ambient scene, which is the
        // whole point of the stack.
        let s = SourceStack::new(10_000, 0);
        assert_eq!(s.max_concurrent(), CONCURRENCY_FLOOR);
        let s2 = SourceStack::new(10_000, 1);
        assert_eq!(s2.max_concurrent(), CONCURRENCY_FLOOR);
        let s3 = SourceStack::new(10_000, 7);
        assert_eq!(s3.max_concurrent(), 7);
    }

    #[test]
    fn the_concurrency_limit_is_honoured_even_with_budget_to_spare() {
        // Each concurrent source costs a resident program and a render buffer,
        // not just instructions, so the count is a real limit of its own.
        let mut s = SourceStack::new(1_000_000, 2);
        let mut c = Vec::new();
        for n in 1..=4u8 {
            s.push(0, src(n, 200 - n, Some(9_000), 1), &mut c).unwrap();
        }
        assert_eq!(s.admitted().len(), 2);
        assert_eq!(s.admitted(), &[uuid(1), uuid(2)]);
    }

    #[test]
    fn spare_budget_reflects_only_what_is_admitted() {
        // The concurrency floor is two, so the exclusion has to come from the
        // budget rather than the count.
        let mut s = SourceStack::new(100, 8);
        let mut c = Vec::new();
        s.push(0, src(1, 200, Some(9_000), 80), &mut c).unwrap();
        s.push(0, src(2, 100, Some(9_000), 80), &mut c).unwrap();
        assert_eq!(s.admitted(), &[uuid(1)]);
        // Only the first is admitted, so the second's cost is not spent.
        assert_eq!(s.spare_budget(), 20);
    }

    #[test]
    fn an_empty_stack_renders_nothing_and_does_not_panic() {
        let (mut s, mut c) = stack();
        assert!(s.top().is_none());
        s.advance(1_000_000, &mut c);
        assert!(s.top().is_none());
        assert_eq!(s.spare_budget(), 1_000);
    }

    #[test]
    fn ordering_is_total_so_two_devices_cannot_disagree() {
        // Every device resolves the stack independently. If the ordering were
        // not total, two devices covering one zone could pick different sources
        // and the zone would render two things at once.
        let mut s = SourceStack::new(10_000, 16);
        let mut c = Vec::new();
        for n in 1..=6u8 {
            let mut src = src(n, 100, Some(9_000), 1);
            src.pushed_at_us = 500;
            s.push(0, src, &mut c).unwrap();
        }
        let first = s.admitted().to_vec();

        // The same set, pushed in the opposite order, must resolve identically.
        let mut s2 = SourceStack::new(10_000, 16);
        let mut c2 = Vec::new();
        for n in (1..=6u8).rev() {
            let mut src = src(n, 100, Some(9_000), 1);
            src.pushed_at_us = 500;
            s2.push(0, src, &mut c2).unwrap();
        }
        assert_eq!(first, s2.admitted(), "push order changed the outcome");
    }

    #[test]
    fn a_source_outranks_itself_never() {
        let a = src(1, 100, Some(1), 1);
        assert!(!a.outranks(&a));
    }

    #[test]
    fn an_ambient_source_is_recognised_by_its_band() {
        assert!(src(1, 0, None, 1).is_ambient());
        assert!(src(1, AMBIENT_FLOOR_MAX, None, 1).is_ambient());
        assert!(!src(1, AMBIENT_FLOOR_MAX + 1, Some(1), 1).is_ambient());
    }

    #[test]
    fn expiry_is_inclusive_of_the_instant_it_names() {
        // Two devices comparing `>` and `>=` would drop a source one frame
        // apart, which is visible on a long strip spanning both.
        let s = src(1, 100, Some(1_000), 1);
        assert!(!s.has_expired(999));
        assert!(s.has_expired(1_000));
        assert!(s.has_expired(1_001));
        assert!(!src(2, 0, None, 1).has_expired(u64::MAX));
    }

    #[test]
    fn changes_are_reported_once_per_transition_not_per_frame() {
        // A rejection repeated every frame would flood the event stream and make
        // the app's explanation flicker.
        let mut s = SourceStack::new(100, 8);
        let mut c = Vec::new();
        s.push(0, src(1, 240, Some(90_000), 80), &mut c).unwrap();
        s.push(0, src(2, 100, Some(90_000), 50), &mut c).unwrap();
        c.clear();
        for step in 1..10u64 {
            s.advance(step * 1_000, &mut c);
        }
        assert!(c.is_empty(), "a settled stack kept emitting: {c:?}");
    }

    // ---- Fading in ---------------------------------------------------------

    fn fading_in(fade_in_ms: u16, pushed_at_us: u64) -> Source {
        let mut s = src(1, 10, None, 10);
        s.fade_in_ms = fade_in_ms;
        s.pushed_at_us = pushed_at_us;
        s
    }

    #[test]
    fn a_source_with_no_fade_in_is_fully_present_immediately() {
        // The overwhelming majority of sources. `fade_in_ms` defaults to zero
        // and a thing that appears should appear.
        let s = fading_in(0, 1_000);
        assert_eq!(s.fade_in_alpha(1_000), Q16::ONE);
        assert_eq!(s.fade_in_alpha(0), Q16::ONE);
    }

    #[test]
    fn a_fade_in_runs_from_nothing_to_everything() {
        let s = fading_in(1_000, 5_000_000);
        assert_eq!(s.fade_in_alpha(5_000_000), Q16::ZERO);
        assert_eq!(s.fade_in_alpha(5_500_000), Q16::HALF);
        assert_eq!(s.fade_in_alpha(6_000_000), Q16::ONE);
        // And stays there rather than wrapping or overshooting.
        assert_eq!(s.fade_in_alpha(60_000_000), Q16::ONE);
    }

    #[test]
    fn a_fade_in_that_has_not_started_shows_nothing_rather_than_everything() {
        // A source pushed with a show time in the future, which is what a
        // scheduled activation looks like. Saturating the subtraction the wrong
        // way would make it appear at full brightness early - the one failure
        // here that is visible from across a room.
        let s = fading_in(1_000, 10_000_000);
        assert_eq!(s.fade_in_alpha(9_000_000), Q16::ZERO);
    }

    #[test]
    fn the_longest_fade_in_the_wire_format_allows_does_not_overflow() {
        // `fade_in_ms` is a u16, so just over a minute, and the intermediate
        // multiply by 65 536 is where a narrower type would have wrapped.
        let s = fading_in(u16::MAX, 0);
        assert_eq!(s.fade_in_alpha(0), Q16::ZERO);
        assert_eq!(s.fade_in_alpha(32_767 * 1_000), Q16(32_767));
        assert_eq!(s.fade_in_alpha(u16::MAX as u64 * 1_000), Q16::ONE);
    }
}
