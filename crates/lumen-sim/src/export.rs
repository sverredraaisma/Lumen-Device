//! Export a recording as a `lumen-spec` behavioural vector.
//!
//! The project rule is that a bug reproduced in `lumen-sim` becomes a checked-in
//! scenario and then a vector, so every implementation inherits the regression
//! test. This is the second half of that: a recording goes in, JSON in the shape
//! `lumen-spec/vectors/behavioural/` expects comes out, and the file is dropped
//! into that repo by hand.
//!
//! It is deliberately *export only*. Reading vectors back is the conformance
//! runner's job and it lives in `lumen-spec`; duplicating a parser here would
//! give two answers to "what does this vector mean", and the point of a vector
//! is that there is exactly one.
//!
//! JSON is hand-written for the same reason the rest of the crate has no
//! dependencies. The subset needed is small — objects, arrays, integers,
//! strings — and the alternative is a serde tree in a crate whose whole job is
//! to be predictable.

use std::fmt::Write as _;

use crate::net::NetFaults;
use crate::record::Recording;
use crate::scenario::Op;
use crate::world::{op_node, op_tag, EventKind, TraceKind};
use lumen_device::Action;

/// Schema version of the emitted vector. Matches the `"schema": 1` the codec
/// vectors already carry, so the runner's dispatch does not need a special
/// case for behavioural files.
pub const VECTOR_SCHEMA: u32 = 1;

/// Turn a recording into a behavioural conformance vector.
///
/// `name` is the file's stem in `vectors/behavioural/`; `description` is the
/// sentence a reviewer reads first, and should say what failure the vector
/// pins rather than what the scenario does — "the master vanishing mid-show
/// must not interrupt rendering", not "kills node 1".
pub fn to_behavioural_vector(recording: &Recording, name: &str, description: &str) -> String {
    let mut out = String::new();
    let scenario = &recording.scenario;

    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "  \"schema\": {VECTOR_SCHEMA},");
    let _ = writeln!(out, "  \"kind\": \"behavioural\",");
    let _ = writeln!(out, "  \"name\": {},", json_string(name));
    let _ = writeln!(out, "  \"description\": {},", json_string(description));
    // The seed is recorded so the vector can be regenerated from lumen-sim,
    // but an implementation must NOT need it: a vector is a list of events and
    // required actions, and anything that only reproduces under our PRNG is not
    // a conformance requirement at all.
    let _ = writeln!(out, "  \"source\": \"lumen-sim\",");
    let _ = writeln!(out, "  \"seed\": {},", scenario.seed);
    let _ = writeln!(out, "  \"duration_us\": {},", scenario.duration_us);

    let _ = writeln!(out, "  \"initial_state\": {{");
    let _ = writeln!(out, "    \"nodes\": [");
    for (i, n) in scenario.nodes.iter().enumerate() {
        let comma = if i + 1 == scenario.nodes.len() {
            ""
        } else {
            ","
        };
        let _ = writeln!(
            out,
            "      {{ \"id\": {}, \"pixel_count\": {}, \"skew_us\": {}, \"drift_ppm\": {} }}{comma}",
            n.id, n.pixel_count, n.skew_us, n.drift_ppm
        );
    }
    let _ = writeln!(out, "    ],");
    let _ = writeln!(out, "    \"network\": {}", faults_json(&scenario.faults));
    let _ = writeln!(out, "  }},");

    let _ = writeln!(out, "  \"events\": [");
    let entries: Vec<String> = scenario
        .script
        .iter()
        .map(|s| {
            format!(
                "    {{ \"at_us\": {}, \"node\": {}, \"op\": {}, \"detail\": {} }}",
                s.at_us,
                op_node(&s.op),
                json_string(op_tag(&s.op)),
                op_detail(&s.op)
            )
        })
        .collect();
    let _ = writeln!(out, "{}", entries.join(",\n"));
    let _ = writeln!(out, "  ],");

    // Only the core's observable outputs go in `expected`. Trace lines about
    // the harness applying its own script are not a requirement on anyone —
    // shipping them would make the vector assert that other implementations
    // have our simulator inside them.
    let _ = writeln!(out, "  \"expected\": [");
    let expected: Vec<String> = recording
        .report
        .trace
        .iter()
        .filter_map(|e| expected_json(&e.kind).map(|body| (e.at_us, e.node, body)))
        .map(|(at_us, node, body)| {
            format!("    {{ \"at_us\": {at_us}, \"node\": {node}, {body} }}")
        })
        .collect();
    let _ = writeln!(out, "{}", expected.join(",\n"));
    let _ = writeln!(out, "  ]");
    let _ = writeln!(out, "}}");
    out
}

