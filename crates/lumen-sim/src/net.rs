//! An in-memory mesh network with faults.
//!
//! A real mesh fails in five ways that matter to the state machines: packets
//! vanish, they arrive late, they arrive out of order, they arrive twice, and
//! sometimes two halves of the house cannot see each other at all. All five are
//! modelled here, all five are driven from the seeded PRNG, and none of them
//! needs a socket.
//!
//! The fabric is shared: every node's [`SimNet`] is a handle onto one
//! [`SimNetwork`], because a network that each node owned a private copy of
//! would not be a network. Sharing is `Rc<RefCell<..>>` rather than
//! `Arc<Mutex<..>>` on purpose — the simulator is single-threaded by
//! construction, and a thread would reintroduce exactly the nondeterminism the
//! harness exists to remove.

use core::cell::RefCell;
use lumen_hal::{IpAddr, Net, SocketAddr};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use crate::rng::SimRng;

/// A node's identity inside a world. Small, ordered, and used as the iteration
/// key everywhere — ordering is what makes "deliver to every group member"
/// reproducible.
pub type NodeId = u16;

/// Largest datagram the simulated fabric will carry.
///
/// The wire format fixes this at 1200, and `the_simulated_fabric_carries_what_
/// the_wire_format_allows` asserts the two agree. Restated rather than imported
/// because the codec is a dev-dependency here: the simulated network moves
/// bytes and never looks inside them, and pulling the codec into the library to
/// borrow one integer would be the wrong trade.
///
/// A simulator that happily delivered 1400 bytes would pass a bug that only
/// ever appears on somebody's VPN.
pub const MTU: usize = 1200;

/// The port every simulated node listens on.
pub const NODE_PORT: u16 = 5680;

/// What can go wrong at the API level. Loss, partition and a powered-off peer
/// are deliberately *not* here: on a real network those are silent, and a
/// state machine that only works because `send` told it the truth is a state
/// machine that will not work.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetError {
    /// The sending node is powered off. This one is an error rather than a
    /// silent drop because it is a harness bug, not a network condition.
    NodeDown,
    /// Payload above [`MTU`]. Fragmentation is the caller's problem.
    PayloadTooLarge,
    /// `recv` was handed a buffer smaller than the waiting datagram. Real UDP
    /// truncates; the simulator refuses, because silent truncation is how a
    /// codec bug survives a test suite.
    BufferTooSmall,
}

/// The fault model. All-zero means a perfect network, which is the right
/// default: a test that fails on a perfect network has a real bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NetFaults {
    /// Probability in per mille that a datagram is dropped outright.
    pub loss_permille: u16,
    /// Fixed one-way delay.
    pub latency_us: u64,
    /// Extra delay drawn uniformly from `0..=jitter_us`. Jitter alone already
    /// reorders traffic, which is the realistic way it happens.
    pub jitter_us: u64,
    /// Probability in per mille that a datagram is additionally held back by
    /// `reorder_extra_us`. This is the *pathological* reordering — one packet
    /// arriving long after its successors — that jitter alone rarely produces
    /// and that sequence-number handling gets wrong.
    pub reorder_permille: u16,
    /// How far behind a reordered datagram falls.
    pub reorder_extra_us: u64,
    /// Probability in per mille that a datagram is delivered twice. Duplicates
    /// are what a retransmitting sender plus a slow link look like, and an
    /// idempotence bug is invisible without them.
    pub duplicate_permille: u16,
}

impl NetFaults {
    /// A network that behaves.
    pub fn perfect() -> Self {
        Self::default()
    }

    /// A plausible domestic WiFi mesh: a little loss, a few milliseconds of
    /// latency, and jitter of the same order.
    pub fn lossy_wifi() -> Self {
        Self {
            loss_permille: 20,
            latency_us: 3_000,
            jitter_us: 4_000,
            reorder_permille: 5,
            reorder_extra_us: 40_000,
            duplicate_permille: 2,
        }
    }
}

/// Counters over a run. Cheap, and the first thing worth looking at when a
/// scenario behaves oddly — "nothing was delivered" and "everything was
/// delivered twice" look identical from the state machine's side.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NetStats {
    pub sent: u64,
    pub delivered: u64,
    pub dropped_loss: u64,
    pub dropped_partition: u64,
    pub dropped_unreachable: u64,
    pub duplicated: u64,
    pub reordered: u64,
}

