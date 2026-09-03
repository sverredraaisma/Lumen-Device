//! Replicated records: hybrid logical clocks, signing authority, and gossip.
//!
//! Everything the mesh remembers is a record with a UUID, a type, a hybrid
//! logical clock timestamp, an author, and a signature.
//!
//! # Every record is signed, and by whom matters
//!
//! The mesh key is symmetric and shared, so any paired device — including a
//! cheap bridged node someone tampered with — could otherwise forge a `scene`,
//! `schedule` or `binding` and have it replicate as genuine. Programs were
//! already signed; the records deciding *which* program runs, when, and at what
//! priority have to be too, or signing the programs achieves very little.
//!
//! There are two authorities:
//!
//! - Everything authored by a person is signed by a **controller** key.
//! - A `device` record is signed by that **device's own** identity key, because
//!   a device is authoritative about itself and has no controller key.
//!
//! A device's self-signed record is accepted **only for its own UUID**. So a
//! compromised device can lie about itself and nothing else — a much smaller
//! blast radius, and the natural boundary.
//!
//! # Conflicts resolve simply, on purpose
//!
//! Last writer wins per record, ordered by HLC then author as a tiebreak.
//! Records are small and edits are rare and human-driven, so the pathological
//! cases that motivate real CRDTs do not arise. A record is replaced **whole**,
//! never merged field by field: two people editing one effect at once loses an
//! edit, which is acceptable and which the editor should warn about, whereas a
//! field-wise merge would produce an effect neither of them wrote.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lumen_proto::crypto::{Verifier, PUBLIC_KEY_LEN, SIGNATURE_LEN};
use lumen_proto::Uuid;

/// What a record holds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum RecordType {
    /// A device's own name, coordinates, output config and capabilities.
    Device = 0,
    Zone = 1,
    Effect = 2,
    Scene = 3,
    Show = 4,
    Schedule = 5,
    Binding = 6,
    Channel = 7,
    /// Authorised controller public keys, and revocations.
    Key = 8,
}

impl RecordType {
    pub const fn from_u8(v: u8) -> Option<RecordType> {
        Some(match v {
            0 => RecordType::Device,
            1 => RecordType::Zone,
            2 => RecordType::Effect,
            3 => RecordType::Scene,
            4 => RecordType::Show,
            5 => RecordType::Schedule,
            6 => RecordType::Binding,
            7 => RecordType::Channel,
            8 => RecordType::Key,
            _ => return None,
        })
    }

    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Whether this type is signed by the device it describes rather than by a
    /// controller.
    pub const fn is_self_signed(self) -> bool {
        matches!(self, RecordType::Device)
    }
}

/// A hybrid logical clock.
///
/// Wall time in the high bits, a counter in the low bits. The wall part keeps
/// ordering roughly meaningful to a human reading a log; the counter keeps it
/// **total** when two edits land in the same millisecond, which on a mesh where
/// several keepers accept writes is not rare.
///
/// Packed into one `u64` because that is what the wire carries and what a
/// digest compares — splitting it would mean two implementations could disagree
/// about which half sorts first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub struct Hlc(pub u64);

/// Bits of the counter. 16 gives 65 536 edits per millisecond before the
/// counter has to borrow from the wall part, which no human-driven edit stream
/// will reach.
const COUNTER_BITS: u32 = 16;

impl Hlc {
    pub const ZERO: Hlc = Hlc(0);

    pub fn new(wall_ms: u64, counter: u16) -> Hlc {
        Hlc((wall_ms << COUNTER_BITS) | counter as u64)
    }

    pub fn wall_ms(self) -> u64 {
        self.0 >> COUNTER_BITS
    }

    pub fn counter(self) -> u16 {
        (self.0 & ((1 << COUNTER_BITS) - 1)) as u16
    }

    /// The next timestamp for a local write at `wall_ms`.
    ///
    /// Advances the counter rather than the wall part when the clock has not
    /// moved, and **never goes backwards** even if the wall clock does — which
    /// it does, whenever a device learns the real time after boot. A stamp that
    /// went backwards would make a new edit lose to an old one.
    pub fn tick(self, wall_ms: u64) -> Hlc {
        if wall_ms > self.wall_ms() {
            Hlc::new(wall_ms, 0)
        } else {
            Hlc(self.0.saturating_add(1))
        }
    }

    /// Merge a timestamp seen from a peer.
    ///
    /// Taking the maximum is what makes causality hold across the mesh: after
    /// receiving an edit, every later local edit sorts after it, whatever the
    /// two wall clocks think.
    pub fn observe(self, other: Hlc, wall_ms: u64) -> Hlc {
        let highest = self.max(other);
        highest.tick(wall_ms.max(highest.wall_ms()))
    }
}

