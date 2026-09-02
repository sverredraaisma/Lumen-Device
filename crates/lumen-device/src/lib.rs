//! Mesh state machines.
//!
//! Sans-IO throughout: every state machine here is `on_event(now, ev) ->
//! Vec<Action>`. The shell around it owns the sockets, the timers and the flash
//! writes; this crate cannot reach them, which is what makes replay
//! deterministic and behavioural conformance testable.
//!
//! # What is here
//!
//! - [`sync`] — the time-sync state machine. Unsynced → Syncing → Synced, with
//!   RTT filtering and a clock that **slews and never steps**.
//! - [`election`] — timebase election. Compares capacity only, never load.
//! - [`Node`] — the two together, decoding datagrams and emitting them.
//! - [`sources`] — the source stack. One mechanism for shows, schedules,
//!   alerts, manual control and streams, with priority and expiry on every one.
//! - [`zones`] — zone selectors and projections, evaluated on each device
//!   against its own LEDs so a zone never has to be resolved centrally.
//! - [`records`] — replicated state: hybrid logical clocks, who may sign what,
//!   and gossip. Every record is signed, because the mesh key is symmetric and
//!   a shared secret alone would let any paired device forge a scene.
//! - [`channels`] — the broadcast uniforms an effect reads, with claim-and-lease
//!   ownership and a defined decay when a producer dies.
//! - [`render`] — the render loop, where the stack, the zones and the VM meet
//!   and a pixel finally gets a colour.
//!
//! # What is not
//!
//! Discovery is deliberately absent. Finding peers is mDNS, a broadcast probe,
//! or a static list depending on the transport — all of it I/O, none of it a
//! decision. The shell tells the core about a peer with
//! [`Event::PeerDiscovered`] and the core does not care how it found out.

#![forbid(unsafe_code)]

extern crate alloc;

pub mod channels;
pub mod election;
pub mod node;
pub mod records;
pub mod render;
pub mod sources;
pub mod sync;
pub mod zones;

use lumen_proto::Uuid;

pub use channels::{Channel, Channels};
pub use election::{Candidacy, Election, Role};
pub use node::Node;
pub use records::{Authority, Hlc, Record, RecordType, Store};
pub use render::{Renderer, Rgb};
pub use sources::{Source, SourceStack};
pub use sync::{Sync, SyncState};
pub use zones::{MapQuality, Projection, Zone};

/// Microseconds on the show clock.
pub type ShowTimeUs = u64;

/// How a datagram should be sent.
///
/// The core says *what kind of reach* a message needs, not which socket. A
/// `TICK` goes to everyone; a `SYNC_REQ` goes to one peer. The shell maps that
/// onto multicast groups and addresses, which is exactly the knowledge it has
/// and the core does not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Destination {
    /// Every device in the mesh.
    Mesh,
    /// One peer, identified by the prefix that appears in a header.
    Peer([u8; 4]),
}

/// Everything the outside world can tell a node about.
///
/// Borrowed rather than owned: a datagram is parsed in place and never copied,
/// which is what keeps the receive path allocation-free on a device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event<'a> {
    /// A timer the core asked for has fired.
    Tick,
    /// Bytes arrived. The core parses them; the shell does not look inside.
    Datagram { bytes: &'a [u8] },
    /// The shell found a peer, however it does that.
    PeerDiscovered { prefix: [u8; 4] },
    /// The shell lost a peer.
    PeerLost { prefix: [u8; 4] },
}

/// Everything a node can ask the outside world to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Call back in at most this many microseconds.
    ///
    /// A hint, not a contract: waking late is a quality problem, waking early is
    /// free. The core recomputes what it owes on every event.
    SetTimer { in_us: u64 },
    /// Send these bytes. Already framed and ready for the wire.
    Send { to: Destination, datagram: Vec<u8> },
    /// Move the show clock by this much, by **slewing the rate**.
    ///
    /// Never a step. A stepped render clock is a visible glitch, and every
    /// effect is a function of this clock.
    DisciplineClock { offset_us: i64 },
    /// This node took or lost the timebase.
    RoleChanged { role: Role, epoch: u32 },
    /// Suppress tightly-synced content: the clock is not trustworthy.
    ///
    /// Rendering a synchronised show while unsynced looks broken in a way that
    /// suggests a hardware fault. Not rendering it is the defined degradation.
    SyncLost,
    /// The clock is trustworthy again.
    SyncAcquired,
}

/// Who this device is, for election purposes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    pub uuid: Uuid,
    /// Static benchmark score, "VM instructions per second ÷ 1000".
    ///
    /// **Never current load.** Load changes constantly, so electing on it makes
    /// the role flap between devices, and a role that flaps is worse than a
    /// slightly suboptimal one that holds. Load is advisory and used for budgets
    /// and the UI, nowhere near this.
    pub capacity: u32,
}

impl Identity {
    pub fn new(uuid: Uuid, capacity: u32) -> Identity {
        Identity { uuid, capacity }
    }

    pub fn candidacy(&self) -> Candidacy {
        Candidacy {
            capacity: self.capacity,
            uuid: self.uuid,
        }
    }

    pub fn prefix(&self) -> [u8; 4] {
        self.uuid.sender_prefix()
    }
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
