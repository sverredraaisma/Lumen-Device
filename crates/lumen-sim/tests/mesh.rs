//! Whole-mesh scenarios driving the real state machines.
//!
//! `scenarios.rs` exercises the harness. This exercises the *behaviour*: real
//! `Node`s, real datagrams, over the simulated network, with time that only
//! moves when the runner says so.
//!
//! Every test here corresponds to a scenario the wire format says is worth
//! shipping from the start.

use std::cell::RefCell;
use std::rc::Rc;

use lumen_device::{Action, Destination, Event, Identity, Node, Role};
use lumen_proto::Uuid;
use lumen_sim::{NodeCore, NodeId};

const MESH: Uuid = Uuid([0x5A; 16]);

/// A `Node` behind the harness's core trait, plus a shared view of what it did.
///
/// The shared handle is what lets a test assert on the role a node reached
/// without the harness needing to know what a role is.
struct MeshNode {
    node: Node,
    observed: Rc<RefCell<Observed>>,
}

#[derive(Default, Debug)]
struct Observed {
    role: Option<Role>,
    epoch: u32,
    synced: bool,
    corrections: Vec<i64>,
    sent: usize,
}

impl MeshNode {
    fn new(capacity: u32, tag: u8, observed: Rc<RefCell<Observed>>) -> MeshNode {
        let mut uuid = [0u8; 16];
        uuid[0] = tag;
        uuid[1] = tag;
        uuid[2] = tag;
        uuid[3] = tag;
        MeshNode {
            node: Node::new(Identity::new(Uuid(uuid), capacity), MESH, 1, 0),
            observed,
        }
    }

    fn record(&self, actions: &[Action]) -> Vec<Action> {
        let mut o = self.observed.borrow_mut();
        for a in actions {
            match a {
                Action::RoleChanged { role, epoch } => {
                    o.role = Some(*role);
                    o.epoch = *epoch;
                }
                Action::SyncAcquired => o.synced = true,
                Action::SyncLost => o.synced = false,
                Action::DisciplineClock { offset_us } => o.corrections.push(*offset_us),
                Action::Send { .. } => o.sent += 1,
                Action::SetTimer { .. } => {}
            }
        }
        actions.to_vec()
    }
}

impl NodeCore for MeshNode {
    fn on_event(&mut self, now_us: u64, ev: Event<'_>) -> Vec<Action> {
        let actions = self.node.on_event(now_us, ev);
        self.record(&actions)
    }
}

/// Drive a set of nodes against each other directly.
///
/// The `SimNetwork` fabric is built around HAL sockets, and these cores emit
/// `Action::Send` rather than calling one — wiring the two together is the
/// runner change that belongs with the render loop. Until then this is an
/// explicit, deterministic delivery loop: no randomness, no hidden ordering, and
/// every hop visible in the test.
struct Mesh {
    nodes: Vec<MeshNode>,
    views: Vec<Rc<RefCell<Observed>>>,
    /// `(from, destination, bytes)` waiting to be delivered.
    in_flight: Vec<(usize, Destination, Vec<u8>)>,
    /// Peers that cannot hear each other.
    partitioned: Vec<(usize, usize)>,
}

impl Mesh {
    fn new(capacities: &[u32]) -> Mesh {
        let mut nodes = Vec::new();
        let mut views = Vec::new();
        for (i, &cap) in capacities.iter().enumerate() {
            let view = Rc::new(RefCell::new(Observed::default()));
            views.push(view.clone());
            nodes.push(MeshNode::new(cap, (i + 1) as u8, view));
        }
        Mesh {
            nodes,
            views,
            in_flight: Vec::new(),
            partitioned: Vec::new(),
        }
    }

    fn partition(&mut self, a: usize, b: usize) {
        self.partitioned.push((a.min(b), a.max(b)));
    }

    fn heal(&mut self) {
        self.partitioned.clear();
    }

    fn reachable(&self, a: usize, b: usize) -> bool {
        !self.partitioned.contains(&(a.min(b), a.max(b)))
    }