/// A replicated record.
///
/// The body is opaque here. Interpreting a `scene` is the runtime's job;
/// replication only needs identity, ordering, authorship and integrity, and
/// keeping it that way is what lets a new record type replicate with no change
/// to this module.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Record {
    pub id: Uuid,
    pub kind: RecordType,
    pub hlc: Hlc,
    /// Who signed it: a controller, or the device itself for a `device` record.
    pub author: Uuid,
    pub body: Vec<u8>,
    pub signature: [u8; SIGNATURE_LEN],
}

impl Record {
    /// The bytes a signature covers: `id ‖ kind ‖ hlc ‖ author ‖ body`.
    ///
    /// The single definition, matching `lumen_proto::msg::StateRecord`. A signer
    /// and a verifier that disagreed about this would reject every record while
    /// each believed the other was at fault.
    pub fn signed_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + 1 + 8 + 16 + self.body.len());
        out.extend_from_slice(self.id.as_bytes());
        out.push(self.kind.to_u8());
        out.extend_from_slice(&self.hlc.0.to_le_bytes());
        out.extend_from_slice(self.author.as_bytes());
        out.extend_from_slice(&self.body);
        out
    }

    /// Whether this record supersedes `other`.
    ///
    /// HLC first, then author as a tiebreak. The tiebreak exists so that two
    /// keepers handed the same pair reach the same answer; without it a genuine
    /// tie would resolve by arrival order and the mesh would diverge.
    pub fn supersedes(&self, other: &Record) -> bool {
        match self.hlc.cmp(&other.hlc) {
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Less => false,
            core::cmp::Ordering::Equal => self.author.0 > other.author.0,
        }
    }
}

/// Why a record was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RejectReason {
    /// The signature did not verify.
    BadSignature,
    /// The author is not a currently-authorised controller.
    UnknownAuthor,
    /// A `device` record signed by some other device.
    ///
    /// The rule that keeps a compromised device able to lie about itself and
    /// nothing else.
    NotItsOwnDeviceRecord,
    /// A controller key signed a record that only a device may sign, or the
    /// reverse.
    WrongAuthority,
    /// Older than what is already held.
    Superseded,
}

/// Who may sign what.
///
/// Held separately from the store so the authorisation rules are testable
/// without a store, and so revoking a key is one call rather than a sweep.
#[derive(Clone, Default, Debug)]
pub struct Authority {
    controllers: BTreeMap<Uuid, [u8; PUBLIC_KEY_LEN]>,
    devices: BTreeMap<Uuid, [u8; PUBLIC_KEY_LEN]>,
}

impl Authority {
    pub fn new() -> Authority {
        Authority::default()
    }

    pub fn authorise_controller(&mut self, id: Uuid, key: [u8; PUBLIC_KEY_LEN]) {
        self.controllers.insert(id, key);
    }

    /// Revoke a controller.
    ///
    /// Revocation replicates as a signed `key` record like any other state, so
    /// this is what applying one does locally.
    pub fn revoke_controller(&mut self, id: Uuid) -> bool {
        self.controllers.remove(&id).is_some()
    }

    /// Register a device's identity key, learned at pairing.
    pub fn register_device(&mut self, id: Uuid, key: [u8; PUBLIC_KEY_LEN]) {
        self.devices.insert(id, key);
    }

    pub fn is_controller(&self, id: Uuid) -> bool {
        self.controllers.contains_key(&id)
    }

    /// The key that may have signed `record`, and nothing else.
    fn key_for(&self, record: &Record) -> Result<&[u8; PUBLIC_KEY_LEN], RejectReason> {
        if record.kind.is_self_signed() {
            // A device record is authoritative only about the device that signed
            // it. Accepting one signed by a different device would let any
            // paired node rewrite another's coordinates and capabilities.
            if record.author != record.id {
                return Err(RejectReason::NotItsOwnDeviceRecord);
            }
            return self
                .devices
                .get(&record.author)
                .ok_or(RejectReason::UnknownAuthor);
        }
        if self.devices.contains_key(&record.author)
            && !self.controllers.contains_key(&record.author)
        {
            // A device key signing a scene or a schedule. Distinguished from an
            // unknown author because the fix is different: this one is a
            // tampered node, not a missing pairing.
            return Err(RejectReason::WrongAuthority);
        }
        self.controllers
            .get(&record.author)
            .ok_or(RejectReason::UnknownAuthor)
    }
}

/// One entry of a gossip digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DigestEntry {
    pub id: Uuid,
    pub hlc: Hlc,
}

