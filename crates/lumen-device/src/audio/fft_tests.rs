//! FFT tests against signals whose transform is known in closed form.
//!
//! Every case here is one where the right answer can be written down rather
//! than measured, so none of them is the implementation checking itself.

use super::*;

fn buffers() -> ([Q16; N], [Q16; N]) {
    ([Q16::ZERO; N], [Q16::ZERO; N])
}

fn mags(re: &[Q16; N], im: &[Q16; N]) -> [Q16; N / 2] {
    let mut out = [Q16::ZERO; N / 2];
    magnitudes(re, im, &mut out);
    out
}

/// The bin with the largest magnitude, and its value.
fn peak(m: &[Q16; N / 2]) -> (usize, Q16) {
    let mut best = 0;
    for (k, v) in m.iter().enumerate() {
        if v.0 > m[best].0 {
            best = k;
        }
    }
    (best, m[best])
}

#[test]
fn a_constant_signal_is_entirely_in_bin_zero() {
    // DC has no frequency content anywhere else, and the scaling means the
    // amplitude comes back as itself rather than multiplied by N.
    let (mut re, mut im) = buffers();
    for r in re.iter_mut() {
        *r = Q16::HALF;
    }
    Fft::new().transform(&mut re, &mut im);
    let m = mags(&re, &im);

    assert_eq!(m[0], Q16::HALF, "bin 0 carries the mean");
    for (k, v) in m.iter().enumerate().skip(1) {
        assert!(v.0.abs() <= 4, "bin {k} should be empty, got {}", v.0);
    }
}

#[test]
fn an_all_zero_signal_transforms_to_nothing() {
    let (mut re, mut im) = buffers();
    Fft::new().transform(&mut re, &mut im);
    for (k, v) in mags(&re, &im).iter().enumerate() {
        assert_eq!(v.0, 0, "bin {k}");
    }
}

#[test]
fn an_impulse_spreads_evenly_across_every_bin() {
    // The transform of a unit impulse is flat. With the 1/N scaling every bin
    // holds 1/N, so this also pins the scaling convention.
    let (mut re, mut im) = buffers();
    re[0] = Q16::ONE;
    Fft::new().transform(&mut re, &mut im);
    let m = mags(&re, &im);

    let expected = Q16::from_ratio(1, N as i32);
    for (k, v) in m.iter().enumerate() {
        assert!(
            (v.0 - expected.0).abs() <= 2,
            "bin {k}: expected {}, got {}",
            expected.0,
            v.0
        );
    }
}

#[test]
fn a_sine_at_a_bin_centre_lands_in_that_bin_and_no_other() {
    // A sinusoid whose period divides the window exactly has no leakage, so
    // this is the strongest statement the transform can be held to. A swapped
    // twiddle sign or a bit-reversal error puts the peak somewhere else.
    let fft = Fft::new();
    for bin in [1usize, 2, 5, 17, 64, 129, N / 4] {
        let (mut re, mut im) = buffers();
        for (n, r) in re.iter_mut().enumerate() {
            let turns = Q16::from_ratio((bin * n) as i32, N as i32);
            *r = turns.sin_turns();
        }
        fft.transform(&mut re, &mut im);
        let m = mags(&re, &im);

        let (at, value) = peak(&m);
        assert_eq!(at, bin, "peak should be at bin {bin}");

        // A real sine of amplitude 1 splits between the bin and its mirror, so
        // each half holds 1/2. Allow for the accumulated per-stage rounding.
        let half = Q16::HALF;
        assert!(
            (value.0 - half.0).abs() < half.0 / 8,
            "bin {bin}: expected about {}, got {}",
            half.0,
            value.0
        );

        // And nothing meaningful anywhere else.
        for (k, v) in m.iter().enumerate() {
            if k != bin {
                assert!(v.0 < value.0 / 8, "bin {bin}: leaked {} into bin {k}", v.0);
            }
        }
    }
}

#[test]
fn a_cosine_lands_in_the_same_bin_as_a_sine() {
    // Phase must not move the peak. If it does, the real and imaginary halves
    // have been crossed somewhere.
    let fft = Fft::new();
    let bin = 20usize;
    let (mut re, mut im) = buffers();
    for (n, r) in re.iter_mut().enumerate() {
        *r = Q16::from_ratio((bin * n) as i32, N as i32).cos_turns();
    }
    fft.transform(&mut re, &mut im);
    assert_eq!(peak(&mags(&re, &im)).0, bin);
}