    /// Tick every node, then deliver everything they sent, repeatedly until the
    /// instant settles. Bounded, so a node pair that answered each other forever
    /// fails the test rather than hanging it.
    fn step(&mut self, now_us: u64) {
        for i in 0..self.nodes.len() {
            let actions = self.nodes[i].on_event(now_us, Event::Tick);
            self.queue(i, &actions);
        }
        for _ in 0..32 {
            if self.in_flight.is_empty() {
                return;
            }
            let batch = std::mem::take(&mut self.in_flight);
            for (from, dest, bytes) in batch {
                for to in 0..self.nodes.len() {
                    if to == from || !self.reachable(from, to) {
                        continue;
                    }
                    if let Destination::Peer(prefix) = dest {
                        if self.prefix_of(to) != prefix {
                            continue;
                        }
                    }
                    let actions =
                        self.nodes[to].on_event(now_us, Event::Datagram { bytes: &bytes });
                    self.queue(to, &actions);
                }
            }
        }
        panic!("the mesh did not settle at {now_us}us");
    }

    fn queue(&mut self, from: usize, actions: &[Action]) {
        for a in actions {
            if let Action::Send { to, datagram } = a {
                self.in_flight.push((from, *to, datagram.clone()));
            }
        }
    }

    fn prefix_of(&self, i: usize) -> [u8; 4] {
        let tag = (i + 1) as u8;
        [tag; 4]
    }

    fn role(&self, i: usize) -> Option<Role> {
        self.views[i].borrow().role
    }

    fn synced(&self, i: usize) -> bool {
        self.views[i].borrow().synced
    }

    fn leaders(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&i| self.role(i) == Some(Role::Leader))
            .collect()
    }

    /// Run for `secs` simulated seconds at 100 ms granularity.
    fn run(&mut self, from_us: u64, secs: u64) -> u64 {
        let mut now = from_us;
        for _ in 0..(secs * 10) {
            now += 100_000;
            self.step(now);
        }
        now
    }
}

#[test]
fn a_cold_mesh_elects_exactly_one_leader() {
    // The M2 milestone: two devices powered on in either order discover each
    // other and agree a timebase, with no coordination but the ticks they send.
    let mut mesh = Mesh::new(&[200, 100]);
    mesh.run(0, 10);

    assert_eq!(
        mesh.leaders(),
        vec![0],
        "the higher-capacity node should lead, and alone"
    );
    assert_ne!(mesh.role(1), Some(Role::Leader));
}

#[test]
fn the_strongest_node_wins_regardless_of_declaration_order() {
    // Order must not decide it, or the outcome depends on which device booted
    // first - which is exactly the thing that differs between a lab and a house.
    for capacities in [
        vec![100u32, 200, 50],
        vec![200, 100, 50],
        vec![50, 100, 200],
    ] {
        let mut mesh = Mesh::new(&capacities);
        mesh.run(0, 10);
        let expected = capacities
            .iter()
            .enumerate()
            .max_by_key(|(_, c)| **c)
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(mesh.leaders(), vec![expected], "for {capacities:?}");
    }
}

#[test]
fn equal_capacity_is_still_decided_and_never_a_tie() {
    // Two identical devices flashed from the same binary is the common case, not
    // an edge case. If the tiebreak failed, both would lead and the mesh would
    // have two timebases.
    let mut mesh = Mesh::new(&[100, 100, 100]);
    mesh.run(0, 10);
    assert_eq!(mesh.leaders().len(), 1, "roles: {:?}", mesh.leaders());
}

#[test]
fn followers_reach_sync_and_the_leader_is_synced_by_definition() {
    let mut mesh = Mesh::new(&[200, 100]);
    mesh.run(0, 20);

    assert!(mesh.synced(0), "the leader is its own timebase");
    assert!(mesh.synced(1), "the follower should have converged");
}

#[test]
fn a_partition_produces_a_leader_on_each_side_and_one_after_healing() {
    // The split-brain case. Both halves must keep working - a device is never
    // dark because of software - and the mesh must converge again when the
    // partition heals rather than staying split.
    let mut mesh = Mesh::new(&[200, 100]);
    mesh.partition(0, 1);
    let t = mesh.run(0, 10);

    assert_eq!(
        mesh.leaders().len(),
        2,
        "each side of a partition needs its own timebase"
    );

    mesh.heal();
    mesh.run(t, 20);
    assert_eq!(
        mesh.leaders(),
        vec![0],
        "the weaker node should yield once it hears the stronger"
    );
}