/// An orderable address. `lumen_hal::SocketAddr` is intentionally minimal and
/// implements neither `Ord` nor `Hash`, so the fabric derives its own key —
/// and a `BTreeMap` on it, never a `HashMap`, because delivery order to a
/// multicast group has to be the same on every run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct AddrKey {
    v6: bool,
    host: [u8; 16],
    port: u16,
}

fn addr_key(addr: &SocketAddr) -> AddrKey {
    let mut host = [0u8; 16];
    let v6 = match addr.ip {
        IpAddr::V4(o) => {
            host[..4].copy_from_slice(&o);
            false
        }
        IpAddr::V6(o) => {
            host.copy_from_slice(&o);
            true
        }
    };
    AddrKey {
        v6,
        host,
        port: addr.port,
    }
}

/// True for the address ranges the mesh uses for group traffic. Channels ride
/// multicast, so "is this a group?" is a routing decision the fabric has to
/// make on every send.
pub fn is_multicast(addr: &SocketAddr) -> bool {
    match addr.ip {
        IpAddr::V4(o) => (224..=239).contains(&o[0]),
        IpAddr::V6(o) => o[0] == 0xFF,
    }
}

/// The address a node with this id answers on: `10.0.<hi>.<lo>:5680`.
pub fn node_addr(id: NodeId) -> SocketAddr {
    SocketAddr {
        ip: IpAddr::V4([10, 0, (id >> 8) as u8, (id & 0xFF) as u8]),
        port: NODE_PORT,
    }
}

/// The address of simulated multicast group `group`: `239.255.0.<group>`.
pub fn multicast_addr(group: u8) -> SocketAddr {
    SocketAddr {
        ip: IpAddr::V4([239, 255, 0, group]),
        port: NODE_PORT,
    }
}

#[derive(Clone, Debug)]
struct Packet {
    from: NodeId,
    to: NodeId,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct Member {
    powered: bool,
    inbox: VecDeque<Packet>,
}

#[derive(Debug)]
struct Fabric {
    now_us: u64,
    rng: SimRng,
    faults: NetFaults,
    members: BTreeMap<NodeId, Member>,
    by_addr: BTreeMap<AddrKey, NodeId>,
    groups: BTreeMap<AddrKey, BTreeSet<NodeId>>,
    /// Keyed by `(deliver_at, sequence)`. The sequence tiebreak makes delivery
    /// FIFO among datagrams that landed on the same microsecond, so the fabric
    /// never reorders by accident — reordering only happens where the fault
    /// model asked for it.
    in_flight: BTreeMap<(u64, u64), Packet>,
    next_seq: u64,
    stats: NetStats,
}

impl Fabric {
    fn partition_key(a: NodeId, b: NodeId) -> (NodeId, NodeId) {
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }
}

/// The shared fabric. Create one per world, attach a [`SimNet`] per node.
#[derive(Clone, Debug)]
pub struct SimNetwork {
    fabric: Rc<RefCell<FabricState>>,
}

/// Fabric plus the partition set. Split out only so `Fabric` stays readable.
#[derive(Debug)]
struct FabricState {
    core: Fabric,
    partitions: BTreeSet<(NodeId, NodeId)>,
}

impl SimNetwork {
    /// A perfect network seeded from `seed`.
    pub fn new(seed: u64) -> Self {
        Self::with_faults(seed, NetFaults::perfect())
    }

    /// A network with a fault model.
    pub fn with_faults(seed: u64, faults: NetFaults) -> Self {
        Self {
            fabric: Rc::new(RefCell::new(FabricState {
                core: Fabric {
                    now_us: 0,
                    // A forked stream: the network's draws must not renumber
                    // the entropy a node consumes, or adding packet loss to a
                    // scenario would change the nonces in it too.
                    rng: SimRng::new(seed).fork(0x4E_45_54),
                    faults,
                    members: BTreeMap::new(),
                    by_addr: BTreeMap::new(),
                    groups: BTreeMap::new(),
                    in_flight: BTreeMap::new(),
                    next_seq: 0,
                    stats: NetStats::default(),
                },
                partitions: BTreeSet::new(),
            })),
        }
    }

