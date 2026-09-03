//! Turning samples into the numbers an effect actually reads.
//!
//! The output shape is fixed by `docs/effects.md`: 32 log-spaced band
//! magnitudes, an overall level and a smoothed one, and an onset flag with a
//! beat phase, a tempo and a confidence.
//!
//! # Why phase and not beats
//!
//! Publishing beat *phase* rather than beat *events* is what makes this survive
//! a lossy network. A receiver that misses a packet can still extrapolate where
//! in the bar it is, so an effect stays on beat instead of stuttering — and a
//! beat that arrives 40 ms late as an event is worse than useless, because the
//! flash lands after the drum.
//!
//! # No wall clock
//!
//! Everything here is timed in frames, and the frame rate follows from the
//! sample rate and the hop size. Nothing reads a clock, so a recorded stream of
//! samples analyses identically on replay — which is the only way a beat
//! tracker's misbehaviour can ever be reproduced and fixed.

use lumen_proto::audio::{AudioFrame, BANDS};
use lumen_vm::q16::Q16;

use super::fft::{magnitudes, Fft, N};

/// Samples between analysis windows. Half the window, so successive frames
/// overlap by 50% and a transient cannot fall between two of them.
pub const HOP: usize = N / 2;

/// How many frames of onset strength the tempo estimator looks back over.
///
/// At 48 kHz and a 512-sample hop this is 2.7 seconds, which is two full bars
/// at 90 BPM — enough for the autocorrelation to see a repeat even at the slow
/// end of the range.
const HISTORY: usize = 256;

/// The tempo range searched, in BPM. Outside this a "beat" is either a bar line
/// or a note, and locking to either produces lighting that feels wrong.
const MIN_BPM: u32 = 60;
const MAX_BPM: u32 = 200;

/// Streaming analyser. Feed it samples; it hands back a frame every [`HOP`].
pub struct Analyzer {
    fft: Fft,
    sample_rate: u32,
    window: [Q16; N],
    edges: [usize; BANDS + 1],

    /// The most recent `N` samples, as a ring. `write` is where the next one
    /// goes, and therefore also the oldest sample in the buffer.
    buf: [Q16; N],
    write: usize,
    /// How many new samples have arrived since the last frame.
    since_frame: usize,
    /// How many samples have ever arrived, so the first window is not analysed
    /// before it is full of real audio.
    seen: usize,

    /// Per-band running peak, for automatic gain control.
    peak: [Q16; BANDS],
    /// Previous frame's band magnitudes, for spectral flux.
    prev: [Q16; BANDS],
    smoothed: Q16,

    /// Onset strength per frame, newest at `flux_at - 1`.
    flux: [Q16; HISTORY],
    flux_at: usize,
    flux_filled: usize,

    /// Beat phase, advanced every frame by one over the estimated period.
    phase: Q16,
    period_frames: u32,
    confidence: u8,
}

impl Analyzer {
    /// `sample_rate` in Hz. 48000 and 44100 are the ones that matter; anything
    /// is accepted, and only the band edges and the tempo scale depend on it.
    pub fn new(sample_rate: u32) -> Analyzer {
        let mut window = [Q16::ZERO; N];
        for (n, w) in window.iter_mut().enumerate() {
            // Hann: 0.5 - 0.5·cos(2πn/N). Without it a tone between two bins
            // smears across the whole spectrum and every band lights up.
            let turns = Q16::from_ratio(n as i32, N as i32);
            *w = Q16((Q16::HALF.0 - turns.cos_turns().0 / 2).max(0));
        }

        Analyzer {
            fft: Fft::new(),
            sample_rate,
            window,
            edges: band_edges(sample_rate),
            buf: [Q16::ZERO; N],
            write: 0,
            since_frame: 0,
            seen: 0,
            peak: [Q16::ZERO; BANDS],
            prev: [Q16::ZERO; BANDS],
            smoothed: Q16::ZERO,
            flux: [Q16::ZERO; HISTORY],
            flux_at: 0,
            flux_filled: 0,
            phase: Q16::ZERO,
            period_frames: 0,
            confidence: 0,
        }
    }

