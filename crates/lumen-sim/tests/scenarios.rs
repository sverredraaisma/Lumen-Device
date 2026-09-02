//! Whole-mesh scenarios, driven through the public API only.
//!
//! The unit tests inside the crate check each simulated device in isolation.
//! These are the ones that justify the crate existing: several nodes, a fault
//! model, a script, and the two properties everything else rests on — that the
//! same seed gives the same run, and that a recorded run replays.
//!
//! The scenario names come from the "worth shipping from the start" list in
//! `lumen-spec/docs/wire-format.md`. The state machines they will eventually
//! exercise do not exist yet (W5/W6/W7), so each one currently pins the
//! *harness* behaviour it needs — the partition really cuts traffic, the
//! reboot really loses the inbox, the clocks really diverge — which is exactly
//! the part that has to be trustworthy before any state machine leans on it.

use lumen_hal::Clock;
use lumen_sim::{
    record, replay, to_behavioural_vector, verify_replay, IdleCore, NetFaults, NodeCore, NodeSpec,
    Op, PeriodicCore, Recording, Scenario, TraceKind, World,
};

/// One arbitrary but fixed seed, used by the determinism pair so the two runs
/// are provably the same input.
const MESH_SEED: u64 = 0xB0A7_1017_5EED;

fn idle(_: &NodeSpec) -> Box<dyn NodeCore> {
    Box::new(IdleCore)
}

fn periodic(_: &NodeSpec) -> Box<dyn NodeCore> {
    Box::new(PeriodicCore::new(1_000_000))
}

/// Five nodes on bad WiFi, ticking, gossiping, being partitioned and rebooted.
/// Deliberately messy: a determinism proof on a quiet scenario proves nothing.
fn busy_mesh(seed: u64) -> Scenario {
    Scenario::new(seed, 60_000_000)
        .with_node(NodeSpec::new(1, 60).with_clock_error(0, 18))
        .with_node(NodeSpec::new(2, 60).with_clock_error(-1_500, -22))
        .with_node(NodeSpec::new(3, 30).with_clock_error(900, 5))
        .with_node(NodeSpec::new(4, 0).with_clock_error(0, -40))
        .with_node(NodeSpec::new(5, 144).with_clock_error(250_000, 0))
        .with_faults(NetFaults::lossy_wifi())
        .every(0, 1_000_000, 60_000_000, |t| Op::Send {
            from: ((t / 1_000_000) % 5) as u16 + 1,
            to: ((t / 1_000_000) % 4) as u16 + 2,
            bytes: (t as u32).to_le_bytes().to_vec(),
        })
        .every(0, 5_000_000, 60_000_000, |t| Op::Present {
            node: ((t / 5_000_000) % 5) as u16 + 1,
            level: (t / 1_000) as u16,
        })
        .at(0, Op::Join { node: 2, group: 1 })
        .at(0, Op::Join { node: 3, group: 1 })
        .at(0, Op::Join { node: 4, group: 1 })
        .at(
            10_000_000,
            Op::Multicast {
                from: 1,
                group: 1,
                bytes: vec![0xC0, 0xDE],
            },
        )
        .at(
            20_000_000,
            Op::Split {
                left: vec![1, 2],
                right: vec![3, 4, 5],
            },
        )
        .at(30_000_000, Op::HealAll)
        .at(35_000_000, Op::Kill(3))
        .at(
            40_000_000,
            Op::Revive {
                node: 3,
                wipe_storage: false,
            },
        )
        .at(45_000_000, Op::Kill(5))
        .at(
            46_000_000,
            Op::Revive {
                node: 5,
                wipe_storage: true,
            },
        )
        .at(50_000_000, Op::SetFaults(NetFaults::perfect()))
        .at(
            55_000_000,
            Op::Discipline {
                node: 5,
                offset_us: -250_000,
            },
        )
}

/// The headline property. If this ever fails, nothing else in the crate means
/// anything.
#[test]
fn the_same_seed_gives_a_byte_identical_run() {
    let a = record(busy_mesh(MESH_SEED), periodic);
    let b = record(busy_mesh(MESH_SEED), periodic);
    assert_eq!(a.report.first_divergence(&b.report), None);
    assert_eq!(a, b);
    assert_eq!(a.report.digest(), b.report.digest());
    assert!(
        a.report.trace.len() > 200,
        "a determinism proof over a trivial run proves nothing: {}",
        a.report.trace.len()
    );
}