/// What a gossip round should do, having compared two digests.
///
/// Both directions from one comparison, because a round is symmetric: whichever
/// node sent the digest, both ends learn the same set of differences and either
/// can act. That is what lets a `STATE_DIGEST` be answered with a single
/// `STATE_PULL` and a single `STATE_PUSH` instead of a negotiation.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Reconcile {
    /// Records to ask the peer for: it has them and this node does not, or its
    /// copy is newer.
    pub pull: Vec<Uuid>,
    /// Records to send the peer: this node has them and the peer does not, or
    /// this copy is newer.
    pub push: Vec<Uuid>,
}

impl Reconcile {
    /// Whether the two nodes already agree.
    ///
    /// The overwhelmingly common outcome. A mesh at rest gossips every five
    /// seconds and finds nothing every time, which is why the digest is a
    /// compact list of ids and clocks rather than anything that has to be
    /// verified — a steady-state round costs no signature checks at all.
    pub fn is_empty(&self) -> bool {
        self.pull.is_empty() && self.push.is_empty()
    }
}

/// The replicated store.
#[derive(Clone, Default, Debug)]
pub struct Store {
    records: BTreeMap<Uuid, Record>,
    /// Highest HLC this node has seen from anywhere, for stamping local writes.
    clock: Hlc,
}

