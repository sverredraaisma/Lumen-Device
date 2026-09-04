//! The two state machines, wired to the wire.
//!
//! [`Node`] is the sans-IO core a shell drives: events in, actions out, with the
//! codec in between. It owns the parts of the protocol that are decisions —
//! which peer to probe, when to stand for election, whether the clock can be
//! trusted — and none of the parts that are I/O.
//!
//! # Datagrams are built here, not in the shell
//!
//! An [`Action::Send`] carries finished bytes. The alternative — handing the
//! shell a struct and letting it encode — puts the wire format on the far side
//! of the boundary, where every implementation would have to reimplement it and
//! the conformance vectors could not check it. Events in, *bytes* out.
//!
//! # Not yet authenticated
//!
//! The AEAD tag is written as zeroes. `lumen-proto`'s `crypto` module defines
//! exactly which bytes it covers and [`Node`] leaves a single call site for it,
//! so W14 is a substitution rather than a redesign. Until then a mesh is trusted
//! at the network layer, which is fine for a simulator and not fine for a room.

use alloc::vec;
use alloc::vec::Vec;

use lumen_proto::header::{Header, MsgType, HEADER_LEN, TAG_LEN};
use lumen_proto::msg::{SyncReq, SyncResp, Tick, WallQuality};
use lumen_proto::{Datagram, Payload, Uuid, Writer};

use crate::election::{self, Election, Outcome as ElectionOutcome, Role};
use crate::sync::{self, Sample, Sync};
use crate::{Action, Destination, Event, Identity, Transport};

/// Largest payload this node builds. Every message it sends is far smaller; the
/// buffer is stack-allocated, so the bound is what keeps it there.
const MAX_PAYLOAD: usize = 128;

/// How long an unanswered probe blocks the next one.
///
/// A round trip on a local network is milliseconds, so a second is generous and
/// still fast enough that a device losing one packet is briefly rather than
/// permanently unsynchronised.
const PROBE_TIMEOUT_US: u64 = 1_000_000;

/// A mesh participant: election, time sync, and the codec between them.
pub struct Node {
    me: Identity,
    mesh_id: Uuid,
    election: Election,
    sync: Sync,
    /// Increments on every datagram sent, per boot. Half the AEAD nonce.
    sequence: u32,
    boot_counter: u32,
    /// The peer currently believed to hold the timebase.
    master_prefix: Option<[u8; 4]>,
    /// `t1` of the outstanding `SYNC_REQ`, if any.
    ///
    /// One at a time: a second request in flight would make the responses
    /// ambiguous, and the exchange is cheap enough that pipelining buys nothing.
    pending_probe_t1: Option<u64>,
    /// When to give up on the outstanding probe and allow another.
    ///
    /// Without this, one lost datagram stops time sync for ever. The probe is
    /// one-at-a-time — a second in flight makes responses ambiguous — and the
    /// flag was cleared only by a matching answer, so a request that never
    /// arrived, an answer that never came back, or an answer arriving after its
    /// question had been forgotten all left the flag set and every later probe
    /// refused.
    ///
    /// It is not a hypothetical. A C3 and a desktop peer exchanged 94 round
    /// trips, lost one, and the device then sat as an unsynchronised follower
    /// indefinitely while ticks kept arriving once a second. A lossless
    /// simulated network never shows it.
    probe_deadline_us: u64,
}

impl Node {
    pub fn new(me: Identity, mesh_id: Uuid, boot_counter: u32, now_us: u64) -> Node {
        Node {
            election: Election::new(me.candidacy(), now_us),
            me,
            mesh_id,
            sync: Sync::new(),
            sequence: 0,
            boot_counter,
            master_prefix: None,
            pending_probe_t1: None,
            probe_deadline_us: u64::MAX,
        }
    }

    pub fn role(&self) -> Role {
        self.election.role()
    }

    pub fn is_synced(&self) -> bool {
        // A leader defines the timebase, so it is synced by definition. Without
        // this it would sit Unsynced forever waiting for a tick it is the one
        // sending, and suppress exactly the content it exists to drive.
        self.election.is_leader() || self.sync.is_synced()
    }

    pub fn epoch(&self) -> u32 {
        self.election.epoch()
    }