#[test]
fn a_neighbouring_seed_gives_a_different_run() {
    let a = record(busy_mesh(MESH_SEED), periodic);
    let b = record(busy_mesh(MESH_SEED + 1), periodic);
    assert_ne!(a.report.digest(), b.report.digest());
    // Same script, so the same number of scripted ops — only the fault pattern
    // moved. That is the point: the divergence is the network, not the plan.
    let ops = |r: &Recording| {
        r.report
            .trace
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::Op { .. }))
            .count()
    };
    assert_eq!(ops(&a), ops(&b));
}

#[test]
fn a_recorded_run_replays_and_survives_a_file_round_trip() {
    let recorded = record(busy_mesh(7), periodic);
    verify_replay(&recorded, periodic).expect("replay must be identical");

    let text = recorded.to_text();
    let parsed = Recording::from_text(&text).expect("the file must parse");
    assert_eq!(parsed, recorded);
    let replayed = replay(&parsed, periodic);
    assert_eq!(replayed, recorded.report);
    assert_eq!(parsed.to_text(), text, "writing is idempotent");
}

#[test]
fn a_partition_isolates_and_healing_reconnects() {
    let scenario = Scenario::new(3, 10_000_000)
        .with_nodes(3, 0)
        .at(
            0,
            Op::Split {
                left: vec![1],
                right: vec![2, 3],
            },
        )
        .every(1_000_000, 1_000_000, 5_000_000, |_| Op::Send {
            from: 1,
            to: 2,
            bytes: vec![1],
        })
        .at(5_000_000, Op::HealAll)
        .every(6_000_000, 1_000_000, 10_000_000, |_| Op::Send {
            from: 1,
            to: 2,
            bytes: vec![1],
        });
    let mut world = World::new(scenario, idle);
    let report = world.run();

    let rx: Vec<u64> = report
        .trace
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::Rx { .. }))
        .map(|e| e.at_us)
        .collect();
    assert_eq!(rx.len(), 4, "only the post-heal sends land");
    assert!(rx.iter().all(|t| *t >= 6_000_000));
    assert_eq!(report.stats.dropped_partition, 4);
}

/// A twenty-four hour drift scenario. The assertion that matters is not the
/// number, it is that this test runs in microseconds — virtual time is free,
/// which is why a soak scenario can live in the normal suite.
#[test]
fn a_full_day_of_clock_drift_costs_almost_nothing() {
    let day_us = 86_400_000_000u64;
    let scenario = Scenario::new(1, day_us)
        .with_node(NodeSpec::new(1, 0).with_clock_error(0, 20))
        .with_node(NodeSpec::new(2, 0).with_clock_error(0, -20))
        // One exchange an hour: 24 steps for a whole day.
        .every(0, 3_600_000_000, day_us, |_| Op::Send {
            from: 1,
            to: 2,
            bytes: vec![0],
        });
    let mut world = World::new(scenario, idle);
    let report = world.run();
    assert_eq!(report.end_us, day_us);

    let fast = world.node(1).unwrap().clock.now_us();
    let slow = world.node(2).unwrap().clock.now_us();
    assert_eq!(fast - slow, 3_456_000, "±20 ppm over 24 h, in µs");
    assert_eq!(report.stats.delivered, 24);
}

/// Disciplining brings two clocks together without either of them stepping.
/// The no-step half is the requirement: a stepped render clock is a visible
/// glitch, and a clock that went backwards would break every duration the
/// core computes.
#[test]
fn a_disciplined_clock_converges_without_stepping() {
    let scenario = Scenario::new(1, 120_000_000)
        .with_node(NodeSpec::new(1, 0).with_clock_error(60_000, 0))
        .at(
            0,
            Op::Discipline {
                node: 1,
                offset_us: -60_000,
            },
        );
    let mut world = World::new(scenario, idle);

    let mut last = 0u64;
    for _ in 0..200 {
        world.apply(&Op::Tick(1));
        let now = world.node(1).unwrap().clock.now_us();
        assert!(now >= last, "the show clock must never run backwards");
        last = now;
        if !world.step() {
            break;
        }
    }
    world.run();
    let node = world.node(1).unwrap();
    assert_eq!(
        node.clock.pending_slew_us(),
        0,
        "the correction was absorbed"
    );
    assert_eq!(node.clock.now_us(), 120_000_000, "and it now agrees");
}