impl Store {
    pub fn new() -> Store {
        Store::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn get(&self, id: Uuid) -> Option<&Record> {
        self.records.get(&id)
    }

    pub fn clock(&self) -> Hlc {
        self.clock
    }

    /// A timestamp for a record authored here.
    pub fn stamp(&mut self, wall_ms: u64) -> Hlc {
        self.clock = self.clock.tick(wall_ms);
        self.clock
    }

    /// Accept a record, verifying it first.
    ///
    /// **Verification only happens when something actually changes.** The digest
    /// exchange compares HLCs, so an unchanged record never reaches here and
    /// steady-state gossip costs no signature checks at all — which is what
    /// makes signing every record affordable on a C3.
    pub fn accept<V: Verifier>(
        &mut self,
        record: Record,
        authority: &Authority,
        verifier: &V,
        wall_ms: u64,
    ) -> Result<(), RejectReason> {
        if let Some(held) = self.records.get(&record.id) {
            if !record.supersedes(held) {
                return Err(RejectReason::Superseded);
            }
        }
        let key = authority.key_for(&record)?;
        if !verifier.verify(key, &record.signed_bytes(), &record.signature) {
            return Err(RejectReason::BadSignature);
        }
        // Only after it is known good: observing the HLC of a record that turned
        // out to be forged would let anyone push this node's clock forward.
        self.clock = self.clock.observe(record.hlc, wall_ms);
        self.records.insert(record.id, record);
        Ok(())
    }

    /// A compact list of what this node holds, for gossip.
    ///
    /// Ordered by id so two nodes produce comparable digests and a diff is a
    /// merge rather than a search.
    pub fn digest(&self) -> Vec<DigestEntry> {
        self.records
            .values()
            .map(|r| DigestEntry {
                id: r.id,
                hlc: r.hlc,
            })
            .collect()
    }

    /// Compare this store against a peer's digest, in both directions.
    ///
    /// [`Store::wanted`] answers half of this — what to pull — and is what a
    /// node uses when it only has to answer a digest. This answers the other
    /// half too, which is what a node running the round needs: a peer that is
    /// *behind* will never ask, so without the push half a record reaches a
    /// stale node only when that node happens to gossip first.
    ///
    /// Ordered by id in both directions, so two nodes handed the same pair of
    /// digests produce the same plan and a recorded round replays.
    ///
    /// **An equal clock means agreement, and nothing is transferred.** A hybrid
    /// logical clock is unique to the edit that produced it, so two records
    /// sharing one are the same record; if they were not, no amount of
    /// transferring would settle which to keep, and that is the conflict
    /// [`Authority`] resolves when a record actually arrives rather than
    /// something a digest can see.
    pub fn reconcile(&self, theirs: &[DigestEntry]) -> Reconcile {
        // The pull half is `wanted`, not a second copy of it. A digest is not
        // promised to be free of duplicates, so this sorts and dedupes on top -
        // pulling the same record twice is a wasted round trip rather than a
        // fault, but a plan that lists it twice is one a test cannot compare.
        let mut pull = self.wanted(theirs);
        pull.sort_unstable();
        pull.dedup();

        // The push half: what this node holds that the peer lacks or is behind
        // on. Indexed rather than scanned, because a digest arrives ordered by
        // id but nothing in the protocol obliges a peer to send it that way.
        let mut peer: BTreeMap<Uuid, Hlc> = BTreeMap::new();
        for entry in theirs {
            // A duplicated id is not conforming. Keeping the newer is the
            // reading that cannot lose an edit.
            peer.entry(entry.id)
                .and_modify(|h| {
                    if entry.hlc > *h {
                        *h = entry.hlc;
                    }
                })
                .or_insert(entry.hlc);
        }
        let push = self
            .records
            .iter()
            .filter(|(id, record)| match peer.get(*id) {
                Some(theirs) => record.hlc > *theirs,
                None => true,
            })
            .map(|(id, _)| *id)
            .collect();

        // `self.records` is a `BTreeMap`, so the pushes are already in id order.
        Reconcile { pull, push }
    }

    /// Which of a peer's records this node wants, given their digest.
    ///
    /// Anything newer than what is held, and anything not held at all.
    pub fn wanted(&self, theirs: &[DigestEntry]) -> Vec<Uuid> {
        theirs
            .iter()
            .filter(|entry| match self.records.get(&entry.id) {
                Some(held) => entry.hlc > held.hlc,
                None => true,
            })
            .map(|entry| entry.id)
            .collect()
    }

    /// Records to send in answer to a pull.
    ///
    /// Silently skips ids this node does not hold: a peer asking for something
    /// that has since been superseded is normal, and an error would turn an
    /// ordinary race into a failure.
    pub fn collect(&self, ids: &[Uuid]) -> Vec<Record> {
        ids.iter()
            .filter_map(|id| self.records.get(id))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn uuid(n: u8) -> Uuid {
        Uuid([n; 16])
    }

    fn key(n: u8) -> [u8; PUBLIC_KEY_LEN] {
        [n; PUBLIC_KEY_LEN]
    }

    /// A verifier that accepts a signature whose first byte matches the key's.
    ///
    /// Not cryptography: it exists to prove the store consults the *right* key
    /// and rejects a mismatch, which is the store's responsibility. Whether
    /// Ed25519 is implemented correctly is Ed25519's.
    struct KeyMatches;

    impl Verifier for KeyMatches {
        fn verify(
            &self,
            public_key: &[u8; PUBLIC_KEY_LEN],
            _message: &[u8],
            signature: &[u8; SIGNATURE_LEN],
        ) -> bool {
            signature[0] == public_key[0]
        }
    }

    fn signed(id: u8, kind: RecordType, hlc: u64, author: u8, by: u8) -> Record {
        let mut signature = [0u8; SIGNATURE_LEN];
        signature[0] = by;
        Record {
            id: uuid(id),
            kind,
            hlc: Hlc(hlc),
            author: uuid(author),
            body: vec![1, 2, 3],
            signature,
        }
    }

    fn authority() -> Authority {
        let mut a = Authority::new();
        a.authorise_controller(uuid(200), key(7));
        a.register_device(uuid(1), key(11));
        a.register_device(uuid(2), key(22));
        a
    }

    // ---- HLC ---------------------------------------------------------------

    #[test]
    fn an_hlc_packs_and_unpacks() {
        let h = Hlc::new(1_700_000_000_000, 42);
        assert_eq!(h.wall_ms(), 1_700_000_000_000);
        assert_eq!(h.counter(), 42);
    }

    #[test]
    fn a_tick_advances_the_counter_when_the_clock_has_not_moved() {
        // Several edits inside one millisecond is not rare when a person drags a
        // slider, and without the counter they would be indistinguishable.
        let a = Hlc::new(1_000, 0);
        let b = a.tick(1_000);
        let c = b.tick(1_000);
        assert!(b > a);
        assert!(c > b);
        assert_eq!(c.wall_ms(), 1_000);
        assert_eq!(c.counter(), 2);
    }

    #[test]
    fn a_tick_never_goes_backwards_when_the_wall_clock_does() {
        // Wall clocks do go backwards - a device learns the real time after
        // boot. A stamp that went with it would make a new edit lose to an old.
        let a = Hlc::new(5_000, 0);
        let b = a.tick(1_000);
        assert!(b > a, "the stamp went backwards");
        assert_eq!(b.wall_ms(), 5_000);
    }

    #[test]
    fn observing_a_peer_makes_later_local_edits_sort_after_it() {
        // What makes causality hold across the mesh, whatever the two wall
        // clocks think.
        let mine = Hlc::new(1_000, 0);
        let theirs = Hlc::new(9_000, 5);
        let next = mine.observe(theirs, 1_000);
        assert!(
            next > theirs,
            "a later local edit must sort after what it saw"
        );
    }

    #[test]
    fn hlcs_order_totally() {
        let a = Hlc::new(1_000, 0);
        let b = Hlc::new(1_000, 1);
        let c = Hlc::new(1_001, 0);
        assert!(a < b && b < c);
        assert_eq!(Hlc::ZERO, Hlc::default());
    }

    // ---- record ordering ---------------------------------------------------

    #[test]
    fn the_later_record_wins() {
        let old = signed(1, RecordType::Scene, 10, 200, 7);
        let new = signed(1, RecordType::Scene, 20, 200, 7);
        assert!(new.supersedes(&old));
        assert!(!old.supersedes(&new));
    }

    #[test]
    fn an_exact_tie_breaks_on_the_author_so_two_keepers_agree() {
        // Without the tiebreak a genuine tie would resolve by arrival order, and
        // two keepers handed the same pair would diverge.
        let a = signed(1, RecordType::Scene, 10, 100, 7);
        let b = signed(1, RecordType::Scene, 10, 200, 7);
        assert!(b.supersedes(&a));
        assert!(!a.supersedes(&b));
        assert!(!a.supersedes(&a), "a record never supersedes itself");
    }

    #[test]
    fn the_signature_preimage_is_the_documented_order() {
        let r = signed(1, RecordType::Zone, 0x1122, 200, 7);
        let bytes = r.signed_bytes();
        assert_eq!(&bytes[0..16], uuid(1).as_bytes());
        assert_eq!(bytes[16], RecordType::Zone.to_u8());
        assert_eq!(&bytes[17..25], &0x1122u64.to_le_bytes());
        assert_eq!(&bytes[25..41], uuid(200).as_bytes());
        assert_eq!(&bytes[41..], &[1, 2, 3]);
        assert_eq!(
            bytes.len(),
            16 + 1 + 8 + 16 + 3,
            "the signature must not cover itself"
        );
    }

    #[test]
    fn every_record_type_maps_both_ways() {
        for t in [
            RecordType::Device,
            RecordType::Zone,
            RecordType::Effect,
            RecordType::Scene,
            RecordType::Show,
            RecordType::Schedule,
            RecordType::Binding,
            RecordType::Channel,
            RecordType::Key,
        ] {
            assert_eq!(RecordType::from_u8(t.to_u8()), Some(t));
        }
        assert_eq!(RecordType::from_u8(99), None);
    }

    #[test]
    fn only_a_device_record_is_self_signed() {
        assert!(RecordType::Device.is_self_signed());
        for t in [
            RecordType::Zone,
            RecordType::Effect,
            RecordType::Scene,
            RecordType::Show,
            RecordType::Schedule,
            RecordType::Binding,
            RecordType::Channel,
            RecordType::Key,
        ] {
            assert!(!t.is_self_signed(), "{t:?}");
        }
    }

    // ---- acceptance --------------------------------------------------------

    #[test]
    fn a_correctly_signed_controller_record_is_accepted() {
        let mut s = Store::new();
        let r = signed(10, RecordType::Scene, 5, 200, 7);
        assert_eq!(s.accept(r, &authority(), &KeyMatches, 1_000), Ok(()));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn a_forged_signature_is_rejected() {
        // The whole reason records are signed: the mesh key is symmetric, so any
        // paired device could otherwise forge a scene and have it replicate as
        // genuine.
        let mut s = Store::new();
        let r = signed(10, RecordType::Scene, 5, 200, 99);
        assert_eq!(
            s.accept(r, &authority(), &KeyMatches, 1_000),
            Err(RejectReason::BadSignature)
        );
        assert!(s.is_empty(), "a forged record must not be stored");
    }

    #[test]
    fn an_unknown_author_is_rejected() {
        let mut s = Store::new();
        let r = signed(10, RecordType::Scene, 5, 250, 7);
        assert_eq!(
            s.accept(r, &authority(), &KeyMatches, 1_000),
            Err(RejectReason::UnknownAuthor)
        );
    }

    #[test]
    fn a_device_may_sign_its_own_record_and_only_its_own() {
        // A compromised device can lie about itself and nothing else. That is a
        // far smaller blast radius than the alternative.
        let mut s = Store::new();
        let own = Record {
            id: uuid(1),
            author: uuid(1),
            ..signed(1, RecordType::Device, 5, 1, 11)
        };
        assert_eq!(s.accept(own, &authority(), &KeyMatches, 1_000), Ok(()));

        let someone_elses = Record {
            id: uuid(2),
            author: uuid(1),
            ..signed(2, RecordType::Device, 5, 1, 11)
        };
        assert_eq!(
            s.accept(someone_elses, &authority(), &KeyMatches, 1_000),
            Err(RejectReason::NotItsOwnDeviceRecord)
        );
    }

    #[test]
    fn a_device_key_cannot_sign_a_scene() {
        // Distinguished from an unknown author because the fix differs: this is
        // a tampered node, not a missing pairing.
        let mut s = Store::new();
        let r = signed(10, RecordType::Scene, 5, 1, 11);
        assert_eq!(
            s.accept(r, &authority(), &KeyMatches, 1_000),
            Err(RejectReason::WrongAuthority)
        );
    }

    #[test]
    fn a_revoked_controller_can_no_longer_write() {
        let mut a = authority();
        let mut s = Store::new();
        assert_eq!(
            s.accept(
                signed(10, RecordType::Scene, 5, 200, 7),
                &a,
                &KeyMatches,
                1_000
            ),
            Ok(())
        );
        assert!(a.revoke_controller(uuid(200)));
        assert!(!a.is_controller(uuid(200)));
        assert_eq!(
            s.accept(
                signed(11, RecordType::Scene, 6, 200, 7),
                &a,
                &KeyMatches,
                1_000
            ),
            Err(RejectReason::UnknownAuthor)
        );
        assert!(
            !a.revoke_controller(uuid(200)),
            "revoking twice says nothing new"
        );
    }

    #[test]
    fn an_older_record_is_refused_without_a_signature_check() {
        // The digest exchange compares HLCs first, so an unchanged record never
        // costs a verify - which is what makes signing every record affordable
        // on a C3. Here the signature is deliberately invalid and it still
        // reports Superseded, proving the order of the two checks.
        let mut s = Store::new();
        s.accept(
            signed(10, RecordType::Scene, 20, 200, 7),
            &authority(),
            &KeyMatches,
            0,
        )
        .unwrap();
        let stale = signed(10, RecordType::Scene, 10, 200, 99);
        assert_eq!(
            s.accept(stale, &authority(), &KeyMatches, 1_000),
            Err(RejectReason::Superseded)
        );
    }

    #[test]
    fn a_rejected_record_does_not_move_the_clock() {
        // Otherwise anyone able to send a forged record could push this node's
        // clock arbitrarily far forward and make every genuine edit lose.
        let mut s = Store::new();
        let before = s.clock();
        let forged = Record {
            hlc: Hlc::new(u64::MAX >> 20, 0),
            ..signed(10, RecordType::Scene, 0, 200, 99)
        };
        assert!(s.accept(forged, &authority(), &KeyMatches, 1_000).is_err());
        assert_eq!(s.clock(), before);
    }

    #[test]
    fn a_record_is_replaced_whole_rather_than_merged() {
        // Field-wise merging would produce an effect neither author wrote.
        let mut s = Store::new();
        let mut first = signed(10, RecordType::Effect, 10, 200, 7);
        first.body = vec![1, 1, 1];
        s.accept(first, &authority(), &KeyMatches, 0).unwrap();

        let mut second = signed(10, RecordType::Effect, 20, 200, 7);
        second.body = vec![2, 2];
        s.accept(second, &authority(), &KeyMatches, 0).unwrap();

        assert_eq!(s.get(uuid(10)).unwrap().body, vec![2, 2]);
        assert_eq!(s.len(), 1);
    }

    // ---- gossip ------------------------------------------------------------

    fn populated(entries: &[(u8, u64)]) -> Store {
        let mut s = Store::new();
        for (id, hlc) in entries {
            s.accept(
                signed(*id, RecordType::Scene, *hlc, 200, 7),
                &authority(),
                &KeyMatches,
                0,
            )
            .unwrap();
        }
        s
    }

    #[test]
    fn a_digest_lists_everything_held_in_a_stable_order() {
        // Two nodes must produce comparable digests, or a diff is a search.
        let s = populated(&[(3, 30), (1, 10), (2, 20)]);
        let d = s.digest();
        assert_eq!(d.len(), 3);
        let ids: Vec<Uuid> = d.iter().map(|e| e.id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn a_node_asks_for_what_it_lacks_and_what_is_newer() {
        let mine = populated(&[(1, 10), (2, 20)]);
        let theirs = populated(&[(1, 10), (2, 30), (3, 5)]);
        let want = mine.wanted(&theirs.digest());
        assert!(
            !want.contains(&uuid(1)),
            "identical records need no transfer"
        );
        assert!(want.contains(&uuid(2)), "a newer record should be pulled");
        assert!(want.contains(&uuid(3)), "a missing record should be pulled");
        assert_eq!(want.len(), 2);
    }

    #[test]
    fn a_node_with_everything_asks_for_nothing() {
        // Steady state must cost no transfers, or gossip is a permanent tax.
        let mine = populated(&[(1, 10), (2, 20)]);
        let theirs = populated(&[(1, 10), (2, 20)]);
        assert!(mine.wanted(&theirs.digest()).is_empty());
    }

    #[test]
    fn a_pull_for_something_no_longer_held_is_not_an_error() {
        // A peer asking for a record that has since been superseded is an
        // ordinary race; failing the exchange would turn it into an outage.
        let s = populated(&[(1, 10)]);
        let got = s.collect(&[uuid(1), uuid(9)]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, uuid(1));
    }

    #[test]
    fn two_nodes_converge_after_a_partition_heals() {
        // The scenario the design is judged on. Each edits while separated, and
        // afterwards both hold the same thing.
        let a_auth = authority();
        let mut a = populated(&[(1, 10)]);
        let mut b = populated(&[(1, 10)]);

        // Divergent edits while apart.
        a.accept(
            signed(2, RecordType::Scene, 50, 200, 7),
            &a_auth,
            &KeyMatches,
            0,
        )
        .unwrap();
        b.accept(
            signed(3, RecordType::Scene, 60, 200, 7),
            &a_auth,
            &KeyMatches,
            0,
        )
        .unwrap();
        b.accept(
            signed(1, RecordType::Scene, 70, 200, 7),
            &a_auth,
            &KeyMatches,
            0,
        )
        .unwrap();

        // One gossip round each way.
        for _ in 0..2 {
            for id in a.wanted(&b.digest()) {
                for r in b.collect(&[id]) {
                    let _ = a.accept(r, &a_auth, &KeyMatches, 0);
                }
            }
            for id in b.wanted(&a.digest()) {
                for r in a.collect(&[id]) {
                    let _ = b.accept(r, &a_auth, &KeyMatches, 0);
                }
            }
        }

        assert_eq!(a.digest(), b.digest(), "the two nodes did not converge");
        assert_eq!(a.len(), 3);
        assert_eq!(
            a.get(uuid(1)).unwrap().hlc,
            Hlc(70),
            "the later edit to record 1 should have won"
        );
    }

    #[test]
    fn convergence_does_not_depend_on_which_side_gossips_first() {
        // If it did, a partition would heal to different states depending on
        // timing, which is the failure mode that makes distributed bugs
        // irreproducible.
        let auth = authority();
        let build = |first_a: bool| {
            let mut a = populated(&[(1, 10)]);
            let mut b = populated(&[(1, 99)]);
            let order: [bool; 2] = if first_a {
                [true, false]
            } else {
                [false, true]
            };
            for a_first in order {
                if a_first {
                    for id in a.wanted(&b.digest()) {
                        for r in b.collect(&[id]) {
                            let _ = a.accept(r, &auth, &KeyMatches, 0);
                        }
                    }
                } else {
                    for id in b.wanted(&a.digest()) {
                        for r in a.collect(&[id]) {
                            let _ = b.accept(r, &auth, &KeyMatches, 0);
                        }
                    }
                }
            }
            (a.digest(), b.digest())
        };
        let (a1, b1) = build(true);
        let (a2, b2) = build(false);
        assert_eq!(a1, b1);
        assert_eq!(a2, b2);
        assert_eq!(a1, a2, "the outcome depended on gossip order");
    }

    #[test]
    fn a_forged_record_is_not_gossiped_onward() {
        // It never entered the store, so it cannot be in a digest - which is how
        // a bad record stops at the first honest node rather than spreading.
        let mut s = populated(&[(1, 10)]);
        let _ = s.accept(
            signed(2, RecordType::Scene, 20, 200, 99),
            &authority(),
            &KeyMatches,
            0,
        );
        assert_eq!(s.digest().len(), 1);
        assert!(s.collect(&[uuid(2)]).is_empty());
    }

    #[test]
    fn a_local_stamp_advances_and_never_repeats() {
        let mut s = Store::new();
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(s.stamp(1_000));
        }
        for pair in seen.windows(2) {
            assert!(pair[1] > pair[0], "a stamp repeated: {seen:?}");
        }
    }

    #[test]
    fn an_empty_store_gossips_nothing_and_wants_everything() {
        let empty = Store::new();
        assert!(empty.is_empty());
        assert!(empty.digest().is_empty());
        let theirs = populated(&[(1, 10), (2, 20)]);
        assert_eq!(empty.wanted(&theirs.digest()).len(), 2);
    }

    // ---- Reconciling a gossip round ----------------------------------------

    fn digest_of(entries: &[(u8, u64)]) -> Vec<DigestEntry> {
        entries
            .iter()
            .map(|(id, hlc)| DigestEntry {
                id: uuid(*id),
                hlc: Hlc(*hlc),
            })
            .collect()
    }

    /// A store holding exactly `entries`, at the clocks given.
    ///
    /// Built through `accept` rather than by reaching into the map, so these
    /// tests exercise a store that was populated the way a real one is.
    fn store_of(entries: &[(u8, u64)]) -> Store {
        let a = authority();
        let mut s = Store::new();
        for (id, hlc) in entries {
            // Authored by the controller and signed with its key, which is what
            // the shared `authority()` fixture authorises for a scene.
            s.accept(
                signed(*id, RecordType::Scene, *hlc, 200, 7),
                &a,
                &KeyMatches,
                *hlc,
            )
            .expect("the fixture record is acceptable");
        }
        s
    }

    #[test]
    fn two_nodes_that_agree_transfer_nothing() {
        // The common case by an enormous margin: a mesh at rest gossips every
        // five seconds and finds nothing every time.
        let mine = store_of(&[(1, 100), (2, 200)]);
        let plan = mine.reconcile(&digest_of(&[(1, 100), (2, 200)]));
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn a_record_only_the_peer_has_is_pulled() {
        let mine = store_of(&[(1, 100)]);
        let plan = mine.reconcile(&digest_of(&[(1, 100), (2, 50)]));
        assert_eq!(plan.pull, alloc::vec![uuid(2)]);
        assert!(plan.push.is_empty());
    }

    #[test]
    fn a_record_only_this_node_has_is_pushed() {
        let mine = store_of(&[(1, 100), (2, 50)]);
        let plan = mine.reconcile(&digest_of(&[(1, 100)]));
        assert_eq!(plan.push, alloc::vec![uuid(2)]);
        assert!(plan.pull.is_empty());
    }

    #[test]
    fn the_newer_clock_decides_which_way_a_record_moves() {
        let mine = store_of(&[(1, 100), (2, 100)]);
        let plan = mine.reconcile(&digest_of(&[(1, 200), (2, 50)]));
        assert_eq!(plan.pull, alloc::vec![uuid(1)]);
        assert_eq!(plan.push, alloc::vec![uuid(2)]);
    }

    #[test]
    fn a_peer_that_is_behind_is_pushed_to_without_asking() {
        // The case the push half exists for, and the one the conformance
        // vectors originally missed: a node that is behind never sends a pull,
        // so if the round did not push, that record would reach it only if it
        // happened to gossip first. Convergence would depend on who spoke.
        let mine = store_of(&[(1, 200)]);
        let plan = mine.reconcile(&digest_of(&[(1, 100)]));
        assert_eq!(plan.push, alloc::vec![uuid(1)]);
        assert!(plan.pull.is_empty());
    }

    #[test]
    fn an_equal_clock_means_the_same_record() {
        // An HLC is unique to the edit that produced it, so two records sharing
        // one are the same record. If they were not, transferring would not
        // settle which to keep - that is the conflict `Authority` resolves when
        // a record arrives, not something a digest can see.
        let mine = store_of(&[(1, 100)]);
        assert!(mine.reconcile(&digest_of(&[(1, 100)])).is_empty());
    }

    #[test]
    fn a_node_that_has_just_joined_pulls_everything() {
        let mine = Store::new();
        let plan = mine.reconcile(&digest_of(&[(3, 30), (1, 10), (2, 20)]));
        assert_eq!(plan.pull, alloc::vec![uuid(1), uuid(2), uuid(3)]);
        assert!(plan.push.is_empty());
    }

    #[test]
    fn a_plan_is_ordered_so_two_nodes_produce_the_same_one() {
        // Reproducibility, which is what makes a gossip round testable and a
        // recorded run replayable. A peer is not obliged by anything in the
        // protocol to send its digest in order.
        let mine = store_of(&[(5, 10), (1, 10), (9, 10)]);
        let plan = mine.reconcile(&digest_of(&[(8, 10), (2, 10), (4, 10)]));
        assert_eq!(plan.push, alloc::vec![uuid(1), uuid(5), uuid(9)]);
        assert_eq!(plan.pull, alloc::vec![uuid(2), uuid(4), uuid(8)]);
    }

    #[test]
    fn a_duplicated_id_in_a_digest_keeps_the_newer_clock() {
        // Not conforming, but the reading that cannot lose an edit: treating the
        // older one as authoritative would leave this node believing it is up to
        // date when it is not.
        let mine = store_of(&[(1, 100)]);
        assert_eq!(
            mine.reconcile(&digest_of(&[(1, 50), (1, 200)])).pull,
            alloc::vec![uuid(1)]
        );
        assert!(mine.reconcile(&digest_of(&[(1, 200), (1, 50)])).pull.len() == 1);
    }

    #[test]
    fn an_empty_digest_from_a_peer_pushes_everything() {
        let mine = store_of(&[(1, 10), (2, 20)]);
        let plan = mine.reconcile(&[]);
        assert_eq!(plan.push, alloc::vec![uuid(1), uuid(2)]);
        assert!(plan.pull.is_empty());
    }

    #[test]
    fn two_empty_stores_have_nothing_to_say() {
        assert!(Store::new().reconcile(&[]).is_empty());
    }
}