    /// Frames produced per second, which is what the tempo estimator counts in.
    pub fn frame_rate(&self) -> u32 {
        self.sample_rate / HOP as u32
    }

    /// Feed samples, calling `on_frame` for each analysis window they complete.
    ///
    /// A closure rather than a return value because one call can complete any
    /// number of windows: the shell hands over whatever the capture buffer held,
    /// which has nothing to do with [`HOP`]. Returning a single frame would
    /// silently drop the rest, and the number analysed would then depend on how
    /// the caller happened to chunk its reads.
    pub fn push(&mut self, samples: &[i16], mut on_frame: impl FnMut(AudioFrame)) {
        for &s in samples {
            // A ring: the write position is also the oldest sample, so nothing
            // moves. Shifting the window along instead would be N copies per
            // sample — 49 million a second at 48 kHz, which is not a rounding
            // error on a C3, it is the whole chip.
            self.buf[self.write] = Q16((s as i32) * 2);
            self.write = (self.write + 1) % N;
            self.seen += 1;
            self.since_frame += 1;

            if self.since_frame >= HOP {
                self.since_frame = 0;
                // Not before the window holds real audio: analysing a
                // half-filled buffer reports its zeros as though they were
                // signal, and the first frame would show a spectrum that is
                // mostly the absence of one.
                if self.seen >= N {
                    let f = self.analyze();
                    on_frame(f);
                }
            }
        }
    }

    fn analyze(&mut self) -> AudioFrame {
        let mut re = [Q16::ZERO; N];
        let mut im = [Q16::ZERO; N];
        for (i, (r, w)) in re.iter_mut().zip(self.window.iter()).enumerate() {
            // Oldest first: the ring's write position is the oldest sample.
            let s = self.buf[(self.write + i) % N];
            *r = Q16(((s.0 as i64 * w.0 as i64) >> 16) as i32);
        }
        self.fft.transform(&mut re, &mut im);

        let mut mag = [Q16::ZERO; N / 2];
        magnitudes(&re, &im, &mut mag);

        // Band energies: the mean magnitude across each band's bins, so a wide
        // high band is not louder than a narrow low one purely by covering more
        // of the spectrum.
        let mut raw = [Q16::ZERO; BANDS];
        for (b, out) in raw.iter_mut().enumerate() {
            let (lo, hi) = (self.edges[b], self.edges[b + 1].max(self.edges[b] + 1));
            let mut sum: i64 = 0;
            for m in mag.iter().take(hi.min(N / 2)).skip(lo) {
                sum += m.0 as i64;
            }
            let count = (hi.min(N / 2) - lo).max(1) as i64;
            *out = Q16((sum / count) as i32);
        }

        let bands = self.normalise(&raw);
        let flux = self.spectral_flux(&raw);
        self.prev = raw;

        let level = self.overall_level(&bands);
        let onset = self.record_flux(flux);
        self.track_tempo();
        self.advance_phase(onset);

        AudioFrame {
            bands,
            level,
            smoothed_level: (self.smoothed.0 >> 8).clamp(0, 255) as u8,
            onset,
            // The phase is tracked in Q16 because that is what the arithmetic
            // is in, and published as the fraction alone: it wraps, it has no
            // integer part, and a u16 wraps at exactly the right place.
            beat_phase: (self.phase.0 & 0xFFFF) as u16,
            bpm_x4: self.bpm_x4(),
            confidence: self.confidence,
        }
    }

    /// Per-band automatic gain control.
    ///
    /// A band is reported relative to its own recent peak, so quiet music fills
    /// the range and a loud passage does not sit pinned at 255. The decay is
    /// slow enough that a single loud transient does not flatten the next
    /// second of the display.
    fn normalise(&mut self, raw: &[Q16; BANDS]) -> [u8; BANDS] {
        let mut out = [0u8; BANDS];
        for ((o, peak), r) in out.iter_mut().zip(self.peak.iter_mut()).zip(raw.iter()) {
            let decayed = peak.0 - (peak.0 >> 7);
            *peak = Q16(decayed.max(r.0));

            // A floor, so silence is reported as silence rather than amplified
            // into whatever noise the microphone has.
            const FLOOR: i32 = 16;
            *o = if peak.0 <= FLOOR {
                0
            } else {
                (((r.0 as i64) * 255) / peak.0 as i64).clamp(0, 255) as u8
            };
        }
        out
    }

