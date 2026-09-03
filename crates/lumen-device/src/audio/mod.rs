//! Audio analysis.
//!
//! Analysis happens at the source, once, and only the result is broadcast —
//! never raw audio. This module is that analysis, and like everything else in
//! this crate it performs no I/O: the shell captures samples from I2S, a line
//! input or desktop loopback and hands them in.

pub mod fft;

pub mod analyzer;
