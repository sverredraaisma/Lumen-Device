//! Simulated HAL and scenario harness — skeleton (W8 fills this in).
//!
//! Runs whole meshes on a simulated clock and a simulated network, with fault
//! injection (loss, jitter, partitions, reboots) and deterministic replay. No
//! wall-clock waiting: a 24-hour scenario runs in milliseconds.
//!
//! Doing this before the system grows is the cheapest it will ever be, and
//! everything built afterwards gets a test harness for free.

#![forbid(unsafe_code)]

/// A scenario is a seed plus a scripted list of events. Same seed and same
/// script, same run, byte for byte — that is the entire contract, and it is
/// what turns an irreproducible distributed bug into a checked-in test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Scenario {
    pub seed: u64,
    pub duration_us: u64,
}