fn faults_json(f: &NetFaults) -> String {
    format!(
        "{{ \"loss_permille\": {}, \"latency_us\": {}, \"jitter_us\": {}, \"reorder_permille\": {}, \"reorder_extra_us\": {}, \"duplicate_permille\": {} }}",
        f.loss_permille,
        f.latency_us,
        f.jitter_us,
        f.reorder_permille,
        f.reorder_extra_us,
        f.duplicate_permille
    )
}

fn op_detail(op: &Op) -> String {
    match op {
        Op::Tick(_) | Op::Kill(_) | Op::HealAll => "{}".to_string(),
        Op::Send { to, bytes, .. } => {
            format!(
                "{{ \"to\": {to}, \"bytes\": {} }}",
                json_string(&hex(bytes))
            )
        }
        Op::Multicast { group, bytes, .. } => format!(
            "{{ \"group\": {group}, \"bytes\": {} }}",
            json_string(&hex(bytes))
        ),
        Op::Join { group, .. } => format!("{{ \"group\": {group} }}"),
        Op::Revive { wipe_storage, .. } => format!("{{ \"wipe_storage\": {wipe_storage} }}"),
        Op::Partition(a, b) | Op::Heal(a, b) => format!("{{ \"a\": {a}, \"b\": {b} }}"),
        Op::Split { left, right } => format!(
            "{{ \"left\": {}, \"right\": {} }}",
            json_ids(left),
            json_ids(right)
        ),
        Op::SetFaults(f) => faults_json(f),
        Op::Skew { offset_us, .. } | Op::Discipline { offset_us, .. } => {
            format!("{{ \"offset_us\": {offset_us} }}")
        }
        Op::Drift { ppm, .. } => format!("{{ \"ppm\": {ppm} }}"),
        Op::Present { level, .. } => format!("{{ \"level\": {level} }}"),
        Op::Store { key, value, .. } => format!(
            "{{ \"key\": {}, \"value\": {} }}",
            json_string(key),
            json_string(&hex(value))
        ),
        Op::Erase { key, .. } => format!("{{ \"key\": {} }}", json_string(key)),
    }
}

/// What a conforming implementation must produce. `None` for trace lines that
/// describe the harness rather than the device.
fn expected_json(kind: &TraceKind) -> Option<String> {
    match kind {
        TraceKind::Op { .. } => None,
        TraceKind::Event(EventKind::Tick) => Some("\"event\": \"tick\"".to_string()),
        TraceKind::Event(EventKind::Datagram { len }) => {
            Some(format!("\"event\": \"datagram\", \"len\": {len}"))
        }
        TraceKind::Event(EventKind::PeerDiscovered { prefix }) => Some(format!(
            "\"event\": \"peer_discovered\", \"prefix\": \"{}\"",
            hex(prefix)
        )),
        TraceKind::Event(EventKind::PeerLost { prefix }) => Some(format!(
            "\"event\": \"peer_lost\", \"prefix\": \"{}\"",
            hex(prefix)
        )),
        TraceKind::Action(Action::SetTimer { in_us }) => {
            Some(format!("\"action\": \"set_timer\", \"in_us\": {in_us}"))
        }
        TraceKind::Action(Action::Send { to, datagram }) => Some(format!(
            "\"action\": \"send\", \"to\": {}, \"datagram\": \"{}\"",
            match to {
                lumen_device::Destination::Mesh => "\"mesh\"".to_string(),
                lumen_device::Destination::Peer(p) => format!("\"{}\"", hex(p)),
            },
            hex(datagram)
        )),
        TraceKind::Action(Action::DisciplineClock { offset_us }) => Some(format!(
            "\"action\": \"discipline\", \"offset_us\": {offset_us}"
        )),
        TraceKind::Action(Action::RoleChanged { role, epoch }) => Some(format!(
            "\"action\": \"role\", \"role\": \"{}\", \"epoch\": {epoch}",
            role_tag(*role)
        )),
        TraceKind::Action(Action::SyncLost) => Some("\"action\": \"sync_lost\"".to_string()),
        TraceKind::Action(Action::SyncAcquired) => {
            Some("\"action\": \"sync_acquired\"".to_string())
        }
        TraceKind::Rx { from, len, .. } => {
            Some(format!("\"rx\": {{ \"from\": {from}, \"len\": {len} }}"))
        }
        TraceKind::Frame { digest } => Some(format!("\"frame_digest\": \"{digest:016x}\"")),
        // Failure outcomes are part of the contract — "how an implementation
        // handles rubbish is part of the protocol" — so they are exported, as
        // a named reason rather than our error enum's spelling.
        TraceKind::Net(_) => Some("\"rejected\": \"net\"".to_string()),
        TraceKind::Store(_) => Some("\"rejected\": \"storage\"".to_string()),
        TraceKind::Led(_) => Some("\"rejected\": \"led\"".to_string()),
    }
}

