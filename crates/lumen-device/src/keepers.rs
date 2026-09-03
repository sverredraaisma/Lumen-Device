//! Choosing which devices hold the records.
//!
//! Keepers store the mesh's state — zones, scenes, schedules, bindings — and
//! gossip it between themselves. Everyone else pulls what they need read-only
//! and caches it.
//!
//! # There is no election protocol, and that is the point
//!
//! Every device already knows the peer table: who exists, how much flash they
//! have, what they can render. Ranking that table is a **pure function**, so
//! every device computes the same keeper set independently, at the same moment,
//! with no messages exchanged and nothing to disagree about.
//!
//! That is worth more than it sounds. A timebase election needs messages because
//! it has to converge on *one* device and a split brain is visible as a torn
//! show. A keeper set does not: it has to converge on a *set*, the set changes
//! only when the peer table does, and a device that is briefly wrong about it
//! pulls a record from someone who is not a keeper — which works, because a
//! non-keeper that has the record serves it and one that does not says so.
//!
//! So the failure mode of being wrong here is a wasted round trip, not a broken
//! show, and paying for a protocol to prevent a wasted round trip would be the
//! wrong trade.
//!
//! # Ranked by flash, then by capacity
//!
//! Flash first because the job is *storing* things, and a device with room for
//! the record set is qualified for it in a way a fast one with no room is not.
//! Capacity second because a keeper also answers pulls, and a device already
//! struggling to render should not also be serving state. The UUID breaks
//! whatever is left, in the same direction the timebase election breaks its ties,
//! so two devices flashed from one binary still order deterministically.

use lumen_proto::Uuid;

/// How many keepers a mesh runs.
///
/// The data model allows five to seven. Five is chosen because gossip is the
/// cost: every keeper sends a digest every five seconds, so the traffic grows
/// with the *number of keepers*, not with the size of the mesh. Five gives two
/// spare copies beyond the three that make a healed partition converge, which is
/// enough redundancy for a house, and seven would spend a third more bandwidth
/// buying a fourth spare copy nobody has needed.
pub const MAX_KEEPERS: usize = 5;

/// Below this many keepers, a healed partition may not converge on its own.
pub const QUORUM: usize = 3;

/// A device's claim to be a keeper.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Claim {
    pub uuid: Uuid,
    /// Flash available for records, in kibibytes.
    pub flash_kib: u32,
    /// The device's static capacity score, the same one the timebase election
    /// uses.
    pub capacity: u32,
    /// Whether this device may keep records at all.
    ///
    /// A bridged node is never a keeper: it reaches the mesh through another
    /// device, so making it hold state puts a hop between the records and
    /// everyone who needs them, and takes the state offline whenever its bridge
    /// reboots.
    pub eligible: bool,
}

impl Claim {
    /// Keeper order: the better claim sorts first.
    ///
    /// A genuine total order — reflexive, antisymmetric, transitive — rather
    /// than a "does this beat that" predicate. Sorting needs the real thing:
    /// a comparator that answered "greater" when handed a claim twice would
    /// break `sort_unstable_by`'s contract, and the sort is entitled to compare
    /// an element with itself.
    pub fn rank(&self, other: &Claim) -> core::cmp::Ordering {
        // Ineligible last, then most flash, then most capacity. Reversed
        // operands where a larger value should sort earlier.
        other
            .eligible
            .cmp(&self.eligible)
            .then_with(|| other.flash_kib.cmp(&self.flash_kib))
            .then_with(|| other.capacity.cmp(&self.capacity))
            // The same direction the timebase election breaks its ties. What
            // matters is not which way it goes but that every device gets the
            // same answer from the same two claims.
            .then_with(|| self.uuid.0.cmp(&other.uuid.0))
    }

    /// Whether this device should be a keeper before `other`.
    pub fn ranks_above(&self, other: &Claim) -> bool {
        self.rank(other) == core::cmp::Ordering::Less
    }
}

/// How well the chosen keepers protect the mesh's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quorum {
    /// Three or more. A partition that heals converges on HLC order by itself.
    Healthy,
    /// Two. State survives one device dying, but a partition can leave two
    /// halves each believing they have the newer record and no majority to
    /// settle it.
    Thin,
    /// One. Losing that device loses the show, and nothing here can prevent it —
    /// the app should be telling somebody to add a second mains-powered device.
    Fragile,
    /// Nothing eligible. Every device is bridged, or the mesh is empty.
    None,
}

