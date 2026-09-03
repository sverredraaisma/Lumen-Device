//! The world and the runner.
//!
//! A [`World`] is several simulated nodes on one simulated network and one
//! virtual clock. The runner does not tick: it jumps to the next instant at
//! which *something* is due — a scripted op, a datagram arriving, a timer a
//! core asked for — and settles the world there before jumping again. That is
//! why a twenty-four hour scenario with a message a minute costs 1440 steps
//! rather than 86 400 000, and it is the difference between a suite you run on
//! every commit and one you never run.

use lumen_device::{Action, Event};
use lumen_hal::{Clock, LedOut, Net, Rgbw, Storage};
use std::collections::BTreeMap;

use crate::clock::SimClock;
use crate::entropy::SimEntropy;
use crate::led::{fnv1a, fnv1a_new, LedError, SimLedOut};
use crate::net::{multicast_addr, node_addr, NetError, NetStats, NodeId, SimNet, SimNetwork, MTU};
use crate::scenario::{NodeSpec, Op, Scenario};
use crate::storage::{SimStorage, StorageError};

/// The sans-IO core the harness drives.
///
/// `lumen-device` owns the state machines; this trait is the shell's view of
/// one. It deliberately contains no I/O of any kind — events in, actions out —
/// so that plugging a real `lumen-device` machine in later is a one-line impl
/// and not a redesign.
pub trait NodeCore {
    /// The `on_event(now, ev) -> Vec<Action>` contract, unchanged.
    fn on_event(&mut self, now_us: u64, ev: Event<'_>) -> Vec<Action>;

    /// A datagram arrived.
    ///
    /// This is a seam, not a second contract. `lumen_device::Event` has no
    /// `Datagram` variant yet — W5/W6/W7 add it along with the state machines
    /// that need it — so the shell has nowhere to put the bytes. Rather than
    /// invent an `Event` variant in the simulator, which would put the wire
    /// format on the wrong side of the licence boundary and be wrong by the
    /// time the real one lands, the harness hands the bytes over here and
    /// delivers a `Tick` to wake the core. When the real variant exists this
    /// method collapses into `on_event` and every implementor loses a line.
    fn on_datagram(&mut self, _now_us: u64, _from: NodeId, _bytes: &[u8]) -> Vec<Action> {
        Vec::new()
    }
}

/// A core that does nothing. The right default for a node that only needs to
/// exist so that something else can talk to it.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdleCore;

impl NodeCore for IdleCore {
    fn on_event(&mut self, _now_us: u64, _ev: Event<'_>) -> Vec<Action> {
        Vec::new()
    }
}

/// A core that re-arms a timer on every event.
///
/// It exists to exercise the timer path — the one piece of runner machinery
/// with no other user until the real state machines land, and the one whose
/// bugs (a timer that never re-arms, a timer that fires twice) would otherwise
/// only surface once something depended on it.
#[derive(Clone, Copy, Debug)]
pub struct PeriodicCore {
    period_us: u64,
    fired: u64,
}

impl PeriodicCore {
    /// A core asking to be woken every `period_us`.
    pub fn new(period_us: u64) -> Self {
        Self {
            period_us,
            fired: 0,
        }
    }

    /// How many events it has seen.
    pub fn fired(&self) -> u64 {
        self.fired
    }
}

impl NodeCore for PeriodicCore {
    fn on_event(&mut self, _now_us: u64, _ev: Event<'_>) -> Vec<Action> {
        self.fired += 1;
        vec![Action::SetTimer {
            in_us: self.period_us,
        }]
    }

    fn on_datagram(&mut self, _now_us: u64, _from: NodeId, _bytes: &[u8]) -> Vec<Action> {
        self.fired += 1;
        vec![Action::SetTimer {
            in_us: self.period_us,
        }]
    }
}

/// One simulated device: a core plus the whole HAL.
pub struct Node {
    pub spec: NodeSpec,
    pub clock: SimClock,
    pub net: SimNet,
    pub storage: SimStorage,
    pub led: SimLedOut,
    pub entropy: SimEntropy,
    pub core: Box<dyn NodeCore>,
    powered: bool,
    timer_at_us: Option<u64>,
}

impl Node {
    /// Whether this node is powered.
    pub fn is_powered(&self) -> bool {
        self.powered
    }

    /// When this node's timer next fires, if it has one armed.
    pub fn timer_at_us(&self) -> Option<u64> {
        self.timer_at_us
    }
}

impl core::fmt::Debug for Node {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `Box<dyn NodeCore>` is not `Debug` and should not be forced to be:
        // requiring it would push a bound onto every state machine in
        // `lumen-device` for the benefit of the simulator alone.
        f.debug_struct("Node")
            .field("spec", &self.spec)
            .field("powered", &self.powered)
            .field("timer_at_us", &self.timer_at_us)
            .finish_non_exhaustive()
    }
}

/// One line of the run's history.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceEntry {
    pub at_us: u64,
    pub node: NodeId,
    pub kind: TraceKind,
}

/// What happened. Everything a run does that could differ between two runs is
/// in here, because "identical run" is defined as "identical trace" and
/// anything left out of the trace is something replay would not catch.
/// A borrow-free description of an [`Event`], for the trace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Tick,
    Datagram { len: usize },
    PeerDiscovered { prefix: [u8; 4] },
    PeerLost { prefix: [u8; 4] },
}

