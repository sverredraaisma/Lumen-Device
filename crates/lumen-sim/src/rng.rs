//! The one source of randomness in the simulator.
//!
//! Every coin flip the harness makes — a dropped packet, a jitter draw, a byte
//! of entropy handed to a node — comes from here, so a seed is a complete
//! description of a run. If any other source of randomness creeps in, replay
//! stops being a proof and becomes a coincidence.
//!
//! Hand-written on purpose. `rand` is a fine crate, but its output is only
//! stable within a major version and across the algorithms it happens to pick;
//! a recorded scenario has to still reproduce after a `cargo update`, and the
//! only way to guarantee that is to own the generator.

/// SplitMix64 — the generator, and deliberately a boring one.
///
/// Chosen because it is four lines, has no platform-dependent behaviour (pure
/// wrapping `u64` arithmetic, no floats, no `usize`), and passes enough of
/// BigCrush for a fault-injection harness. Nothing here is cryptographic and
/// nothing should ever treat it as if it were — [`crate::SimEntropy`] exists to
/// make a node's key material *reproducible*, not secret.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SimRng {
    state: u64,
}

/// The SplitMix64 increment (the golden-ratio odd constant).
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

impl SimRng {
    /// A generator for `seed`. Every seed is valid, including zero — SplitMix64
    /// has no bad states, which is half the reason it is here.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// The next 32 bits, taken from the high half — the low bits of a
    /// SplitMix64 output are the weakest, and a `% 1000` on them is exactly the
    /// use that would notice.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A value in `0..bound`, or `0` when `bound` is zero.
    ///
    /// Lemire's multiply-shift rather than rejection sampling: it always draws
    /// exactly one word, so the number of draws a run makes does not depend on
    /// the values it happens to get. That keeps two runs of the same scenario
    /// in step even when one of them is being stepped by a debugger, and the
    /// residual modulo bias (below 2^-64 relative) is irrelevant to deciding
    /// whether a packet is late.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let product = (self.next_u64() as u128) * (bound as u128);
        (product >> 64) as u64
    }

    /// A value in `lo..=hi`. Arguments are swapped if given the wrong way round
    /// rather than panicking: a scenario file is data, and data should not be
    /// able to abort the harness.
    pub fn range_inclusive(&mut self, lo: u64, hi: u64) -> u64 {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        lo + self.below(hi - lo + 1)
    }

    /// True with probability `permille / 1000`. Saturates: 1000 and above is
    /// always, 0 is never.
    ///
    /// Per-mille integers rather than a float probability because the whole
    /// project bans floating point in anything whose result has to be identical
    /// on two different chips, and a harness that disagrees with itself across
    /// platforms is worse than no harness.
    pub fn chance_permille(&mut self, permille: u16) -> bool {
        if permille == 0 {
            // Still no draw: a zero-probability fault must not consume a word,
            // or turning loss off would shift every later draw and change the
            // rest of the run for reasons unrelated to loss.
            return false;
        }
        if permille >= 1000 {
            return true;
        }
        self.below(1000) < permille as u64
    }

    /// Fill `buf` with successive words, little-endian.
    ///
    /// Explicitly little-endian rather than native: a recording made on an x86
    /// host has to replay identically on a big-endian CI runner, and
    /// `to_ne_bytes` is precisely the kind of thing that is correct until it is
    /// not.
    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }

    /// A child generator for an independent concern.
    ///
    /// Giving the network and a node's entropy separate streams means adding a
    /// packet-loss draw does not renumber the nonces a node generates, so an
    /// unrelated change to the fault model leaves the rest of a recording
    /// alone. `label` names the stream; the same label off the same parent
    /// state always yields the same child.
    pub fn fork(&mut self, label: u64) -> SimRng {
        let mixed = self.next_u64() ^ label.wrapping_mul(GAMMA);
        SimRng::new(mixed)
    }

    /// The raw state. Exposed so a test can assert that two runs left the
    /// generator in the same place — a cheap way to catch a stray draw.
    pub fn state(&self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = SimRng::new(7);
        let mut b = SimRng::new(7);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_eq!(a.state(), b.state());
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = SimRng::new(1);
        let mut b = SimRng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn zero_seed_is_a_valid_state() {
        let mut a = SimRng::new(0);
        let first = a.next_u64();
        assert_ne!(first, 0);
        assert_ne!(a.next_u64(), first);
    }

    /// Pins the actual numbers. This is the test that fails if anyone ever
    /// "improves" the generator, which would silently invalidate every recorded
    /// scenario in the repo.
    #[test]
    fn stream_is_pinned() {
        let mut r = SimRng::new(0);
        let got: Vec<u64> = (0..4).map(|_| r.next_u64()).collect();
        assert_eq!(
            got,
            vec![
                0xE220_A839_7B1D_CDAF,
                0x6E78_9E6A_A1B9_65F4,
                0x06C4_5D18_8009_454F,
                0xF88B_B8A8_724C_81EC,
            ]
        );
    }

    #[test]
    fn next_u32_takes_the_high_half() {
        let mut a = SimRng::new(99);
        let mut b = SimRng::new(99);
        assert_eq!(a.next_u32() as u64, b.next_u64() >> 32);
    }

    #[test]
    fn below_respects_the_bound() {
        let mut r = SimRng::new(3);
        assert_eq!(r.below(0), 0);
        assert_eq!(r.below(1), 0);
        for _ in 0..1000 {
            assert!(r.below(10) < 10);
        }
    }

    #[test]
    fn below_covers_its_range() {
        let mut r = SimRng::new(5);
        let mut seen = [false; 6];
        for _ in 0..500 {
            seen[r.below(6) as usize] = true;
        }
        assert!(seen.iter().all(|s| *s));
    }

    #[test]
    fn range_inclusive_includes_both_ends() {
        let mut r = SimRng::new(11);
        let mut low = false;
        let mut high = false;
        for _ in 0..500 {
            let v = r.range_inclusive(10, 12);
            assert!((10..=12).contains(&v));
            low |= v == 10;
            high |= v == 12;
        }
        assert!(low && high);
    }

    #[test]
    fn range_inclusive_is_a_point_when_lo_equals_hi() {
        let mut r = SimRng::new(11);
        assert_eq!(r.range_inclusive(42, 42), 42);
    }

    #[test]
    fn range_inclusive_swaps_reversed_bounds() {
        let mut r = SimRng::new(13);
        for _ in 0..100 {
            assert!((3..=9).contains(&r.range_inclusive(9, 3)));
        }
    }

    #[test]
    fn chance_saturates_at_both_ends_without_drawing() {
        let mut r = SimRng::new(17);
        let before = r.state();
        assert!(!r.chance_permille(0));
        assert_eq!(r.state(), before, "a zero chance must not consume a word");
        assert!(r.chance_permille(1000));
        assert!(r.chance_permille(60_000));
    }

    #[test]
    fn chance_is_roughly_calibrated() {
        let mut r = SimRng::new(19);
        let hits = (0..10_000).filter(|_| r.chance_permille(250)).count();
        // Wide bounds on purpose: this checks the arithmetic is not inverted or
        // off by an order of magnitude, not the quality of SplitMix64.
        assert!((2200..2800).contains(&hits), "hits = {hits}");
    }

    #[test]
    fn fill_bytes_handles_a_ragged_tail() {
        let mut r = SimRng::new(23);
        let mut buf = [0u8; 13];
        r.fill_bytes(&mut buf);
        assert!(buf.iter().any(|b| *b != 0));

        let mut same = SimRng::new(23);
        let mut expect = [0u8; 13];
        let w0 = same.next_u64().to_le_bytes();
        let w1 = same.next_u64().to_le_bytes();
        expect[..8].copy_from_slice(&w0);
        expect[8..].copy_from_slice(&w1[..5]);
        assert_eq!(buf, expect);
    }

    #[test]
    fn fill_bytes_on_an_empty_buffer_draws_nothing() {
        let mut r = SimRng::new(29);
        let before = r.state();
        r.fill_bytes(&mut []);
        assert_eq!(r.state(), before);
    }

    #[test]
    fn forks_are_reproducible_and_independent() {
        let mut parent = SimRng::new(31);
        let mut a = parent.fork(1);
        let mut b = parent.fork(2);
        let first_a = a.next_u64();
        let first_b = b.next_u64();
        assert_ne!(first_a, first_b);

        // And the derivation is stable across runs.
        let mut parent2 = SimRng::new(31);
        let mut a2 = parent2.fork(1);
        let mut b2 = parent2.fork(2);
        assert_eq!(a2.next_u64(), first_a);
        assert_eq!(b2.next_u64(), first_b);
    }

    #[test]
    fn clone_continues_the_same_stream() {
        let mut a = SimRng::new(37);
        a.next_u64();
        let mut b = a.clone();
        assert_eq!(a, b);
        assert_eq!(a.next_u64(), b.next_u64());
    }
}
