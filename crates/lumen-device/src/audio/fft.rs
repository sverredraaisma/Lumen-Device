//! A fixed-point radix-2 FFT.
//!
//! Fixed point rather than `f32` for the same reason as everything else here:
//! this runs inside `lumen-device`, which forbids floating point so a recorded
//! run replays identically. Audio analysis happens on one device and only the
//! result is broadcast, so cross-chip bit-identity is not strictly required —
//! but replay determinism is, and a scenario that cannot be replayed is a bug
//! that cannot be fixed.
//!
//! # Scaling
//!
//! Each of the `LOG2_N` stages shifts its butterfly output right by one, so the
//! result is the true transform divided by [`N`] and no intermediate can
//! overflow regardless of the input. That costs one bit of precision per stage,
//! which is the standard trade and is invisible here: the bands are
//! AGC-normalised to a byte immediately afterwards, so only the *shape* of the
//! spectrum survives to the wire.

use lumen_vm::q16::Q16;

/// log2 of the transform size.
pub const LOG2_N: usize = 10;

/// Transform size. At 48 kHz this is a 21 ms window and a 47 Hz bin, which is
/// the coarsest that still separates the low bands people actually see: a bass
/// drum and the note above it land in different bins.
pub const N: usize = 1 << LOG2_N;

/// Precomputed twiddle factors.
///
/// Built once and reused. Computing them per transform would mean `N/2 *
/// LOG2_N` trig lookups per frame — correct, but it is 5120 of them at 60 Hz on
/// a chip that also has pixels to render.
pub struct Fft {
    cos: [Q16; N / 2],
    sin: [Q16; N / 2],
}

impl Default for Fft {
    fn default() -> Self {
        Self::new()
    }
}

impl Fft {
    pub fn new() -> Fft {
        let mut cos = [Q16::ZERO; N / 2];
        let mut sin = [Q16::ZERO; N / 2];
        for (k, (c, s)) in cos.iter_mut().zip(sin.iter_mut()).enumerate() {
            // Turns rather than radians: `sin_turns` is exact at the cardinal
            // points, which is what keeps a pure tone at bin N/4 from leaking.
            let turns = Q16::from_ratio(-(k as i32), N as i32);
            *c = turns.cos_turns();
            *s = turns.sin_turns();
        }
        Fft { cos, sin }
    }

    /// In-place complex FFT. On return `re`/`im` hold the transform divided by
    /// [`N`], in natural bin order.
    pub fn transform(&self, re: &mut [Q16; N], im: &mut [Q16; N]) {
        bit_reverse(re);
        bit_reverse(im);

        let mut half = 1usize;
        while half < N {
            let span = half * 2;
            // Twiddle stride: this stage uses every (N / span)th factor.
            let stride = N / span;
            for start in (0..N).step_by(span) {
                for k in 0..half {
                    let w = k * stride;
                    let (wr, wi) = (self.cos[w], self.sin[w]);
                    let i = start + k;
                    let j = i + half;

                    // (wr + i·wi) * (re[j] + i·im[j])
                    let tr = mul(wr, re[j]) - mul(wi, im[j]);
                    let ti = mul(wr, im[j]) + mul(wi, re[j]);

                    // Halve both outputs so the stage cannot grow the signal.
                    let ur = re[i].0;
                    let ui = im[i].0;
                    re[i] = Q16((ur + tr) >> 1);
                    im[i] = Q16((ui + ti) >> 1);
                    re[j] = Q16((ur - tr) >> 1);
                    im[j] = Q16((ui - ti) >> 1);
                }
            }
            half = span;
        }
    }
}

/// Q16 multiply, widened so the product cannot overflow before the shift.
fn mul(a: Q16, b: Q16) -> i32 {
    (((a.0 as i64) * (b.0 as i64)) >> 16) as i32
}

/// Reorder into bit-reversed index order, which is what lets the butterflies
/// run in place.
fn bit_reverse(v: &mut [Q16; N]) {
    let mut j = 0usize;
    for i in 1..N {
        let mut bit = N >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            v.swap(i, j);
        }
    }
}

/// Magnitude of each bin below Nyquist.
///
/// Only the first half is meaningful: the input is real, so the upper half is
/// the mirror image and carries no information.
pub fn magnitudes(re: &[Q16; N], im: &[Q16; N], out: &mut [Q16; N / 2]) {
    for (k, o) in out.iter_mut().enumerate() {
        *o = magnitude(re[k], im[k]);
    }
}

/// `hypot`, computed without ever leaving the exact range.
///
/// Not `Q16::len2`, which squares in Q16 and so rounds anything below about
/// 1/256 to zero. After the transform's 1/N scaling that is most of the
/// spectrum: a full-scale impulse comes back as 64 raw units per bin, whose
/// square is 0.06 of a raw unit, and every bin would read zero.
///
/// Squaring in `i64` keeps the product exact — two Q16 values multiply to a
/// Q32 — and the integer square root of a Q32 is a Q16 directly.
fn magnitude(re: Q16, im: Q16) -> Q16 {
    let r = re.0 as i64;
    let i = im.0 as i64;
    Q16(isqrt(r * r + i * i) as i32)
}

/// Integer square root by Newton's method.
///
/// `n` is non-negative by construction: it is a sum of two squares.
fn isqrt(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    // A power-of-two start above the root keeps the iteration monotonically
    // decreasing, so it terminates without a convergence check.
    let mut x = 1i64 << ((64 - n.leading_zeros()).div_ceil(2));
    loop {
        let next = (x + n / x) / 2;
        if next >= x {
            return x;
        }
        x = next;
    }
}

#[cfg(test)]
#[path = "fft_tests.rs"]
mod tests;