    /// Attach a node and get its socket handle. Attaching an id twice replaces
    /// the previous member, which is what a reflashed device looks like.
    pub fn attach(&mut self, id: NodeId) -> SimNet {
        let mut fs = self.fabric.borrow_mut();
        fs.core.members.insert(
            id,
            Member {
                powered: true,
                inbox: VecDeque::new(),
            },
        );
        fs.core.by_addr.insert(addr_key(&node_addr(id)), id);
        drop(fs);
        SimNet {
            id,
            fabric: Rc::clone(&self.fabric),
        }
    }

    /// Replace the fault model mid-run. This is how "the WiFi got bad at 3am"
    /// is expressed.
    pub fn set_faults(&mut self, faults: NetFaults) {
        self.fabric.borrow_mut().core.faults = faults;
    }

    /// The current fault model.
    pub fn faults(&self) -> NetFaults {
        self.fabric.borrow().core.faults
    }

    /// Cut `a` and `b` off from each other. Symmetric: an asymmetric partition
    /// is a real and nastier failure, but it is not what this models, and
    /// pretending otherwise would hide which one a test is exercising.
    pub fn partition(&mut self, a: NodeId, b: NodeId) {
        self.fabric
            .borrow_mut()
            .partitions
            .insert(Fabric::partition_key(a, b));
    }

    /// Restore one link.
    pub fn heal(&mut self, a: NodeId, b: NodeId) {
        self.fabric
            .borrow_mut()
            .partitions
            .remove(&Fabric::partition_key(a, b));
    }

    /// Restore every link.
    pub fn heal_all(&mut self) {
        self.fabric.borrow_mut().partitions.clear();
    }

    /// Split the mesh: every pair with one node in `left` and one in `right`
    /// stops talking. The three-way-partition scenarios in the spec are two
    /// calls to this.
    pub fn split(&mut self, left: &[NodeId], right: &[NodeId]) {
        for a in left {
            for b in right {
                self.partition(*a, *b);
            }
        }
    }

    /// Whether `a` and `b` are currently cut off.
    pub fn is_partitioned(&self, a: NodeId, b: NodeId) -> bool {
        self.fabric
            .borrow()
            .partitions
            .contains(&Fabric::partition_key(a, b))
    }

    /// Power a node up or down.
    ///
    /// Powering down discards the node's inbox and every datagram still in
    /// flight *towards* it: a dead radio does not buffer. Datagrams the node
    /// already sent stay in flight, because they are already on the air and a
    /// receiver has no way to know the sender has since died — which is
    /// precisely the race election has to survive.
    pub fn set_powered(&mut self, id: NodeId, on: bool) {
        let mut fs = self.fabric.borrow_mut();
        let Some(member) = fs.core.members.get_mut(&id) else {
            return;
        };
        member.powered = on;
        if on {
            return;
        }
        member.inbox.clear();
        let doomed: Vec<(u64, u64)> = fs
            .core
            .in_flight
            .iter()
            .filter(|(_, p)| p.to == id)
            .map(|(k, _)| *k)
            .collect();
        for key in doomed {
            fs.core.in_flight.remove(&key);
            fs.core.stats.dropped_unreachable += 1;
        }
    }

    /// Whether a node is currently powered. Unknown ids read as off.
    pub fn is_powered(&self, id: NodeId) -> bool {
        self.fabric
            .borrow()
            .core
            .members
            .get(&id)
            .is_some_and(|m| m.powered)
    }

    /// Advance fabric time, moving everything now due into its inbox.
    pub fn advance_to(&mut self, now_us: u64) {
        let mut fs = self.fabric.borrow_mut();
        if now_us < fs.core.now_us {
            return;
        }
        fs.core.now_us = now_us;
        // `split_off` at the first key strictly after `now_us` leaves the due
        // packets behind, in key order, which is the delivery order.
        let still_flying = fs.core.in_flight.split_off(&(now_us + 1, 0));
        let due = core::mem::replace(&mut fs.core.in_flight, still_flying);
        for (_, packet) in due {
            let Some(member) = fs.core.members.get_mut(&packet.to) else {
                fs.core.stats.dropped_unreachable += 1;
                continue;
            };
            if !member.powered {
                fs.core.stats.dropped_unreachable += 1;
                continue;
            }
            member.inbox.push_back(packet);
            fs.core.stats.delivered += 1;
        }
    }