/// A node that reboots comes back knowing who it is; one that is factory reset
/// does not. Both paths have to work, and confusing them is the bug that leaves
/// a mesh with two devices claiming one identity.
#[test]
fn a_reboot_keeps_identity_and_a_factory_reset_does_not() {
    let scenario = Scenario::new(1, 10_000)
        .with_nodes(2, 0)
        .at(
            0,
            Op::Store {
                node: 1,
                key: "node/uuid".into(),
                value: vec![1, 2, 3, 4],
            },
        )
        .at(
            0,
            Op::Store {
                node: 2,
                key: "node/uuid".into(),
                value: vec![5, 6, 7, 8],
            },
        )
        .at(1_000, Op::Kill(1))
        .at(1_000, Op::Kill(2))
        .at(
            2_000,
            Op::Revive {
                node: 1,
                wipe_storage: false,
            },
        )
        .at(
            2_000,
            Op::Revive {
                node: 2,
                wipe_storage: true,
            },
        );
    let mut world = World::new(scenario, idle);
    world.run();
    assert_eq!(
        world.node(1).unwrap().storage.get("node/uuid"),
        Some(&[1u8, 2, 3, 4][..])
    );
    assert_eq!(world.node(2).unwrap().storage.get("node/uuid"), None);
}

/// Total loss for a stretch, then recovery. The mesh has to keep rendering
/// through it — rule 4, a device is never dark because of software — and the
/// harness has to be able to show that it did.
#[test]
fn a_network_outage_does_not_stop_rendering() {
    let scenario = Scenario::new(11, 30_000_000)
        .with_nodes(2, 8)
        .at(
            10_000_000,
            Op::SetFaults(NetFaults {
                loss_permille: 1000,
                ..NetFaults::perfect()
            }),
        )
        .at(20_000_000, Op::SetFaults(NetFaults::perfect()))
        .every(0, 1_000_000, 30_000_000, |t| Op::Send {
            from: 1,
            to: 2,
            bytes: (t as u32).to_le_bytes().to_vec(),
        })
        .every(0, 1_000_000, 30_000_000, |t| Op::Present {
            node: 2,
            level: 1 + (t / 1_000_000) as u16,
        });
    let mut world = World::new(scenario, idle);
    let report = world.run();

    assert_eq!(report.stats.dropped_loss, 10, "the ten-second blackout");
    assert_eq!(report.frames[&2].0, 30, "rendering never paused");
    assert!(
        !world.node(2).unwrap().led.is_dark(),
        "a lost network keeps rendering"
    );
}

/// The last mile of the project rule: a scenario reproduced here leaves a file
/// behind that `lumen-spec` can carry, so every implementation inherits it.
#[test]
fn a_scenario_exports_as_a_conformance_vector() {
    let recorded = record(busy_mesh(99), periodic);
    let json = to_behavioural_vector(
        &recorded,
        "partition_and_heal",
        "A five-node mesh split in two for ten seconds. Rendering must continue \
         on both sides and the mesh must reconverge on heal.",
    );
    assert!(json.contains("\"kind\": \"behavioural\""));
    assert!(json.contains("\"name\": \"partition_and_heal\""));
    assert!(json.contains("\"op\": \"split\""));
    assert!(json.contains("\"frame_digest\""));
    // Braces balance — the cheap structural check a hand-written encoder needs.
    assert_eq!(
        json.chars().filter(|c| *c == '{').count(),
        json.chars().filter(|c| *c == '}').count()
    );
}

/// Every simulated node gets its own entropy stream, and the streams are
/// stable. Election compares UUIDs, so an unseeded byte here would change who
/// wins a tie and make the vector for it worthless.
#[test]
fn node_entropy_is_per_node_and_reproducible() {
    use lumen_hal::Entropy;

    let draw = |seed: u64| {
        let mut world = World::new(Scenario::new(seed, 1).with_nodes(3, 0), idle);
        (1..=3u16)
            .map(|id| {
                let mut uuid = [0u8; 16];
                world.node_mut(id).unwrap().entropy.fill(&mut uuid);
                uuid
            })
            .collect::<Vec<_>>()
    };
    let a = draw(5);
    let b = draw(5);
    assert_eq!(a, b, "same seed, same identities");
    assert_ne!(a[0], a[1], "two nodes must not share a UUID");
    assert_ne!(a, draw(6), "a different world is a different mesh");
}