#[test]
fn a_leader_that_vanishes_is_replaced() {
    // Kill the master mid-show. The survivor must take over, which is what makes
    // the mesh autonomous rather than dependent on one device staying up.
    let mut mesh = Mesh::new(&[200, 100]);
    let t = mesh.run(0, 10);
    assert_eq!(mesh.leaders(), vec![0]);

    // Isolating the leader is indistinguishable, from the follower's side, from
    // it having died.
    mesh.partition(0, 1);
    mesh.run(t, 10);
    assert_eq!(
        mesh.role(1),
        Some(Role::Leader),
        "the survivor should have taken the timebase"
    );
}

#[test]
fn a_settled_mesh_stops_changing_roles() {
    // Role flapping is worse than a suboptimal choice: every handover is visible
    // because the show clock changes hands.
    let mut mesh = Mesh::new(&[200, 150, 100]);
    let t = mesh.run(0, 10);
    let before: Vec<_> = (0..3).map(|i| mesh.role(i)).collect();
    let epochs: Vec<u32> = (0..3).map(|i| mesh.views[i].borrow().epoch).collect();

    mesh.run(t, 30);
    let after: Vec<_> = (0..3).map(|i| mesh.role(i)).collect();
    let epochs_after: Vec<u32> = (0..3).map(|i| mesh.views[i].borrow().epoch).collect();

    assert_eq!(before, after, "roles changed in a settled mesh");
    assert_eq!(epochs, epochs_after, "the epoch advanced with no reason to");
}

#[test]
fn a_follower_keeps_its_clock_disciplined_without_stepping() {
    // Corrections must arrive as offsets to slew, never as a new time to jump
    // to. Every effect is a function of this clock, so a step is a visible
    // glitch on every light in the room.
    let mut mesh = Mesh::new(&[200, 100]);
    mesh.run(0, 30);
    let view = mesh.views[1].borrow();
    // With no simulated skew every correction should be tiny; what matters is
    // that they are corrections at all and not absolute times.
    for c in &view.corrections {
        assert!(
            c.abs() < 1_000_000,
            "a correction of {c}us looks like a step, not a slew"
        );
    }
}

#[test]
fn a_lone_node_leads_itself_and_keeps_ticking() {
    // A single device is a mesh of one. It must not sit unsynced waiting for a
    // peer that will never arrive, or it would suppress everything it exists to
    // render.
    let mut mesh = Mesh::new(&[100]);
    mesh.run(0, 10);
    assert_eq!(mesh.leaders(), vec![0]);
    assert!(mesh.synced(0));
    assert!(
        mesh.views[0].borrow().sent > 5,
        "a leader should keep ticking"
    );
}

#[test]
fn nodes_ignore_a_neighbouring_mesh_entirely() {
    // Two meshes on one LAN is normal in a block of flats. Neither may elect
    // into the other, and neither may be delayed by the other's traffic.
    let mut a = Mesh::new(&[200, 100]);
    a.run(0, 10);

    let stranger = {
        let mut n = Node::new(Identity::new(Uuid([9; 16]), 9_999), Uuid([0x11; 16]), 1, 0);
        let actions = n.on_event(5_000_000, Event::Tick);
        actions
            .into_iter()
            .find_map(|x| match x {
                Action::Send { datagram, .. } => Some(datagram),
                _ => None,
            })
            .expect("the stranger should have announced itself")
    };

    // A candidacy far stronger than anything in our mesh, from another mesh.
    for i in 0..2 {
        a.nodes[i].on_event(11_000_000, Event::Datagram { bytes: &stranger });
    }
    a.run(11_000_000, 10);
    assert_eq!(
        a.leaders(),
        vec![0],
        "another mesh must not be able to take our timebase"
    );
}

/// The harness's node ids are unused here, but the trait bound requires the
/// type to exist; naming it keeps the import honest.
#[allow(dead_code)]
fn _node_id_is_used(_: NodeId) {}