    /// The sans-IO contract.
    pub fn on_event(&mut self, now_us: u64, ev: Event<'_>) -> Vec<Action> {
        let mut out = Vec::new();
        match ev {
            Event::Tick => self.on_timer(now_us, &mut out),
            Event::Datagram { bytes } => self.on_datagram(now_us, bytes, &mut out),
            Event::PeerDiscovered { .. } | Event::PeerLost { .. } => {
                // Nothing to decide. Discovery tells the shell where to send;
                // the core learns who exists from the traffic itself, which is
                // the only source that cannot be stale.
            }
        }
        out.push(Action::SetTimer {
            in_us: self.next_deadline_us(now_us),
        });
        out
    }

    fn next_deadline_us(&self, now_us: u64) -> u64 {
        // The outstanding probe's timeout counts: a node that only woke for the
        // election and the resync interval would take thirty seconds to notice
        // a lost probe, and would spend them telling anyone who asked that it
        // was unsynchronised.
        let probe = self.probe_deadline_us.saturating_sub(now_us);
        self.election
            .next_deadline_us(now_us)
            .min(self.sync.next_deadline_us(now_us))
            .min(probe)
            // Never zero: a shell that honoured a zero delay would spin.
            .max(1_000)
    }

    fn on_timer(&mut self, now_us: u64, out: &mut Vec<Action>) {
        let before = self.is_synced();

        // Give up on a probe nobody answered, so the next one may go out. A
        // round trip on a local network is milliseconds; a second means the
        // request or the answer is gone.
        if now_us >= self.probe_deadline_us {
            self.pending_probe_t1 = None;
            self.probe_deadline_us = u64::MAX;
        }

        match self.election.on_timer(now_us) {
            ElectionOutcome::Announce | ElectionOutcome::SendTick => self.send_tick(now_us, out),
            ElectionOutcome::Became(role) => {
                out.push(Action::RoleChanged {
                    role,
                    epoch: self.election.epoch(),
                });
                if role == Role::Leader {
                    self.send_tick(now_us, out);
                }
            }
            ElectionOutcome::Idle => {}
        }

        match self.sync.on_timer(now_us) {
            sync::Outcome::Probe => self.send_sync_req(now_us, out),
            sync::Outcome::Discipline(offset_us) => out.push(Action::DisciplineClock { offset_us }),
            sync::Outcome::Lost | sync::Outcome::Acquired | sync::Outcome::Idle => {}
        }

        self.announce_sync_change(before, out);
    }

    fn on_datagram(&mut self, now_us: u64, bytes: &[u8], out: &mut Vec<Action>) {
        let Ok(dg) = Datagram::decode(bytes) else {
            // Rubbish is dropped in silence. Replying would make the node an
            // amplifier for anyone who can send a malformed packet.
            return;
        };
        if dg.header.mesh_prefix != self.mesh_id.mesh_prefix() {
            // Another mesh on the same LAN. Two bytes, no decrypt.
            return;
        }
        if dg.header.sender_prefix == self.me.prefix() {
            // Our own multicast, looped back.
            return;
        }
        let Ok(Some(payload)) = dg.parse_payload() else {
            // An unknown type is ignored, not rejected: that is what makes
            // minor-version additions safe.
            return;
        };

        let before = self.is_synced();
        match payload {
            Payload::Tick(tick) => self.on_tick_message(now_us, &dg.header, tick, out),
            Payload::SyncReq(req) => self.on_sync_req(now_us, &dg.header, req, out),
            Payload::SyncResp(resp) => self.on_sync_resp(now_us, resp, out),
            _ => {}
        }
        self.announce_sync_change(before, out);
    }

    fn on_tick_message(&mut self, now_us: u64, header: &Header, tick: Tick, out: &mut Vec<Action>) {
        let who = election::Candidacy {
            capacity: tick.master_capacity,
            uuid: tick.master_uuid,
        };
        // Probe whoever is actually leading. Following a stale master is how a
        // node ends up synced to a device that left the mesh.
        self.master_prefix = Some(header.sender_prefix);

        match self.election.on_tick(now_us, who, tick.election_epoch) {
            ElectionOutcome::Became(role) => {
                out.push(Action::RoleChanged {
                    role,
                    epoch: self.election.epoch(),
                });
            }
            ElectionOutcome::Announce | ElectionOutcome::SendTick => self.send_tick(now_us, out),
            ElectionOutcome::Idle => {}
        }

        // A leader does not sync to anyone; it is the reference.
        if self.election.is_leader() {
            return;
        }
        if let sync::Outcome::Probe = self.sync.on_tick(now_us) {
            self.send_sync_req(now_us, out);
        }
    }