    /// When the next datagram is due, if any. The world runner uses this to
    /// jump straight to the next interesting instant instead of ticking.
    pub fn next_delivery_us(&self) -> Option<u64> {
        self.fabric
            .borrow()
            .core
            .in_flight
            .keys()
            .next()
            .map(|(at, _)| *at)
    }

    /// How many datagrams are still in the air.
    pub fn in_flight(&self) -> usize {
        self.fabric.borrow().core.in_flight.len()
    }

    /// Counters for the run so far.
    pub fn stats(&self) -> NetStats {
        self.fabric.borrow().core.stats
    }

    /// Fabric time.
    pub fn now_us(&self) -> u64 {
        self.fabric.borrow().core.now_us
    }
}

/// One node's socket. Implements [`lumen_hal::Net`], and does nothing a real
/// UDP socket would not.
#[derive(Clone, Debug)]
pub struct SimNet {
    id: NodeId,
    fabric: Rc<RefCell<FabricState>>,
}

impl SimNet {
    /// This socket's node id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// This socket's address.
    pub fn addr(&self) -> SocketAddr {
        node_addr(self.id)
    }

    /// How many datagrams are waiting to be read.
    pub fn pending(&self) -> usize {
        self.fabric
            .borrow()
            .core
            .members
            .get(&self.id)
            .map(|m| m.inbox.len())
            .unwrap_or(0)
    }
}

impl Net for SimNet {
    type Error = NetError;

    fn send_to(&mut self, addr: &SocketAddr, buf: &[u8]) -> Result<(), Self::Error> {
        if buf.len() > MTU {
            return Err(NetError::PayloadTooLarge);
        }
        let mut fs = self.fabric.borrow_mut();
        if !fs
            .core
            .members
            .get(&self.id)
            .is_some_and(|member| member.powered)
        {
            return Err(NetError::NodeDown);
        }
        fs.core.stats.sent += 1;

        let key = addr_key(addr);
        let targets: Vec<NodeId> = if is_multicast(addr) {
            // A group member never receives its own multicast. Real stacks
            // vary here; picking the loopback-free reading means a state
            // machine can never accidentally depend on hearing itself, which
            // it would then get wrong on hardware.
            fs.core
                .groups
                .get(&key)
                .map(|members| members.iter().copied().filter(|m| *m != self.id).collect())
                .unwrap_or_default()
        } else {
            fs.core.by_addr.get(&key).copied().into_iter().collect()
        };

        if targets.is_empty() {
            // Sending into the void succeeds, exactly as UDP does.
            fs.core.stats.dropped_unreachable += 1;
            return Ok(());
        }

        for to in targets {
            let blocked = fs.partitions.contains(&Fabric::partition_key(self.id, to));
            if blocked {
                fs.core.stats.dropped_partition += 1;
                continue;
            }
            if !fs.core.members.get(&to).is_some_and(|m| m.powered) {
                fs.core.stats.dropped_unreachable += 1;
                continue;
            }
            let copies = schedule_copies(&mut fs.core);
            for delay in copies {
                let seq = fs.core.next_seq;
                fs.core.next_seq += 1;
                let at = fs.core.now_us.saturating_add(delay);
                fs.core.in_flight.insert(
                    (at, seq),
                    Packet {
                        from: self.id,
                        to,
                        bytes: buf.to_vec(),
                    },
                );
            }
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>, Self::Error> {
        let mut fs = self.fabric.borrow_mut();
        let Some(member) = fs.core.members.get_mut(&self.id) else {
            return Ok(None);
        };
        if !member.powered {
            return Ok(None);
        }
        let Some(packet) = member.inbox.front() else {
            return Ok(None);
        };
        if packet.bytes.len() > buf.len() {
            // Drop it rather than leave it at the head of the queue: keeping it
            // would wedge the node in a loop that never drains, which is a far
            // worse failure to debug than a loud error.
            member.inbox.pop_front();
            return Err(NetError::BufferTooSmall);
        }
        let packet = member.inbox.pop_front().expect("front was Some");
        buf[..packet.bytes.len()].copy_from_slice(&packet.bytes);
        Ok(Some((packet.bytes.len(), node_addr(packet.from))))
    }

    fn join_multicast(&mut self, group: &SocketAddr) -> Result<(), Self::Error> {
        let mut fs = self.fabric.borrow_mut();
        if !fs
            .core
            .members
            .get(&self.id)
            .is_some_and(|member| member.powered)
        {
            return Err(NetError::NodeDown);
        }
        let key = addr_key(group);
        fs.core.groups.entry(key).or_default().insert(self.id);
        Ok(())
    }
}

/// Decide whether a datagram survives, and how late each surviving copy is.
///
/// Returns one delay per copy to schedule; empty means the fault model ate it.
/// Pulled out of `send_to` so the draw order — loss, jitter, reorder,
/// duplicate — is stated in one place. That order is part of the recording
/// format in practice: change it and every checked-in seed produces a
/// different run.
fn schedule_copies(core: &mut Fabric) -> Vec<u64> {
    let faults = core.faults;
    if core.rng.chance_permille(faults.loss_permille) {
        core.stats.dropped_loss += 1;
        return Vec::new();
    }

    let mut delay = faults.latency_us;
    if faults.jitter_us > 0 {
        delay += core.rng.below(faults.jitter_us + 1);
    }
    if core.rng.chance_permille(faults.reorder_permille) {
        delay += faults.reorder_extra_us;
        core.stats.reordered += 1;
    }

    let mut copies = vec![delay];
    if core.rng.chance_permille(faults.duplicate_permille) {
        let mut dup = faults.latency_us;
        if faults.jitter_us > 0 {
            dup += core.rng.below(faults.jitter_us + 1);
        }
        copies.push(dup);
        core.stats.duplicated += 1;
    }
    copies
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_nodes() -> (SimNetwork, SimNet, SimNet) {
        let mut net = SimNetwork::new(1);
        let a = net.attach(1);
        let b = net.attach(2);
        (net, a, b)
    }

    fn recv_all(sock: &mut SimNet) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; MTU];
        while let Ok(Some((len, _))) = sock.recv(&mut buf) {
            out.push(buf[..len].to_vec());
        }
        out
    }

