//! Analyser tests.
//!
//! Every signal here is generated, so the right answer is known before the code
//! runs: silence has no bands, a pure tone lights one, a click train at a stated
//! tempo must be recovered as that tempo.

use super::*;

const SR: u32 = 48_000;

fn analyzer() -> Analyzer {
    Analyzer::new(SR)
}

/// Feed a signal and collect every frame it produces.
fn run(a: &mut Analyzer, samples: &[i16]) -> alloc::vec::Vec<AudioFrame> {
    let mut out = alloc::vec::Vec::new();
    a.push(samples, |f| out.push(f));
    out
}

/// A sine of `hz` at `amp` (0..32767), `n` samples long.
fn tone(hz: u32, amp: i32, n: usize) -> alloc::vec::Vec<i16> {
    (0..n)
        .map(|i| {
            let turns = Q16::from_ratio((hz as i32) * (i as i32 % SR as i32), SR as i32);
            ((turns.sin_turns().0 * amp) >> 16) as i16
        })
        .collect()
}

// ---- band layout -----------------------------------------------------------

#[test]
fn the_band_edges_are_increasing_and_cover_the_spectrum() {
    let edges = band_edges(SR);
    for b in 1..=BANDS {
        assert!(
            edges[b] > edges[b - 1],
            "edge {b} ({}) must be above {}",
            edges[b],
            edges[b - 1]
        );
    }
    assert!(edges[0] >= 1, "DC is excluded");
    assert_eq!(edges[BANDS], N / 2, "the last edge is Nyquist");
}

#[test]
fn the_bands_are_log_spaced_not_linear() {
    // The point of log spacing: the low bands are narrow and the high ones
    // wide. Linear spacing would give every band the same width and put
    // almost the whole scale above 5 kHz.
    let edges = band_edges(SR);
    let first = edges[1] - edges[0];
    let last = edges[BANDS] - edges[BANDS - 1];
    assert!(
        last > first * 4,
        "top band {last} bins should be far wider than the bottom {first}"
    );
}

// ---- level -----------------------------------------------------------------

#[test]
fn silence_produces_no_level_and_no_bands() {
    let mut a = analyzer();
    let frames = run(&mut a, &alloc::vec![0i16; N * 4]);
    assert!(!frames.is_empty(), "silence still produces frames");
    for f in &frames {
        assert_eq!(f.level, 0, "silence has no level");
        assert!(f.bands.iter().all(|&b| b == 0), "silence has no bands");
        assert!(!f.onset, "silence has no onsets");
    }
}

#[test]
fn no_frame_is_produced_before_the_window_is_full() {
    // Analysing a half-filled buffer would report a spectrum of mostly zeros
    // as though it were the signal.
    let mut a = analyzer();
    assert_eq!(
        run(&mut a, &alloc::vec![1000i16; HOP]).len(),
        0,
        "first hop"
    );
    assert_eq!(
        run(&mut a, &alloc::vec![1000i16; HOP]).len(),
        1,
        "the second hop fills the window"
    );
}

#[test]
fn a_loud_tone_produces_a_level() {
    let mut a = analyzer();
    let frames = run(&mut a, &tone(1000, 20_000, N * 6));
    let last = frames.last().expect("frames");
    assert!(last.level > 0, "a loud tone must register");
}

// ---- spectrum --------------------------------------------------------------

/// The band a given frequency falls in.
fn band_of(hz: u32) -> usize {
    let edges = band_edges(SR);
    let bin = (hz as usize * N) / SR as usize;
    (0..BANDS)
        .find(|&b| bin >= edges[b] && bin < edges[b + 1])
        .unwrap_or(BANDS - 1)
}

/// The contiguous run of bands that are lit at all.
fn lit_range(f: &AudioFrame) -> (usize, usize) {
    let lo = (0..BANDS).find(|&b| f.bands[b] > 0).unwrap_or(0);
    let hi = (0..BANDS).rev().find(|&b| f.bands[b] > 0).unwrap_or(0);
    (lo, hi)
}

