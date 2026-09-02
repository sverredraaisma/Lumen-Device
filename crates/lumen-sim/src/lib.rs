//! Simulated HAL, whole-mesh scenario harness and deterministic replay.
//!
//! `lumen-sim` is the *shell*. It owns everything `lumen-device` is forbidden to
//! touch — the clock, the sockets, the flash, the LEDs, the randomness — and
//! hands the results to the sans-IO cores as events. It may use `std` freely;
//! what it may not do is push any of that back across the boundary, because
//! three things depend on the core staying pure:
//!
//! 1. **Deterministic replay.** Record the event stream, feed it back, get an
//!    identical run. [`record`], [`replay`], [`verify_replay`].
//! 2. **Tests with no hardware, no network and no waiting.** Virtual time only
//!    advances when the runner says so, so a 24-hour clock-drift scenario is a
//!    few thousand arithmetic steps.
//! 3. **Behavioural conformance.** A run exports as a `lumen-spec` vector
//!    ([`export::to_behavioural_vector`]), so a bug reproduced here leaves a
//!    regression test behind for every implementation, not just this one.
//!
//! # Determinism is the product
//!
//! One seed drives everything: packet loss, jitter, delivery order, every byte
//! of node entropy. Nothing iterates a `HashMap`; ordered maps and explicit
//! sorts throughout. Two runs of the same [`Scenario`] produce byte-identical
//! traces, and there is a test that says so.
//!
//! ```
//! use lumen_sim::{record, verify_replay, IdleCore, NodeCore, NodeSpec, Op, Scenario};
//!
//! let scenario = Scenario::new(0xC0FFEE, 5_000_000)
//!     .with_nodes(3, 60)
//!     .at(1_000_000, Op::Partition(1, 2))
//!     .at(2_000_000, Op::Send { from: 1, to: 2, bytes: vec![0xAA] })
//!     .at(3_000_000, Op::HealAll);
//!
//! let factory = |_: &NodeSpec| -> Box<dyn NodeCore> { Box::new(IdleCore) };
//! let recorded = record(scenario, factory);
//! verify_replay(&recorded, factory).expect("same seed, same run");
//! ```
//!
//! # What is not here yet
//!
//! The mesh state machines (W5/W6/W7) do not exist: `lumen_device::Event` has
//! only `Tick` and `Action` only `SetTimer`. The harness is therefore complete
//! and the *behaviour* it drives is not. [`NodeCore::on_datagram`] marks the one
//! place that shows, and collapses into `on_event` the day a `Datagram` variant
//! lands.

#![forbid(unsafe_code)]

pub mod clock;
pub mod entropy;
pub mod export;
pub mod led;
pub mod net;
pub mod record;
pub mod rng;
pub mod scenario;
pub mod storage;
pub mod world;

pub use clock::{Ppm, SimClock, DEFAULT_SLEW_PPM};
pub use entropy::SimEntropy;
pub use export::to_behavioural_vector;
pub use led::{Frame, LedError, SimLedOut};
pub use net::{
    multicast_addr, node_addr, NetError, NetFaults, NetStats, NodeId, SimNet, SimNetwork, MTU,
};
pub use record::{record, replay, verify_replay, Divergence, ParseError, Recording};
pub use rng::SimRng;
pub use scenario::{NodeSpec, Op, Scenario, ScriptedOp};
pub use storage::{SimStorage, StorageError};
pub use world::{
    EventKind, IdleCore, Node, NodeCore, PeriodicCore, RunReport, TraceEntry, TraceKind, World,
};