impl Quorum {
    fn of(count: usize) -> Quorum {
        match count {
            0 => Quorum::None,
            1 => Quorum::Fragile,
            2 => Quorum::Thin,
            _ => Quorum::Healthy,
        }
    }

    /// Whether the user should be told about this.
    ///
    /// Anything short of three, because "my lights forgot everything" is the
    /// worst failure this system has and the only cure is a second device that
    /// stays powered.
    pub fn needs_warning(self) -> bool {
        !matches!(self, Quorum::Healthy)
    }
}

/// Sort `claims` into keeper order and say how many of them are keepers.
///
/// Sorts in place and takes a caller-provided slice, so nothing here allocates
/// and a device can rank its peer table where it already lives. The first
/// element of the returned count are the keepers, in rank order; ineligible
/// devices sort to the end and are never counted.
pub fn select(claims: &mut [Claim]) -> (usize, Quorum) {
    // `sort_unstable` because the ranking already distinguishes every pair by
    // UUID, so stability cannot change the result — and because the stable sort
    // would allocate, which this must not.
    claims.sort_unstable_by(Claim::rank);

    let eligible = claims.iter().filter(|c| c.eligible).count();
    let count = eligible.min(MAX_KEEPERS);
    (count, Quorum::of(count))
}

/// Whether `me` is a keeper, given the whole peer table including itself.
///
/// The question every device actually asks. Kept separate from [`select`] so a
/// device that only wants a yes or no does not have to reason about ordering.
pub fn is_keeper(claims: &mut [Claim], me: Uuid) -> bool {
    let (count, _) = select(claims);
    claims[..count].iter().any(|c| c.uuid == me)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u8) -> Uuid {
        Uuid([n; 16])
    }

    fn claim(n: u8, flash_kib: u32, capacity: u32) -> Claim {
        Claim {
            uuid: uuid(n),
            flash_kib,
            capacity,
            eligible: true,
        }
    }

    #[test]
    fn more_flash_keeps_the_records() {
        // The job is storing things, so room to store them ranks first.
        let big = claim(1, 4096, 10);
        let fast = claim(2, 512, 900);
        assert!(big.ranks_above(&fast));
        assert!(!fast.ranks_above(&big));
    }

    #[test]
    fn equal_flash_breaks_on_capacity() {
        // A keeper also answers pulls, so a device already struggling to render
        // should not also be serving state.
        let idle = claim(1, 4096, 900);
        let busy = claim(2, 4096, 100);
        assert!(idle.ranks_above(&busy));
    }

    #[test]
    fn two_identical_devices_still_order() {
        // Two strips flashed from one binary is the normal case, not the corner
        // one. If this were a tie, two devices could disagree about the keeper
        // set and pull records from each other forever.
        let a = claim(1, 4096, 500);
        let b = claim(2, 4096, 500);
        assert!(a.ranks_above(&b));
        assert!(!b.ranks_above(&a));
    }

    #[test]
    fn the_ranking_is_a_total_order() {
        // The property the whole design rests on: every device sorts the same
        // table into the same sequence, so no messages are needed. Checked
        // exhaustively over a set with every kind of tie in it.
        let claims = [
            claim(1, 4096, 500),
            claim(2, 4096, 500),
            claim(3, 4096, 900),
            claim(4, 512, 900),
            claim(5, 512, 500),
        ];
        use core::cmp::Ordering;
        for a in &claims {
            // Reflexive. A sort is entitled to compare an element with itself,
            // and a comparator that called that "greater" would be lying to it.
            assert_eq!(a.rank(a), Ordering::Equal, "a claim does not equal itself");
            for b in &claims {
                // Antisymmetric.
                assert_eq!(
                    a.rank(b),
                    b.rank(a).reverse(),
                    "{:?} and {:?} do not order",
                    a.uuid,
                    b.uuid
                );
                // Transitive.
                for c in &claims {
                    if a.rank(b) != Ordering::Greater && b.rank(c) != Ordering::Greater {
                        assert_ne!(
                            a.rank(c),
                            Ordering::Greater,
                            "{:?} <= {:?} <= {:?} but not {:?} <= {:?}",
                            a.uuid,
                            b.uuid,
                            c.uuid,
                            a.uuid,
                            c.uuid
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_device_picks_the_same_keepers_from_the_same_table() {
        // Stated as: the answer does not depend on the order the peer table
        // happens to be in, which is what differs between devices - each one
        // learned about its peers in whatever order they turned up.
        let base = [
            claim(9, 1024, 100),
            claim(3, 4096, 500),
            claim(7, 2048, 800),
            claim(1, 4096, 900),
            claim(5, 512, 200),
            claim(2, 2048, 800),
        ];

        let mut expected = base;
        let (count, _) = select(&mut expected);
        let expected: alloc::vec::Vec<Uuid> = expected[..count].iter().map(|c| c.uuid).collect();

        // Every rotation of the same table.
        for shift in 1..base.len() {
            let mut rotated = base;
            rotated.rotate_left(shift);
            let (n, _) = select(&mut rotated);
            let got: alloc::vec::Vec<Uuid> = rotated[..n].iter().map(|c| c.uuid).collect();
            assert_eq!(got, expected, "rotation by {shift} chose differently");
        }
    }

    #[test]
    fn a_bridged_device_is_never_a_keeper() {
        // It reaches the mesh through another device, so keeping state there
        // puts a hop in front of the records and takes them offline whenever the
        // bridge reboots.
        let mut claims = [
            Claim {
                eligible: false,
                ..claim(1, 65_536, 999)
            },
            claim(2, 512, 100),
        ];
        let (count, quorum) = select(&mut claims);
        assert_eq!(count, 1);
        assert_eq!(claims[0].uuid, uuid(2));
        assert_eq!(quorum, Quorum::Fragile);
        assert!(!is_keeper(&mut claims, uuid(1)));
    }

    #[test]
    fn a_large_mesh_keeps_the_cap() {
        // Gossip cost grows with the number of keepers, not the size of the
        // mesh, so this is the number that must not drift upward with it.
        let mut claims: alloc::vec::Vec<Claim> =
            (1..=40).map(|n| claim(n, 1024 + n as u32, 500)).collect();
        let (count, quorum) = select(&mut claims);
        assert_eq!(count, MAX_KEEPERS);
        assert_eq!(quorum, Quorum::Healthy);
        // The five with the most flash, which are the highest numbered here.
        for (rank, c) in claims[..count].iter().enumerate() {
            assert_eq!(c.uuid, uuid(40 - rank as u8));
        }
    }

    #[test]
    fn quorum_reports_what_the_user_needs_telling() {
        assert_eq!(Quorum::of(0), Quorum::None);
        assert_eq!(Quorum::of(1), Quorum::Fragile);
        assert_eq!(Quorum::of(2), Quorum::Thin);
        assert_eq!(Quorum::of(3), Quorum::Healthy);
        assert_eq!(Quorum::of(99), Quorum::Healthy);

        assert!(Quorum::None.needs_warning());
        assert!(Quorum::Fragile.needs_warning());
        assert!(Quorum::Thin.needs_warning());
        assert!(!Quorum::Healthy.needs_warning());
        assert_eq!(QUORUM, 3);
    }

    #[test]
    fn a_mesh_with_nothing_eligible_has_no_keepers() {
        let mut claims = [Claim {
            eligible: false,
            ..claim(1, 4096, 500)
        }];
        let (count, quorum) = select(&mut claims);
        assert_eq!(count, 0);
        assert_eq!(quorum, Quorum::None);
        assert!(!is_keeper(&mut claims, uuid(1)));
    }

    #[test]
    fn an_empty_mesh_does_not_panic() {
        let mut claims: [Claim; 0] = [];
        assert_eq!(select(&mut claims), (0, Quorum::None));
        assert!(!is_keeper(&mut claims, uuid(1)));
    }

    #[test]
    fn a_device_learns_whether_it_is_a_keeper_from_the_table_it_already_has() {
        let mut claims = [
            claim(1, 4096, 500),
            claim(2, 2048, 500),
            claim(3, 1024, 500),
        ];
        assert!(is_keeper(&mut claims, uuid(1)));
        assert!(is_keeper(&mut claims, uuid(3)));
        // Somebody who is not in the table at all.
        assert!(!is_keeper(&mut claims, uuid(9)));
    }
}