impl EventKind {
    pub fn of(ev: &Event<'_>) -> EventKind {
        match ev {
            Event::Tick => EventKind::Tick,
            Event::Datagram { bytes } => EventKind::Datagram { len: bytes.len() },
            Event::PeerDiscovered { prefix } => EventKind::PeerDiscovered { prefix: *prefix },
            Event::PeerLost { prefix } => EventKind::PeerLost { prefix: *prefix },
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TraceKind {
    /// A scripted op was applied. `tag` names the variant.
    Op { tag: &'static str },
    /// An event was delivered to a core.
    ///
    /// Recorded as a description rather than the event itself: `Event` borrows
    /// the received bytes, and a trace entry has to outlive the buffer they
    /// came from. The description is what a diff between two runs compares
    /// anyway.
    Event(EventKind),
    /// A core emitted an action.
    Action(Action),
    /// A datagram reached a core.
    Rx {
        from: NodeId,
        len: usize,
        digest: u64,
    },
    /// A frame was presented.
    Frame { digest: u64 },
    /// The HAL refused something. Failure paths are traced too: a run in which
    /// every write started failing must not compare equal to one in which they
    /// all succeeded.
    Net(NetError),
    /// Storage refused something.
    Store(StorageError),
    /// The LED output refused something.
    Led(LedError),
}

/// The result of a run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunReport {
    pub trace: Vec<TraceEntry>,
    pub stats: NetStats,
    pub end_us: u64,
    /// Per node: frames presented, and the digest over all of them.
    pub frames: BTreeMap<NodeId, (u64, u64)>,
}

impl RunReport {
    /// One number covering the whole run. Two runs are identical when their
    /// digests match; comparing the full traces says *where* they diverged.
    pub fn digest(&self) -> u64 {
        let mut h = fnv1a_new();
        h = fnv1a(h, &self.end_us.to_le_bytes());
        for entry in &self.trace {
            h = fnv1a(h, &entry.at_us.to_le_bytes());
            h = fnv1a(h, &entry.node.to_le_bytes());
            h = fnv1a(h, format!("{:?}", entry.kind).as_bytes());
        }
        for (node, (count, digest)) in &self.frames {
            h = fnv1a(h, &node.to_le_bytes());
            h = fnv1a(h, &count.to_le_bytes());
            h = fnv1a(h, &digest.to_le_bytes());
        }
        h
    }

    /// The first place two runs differ, as `(index, mine, theirs)`. `None` when
    /// they are identical up to the length of the shorter one.
    pub fn first_divergence(
        &self,
        other: &RunReport,
    ) -> Option<(usize, Option<TraceEntry>, Option<TraceEntry>)> {
        let n = self.trace.len().max(other.trace.len());
        for i in 0..n {
            let a = self.trace.get(i);
            let b = other.trace.get(i);
            if a != b {
                return Some((i, a.cloned(), b.cloned()));
            }
        }
        None
    }
}

/// How many settle passes one instant may take before the runner gives up.
///
/// Zero-latency sends can ping-pong: A wakes B, B wakes A, forever, all at the
/// same microsecond. A real network cannot do that but a simulated one with
/// `latency_us: 0` can, and a harness that hangs is worse than one that stops —
/// so the runner caps the passes and records that it did.
const MAX_SETTLE_PASSES: usize = 64;

/// Several nodes, one network, one virtual clock.
pub struct World {
    scenario: Scenario,
    net: SimNetwork,
    nodes: BTreeMap<NodeId, Node>,
    now_us: u64,
    cursor: usize,
    trace: Vec<TraceEntry>,
    settle_overruns: u64,
}

impl World {
    /// Build a world from a scenario, using `factory` to make each node's core.
    ///
    /// The factory is a closure rather than a stored type so a test can give
    /// one node a real state machine and the rest [`IdleCore`], which is how
    /// most single-behaviour scenarios are written.
    pub fn new(
        scenario: Scenario,
        mut factory: impl FnMut(&NodeSpec) -> Box<dyn NodeCore>,
    ) -> Self {
        let scenario = scenario.normalise();
        let mut net = SimNetwork::with_faults(scenario.seed, scenario.faults);
        let mut nodes = BTreeMap::new();
        for spec in &scenario.nodes {
            let sim_net = net.attach(spec.id);
            nodes.insert(
                spec.id,
                Node {
                    spec: *spec,
                    clock: SimClock::with_error(spec.skew_us, spec.drift_ppm),
                    net: sim_net,
                    storage: SimStorage::with_capacity(spec.storage_capacity),
                    led: SimLedOut::new(spec.pixel_count),
                    entropy: SimEntropy::for_node(scenario.seed, spec.id),
                    core: factory(spec),
                    powered: true,
                    timer_at_us: None,
                },
            );
        }
        Self {
            scenario,
            net,
            nodes,
            now_us: 0,
            cursor: 0,
            trace: Vec::new(),
            settle_overruns: 0,
        }
    }

    /// The scenario this world was built from.
    pub fn scenario(&self) -> &Scenario {
        &self.scenario
    }

    /// Current virtual time.
    pub fn now_us(&self) -> u64 {
        self.now_us
    }

    /// A node, for assertions.
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// A node, mutably. Tests reach for this to seed a core's state.
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(&id)
    }

    /// The network, for assertions and out-of-band fault injection.
    pub fn net(&self) -> &SimNetwork {
        &self.net
    }

    /// How many instants hit the settle cap. Non-zero means a scenario has a
    /// zero-latency loop in it and its results past that point are suspect.
    pub fn settle_overruns(&self) -> u64 {
        self.settle_overruns
    }

    /// Run to completion and report.
    pub fn run(&mut self) -> RunReport {
        while let Some(next) = self.next_instant() {
            if next > self.scenario.duration_us {
                break;
            }
            self.step_to(next);
        }
        // Advance the clocks to the declared end even when nothing is
        // scheduled there, so `duration_us` means what it says and a drift
        // assertion at the end of a quiet day still works.
        self.advance_time_to(self.scenario.duration_us);
        self.report()
    }

    /// Run one instant. Returns false when there is nothing left to do inside
    /// the scenario's duration. Exposed so a test can single-step.
    pub fn step(&mut self) -> bool {
        match self.next_instant() {
            Some(next) if next <= self.scenario.duration_us => {
                self.step_to(next);
                true
            }
            _ => false,
        }
    }

    /// The report as of now, without running further.
    pub fn report(&self) -> RunReport {
        RunReport {
            trace: self.trace.clone(),
            stats: self.net.stats(),
            end_us: self.now_us,
            frames: self
                .nodes
                .iter()
                .map(|(id, n)| (*id, (n.led.presented(), n.led.digest())))
                .collect(),
        }
    }

    /// The earliest instant at which anything is due.
    ///
    /// Timers at or before *now* are deliberately not counted. Everything due
    /// at the current instant has already been settled by [`Self::step_to`];
    /// the only way one is left over is that the settle cap cut a zero-delay
    /// loop short, and honouring it here would put the runner straight back
    /// into that loop and hang instead of stopping.
    fn next_instant(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        let consider = |best: &mut Option<u64>, t: u64| {
            let t = t.max(self.now_us);
            *best = Some(best.map_or(t, |b: u64| b.min(t)));
        };
        if let Some(op) = self.scenario.script.get(self.cursor) {
            consider(&mut best, op.at_us);
        }
        if let Some(t) = self.net.next_delivery_us() {
            consider(&mut best, t);
        }
        for node in self.nodes.values() {
            match node.timer_at_us {
                Some(t) if t > self.now_us => consider(&mut best, t),
                _ => {}
            }
        }
        best
    }

    fn step_to(&mut self, next: u64) {
        self.advance_time_to(next);
        self.apply_due_ops();
        // Settle: deliveries can arm timers, timers can send, sends with zero
        // latency are due immediately. Loop until the instant is quiet.
        let mut passes = 0;
        loop {
            self.net.advance_to(self.now_us);
            let worked = self.deliver_datagrams() | self.fire_timers();
            if !worked {
                break;
            }
            passes += 1;
            if passes >= MAX_SETTLE_PASSES {
                self.settle_overruns += 1;
                break;
            }
        }
    }

    fn advance_time_to(&mut self, next: u64) {
        if next <= self.now_us {
            return;
        }
        self.now_us = next;
        self.net.advance_to(next);
        for node in self.nodes.values_mut() {
            node.clock.advance_to(next);
            let node_now = node.clock.now_us();
            node.led.set_now_us(node_now);
        }
    }

    fn apply_due_ops(&mut self) {
        while let Some(scripted) = self.scenario.script.get(self.cursor) {
            if scripted.at_us > self.now_us {
                break;
            }
            let op = scripted.op.clone();
            self.cursor += 1;
            self.apply(&op);
        }
    }

    /// Apply one op. Public so a test can inject a fault without writing a
    /// whole scenario around it — and so does not have to reach into privates
    /// to do it.
    pub fn apply(&mut self, op: &Op) {
        let tag = op_tag(op);
        let node_for_trace = op_node(op);
        self.trace.push(TraceEntry {
            at_us: self.now_us,
            node: node_for_trace,
            kind: TraceKind::Op { tag },
        });
        match op {
            Op::Tick(id) => self.deliver_event(*id, Event::Tick),
            Op::Send { from, to, bytes } => {
                let addr = node_addr(*to);
                self.node_send(*from, &addr, bytes);
            }
            Op::Multicast { from, group, bytes } => {
                let addr = multicast_addr(*group);
                self.node_send(*from, &addr, bytes);
            }
            Op::Join { node, group } => {
                let addr = multicast_addr(*group);
                if let Some(n) = self.nodes.get_mut(node) {
                    if let Err(e) = n.net.join_multicast(&addr) {
                        self.trace.push(TraceEntry {
                            at_us: self.now_us,
                            node: *node,
                            kind: TraceKind::Net(e),
                        });
                    }
                }
            }
            Op::Kill(id) => {
                if let Some(n) = self.nodes.get_mut(id) {
                    n.powered = false;
                    n.timer_at_us = None;
                    n.led.set_powered(false);
                }
                self.net.set_powered(*id, false);
            }
            Op::Revive { node, wipe_storage } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    n.powered = true;
                    n.led.set_powered(true);
                    if *wipe_storage {
                        n.storage.wipe();
                    }
                    n.storage.reboot();
                }
                self.net.set_powered(*node, true);
            }
            Op::Partition(a, b) => self.net.partition(*a, *b),
            Op::Heal(a, b) => self.net.heal(*a, *b),
            Op::HealAll => self.net.heal_all(),
            Op::Split { left, right } => self.net.split(left, right),
            Op::SetFaults(f) => self.net.set_faults(*f),
            Op::Skew { node, offset_us } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    n.clock.set_skew_us(*offset_us);
                }
            }
            Op::Drift { node, ppm } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    n.clock.set_drift_ppm(*ppm);
                }
            }
            Op::Discipline { node, offset_us } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    n.clock.discipline(*offset_us);
                }
            }
            Op::Present { node, level } => self.node_present(*node, *level),
            Op::Store { node, key, value } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    if let Err(e) = n.storage.write(key, value) {
                        self.trace.push(TraceEntry {
                            at_us: self.now_us,
                            node: *node,
                            kind: TraceKind::Store(e),
                        });
                    }
                }
            }
            Op::Erase { node, key } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    let _ = n.storage.erase(key);
                }
            }
        }
    }

    fn node_send(&mut self, from: NodeId, addr: &lumen_hal::SocketAddr, bytes: &[u8]) {
        let Some(node) = self.nodes.get_mut(&from) else {
            return;
        };
        if let Err(e) = node.net.send_to(addr, bytes) {
            self.trace.push(TraceEntry {
                at_us: self.now_us,
                node: from,
                kind: TraceKind::Net(e),
            });
        }
    }

    fn node_present(&mut self, id: NodeId, level: u16) {
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        let pixels = vec![
            Rgbw {
                r: level,
                g: level,
                b: level,
                w: 0,
            };
            node.led.pixel_count()
        ];
        match node.led.present(&pixels) {
            Ok(()) => {
                let digest = node.led.digest();
                self.trace.push(TraceEntry {
                    at_us: self.now_us,
                    node: id,
                    kind: TraceKind::Frame { digest },
                });
            }
            Err(e) => self.trace.push(TraceEntry {
                at_us: self.now_us,
                node: id,
                kind: TraceKind::Led(e),
            }),
        }
    }

    /// Drain every node's socket, in node id order. Ordering matters: whose
    /// packets are processed first is observable, and a `HashMap` here would
    /// make two runs of the same scenario disagree.
    fn deliver_datagrams(&mut self) -> bool {
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        let mut worked = false;
        let mut buf = [0u8; MTU];
        for id in ids {
            while let Some(node) = self.nodes.get_mut(&id) {
                if !node.powered {
                    break;
                }
                let now = node.clock.now_us();
                match node.net.recv(&mut buf) {
                    Ok(None) => break,
                    Ok(Some((len, from))) => {
                        worked = true;
                        let from_id = from_node_id(&from);
                        let digest = fnv1a(fnv1a_new(), &buf[..len]);
                        let actions = node
                            .core
                            .on_event(now, Event::Datagram { bytes: &buf[..len] });
                        self.trace.push(TraceEntry {
                            at_us: self.now_us,
                            node: id,
                            kind: TraceKind::Rx {
                                from: from_id,
                                len,
                                digest,
                            },
                        });
                        self.apply_actions(id, &actions);
                    }
                    Err(e) => {
                        worked = true;
                        self.trace.push(TraceEntry {
                            at_us: self.now_us,
                            node: id,
                            kind: TraceKind::Net(e),
                        });
                    }
                }
            }
        }
        worked
    }

    fn fire_timers(&mut self) -> bool {
        let due: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.powered && n.timer_at_us.is_some_and(|t| t <= self.now_us))
            .map(|(id, _)| *id)
            .collect();
        let worked = !due.is_empty();
        for id in due {
            if let Some(node) = self.nodes.get_mut(&id) {
                node.timer_at_us = None;
            }
            self.deliver_event(id, Event::Tick);
        }
        worked
    }

    fn deliver_event(&mut self, id: NodeId, ev: Event) {
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if !node.powered {
            return;
        }
        let now = node.clock.now_us();
        let actions = node.core.on_event(now, ev);
        self.trace.push(TraceEntry {
            at_us: self.now_us,
            node: id,
            kind: TraceKind::Event(EventKind::of(&ev)),
        });
        self.apply_actions(id, &actions);
    }

    fn apply_actions(&mut self, id: NodeId, actions: &[Action]) {
        for action in actions {
            self.trace.push(TraceEntry {
                at_us: self.now_us,
                node: id,
                kind: TraceKind::Action(action.clone()),
            });
            match action {
                Action::SetTimer { in_us } => {
                    if let Some(node) = self.nodes.get_mut(&id) {
                        // Earliest wins. A core that asks for 10 ms and then
                        // 1 s in the same batch wants to be woken in 10 ms;
                        // taking the last would silently drop the tighter
                        // deadline.
                        let at = self.now_us.saturating_add(*in_us);
                        node.timer_at_us = Some(node.timer_at_us.map_or(at, |t| t.min(at)));
                    }
                }
                Action::Send { .. }
                | Action::DisciplineClock { .. }
                | Action::RoleChanged { .. }
                | Action::SyncLost
                | Action::SyncAcquired => {
                    // Traced above, and that is enough for now. Wiring `Send`
                    // into the fabric and `DisciplineClock` into `SimClock` is
                    // what turns the harness-level scenarios in tests/ into
                    // behavioural ones; doing it here without the source stack
                    // and render loop would only be able to assert half of what
                    // those scenarios are for.
                }
            }
        }
    }
}

