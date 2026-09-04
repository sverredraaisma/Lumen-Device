//! Channels: the broadcast uniforms an effect reads.
//!
//! A channel is how the things that **cannot** be a pure function of position
//! and time — audio, sensors, simulations, external state — reach a running
//! program. Everything else is computed on-device from the show clock; a channel
//! is the small, explicit exception, and its cost is one packet per frame
//! rather than one per pixel.
//!
//! # Ownership is claim-and-lease
//!
//! ```text
//! Unowned  | CHAN_CLAIM                          | Owned(claimant)
//! Owned(a) | CHAN_CLAIM from b, prio(b) > prio(a)| Owned(b); a stops immediately
//! Owned(a) | CHAN_CLAIM from b, prio(b) <= prio(a)| Owned(a)
//! Owned(a) | lease lapsed, or CHAN_RELEASE       | Unowned
//! ```
//!
//! **Strictly greater priority preempts; equal priority does not.** That is what
//! stops two identical producers — two microphones, two copies of the desktop
//! app — fighting over a channel forever, each preempting the other on every
//! frame. The lease is what makes a crashed producer release it without anyone
//! noticing it crashed.
//!
//! # Staleness is defined, not incidental
//!
//! A consumer decays a channel toward its declared default over `hold_ms` once
//! the value stops arriving. So a dead audio source fades the lights to steady
//! rather than freezing them mid-beat — the defined visual outcome that
//! "a device is never dark because of software" requires.
//!
//! **`hold 0` means never stale.** Right for a value pushed only on change — a
//! scrolling message, a mode selector — where treating silence as failure would
//! be exactly wrong.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::Uniforms;

/// A producer, identified by the prefix that appears in a datagram header.
pub type ProducerId = [u8; 4];

/// Who holds a channel, and until when.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Owner {
    pub producer: ProducerId,
    pub priority: u8,
    /// Show time at which the lease lapses.
    pub lease_until_us: u64,
    /// Highest sequence seen from this owner, or `None` before its first
    /// packet.
    ///
    /// Latest-wins with hold: a `CHAN` older than the newest seen from the
    /// *current* owner is dropped. `None` rather than zero, and cleared on a
    /// change of owner: two producers' counters are unrelated, and a producer
    /// whose counter happens to start high would otherwise fail the wraparound
    /// window and be unable to publish at all.
    pub last_seq: Option<u16>,
}

/// What happened to a claim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClaimOutcome {
    /// The channel was free.
    Taken,
    /// The previous owner was outranked and must stop immediately.
    Preempted { previous: ProducerId },
    /// Refused: the holder ranks at least as high.
    Refused { holder: ProducerId },
    /// The current owner renewed its own lease.
    Renewed,
}

/// One channel's declared shape and current value.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: u16,
    /// How long a value stays fresh. Zero means never stale.
    pub hold_ms: u32,
    /// What the value decays to when the producer dies.
    ///
    /// Not zero by default: a channel that decays to zero turns the lights off,
    /// and "off" is rarely the sensible resting state for a room.
    pub default: Q16,
    owner: Option<Owner>,
    value: Q16,
    /// When the value last arrived.
    updated_at_us: Option<u64>,
}

impl Channel {
    pub fn new(id: u16, hold_ms: u32, default: Q16) -> Channel {
        Channel {
            id,
            hold_ms,
            default,
            owner: None,
            value: default,
            updated_at_us: None,
        }
    }

    pub fn owner(&self) -> Option<Owner> {
        self.owner
    }

    pub fn is_owned(&self) -> bool {
        self.owner.is_some()
    }

    /// Whether staleness applies at all.
    pub fn never_stale(&self) -> bool {
        self.hold_ms == 0
    }

    /// A producer claims the channel.
    pub fn claim(
        &mut self,
        now_us: u64,
        producer: ProducerId,
        priority: u8,
        lease_ms: u32,
    ) -> ClaimOutcome {
        let lease_until_us = now_us.saturating_add((lease_ms as u64).saturating_mul(1_000));
        match self.owner {
            None => {
                self.take(producer, priority, lease_until_us);
                ClaimOutcome::Taken
            }
            Some(held) if held.producer == producer => {
                // The ordinary case on every frame: the owner renewing. Keeping
                // the sequence is what stops a renewal reopening the channel to
                // replayed packets.
                self.owner = Some(Owner {
                    priority,
                    lease_until_us,
                    ..held
                });
                ClaimOutcome::Renewed
            }
            Some(held) if priority > held.priority => {
                self.take(producer, priority, lease_until_us);
                ClaimOutcome::Preempted {
                    previous: held.producer,
                }
            }
            Some(held) => ClaimOutcome::Refused {
                holder: held.producer,
            },
        }
    }