    #[test]
    fn the_simulated_fabric_carries_what_the_wire_format_allows() {
        // The one place these two numbers are compared. A simulator that
        // carried more than the real network would let a datagram through that
        // fragments or is dropped on somebody's tunnel, and every scenario
        // would keep passing.
        assert_eq!(MTU, lumen_proto::header::MAX_DATAGRAM);
    }

    #[test]
    fn a_perfect_network_delivers_immediately() {
        let (mut net, mut a, mut b) = two_nodes();
        a.send_to(&node_addr(2), b"hello").unwrap();
        net.advance_to(0);
        let mut buf = [0u8; 64];
        let (len, from) = b.recv(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..len], b"hello");
        assert_eq!(from, node_addr(1));
        assert_eq!(b.recv(&mut buf).unwrap(), None);
        assert_eq!(net.stats().delivered, 1);
        assert_eq!(net.now_us(), 0);
    }

    #[test]
    fn addresses_round_trip() {
        assert_eq!(node_addr(0x0102).ip, IpAddr::V4([10, 0, 1, 2]));
        assert_eq!(multicast_addr(7).ip, IpAddr::V4([239, 255, 0, 7]));
        assert!(is_multicast(&multicast_addr(7)));
        assert!(!is_multicast(&node_addr(3)));
        assert!(is_multicast(&SocketAddr {
            ip: IpAddr::V6([0xFF; 16]),
            port: 1
        }));
        assert!(!is_multicast(&SocketAddr {
            ip: IpAddr::V6([0x20; 16]),
            port: 1
        }));
    }

    #[test]
    fn v6_addresses_key_distinctly_from_v4() {
        let v4 = SocketAddr {
            ip: IpAddr::V4([10, 0, 0, 1]),
            port: 1,
        };
        let mut host = [0u8; 16];
        host[..4].copy_from_slice(&[10, 0, 0, 1]);
        let v6 = SocketAddr {
            ip: IpAddr::V6(host),
            port: 1,
        };
        assert_ne!(addr_key(&v4), addr_key(&v6));
    }