    fn on_sync_req(&mut self, now_us: u64, header: &Header, req: SyncReq, out: &mut Vec<Action>) {
        // Anyone answers, not just the leader. The exchange measures a path, and
        // refusing to answer while unsynced would stop a mesh converging at all
        // during the window where every node is still unsynced.
        let payload = SyncResp {
            t1: req.t1,
            t2: now_us,
            t3: now_us,
        };
        self.send(
            now_us,
            MsgType::SyncResp,
            Destination::Peer(header.sender_prefix),
            |w| payload.encode(w),
            out,
        );
    }

    fn on_sync_resp(&mut self, now_us: u64, resp: SyncResp, out: &mut Vec<Action>) {
        let Some(t1) = self.pending_probe_t1 else {
            // Nothing outstanding. A response to a request we did not send is
            // either a duplicate or someone else's; either way it measures
            // nothing about our path.
            return;
        };
        if resp.t1 != t1 {
            return;
        }
        self.pending_probe_t1 = None;
        self.probe_deadline_us = u64::MAX;

        let sample = Sample {
            t1: resp.t1,
            t2: resp.t2,
            t3: resp.t3,
            t4: now_us,
        };
        match self.sync.on_sample(now_us, sample) {
            sync::Outcome::Probe => self.send_sync_req(now_us, out),
            sync::Outcome::Discipline(offset_us) => {
                out.push(Action::DisciplineClock { offset_us });
            }
            sync::Outcome::Acquired | sync::Outcome::Lost | sync::Outcome::Idle => {}
        }
    }

    fn announce_sync_change(&mut self, was_synced: bool, out: &mut Vec<Action>) {
        let now = self.is_synced();
        if now == was_synced {
            return;
        }
        out.push(if now {
            Action::SyncAcquired
        } else {
            Action::SyncLost
        });
    }

    fn send_tick(&mut self, now_us: u64, out: &mut Vec<Action>) {
        let payload = Tick {
            show_time_us: now_us,
            master_uuid: self.me.uuid,
            master_capacity: self.me.capacity,
            election_epoch: self.election.epoch(),
            // Wall time is a separate, optional concern. Claiming a quality this
            // node does not have would make schedules fire at a plausible-looking
            // wrong moment instead of degrading visibly.
            wall_time_us: 0,
            wall_quality: WallQuality::None,
        };
        self.send(
            now_us,
            MsgType::Tick,
            Destination::Mesh,
            |w| payload.encode(w),
            out,
        );
    }

    fn send_sync_req(&mut self, now_us: u64, out: &mut Vec<Action>) {
        let Some(to) = self.master_prefix else {
            // Nobody to ask yet.
            return;
        };
        if self.pending_probe_t1.is_some() {
            // One at a time: a second in flight makes responses ambiguous.
            return;
        }
        self.pending_probe_t1 = Some(now_us);
        self.probe_deadline_us = now_us.saturating_add(PROBE_TIMEOUT_US);
        let payload = SyncReq { t1: now_us };
        self.send(
            now_us,
            MsgType::SyncReq,
            Destination::Peer(to),
            |p| payload.encode(p),
            out,
        );
    }

