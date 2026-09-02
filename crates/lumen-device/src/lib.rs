//! Mesh state machines — skeleton (W5, W6, W7 fill this in).
//!
//! Sans-IO throughout: every state machine here is `on_event(now, ev) ->
//! Vec<Action>`. The shell around it owns the sockets, the timers and the flash
//! writes; this crate cannot reach them, which is what makes replay
//! deterministic and behavioural conformance testable.

#![forbid(unsafe_code)]

/// Everything the outside world can tell a node about.
///
/// The variants land with their workstreams; `Tick` exists from the start
/// because a sans-IO core has no other way to notice that time passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The shell advanced the show clock.
    Tick,
}

/// Everything a node can ask the outside world to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Call back in at most this many microseconds.
    SetTimer { in_us: u64 },
}

/// A source of pixels claiming a zone, with a priority and an expiry.
///
/// One of the four cross-cutting rules: **priority and timeout on everything**.
/// Programs, streams, manual control and status overrides are all this. Nothing
/// can permanently capture a pixel, and a dead publisher releases its claim on
/// its own — which is how a room does not end up stuck red at 3am.
///
/// A source above the ambient floor with no expiry is a bug, and the tooling
/// should refuse it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SourceClaim {
    pub priority: u8,
    /// Show time at which this claim lapses. `None` is only legal at the
    /// ambient floor.
    pub expires_at_us: Option<u64>,
}