    /// A producer releases the channel.
    ///
    /// Only its owner can. A release from anyone else is ignored rather than
    /// honoured: otherwise any device on the mesh could knock the desktop app
    /// off the audio channel with one packet.
    pub fn release(&mut self, producer: ProducerId) -> bool {
        match self.owner {
            Some(held) if held.producer == producer => {
                self.owner = None;
                true
            }
            _ => false,
        }
    }

    /// A value arrived.
    ///
    /// Returns whether it was taken. Refused if the sender does not own the
    /// channel, or if the sequence is not newer than the last seen — latest
    /// wins, and an out-of-order datagram must not undo a fresher one.
    pub fn publish(&mut self, now_us: u64, producer: ProducerId, seq: u16, value: Q16) -> bool {
        let Some(held) = self.owner else {
            return false;
        };
        if held.producer != producer {
            return false;
        }
        // Wrapping comparison: a u16 counter wraps after 65 536 frames, which at
        // 60 Hz is about eighteen minutes. Comparing plainly would stall the
        // channel there until the producer restarted.
        if let Some(last) = held.last_seq {
            if !is_newer(seq, last) {
                return false;
            }
        }
        self.owner = Some(Owner {
            last_seq: Some(seq),
            ..held
        });
        self.value = value;
        self.updated_at_us = Some(now_us);
        true
    }

    /// The value an effect should read at `now_us`.
    ///
    /// Decays toward the default once the value goes stale, so a dead audio
    /// source fades the lights to steady rather than freezing them mid-beat.
    pub fn read(&self, now_us: u64) -> Q16 {
        if self.never_stale() {
            return self.value;
        }
        let Some(updated) = self.updated_at_us else {
            return self.default;
        };
        let hold_us = (self.hold_ms as u64).saturating_mul(1_000);
        let age = now_us.saturating_sub(updated);
        if age >= hold_us.saturating_mul(2) {
            return self.default;
        }
        if age <= hold_us {
            return self.value;
        }
        // Between one and two hold windows, cross-fade to the default. A cliff
        // at exactly `hold_ms` would show as a visible jump the moment a
        // producer hiccuped; a ramp reads as the source fading out, which is
        // what actually happened.
        let span = hold_us.max(1);
        let t = Q16::from_ratio((age - hold_us) as i32, span as i32).clamp(Q16::ZERO, Q16::ONE);
        self.value.lerp(self.default, t)
    }

    /// Whether the value is past its hold window.
    pub fn is_stale(&self, now_us: u64) -> bool {
        if self.never_stale() {
            return false;
        }
        match self.updated_at_us {
            None => true,
            Some(updated) => {
                now_us.saturating_sub(updated) > (self.hold_ms as u64).saturating_mul(1_000)
            }
        }
    }

    /// Expire a lapsed lease.
    ///
    /// Returns the producer that lost it, if any. The lease is what makes a
    /// crashed producer let go without anyone having to notice it crashed.
    pub fn advance(&mut self, now_us: u64) -> Option<ProducerId> {
        let held = self.owner?;
        if now_us < held.lease_until_us {
            return None;
        }
        self.owner = None;
        Some(held.producer)
    }

    fn take(&mut self, producer: ProducerId, priority: u8, lease_until_us: u64) {
        self.owner = Some(Owner {
            producer,
            priority,
            lease_until_us,
            // A new owner's sequence numbers are unrelated to the old owner's.
            // Carrying the old high-water mark over would drop every packet from
            // a producer whose counter happened to start lower.
            last_seq: None,
        });
        // The value is deliberately kept. A handover between two producers of
        // the same thing - the room mic and the desktop app - should not blink.
    }
}