/// A stable name per role, so a vector does not depend on Rust's Debug output.
fn role_tag(role: lumen_device::Role) -> &'static str {
    match role {
        lumen_device::Role::Follower => "follower",
        lumen_device::Role::Candidate => "candidate",
        lumen_device::Role::Leader => "leader",
    }
}

fn json_ids(ids: &[u16]) -> String {
    let body: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    format!("[{}]", body.join(", "))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Minimal but correct JSON string escaping.
///
/// Everything below 0x20 goes to `\uXXXX` rather than being emitted raw, which
/// is the rule most hand-rolled encoders get wrong and the one that produces a
/// file that parses everywhere except in the reviewer's editor.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::record;
    use crate::scenario::{NodeSpec, Scenario};
    use crate::world::{NodeCore, PeriodicCore};

    fn periodic(_: &NodeSpec) -> Box<dyn NodeCore> {
        Box::new(PeriodicCore::new(2_000))
    }

    fn sample() -> Recording {
        let scenario = Scenario::new(42, 20_000)
            .with_node(NodeSpec::new(1, 2).with_clock_error(-100, 15))
            .with_node(NodeSpec::new(2, 0))
            .with_faults(NetFaults::lossy_wifi())
            .at(0, Op::Join { node: 2, group: 3 })
            .at(1_000, Op::Tick(1))
            .at(
                2_000,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![0xCA, 0xFE],
                },
            )
            .at(
                2_500,
                Op::Multicast {
                    from: 1,
                    group: 3,
                    bytes: vec![],
                },
            )
            .at(
                3_000,
                Op::Present {
                    node: 1,
                    level: 900,
                },
            )
            .at(6_000, Op::Partition(1, 2))
            .at(6_500, Op::Heal(1, 2))
            .at(7_000, Op::HealAll)
            .at(
                7_500,
                Op::Split {
                    left: vec![1],
                    right: vec![2],
                },
            )
            .at(8_000, Op::SetFaults(NetFaults::perfect()))
            .at(
                8_500,
                Op::Skew {
                    node: 1,
                    offset_us: 5,
                },
            )
            .at(9_000, Op::Drift { node: 1, ppm: -3 })
            .at(
                9_500,
                Op::Discipline {
                    node: 1,
                    offset_us: 7,
                },
            )
            .at(
                10_000,
                Op::Store {
                    node: 2,
                    key: "zone/\"desk\"".into(),
                    value: vec![1],
                },
            )
            .at(
                10_500,
                Op::Erase {
                    node: 2,
                    key: "zone/desk".into(),
                },
            )
            .at(11_000, Op::Present { node: 2, level: 1 })
            // The power cycle comes late enough that the earlier datagrams have
            // already landed: killing a node discards everything in flight
            // towards it, and an earlier kill would silently empty the vector's
            // `rx` lines.
            .at(14_000, Op::Kill(2))
            .at(
                15_000,
                Op::Revive {
                    node: 2,
                    wipe_storage: true,
                },
            )
            // A present to a dead node: a defined failure outcome, and the one
            // this fixture exists to get into the exported vector.
            .at(15_500, Op::Kill(1))
            .at(16_000, Op::Present { node: 1, level: 1 });
        record(scenario, periodic)
    }

    /// A tiny structural check: the emitted text must be balanced and quoted
    /// correctly. Not a JSON parser — the real check is that `lumen-spec`'s
    /// runner reads it — but enough to catch a missing comma or bracket, which
    /// is what a hand-written encoder actually gets wrong.
    fn is_well_formed(json: &str) -> bool {
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        let mut prev_significant = ' ';
        for c in json.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                    // A trailing comma before a closer is the classic
                    // hand-rolled-JSON bug and is invalid everywhere.
                    if prev_significant == ',' {
                        return false;
                    }
                }
                _ => {}
            }
            if !c.is_whitespace() {
                prev_significant = c;
            }
        }
        depth == 0 && !in_string
    }

    #[test]
    fn the_export_is_well_formed_json() {
        let json = to_behavioural_vector(&sample(), "cold_start", "Cold start and convergence.");
        assert!(is_well_formed(&json), "{json}");
        assert!(json.starts_with("{\n"));
        assert!(json.trim_end().ends_with('}'));
    }

    #[test]
    fn the_export_carries_the_identifying_fields() {
        let json = to_behavioural_vector(&sample(), "cold_start", "Cold start.");
        for needle in [
            "\"schema\": 1",
            "\"kind\": \"behavioural\"",
            "\"name\": \"cold_start\"",
            "\"source\": \"lumen-sim\"",
            "\"seed\": 42",
            "\"duration_us\": 20000",
            "\"skew_us\": -100",
            "\"drift_ppm\": 15",
            "\"loss_permille\": 20",
        ] {
            assert!(json.contains(needle), "missing {needle} in\n{json}");
        }
    }

    #[test]
    fn every_op_variant_exports() {
        let json = to_behavioural_vector(&sample(), "all_ops", "Every op.");
        for tag in [
            "tick",
            "send",
            "multicast",
            "join",
            "kill",
            "revive",
            "partition",
            "heal",
            "heal_all",
            "split",
            "set_faults",
            "skew",
            "drift",
            "discipline",
            "present",
            "store",
            "erase",
        ] {
            assert!(
                json.contains(&format!("\"op\": \"{tag}\"")),
                "missing {tag}"
            );
        }
        assert!(json.contains("\"bytes\": \"cafe\""));
        assert!(json.contains("\"left\": [1], \"right\": [2]"));
        assert!(json.contains("\"wipe_storage\": true"));
        assert!(is_well_formed(&json));
    }

    #[test]
    fn harness_ops_are_not_exported_as_requirements() {
        let json = to_behavioural_vector(&sample(), "x", "y");
        let expected = json.split("\"expected\": [").nth(1).unwrap();
        // The `expected` block must contain only device-observable outcomes.
        assert!(!expected.contains("\"op\":"));
        assert!(expected.contains("\"event\": \"tick\""));
        assert!(expected.contains("\"action\": \"set_timer\""));
        assert!(expected.contains("\"rx\":"));
        assert!(expected.contains("\"frame_digest\":"));
    }

    #[test]
    fn failure_outcomes_are_exported_as_rejections() {
        // Node 2 has no pixels, so presenting to it is a `WrongPixelCount`;
        // that is a defined outcome and belongs in the vector.
        let json = to_behavioural_vector(&sample(), "x", "y");
        assert!(json.contains("\"rejected\": \"led\""));

        assert_eq!(
            expected_json(&TraceKind::Net(crate::net::NetError::NodeDown)),
            Some("\"rejected\": \"net\"".to_string())
        );
        assert_eq!(
            expected_json(&TraceKind::Store(crate::storage::StorageError::Full)),
            Some("\"rejected\": \"storage\"".to_string())
        );
        assert_eq!(expected_json(&TraceKind::Op { tag: "tick" }), None);
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("a\nb\r\tc"), "\"a\\nb\\r\\tc\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(json_string("é"), "\"é\"");
    }

    #[test]
    fn a_quoted_storage_key_does_not_break_the_file() {
        let json = to_behavioural_vector(&sample(), "x", "y");
        assert!(json.contains("zone/\\\"desk\\\""));
        assert!(is_well_formed(&json));
    }

    #[test]
    fn the_well_formed_check_actually_rejects_bad_json() {
        assert!(is_well_formed("{ \"a\": [1, 2] }"));
        assert!(!is_well_formed("{ \"a\": [1, 2] "));
        assert!(!is_well_formed("{ \"a\": [1, 2,] }"));
        assert!(!is_well_formed("}"));
        assert!(!is_well_formed("{ \"unterminated: 1 }"));
        assert!(is_well_formed("{ \"esc\": \"a\\\"}\" }"));
    }

    #[test]
    fn hex_and_ids_encode_plainly() {
        assert_eq!(hex(&[0, 255]), "00ff");
        assert_eq!(hex(&[]), "");
        assert_eq!(json_ids(&[]), "[]");
        assert_eq!(json_ids(&[1, 2, 3]), "[1, 2, 3]");
    }
}