#[test]
fn a_pure_tone_lights_the_band_it_belongs_to_and_its_neighbours() {
    // A windowed tone is a few bins wide and the AGC pins each of those bands
    // to 255, so asking which single band is loudest has no stable answer.
    // What is answerable, and what actually matters, is that the energy sits
    // around the right place and nowhere else.
    let mut a = analyzer();
    let hz = 1000;
    let frames = run(&mut a, &tone(hz, 24_000, N * 8));
    let f = frames.last().expect("frames");

    let want = band_of(hz);
    assert_eq!(f.bands[want], 255, "the tone's own band: {:?}", f.bands);
    let (lo, hi) = lit_range(f);
    assert!(
        lo + 2 >= want && hi <= want + 2,
        "1 kHz should light bands near {want}, lit {lo}..={hi}: {:?}",
        f.bands
    );
}

#[test]
fn a_low_tone_and_a_high_tone_light_disjoint_parts_of_the_spectrum() {
    let mut a = analyzer();
    let low = run(&mut a, &tone(200, 24_000, N * 8));
    let (low_lo, low_hi) = lit_range(low.last().expect("frames"));

    let mut b = analyzer();
    let high = run(&mut b, &tone(6000, 24_000, N * 8));
    let (high_lo, high_hi) = lit_range(high.last().expect("frames"));

    assert!(
        low_hi < high_lo,
        "200 Hz lit {low_lo}..={low_hi} and 6 kHz lit {high_lo}..={high_hi}; they must not overlap"
    );
}

// ---- automatic gain control ------------------------------------------------

#[test]
fn a_quiet_tone_and_a_loud_one_both_fill_the_range() {
    // That is what the AGC is for: a quiet room should not render dark.
    let loudest_of = |amp: i32| {
        let mut a = analyzer();
        let frames = run(&mut a, &tone(1000, amp, N * 8));
        let f = frames.last().expect("frames");
        *f.bands.iter().max().expect("bands")
    };
    let quiet = loudest_of(2_000);
    let loud = loudest_of(28_000);
    assert!(
        quiet > 200,
        "a quiet tone should still fill its band: {quiet}"
    );
    assert!(loud > 200, "a loud tone should too: {loud}");
}

#[test]
fn the_gain_control_decays_rather_than_latching() {
    // A single loud transient must not leave the display dark for the next
    // second. After the loud passage ends, a quiet one must climb back.
    let mut a = analyzer();
    run(&mut a, &tone(1000, 30_000, N * 4));
    let after_loud = run(&mut a, &tone(1000, 3_000, N * 2));
    let settled = run(&mut a, &tone(1000, 3_000, N * 30));

    let first = *after_loud
        .last()
        .expect("frames")
        .bands
        .iter()
        .max()
        .unwrap();
    let later = *settled.last().expect("frames").bands.iter().max().unwrap();
    assert!(
        later > first,
        "the quiet tone should recover as the peak decays: {first} then {later}"
    );
}

// ---- onsets and tempo ------------------------------------------------------

/// A click train: `bpm` beats a minute, each a short burst of a tone.
fn clicks(bpm: u32, beats: usize) -> alloc::vec::Vec<i16> {
    let period = (SR * 60 / bpm) as usize;
    let mut out = alloc::vec![0i16; period * beats];
    for b in 0..beats {
        let start = b * period;
        // 25 ms of tone, which is roughly a kick drum.
        let burst = tone(120, 30_000, SR as usize / 40);
        for (i, s) in burst.iter().enumerate() {
            if start + i < out.len() {
                out[start + i] = *s;
            }
        }
    }
    out
}

#[test]
fn a_click_train_produces_onsets() {
    let mut a = analyzer();
    let frames = run(&mut a, &clicks(120, 16));
    let onsets = frames.iter().filter(|f| f.onset).count();
    assert!(
        onsets >= 8,
        "expected roughly one onset per click, got {onsets}"
    );
}