    #[test]
    fn latency_holds_a_packet_until_its_time() {
        let mut net = SimNetwork::with_faults(
            2,
            NetFaults {
                latency_us: 5_000,
                ..NetFaults::perfect()
            },
        );
        let mut a = net.attach(1);
        let b = net.attach(2);
        a.send_to(&node_addr(2), b"x").unwrap();
        assert_eq!(net.next_delivery_us(), Some(5_000));
        net.advance_to(4_999);
        assert_eq!(b.pending(), 0);
        net.advance_to(5_000);
        assert_eq!(b.pending(), 1);
        assert_eq!(net.in_flight(), 0);
        assert_eq!(net.next_delivery_us(), None);
    }

    #[test]
    fn advancing_backwards_is_ignored() {
        let (mut net, mut a, _b) = two_nodes();
        net.advance_to(1_000);
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(500);
        assert_eq!(net.now_us(), 1_000);
    }

    #[test]
    fn loss_drops_packets_and_is_counted() {
        let mut net = SimNetwork::with_faults(
            3,
            NetFaults {
                loss_permille: 1000,
                ..NetFaults::perfect()
            },
        );
        let mut a = net.attach(1);
        let b = net.attach(2);
        for _ in 0..10 {
            a.send_to(&node_addr(2), b"x").unwrap();
        }
        net.advance_to(1_000_000);
        assert_eq!(b.pending(), 0);
        assert_eq!(net.stats().dropped_loss, 10);
        assert_eq!(net.stats().sent, 10);
    }

    #[test]
    fn partial_loss_is_reproducible_for_a_seed() {
        let run = |seed: u64| {
            let mut net = SimNetwork::with_faults(
                seed,
                NetFaults {
                    loss_permille: 300,
                    ..NetFaults::perfect()
                },
            );
            let mut a = net.attach(1);
            let b = net.attach(2);
            for _ in 0..200 {
                a.send_to(&node_addr(2), b"x").unwrap();
            }
            net.advance_to(1);
            (net.stats().dropped_loss, b.pending())
        };
        assert_eq!(run(11), run(11));
        assert_ne!(run(11), run(12));
        let (dropped, _) = run(11);
        assert!((40..=80).contains(&dropped), "dropped = {dropped}");
    }