#[test]
fn two_tones_produce_two_peaks() {
    let fft = Fft::new();
    let (a, b) = (11usize, 90usize);
    let (mut re, mut im) = buffers();
    for (n, r) in re.iter_mut().enumerate() {
        let ta = Q16::from_ratio((a * n) as i32, N as i32).sin_turns();
        let tb = Q16::from_ratio((b * n) as i32, N as i32).sin_turns();
        // Half each, so the sum stays inside the representable range.
        *r = Q16((ta.0 + tb.0) / 2);
    }
    fft.transform(&mut re, &mut im);
    let m = mags(&re, &im);

    let quarter = Q16::HALF.0 / 2;
    for bin in [a, b] {
        assert!(
            (m[bin].0 - quarter).abs() < quarter / 3,
            "bin {bin}: expected about {quarter}, got {}",
            m[bin].0
        );
    }
    for (k, v) in m.iter().enumerate() {
        if k != a && k != b {
            assert!(v.0 < quarter / 4, "leaked {} into bin {k}", v.0);
        }
    }
}

#[test]
fn the_transform_is_deterministic() {
    // Replay depends on it, and a table built differently on two runs would be
    // the kind of thing that only shows up as a scenario that will not
    // reproduce.
    let fft = Fft::new();
    let make = || {
        // `im` starts at zero and the generator never touches it: a real signal
        // has no imaginary part going in.
        let (mut re, im) = buffers();
        for (n, r) in re.iter_mut().enumerate() {
            *r = Q16::from_ratio((7 * n) as i32, N as i32).sin_turns();
        }
        (re, im)
    };
    let (mut a_re, mut a_im) = make();
    let (mut b_re, mut b_im) = make();
    fft.transform(&mut a_re, &mut a_im);
    fft.transform(&mut b_re, &mut b_im);
    assert_eq!(a_re, b_re);
    assert_eq!(a_im, b_im);

    // And a second `Fft` must agree with the first.
    let (mut c_re, mut c_im) = make();
    Fft::new().transform(&mut c_re, &mut c_im);
    assert_eq!(a_re, c_re);
}

#[test]
fn a_full_scale_input_does_not_overflow() {
    // The per-stage halving exists for this. A full-scale tone is the worst
    // case the analyser will ever see from a 16-bit capture.
    let bin = N / 4;
    let (mut re, mut im) = buffers();
    for (n, r) in re.iter_mut().enumerate() {
        *r = Q16::from_ratio((bin * n) as i32, N as i32).sin_turns();
    }
    Fft::new().transform(&mut re, &mut im);
    let m = mags(&re, &im);

    let (at, value) = peak(&m);
    assert_eq!(at, bin);
    assert!(value.0 > 0, "the peak must survive the scaling");
    for (k, v) in m.iter().enumerate() {
        assert!(v.0 >= 0, "bin {k}: a magnitude cannot be negative");
    }
}

#[test]
fn the_integer_square_root_is_exact_on_perfect_squares() {
    // Up to 1<<30: any larger and `n * n` overflows the i64 the test builds it in.
    for n in [0i64, 1, 4, 9, 16, 1024, 65536, 1 << 20, 1 << 30] {
        let r = isqrt(n * n);
        assert_eq!(r, n, "isqrt({}) should be {n}", n * n);
    }
}

#[test]
fn the_integer_square_root_never_overshoots() {
    // It must floor, so a magnitude is never reported larger than it is.
    for n in [2i64, 3, 5, 10, 99, 12_345, 1 << 33, i64::MAX / 4] {
        let r = isqrt(n);
        assert!(r * r <= n, "isqrt({n}) = {r} overshot");
        assert!(
            (r + 1).saturating_mul(r + 1) > n,
            "isqrt({n}) = {r} undershot"
        );
    }
}

#[test]
fn bit_reversal_is_its_own_inverse() {
    let mut v = [Q16::ZERO; N];
    for (i, x) in v.iter_mut().enumerate() {
        *x = Q16(i as i32);
    }
    let original = v;
    bit_reverse(&mut v);
    assert_ne!(v, original, "the permutation must actually move things");
    bit_reverse(&mut v);
    assert_eq!(v, original, "applying it twice must restore the order");
}