    fn overall_level(&mut self, bands: &[u8; BANDS]) -> u8 {
        let sum: u32 = bands.iter().map(|&b| b as u32).sum();
        let level = (sum / BANDS as u32).min(255) as u8;
        // Smoothing is an exponential moving average in Q16, one eighth of the
        // way to the new value each frame: about 80 ms at a 94 Hz frame rate.
        let target = Q16((level as i32) << 8);
        self.smoothed = Q16(self.smoothed.0 + ((target.0 - self.smoothed.0) >> 3));
        level
    }

    /// Spectral flux: how much energy *rose* since the last frame.
    ///
    /// Only rises count. A note ending is not an onset, and counting falls too
    /// would make every gap in the music look like a drum hit.
    fn spectral_flux(&self, raw: &[Q16; BANDS]) -> Q16 {
        let mut sum: i64 = 0;
        for (now, before) in raw.iter().zip(self.prev.iter()) {
            let d = now.0 - before.0;
            if d > 0 {
                sum += d as i64;
            }
        }
        Q16((sum / BANDS as i64) as i32)
    }

    /// Store the flux and decide whether this frame is an onset.
    ///
    /// The threshold is relative to the recent mean rather than absolute:
    /// music gets louder and quieter, and an absolute threshold would find
    /// every frame of a loud passage to be an onset and none of a quiet one.
    fn record_flux(&mut self, flux: Q16) -> bool {
        self.flux[self.flux_at] = flux;
        self.flux_at = (self.flux_at + 1) % HISTORY;
        self.flux_filled = (self.flux_filled + 1).min(HISTORY);

        if self.flux_filled < 8 {
            return false;
        }
        let mean: i64 = self
            .flux
            .iter()
            .take(self.flux_filled)
            .map(|f| f.0 as i64)
            .sum::<i64>()
            / self.flux_filled as i64;
        // Half again above the mean, and non-trivial in absolute terms so a
        // silent room does not beat.
        flux.0 as i64 > mean * 3 / 2 && flux.0 > 8
    }

    /// Estimate the beat period by autocorrelating the onset-strength history.
    ///
    /// A tempo is a repeat in *when* energy rises, not in the audio itself, so
    /// the correlation runs over the flux history rather than the samples.
    fn track_tempo(&mut self) {
        let fr = self.frame_rate().max(1);
        let min_lag = ((fr * 60) / MAX_BPM).max(2) as usize;
        let max_lag = ((fr * 60) / MIN_BPM).min(HISTORY as u32 / 2) as usize;
        if self.flux_filled < max_lag * 2 || max_lag <= min_lag {
            return;
        }

        // Oldest-first view of the history.
        let at = |i: usize| -> i64 {
            let idx = (self.flux_at + HISTORY - self.flux_filled + i) % HISTORY;
            self.flux[idx].0 as i64
        };
        let n = self.flux_filled;

        // Correlate the flux about its mean, not its raw value. Onset strength
        // is never negative, so a raw correlation is dominated by the constant
        // part and peaks at the shortest lag on offer regardless of the music.
        let mean_flux: i64 = (0..n).map(at).sum::<i64>() / n as i64;

        let mut best_lag = 0usize;
        let mut best = 0i64;
        let mut total = 0i64;
        for lag in min_lag..=max_lag {
            let mut acc = 0i64;
            for i in lag..n {
                // No pre-scaling: flux values are tens of raw units, and
                // shifting them down before multiplying rounded every product
                // to zero and left the estimator with a flat correlation and
                // nothing to choose between.
                acc += (at(i) - mean_flux) * (at(i - lag) - mean_flux);
            }
            // Longer lags correlate over fewer samples, so normalise by the
            // count or the estimator always prefers the shortest period.
            let score = acc / (n - lag) as i64;
            total += score;
            if score > best {
                best = score;
                best_lag = lag;
            }
        }

        if best_lag == 0 || best <= 0 {
            self.confidence = 0;
            return;
        }
        // Confidence is how far the winner stands above the average candidate.
        // A flat correlation means every tempo fits equally, which means none
        // of them does.
        let mean = total / (max_lag - min_lag + 1) as i64;
        self.confidence = if mean <= 0 {
            0
        } else {
            (((best - mean) * 255) / (best.max(1))).clamp(0, 255) as u8
        };
        self.period_frames = best_lag as u32;
    }