    #[test]
    fn jitter_reorders_and_stays_in_range() {
        let mut net = SimNetwork::with_faults(
            5,
            NetFaults {
                latency_us: 1_000,
                jitter_us: 10_000,
                ..NetFaults::perfect()
            },
        );
        let mut a = net.attach(1);
        let mut b = net.attach(2);
        for i in 0..50u8 {
            a.send_to(&node_addr(2), &[i]).unwrap();
        }
        net.advance_to(1_000_000);
        let got = recv_all(&mut b);
        assert_eq!(got.len(), 50);
        let order: Vec<u8> = got.iter().map(|p| p[0]).collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..50u8).collect::<Vec<_>>());
        assert_ne!(order, sorted, "jitter should have shuffled something");
    }

    #[test]
    fn explicit_reordering_puts_one_packet_far_behind() {
        let mut net = SimNetwork::with_faults(
            7,
            NetFaults {
                reorder_permille: 1000,
                reorder_extra_us: 100_000,
                ..NetFaults::perfect()
            },
        );
        let mut a = net.attach(1);
        let b = net.attach(2);
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(99_999);
        assert_eq!(b.pending(), 0);
        net.advance_to(100_000);
        assert_eq!(b.pending(), 1);
        assert_eq!(net.stats().reordered, 1);
    }

    #[test]
    fn duplicates_deliver_twice() {
        let mut net = SimNetwork::with_faults(
            9,
            NetFaults {
                duplicate_permille: 1000,
                ..NetFaults::perfect()
            },
        );
        let mut a = net.attach(1);
        let mut b = net.attach(2);
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(recv_all(&mut b).len(), 2);
        assert_eq!(net.stats().duplicated, 1);
        assert_eq!(net.stats().delivered, 2);
    }

    #[test]
    fn same_instant_deliveries_stay_fifo() {
        let (mut net, mut a, mut b) = two_nodes();
        for i in 0..20u8 {
            a.send_to(&node_addr(2), &[i]).unwrap();
        }
        net.advance_to(0);
        let order: Vec<u8> = recv_all(&mut b).iter().map(|p| p[0]).collect();
        assert_eq!(order, (0..20u8).collect::<Vec<_>>());
    }

    #[test]
    fn a_partition_blocks_both_directions() {
        let (mut net, mut a, mut b) = two_nodes();
        net.partition(2, 1);
        assert!(net.is_partitioned(1, 2));
        a.send_to(&node_addr(2), b"x").unwrap();
        b.send_to(&node_addr(1), b"y").unwrap();
        net.advance_to(0);
        assert_eq!(a.pending(), 0);
        assert_eq!(b.pending(), 0);
        assert_eq!(net.stats().dropped_partition, 2);

        net.heal(1, 2);
        assert!(!net.is_partitioned(1, 2));
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(b.pending(), 1);
    }

    #[test]
    fn split_and_heal_all_cover_a_three_way_break() {
        let mut net = SimNetwork::new(13);
        let mut a = net.attach(1);
        let _b = net.attach(2);
        let _c = net.attach(3);
        net.split(&[1], &[2, 3]);
        assert!(net.is_partitioned(1, 2) && net.is_partitioned(1, 3));
        assert!(!net.is_partitioned(2, 3));
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(net.stats().dropped_partition, 1);
        net.heal_all();
        assert!(!net.is_partitioned(1, 2));
    }

    #[test]
    fn a_dead_node_neither_sends_nor_receives() {
        let (mut net, mut a, mut b) = two_nodes();
        net.set_powered(2, false);
        assert!(!net.is_powered(2));
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(net.stats().dropped_unreachable, 1);

        assert_eq!(b.send_to(&node_addr(1), b"y"), Err(NetError::NodeDown));
        assert_eq!(
            b.join_multicast(&multicast_addr(1)),
            Err(NetError::NodeDown)
        );
        let mut buf = [0u8; 8];
        assert_eq!(b.recv(&mut buf).unwrap(), None);
    }

    #[test]
    fn powering_down_discards_packets_already_in_the_air() {
        let mut net = SimNetwork::with_faults(
            17,
            NetFaults {
                latency_us: 10_000,
                ..NetFaults::perfect()
            },
        );
        let mut a = net.attach(1);
        let b = net.attach(2);
        a.send_to(&node_addr(2), b"x").unwrap();
        assert_eq!(net.in_flight(), 1);
        net.set_powered(2, false);
        assert_eq!(net.in_flight(), 0);
        net.set_powered(2, true);
        net.advance_to(100_000);
        assert_eq!(b.pending(), 0);
        assert_eq!(net.stats().dropped_unreachable, 1);
    }

    #[test]
    fn powering_a_node_back_up_restores_delivery() {
        let (mut net, mut a, b) = two_nodes();
        net.set_powered(2, false);
        net.set_powered(2, true);
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(b.pending(), 1);
    }

    #[test]
    fn powering_an_unknown_node_is_a_no_op() {
        let (mut net, _a, _b) = two_nodes();
        net.set_powered(99, false);
        assert!(!net.is_powered(99));
    }

    #[test]
    fn a_node_that_dies_while_a_packet_is_due_loses_it() {
        let (mut net, mut a, _b) = two_nodes();
        a.send_to(&node_addr(2), b"x").unwrap();
        // Kill after the send but before the advance that would deliver it.
        net.set_powered(2, false);
        net.advance_to(1_000);
        assert_eq!(net.stats().delivered, 0);
    }

    #[test]
    fn multicast_reaches_every_member_but_the_sender() {
        let mut net = SimNetwork::new(19);
        let mut a = net.attach(1);
        let mut b = net.attach(2);
        let mut c = net.attach(3);
        let group = multicast_addr(4);
        for sock in [&mut a, &mut b, &mut c] {
            sock.join_multicast(&group).unwrap();
        }
        a.send_to(&group, b"chan").unwrap();
        net.advance_to(0);
        assert_eq!(a.pending(), 0);
        assert_eq!(b.pending(), 1);
        assert_eq!(c.pending(), 1);
    }

    #[test]
    fn joining_a_group_twice_is_idempotent() {
        let mut net = SimNetwork::new(21);
        let mut a = net.attach(1);
        let mut b = net.attach(2);
        let group = multicast_addr(4);
        b.join_multicast(&group).unwrap();
        b.join_multicast(&group).unwrap();
        a.send_to(&group, b"x").unwrap();
        net.advance_to(0);
        assert_eq!(b.pending(), 1);
    }

    #[test]
    fn multicast_to_an_empty_group_is_not_an_error() {
        let (net, mut a, _b) = two_nodes();
        a.send_to(&multicast_addr(9), b"x").unwrap();
        assert_eq!(net.stats().dropped_unreachable, 1);
        assert_eq!(net.in_flight(), 0);
    }

    #[test]
    fn a_partition_also_blocks_multicast() {
        let mut net = SimNetwork::new(23);
        let mut a = net.attach(1);
        let mut b = net.attach(2);
        let group = multicast_addr(4);
        b.join_multicast(&group).unwrap();
        net.partition(1, 2);
        a.send_to(&group, b"x").unwrap();
        net.advance_to(0);
        assert_eq!(b.pending(), 0);
        assert_eq!(net.stats().dropped_partition, 1);
    }

    #[test]
    fn unicast_to_nobody_succeeds_silently() {
        let (net, mut a, _b) = two_nodes();
        a.send_to(&node_addr(77), b"x").unwrap();
        assert_eq!(net.in_flight(), 0);
        assert_eq!(net.stats().dropped_unreachable, 1);
    }

    #[test]
    fn oversize_payloads_are_refused() {
        let (_net, mut a, _b) = two_nodes();
        let big = vec![0u8; MTU + 1];
        assert_eq!(
            a.send_to(&node_addr(2), &big),
            Err(NetError::PayloadTooLarge)
        );
        assert!(a.send_to(&node_addr(2), &big[..MTU]).is_ok());
    }

    #[test]
    fn a_short_recv_buffer_errors_and_drops_the_datagram() {
        let (mut net, mut a, mut b) = two_nodes();
        a.send_to(&node_addr(2), b"0123456789").unwrap();
        a.send_to(&node_addr(2), b"ok").unwrap();
        net.advance_to(0);
        let mut small = [0u8; 4];
        assert_eq!(b.recv(&mut small), Err(NetError::BufferTooSmall));
        assert_eq!(
            b.recv(&mut small).unwrap().map(|(len, _)| len),
            Some(2),
            "the queue must keep draining"
        );
    }

    #[test]
    fn socket_metadata_is_available() {
        let (_net, a, _b) = two_nodes();
        assert_eq!(a.id(), 1);
        assert_eq!(a.addr(), node_addr(1));
        assert_eq!(a.pending(), 0);
    }

    #[test]
    fn a_detached_socket_reads_empty_rather_than_panicking() {
        let mut net = SimNetwork::new(29);
        let mut orphan = net.attach(1);
        // Simulate a member vanishing from under a stale handle.
        net.fabric.borrow_mut().core.members.remove(&1);
        assert_eq!(orphan.pending(), 0);
        let mut buf = [0u8; 4];
        assert_eq!(orphan.recv(&mut buf).unwrap(), None);
        assert_eq!(orphan.send_to(&node_addr(2), b"x"), Err(NetError::NodeDown));
    }

    #[test]
    fn reattaching_an_id_replaces_the_member() {
        let mut net = SimNetwork::new(31);
        let mut a = net.attach(1);
        let b = net.attach(2);
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(b.pending(), 1);
        let fresh = net.attach(2);
        assert_eq!(fresh.pending(), 0, "a reflashed device boots with no inbox");
    }

    #[test]
    fn faults_can_be_swapped_mid_run() {
        let (mut net, mut a, b) = two_nodes();
        assert_eq!(net.faults(), NetFaults::perfect());
        net.set_faults(NetFaults {
            loss_permille: 1000,
            ..NetFaults::perfect()
        });
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(b.pending(), 0);
        net.set_faults(NetFaults::perfect());
        a.send_to(&node_addr(2), b"x").unwrap();
        net.advance_to(0);
        assert_eq!(b.pending(), 1);
    }

    #[test]
    fn lossy_wifi_is_lossy_but_not_useless() {
        let mut net = SimNetwork::with_faults(37, NetFaults::lossy_wifi());
        let mut a = net.attach(1);
        let mut b = net.attach(2);
        for _ in 0..500 {
            a.send_to(&node_addr(2), b"x").unwrap();
        }
        net.advance_to(10_000_000);
        let got = recv_all(&mut b).len();
        assert!((440..500).contains(&got), "delivered = {got}");
        assert!(net.stats().dropped_loss > 0);
    }
}