/// Whether `seq` is newer than `last`, allowing for wraparound.
///
/// Half the space is treated as "ahead". At 60 Hz a `u16` wraps every eighteen
/// minutes, and a plain `>` would stall the channel there until the producer
/// restarted — a bug that only appears after the demo is over.
fn is_newer(seq: u16, last: u16) -> bool {
    seq != last && seq.wrapping_sub(last) < u16::MAX / 2
}

/// Every channel this device knows about.
#[derive(Clone, Default, Debug)]
pub struct Channels {
    channels: BTreeMap<u16, Channel>,
}

impl Channels {
    pub fn new() -> Channels {
        Channels::default()
    }

    pub fn declare(&mut self, channel: Channel) {
        self.channels.insert(channel.id, channel);
    }

    pub fn get(&self, id: u16) -> Option<&Channel> {
        self.channels.get(&id)
    }

    pub fn get_mut(&mut self, id: u16) -> Option<&mut Channel> {
        self.channels.get_mut(&id)
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Read a channel, or its default if this device has never heard of it.
    ///
    /// An undeclared channel reads zero rather than failing: **defined
    /// degradation**. A program compiled against a channel the mesh no longer
    /// publishes must keep rendering.
    pub fn read(&self, id: u16, now_us: u64) -> Q16 {
        match self.channels.get(&id) {
            Some(c) => c.read(now_us),
            None => Q16::ZERO,
        }
    }

    /// Expire every lapsed lease, returning the channels that came free.
    pub fn advance(&mut self, now_us: u64) -> Vec<u16> {
        let mut freed = Vec::new();
        for (id, channel) in self.channels.iter_mut() {
            if channel.advance(now_us).is_some() {
                freed.push(*id);
            }
        }
        freed
    }
}

/// The device's channels, seen as a program's uniforms.
///
/// `CHREAD` names a **slot**, not a channel id: the program says "my third
/// channel" and its header says which channel that is, so the same bytecode can
/// be pointed at a different producer without recompiling. This is the piece
/// that resolves one into the other, and without it every channel read returns
/// zero — which is not a failure anyone would notice, because zero is also what
/// a channel with no producer correctly returns.
///
/// # Time is fixed for the frame
///
/// `now_us` is captured when this is built rather than read per access, so every
/// pixel of a frame sees the same channel values. Reading the clock per access
/// would let a channel go stale *between two pixels* of one frame, and a strip
/// where the first half heard the beat and the second half did not is a very
/// confusing bug to be handed.
pub struct ChannelUniforms<'a> {
    channels: &'a Channels,
    program: &'a Program<'a>,
    now_us: u64,
}

impl<'a> ChannelUniforms<'a> {
    pub fn new(channels: &'a Channels, program: &'a Program<'a>, now_us: u64) -> Self {
        ChannelUniforms {
            channels,
            program,
            now_us,
        }
    }
}