/// Recover a node id from its simulated address. The address scheme is
/// `10.0.<hi>.<lo>`, so this is exact for anything the fabric itself routed.
fn from_node_id(addr: &lumen_hal::SocketAddr) -> NodeId {
    match addr.ip {
        lumen_hal::IpAddr::V4(o) => ((o[2] as u16) << 8) | o[3] as u16,
        lumen_hal::IpAddr::V6(_) => 0,
    }
}

/// A stable name per op variant, for the trace and the exported vector.
pub fn op_tag(op: &Op) -> &'static str {
    match op {
        Op::Tick(_) => "tick",
        Op::Send { .. } => "send",
        Op::Multicast { .. } => "multicast",
        Op::Join { .. } => "join",
        Op::Kill(_) => "kill",
        Op::Revive { .. } => "revive",
        Op::Partition(_, _) => "partition",
        Op::Heal(_, _) => "heal",
        Op::HealAll => "heal_all",
        Op::Split { .. } => "split",
        Op::SetFaults(_) => "set_faults",
        Op::Skew { .. } => "skew",
        Op::Drift { .. } => "drift",
        Op::Discipline { .. } => "discipline",
        Op::Present { .. } => "present",
        Op::Store { .. } => "store",
        Op::Erase { .. } => "erase",
    }
}