    /// Move the phase on by one frame, and pull it toward a confident onset.
    fn advance_phase(&mut self, onset: bool) {
        if self.period_frames == 0 {
            self.phase = Q16::ZERO;
            return;
        }
        let step = Q16::from_ratio(1, self.period_frames as i32);
        self.phase = Q16((self.phase.0 + step.0) & 0xFFFF);

        // An onset is evidence that the beat is *now*, so nudge the phase back
        // toward zero rather than jumping it there. A jump would make the
        // extrapolation between packets discontinuous, which is the thing
        // publishing phase instead of events exists to avoid.
        if onset && self.confidence > 64 {
            let err = if self.phase.0 > Q16::HALF.0 {
                self.phase.0 - Q16::ONE.0
            } else {
                self.phase.0
            };
            self.phase = Q16((self.phase.0 - (err >> 2)) & 0xFFFF);
        }
    }

    fn bpm_x4(&self) -> u16 {
        if self.period_frames == 0 {
            return 0;
        }
        let bpm4 = (self.frame_rate() as u64 * 60 * 4) / self.period_frames as u64;
        bpm4.min(u16::MAX as u64) as u16
    }
}

/// Log-spaced bin edges from about 40 Hz to Nyquist.
///
/// Log spacing because hearing is logarithmic: linear bands would give
/// twenty-nine of the thirty-two to frequencies above 5 kHz, where almost
/// nothing musically interesting happens, and lump every bass note into one.
fn band_edges(sample_rate: u32) -> [usize; BANDS + 1] {
    let nyquist_bin = (N / 2) as u32;
    let lowest_hz = 40u32;
    let mut edges = [0usize; BANDS + 1];
    // bin index of `lowest_hz`, rounded down but at least 1 so DC is excluded.
    let lo_bin = ((lowest_hz as u64 * N as u64) / sample_rate as u64).max(1) as u32;

    for (b, e) in edges.iter_mut().enumerate() {
        // Geometric interpolation from lo_bin to nyquist_bin, done in integers:
        // ratio^(b/BANDS) with ratio = nyquist/lo. Repeated multiplication in
        // Q16 keeps it exact enough and avoids any floating point.
        let t = Q16::from_ratio(b as i32, BANDS as i32);
        // lo * (hi/lo)^t == lo * 2^(t * log2(hi/lo))
        let ratio = Q16::from_ratio(nyquist_bin as i32, lo_bin as i32);
        let exponent =
            Q16(((t.0 as i64 * ratio.log2().unwrap_or(Q16::ZERO).0 as i64) >> 16) as i32);
        let scale = exponent.exp2();
        let bin = ((lo_bin as i64 * scale.0 as i64) >> 16) as usize;
        *e = bin.clamp(lo_bin as usize, nyquist_bin as usize);
    }
    edges[BANDS] = nyquist_bin as usize;
    // Guarantee the edges are non-decreasing and each band has at least one
    // bin, which the low end otherwise loses to rounding.
    for b in 1..=BANDS {
        if edges[b] <= edges[b - 1] {
            edges[b] = (edges[b - 1] + 1).min(nyquist_bin as usize);
        }
    }
    edges
}

#[cfg(test)]
#[path = "analyzer_tests.rs"]
mod tests;
