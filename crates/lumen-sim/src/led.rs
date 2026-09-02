//! A simulated strip that remembers what it was asked to show.
//!
//! Rendering is the only part of the system a person can see, so "what did the
//! mesh actually put on the wall" has to be assertable. The fourth project rule
//! — every failure has a *defined visual outcome* — is untestable without a
//! recording of the frames, because "a device is never dark because of
//! software" is a statement about pixels, not about state.

use lumen_hal::{LedOut, Rgbw};

/// Default number of frames kept.
///
/// A 24-hour scenario at 60 fps is five million frames; keeping them all would
/// turn a millisecond test into a gigabyte. The ring keeps the recent past,
/// which is what an assertion after the fact actually looks at, and the running
/// digest below covers everything that fell out of it.
pub const DEFAULT_FRAME_HISTORY: usize = 256;

/// What can go wrong presenting a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedError {
    /// The frame is not the length this output drives. A real driver would
    /// happily paint a short frame and leave the tail from last time on the
    /// wall, which looks like a hardware fault; refusing it makes the bug land
    /// where it was written.
    WrongPixelCount,
    /// The device is powered down. Presenting to a dead strip is a harness bug.
    PoweredOff,
}

/// One presented frame, stamped with the show time the harness was at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Frame {
    pub at_us: u64,
    pub pixels: Vec<Rgbw>,
}

impl Frame {
    /// Whether every channel of every pixel is zero — "the strip is dark",
    /// which by rule 4 should almost never be the answer to a failure.
    pub fn is_dark(&self) -> bool {
        self.pixels
            .iter()
            .all(|p| p.r == 0 && p.g == 0 && p.b == 0 && p.w == 0)
    }
}

/// A recording LED output.
#[derive(Clone, Debug)]
pub struct SimLedOut {
    pixel_count: usize,
    history: usize,
    frames: std::collections::VecDeque<Frame>,
    presented: u64,
    /// FNV-1a over every frame ever presented, including the ones the ring has
    /// dropped. This is what a determinism assertion compares: one number, and
    /// it covers a run of any length.
    digest: u64,
    now_us: u64,
    powered: bool,
}

/// FNV-1a offset basis and prime. Chosen over anything cryptographic because
/// the digest only has to detect *divergence between two runs of the same
/// code*, not resist an adversary, and it has to be identical on every host.
const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Fold bytes into an FNV-1a accumulator. Shared with the trace digest so that
/// "the run is identical" is one comparison across frames and actions alike.
pub fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A fresh FNV-1a accumulator.
pub fn fnv1a_new() -> u64 {
    FNV_OFFSET
}

impl SimLedOut {
    /// An output driving `pixel_count` pixels, keeping the default history.
    pub fn new(pixel_count: usize) -> Self {
        Self::with_history(pixel_count, DEFAULT_FRAME_HISTORY)
    }

    /// An output with an explicit history depth. Zero keeps no frames at all
    /// but still accumulates the digest and the count, which is the right
    /// setting for a long soak scenario.
    pub fn with_history(pixel_count: usize, history: usize) -> Self {
        Self {
            pixel_count,
            history,
            frames: std::collections::VecDeque::new(),
            presented: 0,
            digest: fnv1a_new(),
            now_us: 0,
            powered: true,
        }
    }

    /// Tell the output what time it is. `LedOut::present` carries no timestamp
    /// — a driver has no clock — so the harness stamps frames from outside,
    /// which is also the honest model: the time on a frame is the time the
    /// shell thought it was.
    pub fn set_now_us(&mut self, now_us: u64) {
        self.now_us = now_us;
    }

    /// Power the output on or off.
    pub fn set_powered(&mut self, on: bool) {
        self.powered = on;
    }

    /// Whether the output is powered.
    pub fn is_powered(&self) -> bool {
        self.powered
    }

    /// Frames still in the ring, oldest first.
    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter()
    }

    /// The most recent frame, if any.
    pub fn last_frame(&self) -> Option<&Frame> {
        self.frames.back()
    }

    /// Total frames presented over the run, including those the ring dropped.
    pub fn presented(&self) -> u64 {
        self.presented
    }

    /// Digest over every frame ever presented.
    pub fn digest(&self) -> u64 {
        self.digest
    }

    /// Whether the strip is currently dark. `true` with no frames at all:
    /// a device that has never rendered is dark, and that is exactly the state
    /// rule 4 says must not be reachable through a software failure.
    pub fn is_dark(&self) -> bool {
        self.last_frame().map(|f| f.is_dark()).unwrap_or(true)
    }

    /// Forget the recorded frames. The digest and the count carry on, so
    /// clearing history cannot be used to hide divergence.
    pub fn clear_history(&mut self) {
        self.frames.clear();
    }
}

impl LedOut for SimLedOut {
    type Error = LedError;

    fn pixel_count(&self) -> usize {
        self.pixel_count
    }