impl Uniforms for ChannelUniforms<'_> {
    fn channel(&self, slot: u8, offset: u8) -> Q16 {
        // Every miss here returns zero rather than failing: a slot the program
        // does not declare, a channel this device has never been told about, a
        // producer that stopped. **Defined degradation** — a dead audio
        // publisher leaves the lights doing something sensible instead of
        // stopping the show.
        let Some(id) = self.program.channel_id(slot) else {
            return Q16::ZERO;
        };
        let Some(channel) = self.channels.get(id) else {
            return Q16::ZERO;
        };
        // Multi-value channels — audio bands, a sensor with several readings —
        // are consecutive ids from the declared one, so an effect reads
        // `audio[3]` as an offset and the wire carries one channel per band.
        if offset == 0 {
            return channel.read(self.now_us);
        }
        match id
            .checked_add(offset as u16)
            .and_then(|at| self.channels.get(at))
        {
            Some(band) => band.read(self.now_us),
            None => Q16::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIC: ProducerId = [1, 1, 1, 1];
    const DESKTOP: ProducerId = [2, 2, 2, 2];
    const OTHER: ProducerId = [3, 3, 3, 3];

    fn channel() -> Channel {
        Channel::new(7, 250, Q16::ZERO)
    }

    #[test]
    fn an_unowned_channel_is_taken_by_the_first_claim() {
        let mut c = channel();
        assert!(!c.is_owned());
        assert_eq!(c.claim(0, MIC, 10, 1_000), ClaimOutcome::Taken);
        assert_eq!(c.owner().unwrap().producer, MIC);
    }

    #[test]
    fn strictly_greater_priority_preempts() {
        // Desktop audio takes the channel from the room mic.
        let mut c = channel();
        c.claim(0, MIC, 10, 1_000);
        assert_eq!(
            c.claim(0, DESKTOP, 200, 1_000),
            ClaimOutcome::Preempted { previous: MIC }
        );
        assert_eq!(c.owner().unwrap().producer, DESKTOP);
    }

    #[test]
    fn equal_priority_does_not_preempt() {
        // Two identical producers - two microphones, two copies of the app -
        // would otherwise preempt each other on every frame forever.
        let mut c = channel();
        c.claim(0, MIC, 100, 1_000);
        assert_eq!(
            c.claim(0, DESKTOP, 100, 1_000),
            ClaimOutcome::Refused { holder: MIC }
        );
        assert_eq!(c.owner().unwrap().producer, MIC);
    }

    #[test]
    fn lower_priority_does_not_preempt() {
        let mut c = channel();
        c.claim(0, DESKTOP, 200, 1_000);
        assert_eq!(
            c.claim(0, MIC, 10, 1_000),
            ClaimOutcome::Refused { holder: DESKTOP }
        );
    }

    #[test]
    fn the_owner_renewing_is_the_ordinary_case_and_keeps_its_sequence() {
        // A renewal that reset the sequence would reopen the channel to replayed
        // packets on every frame.
        let mut c = channel();
        c.claim(0, MIC, 10, 1_000);
        c.publish(0, MIC, 500, Q16::ONE);
        assert_eq!(c.claim(100, MIC, 10, 1_000), ClaimOutcome::Renewed);
        assert_eq!(c.owner().unwrap().last_seq, Some(500));
        assert!(
            !c.publish(200, MIC, 499, Q16::ZERO),
            "an older packet got in"
        );
    }

    #[test]
    fn a_lapsed_lease_frees_the_channel_without_anyone_noticing_a_crash() {
        let mut c = channel();
        c.claim(0, MIC, 10, 500);
        assert_eq!(c.advance(499_000), None);
        assert_eq!(c.advance(500_000), Some(MIC));
        assert!(!c.is_owned());
        // And a waiting claimant can take it.
        assert_eq!(c.claim(500_000, DESKTOP, 1, 1_000), ClaimOutcome::Taken);
    }

    #[test]
    fn only_the_owner_can_release() {
        // Otherwise any device on the mesh could knock the desktop app off the
        // audio channel with a single packet.
        let mut c = channel();
        c.claim(0, DESKTOP, 200, 1_000);
        assert!(!c.release(OTHER));
        assert!(c.is_owned());
        assert!(c.release(DESKTOP));
        assert!(!c.is_owned());
        assert!(!c.release(DESKTOP), "releasing twice says nothing new");
    }

    #[test]
    fn only_the_owner_can_publish() {
        let mut c = channel();
        c.claim(0, MIC, 10, 1_000);
        assert!(!c.publish(0, OTHER, 1, Q16::ONE));
        assert_eq!(c.read(0), Q16::ZERO);
        assert!(c.publish(0, MIC, 1, Q16::ONE));
        assert_eq!(c.read(0), Q16::ONE);
    }

    #[test]
    fn nobody_can_publish_to_an_unowned_channel() {
        let mut c = channel();
        assert!(!c.publish(0, MIC, 1, Q16::ONE));
    }

    #[test]
    fn latest_wins_and_an_older_packet_cannot_undo_a_fresher_one() {
        let mut c = channel();
        c.claim(0, MIC, 10, 1_000);
        assert!(c.publish(0, MIC, 10, Q16::ONE));
        assert!(!c.publish(0, MIC, 9, Q16::ZERO), "reordering undid a value");
        assert_eq!(c.read(0), Q16::ONE);
        assert!(!c.publish(0, MIC, 10, Q16::ZERO), "a duplicate got in");
    }

    #[test]
    fn a_wrapping_sequence_keeps_working() {
        // A u16 wraps after about eighteen minutes at 60 Hz. A plain comparison
        // would stall the channel there - a bug that appears only after the demo
        // is over.
        let mut c = channel();
        c.claim(0, MIC, 10, 1_000);
        assert!(
            c.publish(0, MIC, u16::MAX - 1, Q16::ONE),
            "a producer whose counter starts high must still be able to publish"
        );
        assert!(c.publish(0, MIC, u16::MAX, Q16::HALF));
        assert!(
            c.publish(0, MIC, 0, Q16::ZERO),
            "the wrap stalled the channel"
        );
        assert!(c.publish(0, MIC, 1, Q16::ONE));
    }

    #[test]
    fn a_handover_between_two_producers_does_not_blink() {
        // The room mic and the desktop app publish the same thing. Resetting the
        // value on handover would show as a one-frame flash.
        let mut c = channel();
        c.claim(0, MIC, 10, 1_000);
        c.publish(0, MIC, 5, Q16::HALF);
        c.claim(0, DESKTOP, 200, 1_000);
        assert_eq!(c.read(0), Q16::HALF, "the value was dropped on handover");
        // And the new owner's own low sequence numbers are accepted.
        assert!(c.publish(0, DESKTOP, 1, Q16::ONE));
    }

    #[test]
    fn a_fresh_value_reads_back_unchanged() {
        let mut c = channel();
        c.claim(0, MIC, 10, 10_000);
        c.publish(0, MIC, 1, Q16::ONE);
        assert_eq!(c.read(0), Q16::ONE);
        assert_eq!(c.read(250_000), Q16::ONE, "still inside the hold window");
        assert!(!c.is_stale(250_000));
    }

    #[test]
    fn a_dead_producer_fades_the_value_to_its_default_rather_than_freezing_it() {
        // The defined visual outcome: a dead audio source fades the lights to
        // steady rather than leaving them stopped mid-beat.
        let mut c = Channel::new(7, 250, Q16::ZERO);
        c.claim(0, MIC, 10, 10_000_000);
        c.publish(0, MIC, 1, Q16::ONE);

        assert_eq!(c.read(250_000), Q16::ONE, "not stale yet");
        assert!(c.is_stale(300_000));
        let midway = c.read(375_000);
        assert!(
            midway < Q16::ONE && midway > Q16::ZERO,
            "expected a fade, got {midway:?}"
        );
        assert_eq!(
            c.read(500_000),
            Q16::ZERO,
            "should have reached the default"
        );
        assert_eq!(c.read(10_000_000), Q16::ZERO);
    }

    #[test]
    fn the_fade_is_a_ramp_rather_than_a_cliff() {
        // A cliff at exactly hold_ms shows as a visible jump the moment a
        // producer hiccups. A ramp reads as the source fading out, which is what
        // actually happened.
        let mut c = Channel::new(7, 100, Q16::ZERO);
        c.claim(0, MIC, 10, 10_000_000);
        c.publish(0, MIC, 1, Q16::ONE);
        let mut previous = Q16::ONE;
        for step in 0..=10 {
            let now = 100_000 + step * 10_000;
            let v = c.read(now);
            assert!(v <= previous, "the fade went back up at {now}");
            previous = v;
        }
        assert_eq!(previous, Q16::ZERO);
    }

    #[test]
    fn a_channel_decays_to_its_declared_default_not_to_zero() {
        // A channel that always decayed to zero would turn the lights off, and
        // off is rarely the sensible resting state for a room.
        let mut c = Channel::new(7, 100, Q16::HALF);
        assert_eq!(c.read(0), Q16::HALF, "before anything arrives");
        c.claim(0, MIC, 10, 10_000_000);
        c.publish(0, MIC, 1, Q16::ONE);
        assert_eq!(c.read(1_000_000), Q16::HALF);
    }

    #[test]
    fn hold_zero_means_never_stale() {
        // Right for a value pushed only on change - a scrolling message, a mode
        // selector - where treating silence as failure would be exactly wrong.
        let mut c = Channel::new(7, 0, Q16::ZERO);
        assert!(c.never_stale());
        c.claim(0, MIC, 10, 10_000_000);
        c.publish(0, MIC, 1, Q16::ONE);
        assert_eq!(c.read(u64::MAX / 2), Q16::ONE, "a held value went stale");
        assert!(!c.is_stale(u64::MAX / 2));
    }

    #[test]
    fn a_channel_that_never_received_anything_reads_its_default() {
        let c = Channel::new(7, 100, Q16::HALF);
        assert_eq!(c.read(0), Q16::HALF);
        assert!(c.is_stale(0));
    }

    // ---- the collection ----------------------------------------------------

    #[test]
    fn an_undeclared_channel_reads_zero_rather_than_failing() {
        // Defined degradation: a program compiled against a channel the mesh no
        // longer publishes has to keep rendering.
        let chans = Channels::new();
        assert!(chans.is_empty());
        assert_eq!(chans.read(42, 1_000), Q16::ZERO);
    }

    #[test]
    fn advancing_frees_every_lapsed_lease_and_reports_which() {
        let mut chans = Channels::new();
        chans.declare(Channel::new(1, 100, Q16::ZERO));
        chans.declare(Channel::new(2, 100, Q16::ZERO));
        chans.declare(Channel::new(3, 100, Q16::ZERO));
        chans.get_mut(1).unwrap().claim(0, MIC, 10, 100);
        chans.get_mut(2).unwrap().claim(0, MIC, 10, 5_000);
        // Channel 3 is never claimed.

        let freed = chans.advance(200_000);
        assert_eq!(freed, alloc::vec![1]);
        assert!(!chans.get(1).unwrap().is_owned());
        assert!(chans.get(2).unwrap().is_owned());
        assert_eq!(chans.len(), 3);
    }

    #[test]
    fn declaring_the_same_id_twice_replaces_it() {
        let mut chans = Channels::new();
        chans.declare(Channel::new(1, 100, Q16::ZERO));
        chans.declare(Channel::new(1, 500, Q16::ONE));
        assert_eq!(chans.len(), 1);
        assert_eq!(chans.get(1).unwrap().hold_ms, 500);
    }

    #[test]
    fn the_preemption_and_handback_cycle_works_end_to_end() {
        // The scenario the design names: desktop audio preempts the room mic and
        // hands back on disconnect.
        let mut c = Channel::new(7, 250, Q16::ZERO);
        c.claim(0, MIC, 10, 1_000);
        c.publish(0, MIC, 1, Q16::from_ratio(1, 4));

        assert!(matches!(
            c.claim(100, DESKTOP, 200, 1_000),
            ClaimOutcome::Preempted { .. }
        ));
        c.publish(100, DESKTOP, 1, Q16::ONE);
        assert_eq!(c.read(100), Q16::ONE);

        // The desktop disconnects; its lease lapses.
        assert_eq!(c.advance(1_100_000), Some(DESKTOP));
        // The mic takes it back.
        assert_eq!(c.claim(1_100_000, MIC, 10, 1_000), ClaimOutcome::Taken);
        assert!(c.publish(1_100_000, MIC, 1, Q16::from_ratio(1, 4)));
        assert_eq!(c.read(1_100_000), Q16::from_ratio(1, 4));
    }
}

#[cfg(test)]
mod uniform_tests {
    use super::*;
    use lumen_vm::isa::{Instruction, OpCode};
    use lumen_vm::program::builder::ProgramBuilder;
    use lumen_vm::program::{Program, Section};

    const ME: ProducerId = [1, 2, 3, 4];

    /// A program reading channel `id` at `offset` and emitting it as red.
    fn reads(id: u16, offset: u8) -> alloc::vec::Vec<u8> {
        let mut b = ProgramBuilder::new();
        let slot = b.channel(id);
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::ChRead, 20, slot, offset),
        );
        b.push(
            Section::Pixel,
            Instruction::new(OpCode::EmitRgb, 20, 20, 20),
        );
        b.build()
    }

    fn value_seen(bytes: &[u8], channels: &Channels, now_us: u64) -> Q16 {
        let program = Program::parse(bytes).expect("a program");
        let u = ChannelUniforms::new(channels, &program, now_us);
        u.channel(0, 0)
    }

    #[test]
    fn a_slot_resolves_through_the_programs_own_table() {
        // The whole point of the indirection: the program says "my first
        // channel", its header says that is channel 7, and the device looks up
        // 7. Get this wrong and every effect reads somebody else's data.
        let bytes = reads(7, 0);
        let mut channels = Channels::new();
        let mut ch = Channel::new(7, 1_000, Q16::ZERO);
        assert_eq!(ch.claim(0, ME, 10, 60_000), ClaimOutcome::Taken);
        assert!(ch.publish(0, ME, 1, Q16::HALF));
        channels.declare(ch);

        assert_eq!(value_seen(&bytes, &channels, 0), Q16::HALF);
    }

    #[test]
    fn a_channel_this_device_has_never_heard_of_reads_zero() {
        // Defined degradation, and the reason this bridge needs a test at all:
        // the failure is silent. A missing bridge, a missing channel and a
        // channel legitimately sitting at zero all look identical on a strip.
        let bytes = reads(7, 0);
        let empty = Channels::new();
        assert_eq!(value_seen(&bytes, &empty, 0), Q16::ZERO);
    }

    #[test]
    fn a_slot_the_program_does_not_declare_reads_zero() {
        let bytes = reads(7, 0);
        let program = Program::parse(&bytes).expect("a program");
        let channels = Channels::new();
        let u = ChannelUniforms::new(&channels, &program, 0);
        // Slot 1 exists in the instruction encoding but not in this program.
        assert_eq!(u.channel(1, 0), Q16::ZERO);
        assert_eq!(u.channel(200, 0), Q16::ZERO);
    }

    #[test]
    fn a_producer_that_stopped_falls_back_to_the_channels_default() {
        // A dead publisher must leave the lights doing something sensible
        // rather than stop the program - the value goes stale and the default
        // takes over, and the effect never learns that anything happened.
        let bytes = reads(7, 0);
        let mut channels = Channels::new();
        let mut ch = Channel::new(7, 200, Q16::from_ratio(1, 4));
        ch.claim(0, ME, 10, 60_000);
        ch.publish(0, ME, 1, Q16::ONE);
        channels.declare(ch);

        assert_eq!(value_seen(&bytes, &channels, 100_000), Q16::ONE);
        // 200 ms of hold, then the default.
        assert_eq!(
            value_seen(&bytes, &channels, 500_000),
            Q16::from_ratio(1, 4)
        );
    }

    #[test]
    fn an_offset_reads_the_next_channel_along() {
        // Multi-value channels - audio bands, a sensor with several readings -
        // are consecutive ids from the declared one, so one effect can read
        // `audio[3]` while the wire carries one channel per band.
        let bytes = reads(7, 0);
        let mut channels = Channels::new();
        for (id, v) in [(7u16, Q16::ZERO), (8, Q16::HALF), (9, Q16::ONE)] {
            let mut ch = Channel::new(id, 1_000, Q16::ZERO);
            ch.claim(0, ME, 10, 60_000);
            ch.publish(0, ME, 1, v);
            channels.declare(ch);
        }
        let program = Program::parse(&bytes).expect("a program");
        let u = ChannelUniforms::new(&channels, &program, 0);
        assert_eq!(u.channel(0, 1), Q16::HALF);
        assert_eq!(u.channel(0, 2), Q16::ONE);
        // Past the end of what was declared, zero rather than a wrap onto
        // whatever channel happens to live there.
        assert_eq!(u.channel(0, 3), Q16::ZERO);
        assert_eq!(u.channel(0, 255), Q16::ZERO);
    }

    #[test]
    fn every_pixel_of_a_frame_sees_the_same_value() {
        // Time is captured once when the uniforms are built. Reading the clock
        // per access would let a channel go stale between two pixels of one
        // frame, and a strip whose first half heard the beat and whose second
        // half did not is a memorably confusing bug.
        let bytes = reads(7, 0);
        let mut channels = Channels::new();
        let mut ch = Channel::new(7, 100, Q16::ZERO);
        ch.claim(0, ME, 10, 60_000);
        ch.publish(0, ME, 1, Q16::ONE);
        channels.declare(ch);

        let program = Program::parse(&bytes).expect("a program");
        let at_the_edge = ChannelUniforms::new(&channels, &program, 99_000);
        let first = at_the_edge.channel(0, 0);
        for _ in 0..300 {
            assert_eq!(at_the_edge.channel(0, 0), first);
        }
    }
}