    /// Frame a payload into a datagram and queue it.
    ///
    /// The one place a header is built, so `sequence`, `mesh_prefix` and
    /// `show_time_us` cannot be forgotten on some path — which is exactly the
    /// bug that would show up as one message type being silently dropped by
    /// every peer.
    fn send<F>(
        &mut self,
        now_us: u64,
        msg_type: MsgType,
        to: Destination,
        encode: F,
        out: &mut Vec<Action>,
    ) where
        F: FnOnce(&mut Writer<'_>) -> Result<(), lumen_proto::EncodeError>,
    {
        let mut body = [0u8; MAX_PAYLOAD];
        let len = {
            let mut w = Writer::new(&mut body);
            if encode(&mut w).is_err() {
                // A payload this node built does not fit a buffer this node
                // sized. That is a bug here, not a condition to handle, and
                // sending a truncated datagram would be worse than sending none.
                return;
            }
            w.position()
        };

        self.sequence = self.sequence.wrapping_add(1);
        let mut header = Header::new(
            msg_type,
            self.mesh_id.mesh_prefix(),
            self.me.prefix(),
            self.sequence,
            now_us,
        );
        header.payload_len = len as u16;

        // The tag is zeroes until W14 substitutes a real AEAD here.
        let tag = [0u8; TAG_LEN];
        let dg = Datagram {
            header,
            payload: &body[..len],
            tag: &tag,
        };
        let mut framed = vec![0u8; HEADER_LEN + len + TAG_LEN];
        if dg.encode(&mut framed).is_err() {
            return;
        }
        let _ = self.boot_counter;
        out.push(Action::Send {
            to,
            datagram: framed,
            // Derived from the message rather than passed in, so a caller
            // cannot send a record unreliably by forgetting to say otherwise —
            // which would replicate nothing and show up much later as two
            // devices disagreeing about a scene.
            transport: transport_for(msg_type),
        });
    }
}

/// Which transport a message type requires.
///
/// The table in `wire-format.md`, in code. Everything that is replaced on a
/// schedule goes best-effort — another tick or frame is along in milliseconds,
/// and a retransmission would arrive after the moment it described. Everything
/// that is said once has to arrive.
fn transport_for(msg_type: MsgType) -> Transport {
    match msg_type {
        MsgType::Tick
        | MsgType::SyncReq
        | MsgType::SyncResp
        | MsgType::Chan
        | MsgType::Frame
        | MsgType::ProbeData => Transport::Datagram,
        _ => Transport::Reliable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_replaced_on_a_schedule_goes_best_effort() {
        // A retransmitted tick describes a moment that has passed, and a
        // retransmitted frame arrives after the one that replaced it. Paying
        // for delivery here would make the hot path slower and the result
        // later.
        for t in [
            MsgType::Tick,
            MsgType::SyncReq,
            MsgType::SyncResp,
            MsgType::Chan,
            MsgType::Frame,
            MsgType::ProbeData,
        ] {
            assert_eq!(transport_for(t), Transport::Datagram, "{t:?}");
        }
    }

    #[test]
    fn everything_said_once_has_to_arrive() {
        // A `STATE_PUSH` sent unreliably is a record that silently does not
        // replicate, and that surfaces much later as two devices disagreeing
        // about a scene with nothing to point at.
        for t in [
            MsgType::StateDigest,
            MsgType::StatePull,
            MsgType::StatePush,
            MsgType::Activate,
            MsgType::SrcPush,
            MsgType::ChanClaim,
            MsgType::ProgBegin,
            MsgType::ProgChunk,
            MsgType::ProgEnd,
            MsgType::Hello,
        ] {
            assert_eq!(transport_for(t), Transport::Reliable, "{t:?}");
        }
    }

    #[test]
    fn a_tick_this_node_sends_is_addressed_to_the_mesh_and_unreliable() {
        // The two halves of the routing decision, on the one message every node
        // sends without being asked.
        let mut a = node(1000, 0x11, 0);
        // Long enough with no better candidate that it takes the timebase.
        let mut sent = None;
        for at in [1_000_000u64, 4_000_000, 7_000_000] {
            for action in a.on_event(at, Event::Tick) {
                if let Action::Send { to, transport, .. } = action {
                    sent = Some((to, transport));
                }
            }
        }
        assert_eq!(sent, Some((Destination::Mesh, Transport::Datagram)));
    }

    const MESH: Uuid = Uuid([0xAB; 16]);

    fn node(capacity: u32, first: u8, now_us: u64) -> Node {
        let mut bytes = [0u8; 16];
        bytes[0] = first;
        bytes[1] = first;
        Node::new(Identity::new(Uuid(bytes), capacity), MESH, 1, now_us)
    }

    fn sent(actions: &[Action]) -> Vec<&Vec<u8>> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send { datagram, .. } => Some(datagram),
                _ => None,
            })
            .collect()
    }

    fn kind_of(datagram: &[u8]) -> Option<MsgType> {
        Datagram::decode(datagram).ok()?.header.typed()
    }

    #[test]
    fn a_lone_node_elects_itself_and_starts_ticking() {
        let mut n = node(100, 1, 0);
        assert_eq!(n.role(), Role::Follower);

        // Stand for election.
        let a = n.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        assert_eq!(sent(&a).len(), 1, "should announce its candidacy");
        assert_eq!(kind_of(sent(&a)[0]), Some(MsgType::Tick));

        // Win it.
        let t = election::FOLLOWER_TIMEOUT_US + election::CANDIDATE_SETTLE_US;
        let b = n.on_event(t, Event::Tick);
        assert_eq!(n.role(), Role::Leader);
        assert!(b.contains(&Action::RoleChanged {
            role: Role::Leader,
            epoch: 1
        }));
        assert!(
            b.contains(&Action::SyncAcquired),
            "a leader is its own timebase"
        );
        assert!(n.is_synced());
    }

    #[test]
    fn every_event_asks_to_be_woken_again() {
        // A core that stopped asking would simply stop, and the shell has no way
        // to know it should have.
        let mut n = node(100, 1, 0);
        for ev in [Event::Tick, Event::PeerDiscovered { prefix: [9; 4] }] {
            let a = n.on_event(1, ev);
            assert!(
                a.iter().any(|x| matches!(x, Action::SetTimer { .. })),
                "no timer after {ev:?}"
            );
        }
    }

    #[test]
    fn the_timer_is_never_zero() {
        // A shell that honoured a zero delay would spin at whatever rate its
        // event loop allows, which on a device is a flat battery.
        let mut n = node(100, 1, 0);
        for now in [0u64, 1, 10_000_000, u64::MAX / 2] {
            let a = n.on_event(now, Event::Tick);
            for act in &a {
                if let Action::SetTimer { in_us } = act {
                    assert!(*in_us >= 1_000, "asked to be woken in {in_us}us");
                }
            }
        }
    }

    #[test]
    fn rubbish_is_dropped_in_silence() {
        // Replying would make the node an amplifier for anyone who can send a
        // malformed packet.
        let mut n = node(100, 1, 0);
        for bytes in [&[][..], &[0xFF; 4][..], &[0x4C; 40][..]] {
            let a = n.on_event(1, Event::Datagram { bytes });
            assert!(sent(&a).is_empty(), "replied to {bytes:?}");
        }
    }

    #[test]
    fn another_meshs_traffic_is_ignored() {
        // Two meshes on one LAN is the normal case in a block of flats.
        let mut leader = node(200, 2, 0);
        let a = leader.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let tick = sent(&a)[0].clone();

        let mut other = Node::new(Identity::new(Uuid([7; 16]), 100), Uuid([0x11; 16]), 1, 0);
        let b = other.on_event(1, Event::Datagram { bytes: &tick });
        assert!(sent(&b).is_empty());
        // And it still stands for election on its own schedule, because as far
        // as it is concerned nothing was heard.
        let c = other.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        assert_eq!(sent(&c).len(), 1);
    }

    #[test]
    fn a_node_ignores_its_own_looped_back_multicast() {
        let mut n = node(100, 1, 0);
        let a = n.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let own = sent(&a)[0].clone();
        let b = n.on_event(
            election::FOLLOWER_TIMEOUT_US + 1,
            Event::Datagram { bytes: &own },
        );
        assert!(sent(&b).is_empty());
        // Crucially it did not reset its own election timer.
        let c = n.on_event(
            election::FOLLOWER_TIMEOUT_US + election::CANDIDATE_SETTLE_US,
            Event::Tick,
        );
        assert!(c.contains(&Action::RoleChanged {
            role: Role::Leader,
            epoch: 1
        }));
    }

    #[test]
    fn a_follower_probes_the_leader_and_answers_probes_itself() {
        let mut leader = node(200, 2, 0);
        let a = leader.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let leader_tick = sent(&a)[0].clone();

        let mut follower = node(100, 3, 0);
        let b = follower.on_event(
            1_000,
            Event::Datagram {
                bytes: &leader_tick,
            },
        );
        let probes = sent(&b);
        assert_eq!(probes.len(), 1, "a tick should start a sync exchange");
        assert_eq!(kind_of(probes[0]), Some(MsgType::SyncReq));

        // The leader answers it.
        let req = probes[0].clone();
        let c = leader.on_event(1_100, Event::Datagram { bytes: &req });
        let resp = sent(&c);
        assert_eq!(resp.len(), 1);
        assert_eq!(kind_of(resp[0]), Some(MsgType::SyncResp));
    }

    #[test]
    fn a_response_to_a_probe_nobody_sent_is_ignored() {
        // Either a duplicate or someone else's. Feeding it in would measure a
        // path this node never used, and the offset it implies is meaningless.
        let mut leader = node(200, 2, 0);
        let a = leader.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let tick = sent(&a)[0].clone();

        let mut follower = node(100, 3, 0);
        follower.on_event(1_000, Event::Datagram { bytes: &tick });

        // Build a response carrying a t1 the follower never sent.
        let mut stranger = node(150, 4, 0);
        let mut built = Vec::new();
        stranger.send(
            2_000,
            MsgType::SyncResp,
            Destination::Mesh,
            |w| {
                SyncResp {
                    t1: 999_999,
                    t2: 1,
                    t3: 2,
                }
                .encode(w)
            },
            &mut built,
        );
        let bogus = match &built[0] {
            Action::Send { datagram, .. } => datagram.clone(),
            other => panic!("{other:?}"),
        };

        let c = follower.on_event(2_100, Event::Datagram { bytes: &bogus });
        assert!(
            sent(&c).is_empty(),
            "a mismatched response must not advance the exchange"
        );
    }

    #[test]
    fn a_probe_nobody_answers_does_not_stop_every_later_one() {
        // Found on hardware. The probe is one-at-a-time, and the flag saying one
        // is outstanding was cleared only by a matching answer - so a single
        // lost datagram stopped time sync for ever. A C3 and a desktop peer
        // exchanged 94 round trips, lost one, and the device then sat as an
        // unsynchronised follower indefinitely while ticks kept arriving once a
        // second.
        //
        // A lossless simulated network never shows this, which is why it
        // survived to reach a strip.
        let mut leader = node(200, 2, 0);
        let a = leader.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let tick = sent(&a)[0].clone();

        let mut follower = node(100, 3, 0);
        let b = follower.on_event(1_000, Event::Datagram { bytes: &tick });
        assert_eq!(sent(&b).len(), 1, "the first probe should go out");

        // Nothing answers it. Time passes - with the leader still ticking, so
        // the election cannot interfere - and the node must try again rather
        // than wait for a response that is never coming.
        //
        // Counting *probes* rather than datagrams, because a follower that
        // times out its leader stands for election and sends a TICK. An earlier
        // version of this test counted anything sent, passed on that TICK, and
        // went on passing with the timeout deleted.
        let mut asked_again = false;
        let mut now = 1_000;
        for _ in 0..40 {
            now += PROBE_TIMEOUT_US / 4;
            // Keep the leader alive, so this stays a follower throughout.
            let keep = leader.on_event(now, Event::Tick);
            for datagram in sent(&keep) {
                follower.on_event(now, Event::Datagram { bytes: datagram });
            }
            let out = follower.on_event(now, Event::Tick);
            if sent(&out).iter().any(|d| d[2] == MsgType::SyncReq.to_u8()) {
                asked_again = true;
                break;
            }
        }
        assert!(
            asked_again,
            "one lost probe silenced every later one - sync is deadlocked"
        );
    }

    #[test]
    fn the_timer_accounts_for_an_outstanding_probe() {
        // A node that only woke for the election and the resync interval would
        // take thirty seconds to notice a lost probe, and would spend them
        // telling anyone who asked that it was unsynchronised.
        let mut leader = node(200, 2, 0);
        let a = leader.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let tick = sent(&a)[0].clone();

        let mut follower = node(100, 3, 0);
        let b = follower.on_event(1_000, Event::Datagram { bytes: &tick });
        let timer = b
            .iter()
            .find_map(|x| match x {
                Action::SetTimer { in_us } => Some(*in_us),
                _ => None,
            })
            .expect("every event asks to be woken again");
        assert!(
            timer <= PROBE_TIMEOUT_US,
            "asked to sleep {timer} us with a probe outstanding for {PROBE_TIMEOUT_US}"
        );
    }

    #[test]
    fn a_leader_does_not_sync_to_anyone() {
        // It is the reference. A leader that probed a follower would be
        // disciplining itself towards a clock derived from its own.
        let mut leader = node(200, 2, 0);
        leader.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let t = election::FOLLOWER_TIMEOUT_US + election::CANDIDATE_SETTLE_US;
        leader.on_event(t, Event::Tick);
        assert_eq!(leader.role(), Role::Leader);

        // A worse peer announces itself.
        let mut weak = node(1, 9, 0);
        let a = weak.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let weak_tick = sent(&a)[0].clone();

        let b = leader.on_event(t + 1, Event::Datagram { bytes: &weak_tick });
        let out = sent(&b);
        assert!(
            out.iter().all(|d| kind_of(d) != Some(MsgType::SyncReq)),
            "a leader must not probe"
        );
        assert_eq!(leader.role(), Role::Leader);
    }

    #[test]
    fn the_weaker_of_two_candidates_stands_down() {
        let mut strong = node(200, 1, 0);
        let mut weak = node(100, 2, 0);

        let s = strong.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let w = weak.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let (s_tick, w_tick) = (sent(&s)[0].clone(), sent(&w)[0].clone());

        weak.on_event(
            election::FOLLOWER_TIMEOUT_US,
            Event::Datagram { bytes: &s_tick },
        );
        strong.on_event(
            election::FOLLOWER_TIMEOUT_US,
            Event::Datagram { bytes: &w_tick },
        );

        let t = election::FOLLOWER_TIMEOUT_US + election::CANDIDATE_SETTLE_US;
        strong.on_event(t, Event::Tick);
        weak.on_event(t, Event::Tick);

        assert_eq!(strong.role(), Role::Leader);
        assert_ne!(weak.role(), Role::Leader);
    }

    #[test]
    fn sequence_numbers_advance_on_every_datagram() {
        // Half the AEAD nonce. A repeated sequence within one boot is nonce
        // reuse under the mesh key.
        let mut n = node(100, 1, 0);
        let mut seen = Vec::new();
        for step in 1..6u64 {
            let a = n.on_event(step * election::FOLLOWER_TIMEOUT_US, Event::Tick);
            for d in sent(&a) {
                seen.push(Datagram::decode(d).unwrap().header.sequence);
            }
        }
        assert!(seen.len() >= 2, "expected several datagrams");
        for pair in seen.windows(2) {
            assert!(
                pair[1] > pair[0],
                "sequence went {} -> {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn every_datagram_carries_the_mesh_prefix_and_this_senders_prefix() {
        let mut n = node(100, 5, 0);
        let a = n.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let d = Datagram::decode(sent(&a)[0]).unwrap();
        assert_eq!(d.header.mesh_prefix, MESH.mesh_prefix());
        assert_eq!(d.header.sender_prefix, n.me.prefix());
    }

    #[test]
    fn a_tick_claims_no_wall_clock_quality_it_does_not_have() {
        // Claiming one would make schedules fire at a plausible-looking wrong
        // moment instead of degrading visibly.
        let mut n = node(100, 1, 0);
        let a = n.on_event(election::FOLLOWER_TIMEOUT_US, Event::Tick);
        let d = Datagram::decode(sent(&a)[0]).unwrap();
        match d.parse_payload().unwrap() {
            Some(Payload::Tick(t)) => {
                assert_eq!(t.wall_quality, WallQuality::None);
                assert_eq!(t.wall_time_us, 0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unknown_message_type_is_ignored_rather_than_answered() {
        let mut n = node(100, 1, 0);
        // A well-formed datagram from this mesh, with a type from the vendor
        // range the spec never assigns.
        let mut header = Header::new(MsgType::Tick, MESH.mesh_prefix(), [9, 9, 9, 9], 1, 0);
        header.msg_type = 0xF7;
        header.payload_len = 0;
        let tag = [0u8; TAG_LEN];
        let dg = Datagram {
            header,
            payload: &[],
            tag: &tag,
        };
        let mut buf = [0u8; HEADER_LEN + TAG_LEN];
        let n_bytes = dg.encode(&mut buf).unwrap();
        let a = n.on_event(
            1,
            Event::Datagram {
                bytes: &buf[..n_bytes],
            },
        );
        assert!(sent(&a).is_empty());
    }
}