#[test]
fn a_steady_tone_produces_far_fewer_onsets_than_a_click_train() {
    // An onset is a *rise* in energy. A tone that never changes has one, at the
    // start, and then nothing.
    let mut a = analyzer();
    let steady = run(&mut a, &tone(440, 24_000, SR as usize * 8));
    let steady_onsets = steady.iter().filter(|f| f.onset).count();

    let mut b = analyzer();
    let beat = run(&mut b, &clicks(120, 16));
    let beat_onsets = beat.iter().filter(|f| f.onset).count();

    assert!(
        beat_onsets > steady_onsets * 2,
        "clicks {beat_onsets} vs steady {steady_onsets}"
    );
}

#[test]
fn the_tempo_of_a_click_train_is_recovered() {
    // The estimator counts in frames, so the recoverable resolution is coarse
    // at the fast end; a whole band of a few BPM either way is a pass.
    for bpm in [90u32, 120, 150] {
        let mut a = analyzer();
        let frames = run(&mut a, &clicks(bpm, 24));
        let f = frames.last().expect("frames");
        assert!(f.bpm_x4 > 0, "{bpm}: no tempo estimated");

        let got = f.bpm_x4 as u32 / 4;
        // Half and double time are the classic ambiguity and are musically
        // defensible, so accept an octave either way.
        let ok = [(bpm as i64), (bpm as i64 * 2), (bpm as i64 / 2)]
            .iter()
            .any(|c| (got as i64 - c).abs() <= 8);
        assert!(ok, "{bpm} BPM was estimated as {got}");
    }
}

#[test]
fn silence_reports_no_tempo_and_no_confidence() {
    let mut a = analyzer();
    let frames = run(&mut a, &alloc::vec![0i16; N * 40]);
    let f = frames.last().expect("frames");
    assert_eq!(f.bpm_x4, 0, "silence has no tempo");
    assert_eq!(f.confidence, 0, "and nothing to be confident about");
    assert_eq!(f.beat_phase, 0);
}

#[test]
fn the_beat_phase_stays_inside_one_turn() {
    // It is published as a fraction of a beat and read as one. A value outside
    // 0..1 would make every effect that uses it jump.
    let mut a = analyzer();
    for f in run(&mut a, &clicks(120, 24)) {
        // A u16 phase cannot leave its range by construction; what it must
        // not do is stall, which the next test covers. Here: it is published
        // as a fraction of a beat, so before a tempo is known it must be zero
        // rather than some leftover value.
        if f.bpm_x4 == 0 {
            assert_eq!(f.beat_phase, 0, "no tempo means no phase");
        }
    }
}

#[test]
fn the_beat_phase_advances_monotonically_between_beats() {
    // Between corrections it must climb, or an effect extrapolating from it
    // would stutter — which is the whole reason phase is published rather than
    // beat events.
    let mut a = analyzer();
    let frames = run(&mut a, &clicks(120, 24));
    let tail = &frames[frames.len() / 2..];
    let mut rises = 0;
    for w in tail.windows(2) {
        if w[1].beat_phase > w[0].beat_phase {
            rises += 1;
        }
    }
    assert!(
        rises * 2 > tail.len(),
        "phase should rise on most frames, rose on {rises} of {}",
        tail.len()
    );
}

// ---- determinism -----------------------------------------------------------

#[test]
fn the_same_samples_analyse_identically_twice() {
    // Replay depends on this, and so does any conformance vector that ever
    // covers audio.
    let signal = clicks(128, 12);
    let mut a = analyzer();
    let mut b = analyzer();
    assert_eq!(run(&mut a, &signal), run(&mut b, &signal));
}

#[test]
fn the_analyser_holds_no_clock() {
    // Everything is counted in frames. Two analysers fed the same samples in
    // different sized chunks must agree, which they cannot if anything inside
    // depends on when `push` was called or how much arrived at once.
    let signal = clicks(120, 12);
    let mut a = analyzer();
    let whole = run(&mut a, &signal);

    let mut b = Analyzer::new(SR);
    let mut chunked = alloc::vec::Vec::new();
    for chunk in signal.chunks(97) {
        // 97 is deliberately not a factor of HOP.
        b.push(chunk, |f| chunked.push(f));
    }
    assert_eq!(
        whole.len(),
        chunked.len(),
        "the same samples must yield the same number of frames"
    );
    assert_eq!(whole, chunked, "and identical ones");
}