    fn present(&mut self, pixels: &[Rgbw]) -> Result<(), Self::Error> {
        if !self.powered {
            return Err(LedError::PoweredOff);
        }
        if pixels.len() != self.pixel_count {
            return Err(LedError::WrongPixelCount);
        }
        self.digest = fnv1a(self.digest, &self.now_us.to_le_bytes());
        for p in pixels {
            self.digest = fnv1a(self.digest, &p.r.to_le_bytes());
            self.digest = fnv1a(self.digest, &p.g.to_le_bytes());
            self.digest = fnv1a(self.digest, &p.b.to_le_bytes());
            self.digest = fnv1a(self.digest, &p.w.to_le_bytes());
        }
        self.presented += 1;
        if self.history > 0 {
            if self.frames.len() == self.history {
                self.frames.pop_front();
            }
            self.frames.push_back(Frame {
                at_us: self.now_us,
                pixels: pixels.to_vec(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(count: usize, level: u16) -> Vec<Rgbw> {
        vec![
            Rgbw {
                r: level,
                g: level,
                b: level,
                w: 0
            };
            count
        ]
    }

    #[test]
    fn frames_are_recorded_with_a_timestamp() {
        let mut led = SimLedOut::new(3);
        assert_eq!(led.pixel_count(), 3);
        led.set_now_us(1_234);
        led.present(&flat(3, 100)).unwrap();
        let frame = led.last_frame().unwrap();
        assert_eq!(frame.at_us, 1_234);
        assert_eq!(frame.pixels.len(), 3);
        assert_eq!(led.presented(), 1);
        assert_eq!(led.frames().count(), 1);
    }

    #[test]
    fn a_wrong_length_frame_is_refused() {
        let mut led = SimLedOut::new(3);
        assert_eq!(led.present(&flat(2, 1)), Err(LedError::WrongPixelCount));
        assert_eq!(led.present(&flat(4, 1)), Err(LedError::WrongPixelCount));
        assert_eq!(led.presented(), 0);
    }

    #[test]
    fn a_powered_down_output_refuses_frames() {
        let mut led = SimLedOut::new(1);
        led.set_powered(false);
        assert!(!led.is_powered());
        assert_eq!(led.present(&flat(1, 5)), Err(LedError::PoweredOff));
        led.set_powered(true);
        assert!(led.present(&flat(1, 5)).is_ok());
    }

    #[test]
    fn a_zero_pixel_output_is_legal() {
        // Audio, sensor and control-surface nodes implement no rendering; a
        // zero-length present must not be special-cased anywhere.
        let mut led = SimLedOut::new(0);
        led.present(&[]).unwrap();
        assert_eq!(led.presented(), 1);
        assert!(led.last_frame().unwrap().is_dark());
    }

    #[test]
    fn history_is_a_ring() {
        let mut led = SimLedOut::with_history(1, 3);
        for i in 0..10u16 {
            led.set_now_us(i as u64);
            led.present(&flat(1, i)).unwrap();
        }
        let times: Vec<u64> = led.frames().map(|f| f.at_us).collect();
        assert_eq!(times, vec![7, 8, 9]);
        assert_eq!(led.presented(), 10);
    }

    #[test]
    fn zero_history_still_counts_and_digests() {
        let mut led = SimLedOut::with_history(1, 0);
        let empty = led.digest();
        led.present(&flat(1, 7)).unwrap();
        assert_eq!(led.frames().count(), 0);
        assert_eq!(led.last_frame(), None);
        assert_eq!(led.presented(), 1);
        assert_ne!(led.digest(), empty);
    }

    #[test]
    fn clearing_history_leaves_the_digest_alone() {
        let mut led = SimLedOut::new(1);
        led.present(&flat(1, 7)).unwrap();
        let digest = led.digest();
        led.clear_history();
        assert_eq!(led.frames().count(), 0);
        assert_eq!(led.digest(), digest);
        assert_eq!(led.presented(), 1);
    }

    #[test]
    fn darkness_is_detectable() {
        let mut led = SimLedOut::new(2);
        assert!(led.is_dark(), "never rendered counts as dark");
        led.present(&flat(2, 0)).unwrap();
        assert!(led.is_dark());
        led.present(&flat(2, 1)).unwrap();
        assert!(!led.is_dark());
    }

    #[test]
    fn the_white_channel_counts_towards_lit() {
        let mut led = SimLedOut::new(1);
        led.present(&[Rgbw {
            r: 0,
            g: 0,
            b: 0,
            w: 9,
        }])
        .unwrap();
        assert!(!led.is_dark());
    }

    #[test]
    fn identical_frame_sequences_digest_identically() {
        let run = || {
            let mut led = SimLedOut::new(4);
            for i in 0..20u16 {
                led.set_now_us(i as u64 * 1000);
                led.present(&flat(4, i)).unwrap();
            }
            led.digest()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn the_digest_notices_a_changed_pixel() {
        let mut a = SimLedOut::new(2);
        let mut b = SimLedOut::new(2);
        a.present(&flat(2, 5)).unwrap();
        b.present(&flat(2, 6)).unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn the_digest_notices_a_changed_timestamp() {
        let mut a = SimLedOut::new(1);
        let mut b = SimLedOut::new(1);
        a.set_now_us(1);
        a.present(&flat(1, 5)).unwrap();
        b.set_now_us(2);
        b.present(&flat(1, 5)).unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn fnv_is_the_standard_one() {
        // Pins the constants: a recording's digests have to keep meaning the
        // same thing across refactors.
        assert_eq!(fnv1a(fnv1a_new(), b""), FNV_OFFSET);
        assert_eq!(fnv1a(fnv1a_new(), b"a"), 0xAF63_DC4C_8601_EC8C);
        assert_eq!(fnv1a(fnv1a_new(), b"foobar"), 0x8594_4171_F739_67E8);
    }
}