/// The node an op is about, or 0 for the ones that are about the whole world.
pub fn op_node(op: &Op) -> NodeId {
    match op {
        Op::Tick(id) | Op::Kill(id) | Op::Partition(id, _) | Op::Heal(id, _) => *id,
        Op::Send { from, .. } | Op::Multicast { from, .. } => *from,
        Op::Join { node, .. }
        | Op::Revive { node, .. }
        | Op::Skew { node, .. }
        | Op::Drift { node, .. }
        | Op::Discipline { node, .. }
        | Op::Present { node, .. }
        | Op::Store { node, .. }
        | Op::Erase { node, .. } => *node,
        Op::HealAll | Op::Split { .. } | Op::SetFaults(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::NetFaults;
    use crate::scenario::NodeSpec;

    fn idle(_: &NodeSpec) -> Box<dyn NodeCore> {
        Box::new(IdleCore)
    }

    /// A core that counts what it was given, so a test can assert delivery
    /// without depending on the (currently trivial) `Event` enum.
    #[derive(Default)]
    struct CountingCore {
        events: u64,
        datagrams: u64,
        rearm_us: Option<u64>,
    }

    impl NodeCore for CountingCore {
        fn on_event(&mut self, _now_us: u64, _ev: Event<'_>) -> Vec<Action> {
            self.events += 1;
            self.rearm_us
                .map(|in_us| vec![Action::SetTimer { in_us }])
                .unwrap_or_default()
        }

        fn on_datagram(&mut self, _now_us: u64, _from: NodeId, _bytes: &[u8]) -> Vec<Action> {
            self.datagrams += 1;
            Vec::new()
        }
    }

    #[test]
    fn a_world_builds_its_nodes() {
        let s = Scenario::new(1, 1_000).with_nodes(3, 8);
        let w = World::new(s, idle);
        assert_eq!(w.now_us(), 0);
        assert_eq!(w.node(2).unwrap().spec.pixel_count, 8);
        assert!(w.node(9).is_none());
        assert_eq!(w.scenario().nodes.len(), 3);
        assert!(w.node(1).unwrap().is_powered());
        assert_eq!(w.node(1).unwrap().timer_at_us(), None);
        assert!(format!("{:?}", w.node(1).unwrap()).contains("Node"));
    }

    #[test]
    fn a_tick_reaches_the_core() {
        let s = Scenario::new(1, 1_000)
            .with_nodes(1, 1)
            .at(100, Op::Tick(1));
        let mut w = World::new(s, idle);
        let report = w.run();
        assert_eq!(report.end_us, 1_000);
        let kinds: Vec<&TraceKind> = report.trace.iter().map(|e| &e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                &TraceKind::Op { tag: "tick" },
                &TraceKind::Event(EventKind::Tick)
            ]
        );
    }

    #[test]
    fn a_timer_re_arms_and_fires_on_schedule() {
        let s = Scenario::new(1, 10_000).with_nodes(1, 0).at(0, Op::Tick(1));
        let mut w = World::new(s, |_| Box::new(PeriodicCore::new(1_000)));
        let report = w.run();
        let fires = report
            .trace
            .iter()
            .filter(|e| e.kind == TraceKind::Event(EventKind::Tick))
            .count();
        // One scripted tick at 0, then one every millisecond up to 10 000.
        assert_eq!(fires, 11);
        let times: Vec<u64> = report
            .trace
            .iter()
            .filter(|e| e.kind == TraceKind::Event(EventKind::Tick))
            .map(|e| e.at_us)
            .collect();
        assert_eq!(times.first(), Some(&0));
        assert_eq!(times.last(), Some(&10_000));
    }

    #[test]
    fn the_earliest_requested_timer_wins() {
        struct TwoTimers;
        impl NodeCore for TwoTimers {
            fn on_event(&mut self, _now_us: u64, _ev: Event<'_>) -> Vec<Action> {
                vec![
                    Action::SetTimer { in_us: 1_000_000 },
                    Action::SetTimer { in_us: 500 },
                ]
            }
        }
        let s = Scenario::new(1, 1_000).with_nodes(1, 0).at(0, Op::Tick(1));
        let mut w = World::new(s, |_| Box::new(TwoTimers));
        w.step();
        assert_eq!(w.node(1).unwrap().timer_at_us(), Some(500));
    }

    #[test]
    fn a_timer_beyond_the_duration_never_fires() {
        let s = Scenario::new(1, 500).with_nodes(1, 0).at(0, Op::Tick(1));
        let mut w = World::new(s, |_| Box::new(PeriodicCore::new(10_000)));
        let report = w.run();
        assert_eq!(
            report
                .trace
                .iter()
                .filter(|e| e.kind == TraceKind::Event(EventKind::Tick))
                .count(),
            1
        );
        assert_eq!(report.end_us, 500);
    }

    #[test]
    fn a_datagram_reaches_the_other_core() {
        let s = Scenario::new(1, 1_000).with_nodes(2, 0).at(
            10,
            Op::Send {
                from: 1,
                to: 2,
                bytes: vec![1, 2, 3],
            },
        );
        let mut w = World::new(s, |_| Box::new(CountingCore::default()));
        let report = w.run();
        let rx: Vec<&TraceEntry> = report
            .trace
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::Rx { .. }))
            .collect();
        assert_eq!(rx.len(), 1);
        assert_eq!(rx[0].node, 2);
        assert!(matches!(
            rx[0].kind,
            TraceKind::Rx {
                from: 1,
                len: 3,
                ..
            }
        ));
        assert_eq!(report.stats.delivered, 1);
    }

    #[test]
    fn multicast_reaches_the_group() {
        let s = Scenario::new(1, 1_000)
            .with_nodes(3, 0)
            .at(0, Op::Join { node: 2, group: 1 })
            .at(0, Op::Join { node: 3, group: 1 })
            .at(
                10,
                Op::Multicast {
                    from: 1,
                    group: 1,
                    bytes: vec![9],
                },
            );
        let mut w = World::new(s, idle);
        let report = w.run();
        assert_eq!(
            report
                .trace
                .iter()
                .filter(|e| matches!(e.kind, TraceKind::Rx { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn a_partition_stops_delivery_and_healing_restores_it() {
        let s = Scenario::new(1, 1_000)
            .with_nodes(2, 0)
            .at(0, Op::Partition(1, 2))
            .at(
                10,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![1],
                },
            )
            .at(20, Op::HealAll)
            .at(
                30,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![2],
                },
            );
        let mut w = World::new(s, idle);
        let report = w.run();
        let rx: Vec<&TraceEntry> = report
            .trace
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::Rx { .. }))
            .collect();
        assert_eq!(rx.len(), 1);
        assert_eq!(rx[0].at_us, 30);
        assert_eq!(report.stats.dropped_partition, 1);
    }

    #[test]
    fn split_and_heal_are_scriptable() {
        let s = Scenario::new(1, 100)
            .with_nodes(3, 0)
            .at(
                0,
                Op::Split {
                    left: vec![1],
                    right: vec![2, 3],
                },
            )
            .at(10, Op::Heal(1, 2));
        let mut w = World::new(s, idle);
        w.run();
        assert!(!w.net().is_partitioned(1, 2));
        assert!(w.net().is_partitioned(1, 3));
    }

    #[test]
    fn killing_a_node_silences_it_and_reviving_wakes_it() {
        let s = Scenario::new(1, 100_000)
            .with_nodes(2, 1)
            .at(0, Op::Tick(1))
            .at(5_000, Op::Kill(1))
            .at(
                6_000,
                Op::Send {
                    from: 2,
                    to: 1,
                    bytes: vec![1],
                },
            )
            .at(
                7_000,
                Op::Revive {
                    node: 1,
                    wipe_storage: false,
                },
            )
            .at(
                8_000,
                Op::Send {
                    from: 2,
                    to: 1,
                    bytes: vec![2],
                },
            );
        let mut w = World::new(s, |_| Box::new(PeriodicCore::new(1_000)));
        let report = w.run();
        let rx: Vec<&TraceEntry> = report
            .trace
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::Rx { .. }))
            .collect();
        assert_eq!(rx.len(), 1, "only the post-revive datagram lands");
        assert_eq!(rx[0].at_us, 8_000);

        // The timer train stops while the node is dead and only restarts when
        // something wakes it — a dead node has no pending work to come back to.
        let ticks_while_dead = report
            .trace
            .iter()
            .filter(|e| e.node == 1 && e.kind == TraceKind::Event(EventKind::Tick))
            .filter(|e| (5_000..7_000).contains(&e.at_us))
            .count();
        assert_eq!(ticks_while_dead, 0);
    }

    #[test]
    fn a_reboot_keeps_storage_and_a_wipe_does_not() {
        let s = Scenario::new(1, 100)
            .with_nodes(2, 0)
            .at(
                0,
                Op::Store {
                    node: 1,
                    key: "id".into(),
                    value: vec![7],
                },
            )
            .at(
                0,
                Op::Store {
                    node: 2,
                    key: "id".into(),
                    value: vec![7],
                },
            )
            .at(10, Op::Kill(1))
            .at(10, Op::Kill(2))
            .at(
                20,
                Op::Revive {
                    node: 1,
                    wipe_storage: false,
                },
            )
            .at(
                20,
                Op::Revive {
                    node: 2,
                    wipe_storage: true,
                },
            );
        let mut w = World::new(s, idle);
        w.run();
        assert_eq!(w.node(1).unwrap().storage.get("id"), Some(&[7u8][..]));
        assert_eq!(w.node(2).unwrap().storage.get("id"), None);
        assert_eq!(w.node(1).unwrap().storage.boots(), 2);
    }

    #[test]
    fn storage_and_erase_failures_are_traced() {
        let s = Scenario::new(1, 100)
            .with_node(NodeSpec::new(1, 0).with_storage_capacity(4))
            .at(
                0,
                Op::Store {
                    node: 1,
                    key: "k".into(),
                    value: vec![0; 10],
                },
            )
            .at(
                1,
                Op::Erase {
                    node: 1,
                    key: "k".into(),
                },
            );
        let mut w = World::new(s, idle);
        let report = w.run();
        assert!(report
            .trace
            .iter()
            .any(|e| e.kind == TraceKind::Store(StorageError::Full)));
    }

    #[test]
    fn frames_are_recorded_and_a_dead_output_is_traced() {
        let s = Scenario::new(1, 100)
            .with_nodes(1, 4)
            .at(0, Op::Present { node: 1, level: 10 })
            .at(10, Op::Kill(1))
            .at(20, Op::Present { node: 1, level: 20 });
        let mut w = World::new(s, idle);
        let report = w.run();
        assert_eq!(report.frames[&1].0, 1);
        assert!(report
            .trace
            .iter()
            .any(|e| e.kind == TraceKind::Led(LedError::PoweredOff)));
        assert!(!w.node(1).unwrap().led.is_dark());
    }

    #[test]
    fn oversize_sends_are_traced_as_net_errors() {
        let s = Scenario::new(1, 100).with_nodes(2, 0).at(
            0,
            Op::Send {
                from: 1,
                to: 2,
                bytes: vec![0; MTU + 1],
            },
        );
        let mut w = World::new(s, idle);
        let report = w.run();
        assert!(report
            .trace
            .iter()
            .any(|e| e.kind == TraceKind::Net(NetError::PayloadTooLarge)));
    }

    #[test]
    fn ops_aimed_at_unknown_nodes_are_ignored() {
        let s = Scenario::new(1, 100)
            .with_nodes(1, 1)
            .at(0, Op::Tick(9))
            .at(0, Op::Kill(9))
            .at(
                0,
                Op::Revive {
                    node: 9,
                    wipe_storage: true,
                },
            )
            .at(
                0,
                Op::Send {
                    from: 9,
                    to: 1,
                    bytes: vec![1],
                },
            )
            .at(
                0,
                Op::Multicast {
                    from: 9,
                    group: 1,
                    bytes: vec![1],
                },
            )
            .at(0, Op::Join { node: 9, group: 1 })
            .at(
                0,
                Op::Skew {
                    node: 9,
                    offset_us: 1,
                },
            )
            .at(0, Op::Drift { node: 9, ppm: 1 })
            .at(
                0,
                Op::Discipline {
                    node: 9,
                    offset_us: 1,
                },
            )
            .at(0, Op::Present { node: 9, level: 1 })
            .at(
                0,
                Op::Store {
                    node: 9,
                    key: "k".into(),
                    value: vec![1],
                },
            )
            .at(
                0,
                Op::Erase {
                    node: 9,
                    key: "k".into(),
                },
            );
        let mut w = World::new(s, idle);
        let report = w.run();
        // Every op is traced; none of them does anything.
        assert_eq!(
            report
                .trace
                .iter()
                .filter(|e| matches!(e.kind, TraceKind::Op { .. }))
                .count(),
            12
        );
        assert_eq!(report.frames[&1].0, 0);
    }

    #[test]
    fn clock_ops_move_a_nodes_time() {
        let s = Scenario::new(1, 1_000_000)
            .with_nodes(1, 0)
            .at(
                0,
                Op::Skew {
                    node: 1,
                    offset_us: 1_000,
                },
            )
            .at(1, Op::Drift { node: 1, ppm: 100 })
            .at(
                2,
                Op::Discipline {
                    node: 1,
                    offset_us: 200,
                },
            );
        let mut w = World::new(s, idle);
        w.run();
        let node = w.node(1).unwrap();
        assert_eq!(node.clock.skew_us(), 1_000);
        assert_eq!(node.clock.drift_ppm(), 100);
        assert_eq!(node.clock.pending_slew_us(), 0, "slewed off over a second");
        assert!(node.clock.now_us() > 1_001_000);
    }

    #[test]
    fn faults_can_be_turned_on_mid_run() {
        let s = Scenario::new(1, 1_000)
            .with_nodes(2, 0)
            .at(
                0,
                Op::SetFaults(NetFaults {
                    loss_permille: 1000,
                    ..NetFaults::perfect()
                }),
            )
            .at(
                10,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![1],
                },
            )
            .at(20, Op::SetFaults(NetFaults::perfect()))
            .at(
                30,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![2],
                },
            );
        let mut w = World::new(s, idle);
        let report = w.run();
        assert_eq!(report.stats.dropped_loss, 1);
        assert_eq!(
            report
                .trace
                .iter()
                .filter(|e| matches!(e.kind, TraceKind::Rx { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn stepping_stops_when_there_is_nothing_left() {
        let s = Scenario::new(1, 1_000).with_nodes(1, 0).at(10, Op::Tick(1));
        let mut w = World::new(s, idle);
        assert!(w.step());
        assert_eq!(w.now_us(), 10);
        assert!(!w.step());
        assert_eq!(w.report().end_us, 10);
    }

    #[test]
    fn a_world_with_nothing_scheduled_still_ends_at_its_duration() {
        let s = Scenario::new(1, 86_400_000_000).with_nodes(2, 0);
        let mut w = World::new(s, idle);
        let report = w.run();
        assert_eq!(report.end_us, 86_400_000_000);
        assert!(report.trace.is_empty());
        assert_eq!(w.node(1).unwrap().clock.now_us(), 86_400_000_000);
    }

    #[test]
    fn an_op_scheduled_past_the_duration_never_runs() {
        let s = Scenario::new(1, 100).with_nodes(1, 0).at(500, Op::Tick(1));
        let mut w = World::new(s, idle);
        let report = w.run();
        assert!(report.trace.is_empty());
        assert_eq!(report.end_us, 100);
    }

    #[test]
    fn apply_can_be_called_directly_for_ad_hoc_faults() {
        let s = Scenario::new(1, 100).with_nodes(2, 0);
        let mut w = World::new(s, idle);
        w.apply(&Op::Partition(1, 2));
        assert!(w.net().is_partitioned(1, 2));
    }

    #[test]
    fn node_mut_lets_a_test_reach_the_hal() {
        let s = Scenario::new(1, 100).with_nodes(1, 0);
        let mut w = World::new(s, idle);
        w.node_mut(1).unwrap().storage.write("k", b"v").unwrap();
        assert_eq!(w.node(1).unwrap().storage.get("k"), Some(&b"v"[..]));
        assert!(w.node_mut(7).is_none());
    }

    #[test]
    fn a_zero_latency_ping_pong_stops_instead_of_hanging() {
        // A core that sends nothing cannot loop, so the loop is built out of
        // the runner's own machinery: a timer of zero re-armed on every event.
        let s = Scenario::new(1, 1_000).with_nodes(1, 0).at(0, Op::Tick(1));
        let mut w = World::new(s, |_| Box::new(PeriodicCore::new(0)));
        let report = w.run();
        assert_eq!(w.settle_overruns(), 1);
        assert!(report.trace.len() < 500, "the cap held");
    }

    #[test]
    fn the_report_digest_reflects_the_trace() {
        let build = |bytes: Vec<u8>| {
            let s = Scenario::new(1, 1_000).with_nodes(2, 0).at(
                10,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes,
                },
            );
            World::new(s, idle).run()
        };
        let a = build(vec![1]);
        let b = build(vec![1]);
        let c = build(vec![2]);
        assert_eq!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
        assert_eq!(a.first_divergence(&b), None);
        let (idx, mine, theirs) = a.first_divergence(&c).unwrap();
        assert!(mine.is_some() && theirs.is_some());
        assert!(idx > 0);
    }

    #[test]
    fn divergence_reports_a_missing_tail() {
        let short = Scenario::new(1, 1_000).with_nodes(1, 0);
        let long = Scenario::new(1, 1_000).with_nodes(1, 0).at(1, Op::Tick(1));
        let a = World::new(short, idle).run();
        let b = World::new(long, idle).run();
        let (idx, mine, theirs) = a.first_divergence(&b).unwrap();
        assert_eq!(idx, 0);
        assert!(mine.is_none());
        assert!(theirs.is_some());
    }

    #[test]
    fn op_tags_are_unique_per_variant() {
        let ops = vec![
            Op::Tick(1),
            Op::Send {
                from: 1,
                to: 2,
                bytes: vec![],
            },
            Op::Multicast {
                from: 1,
                group: 0,
                bytes: vec![],
            },
            Op::Join { node: 1, group: 0 },
            Op::Kill(1),
            Op::Revive {
                node: 1,
                wipe_storage: false,
            },
            Op::Partition(1, 2),
            Op::Heal(1, 2),
            Op::HealAll,
            Op::Split {
                left: vec![],
                right: vec![],
            },
            Op::SetFaults(NetFaults::perfect()),
            Op::Skew {
                node: 1,
                offset_us: 0,
            },
            Op::Drift { node: 1, ppm: 0 },
            Op::Discipline {
                node: 1,
                offset_us: 0,
            },
            Op::Present { node: 1, level: 0 },
            Op::Store {
                node: 1,
                key: String::new(),
                value: vec![],
            },
            Op::Erase {
                node: 1,
                key: String::new(),
            },
        ];
        let mut tags: Vec<&str> = ops.iter().map(op_tag).collect();
        let count = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), count);
        // World-level ops are attributed to node 0.
        assert_eq!(op_node(&Op::HealAll), 0);
        assert_eq!(op_node(&Op::Tick(4)), 4);
        assert_eq!(op_node(&Op::Present { node: 6, level: 0 }), 6);
    }

    #[test]
    fn a_v6_sender_maps_to_node_zero() {
        // The fabric never produces one, but `from_node_id` is total and a
        // future IPv6 address scheme must not silently alias a real node.
        let addr = lumen_hal::SocketAddr {
            ip: lumen_hal::IpAddr::V6([0; 16]),
            port: 1,
        };
        assert_eq!(from_node_id(&addr), 0);
    }

    #[test]
    fn a_short_recv_buffer_is_impossible_at_mtu() {
        // The runner reads into an MTU-sized buffer, so the BufferTooSmall path
        // is unreachable from a scenario. Pin that: if the buffer ever shrinks,
        // this is the test that explains why datagrams started vanishing.
        assert_eq!(MTU, 1200);
    }

    #[test]
    fn idle_and_periodic_cores_behave() {
        let mut idle_core = IdleCore;
        assert!(idle_core.on_event(0, Event::Tick).is_empty());
        assert!(idle_core.on_datagram(0, 1, &[]).is_empty());
        let mut p = PeriodicCore::new(5);
        assert_eq!(p.fired(), 0);
        assert_eq!(
            p.on_event(0, Event::Tick),
            vec![Action::SetTimer { in_us: 5 }]
        );
        assert_eq!(
            p.on_datagram(0, 1, &[]),
            vec![Action::SetTimer { in_us: 5 }]
        );
        assert_eq!(p.fired(), 2);
    }

    #[test]
    fn a_counting_core_sees_both_kinds_of_input() {
        let s = Scenario::new(1, 1_000)
            .with_nodes(2, 0)
            .at(0, Op::Tick(2))
            .at(
                10,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![1],
                },
            );
        let mut w = World::new(s, |_| {
            Box::new(CountingCore {
                rearm_us: None,
                ..Default::default()
            })
        });
        w.run();
        // The core is behind a `dyn` box, so assert through the trace instead.
        let report = w.report();
        assert_eq!(
            report
                .trace
                .iter()
                .filter(|e| e.node == 2)
                .filter(|e| matches!(e.kind, TraceKind::Event(_) | TraceKind::Rx { .. }))
                .count(),
            2
        );
    }
}
