//! Record a run, write it down, read it back, run it again.
//!
//! The point is not archival. It is that a distributed bug which took a
//! thousand-node-second scenario and a particular packet-loss pattern to
//! produce should survive as a file somebody can check in — and that the file
//! is small, diffable text rather than a memory dump, so a reviewer can see
//! what the failing run actually did.
//!
//! The format is line-oriented rather than JSON because the only consumers are
//! this crate and a human reading a diff; JSON would need a hand-written parser
//! (no third-party dependencies) for no gain. The *export* to `lumen-spec` is
//! JSON, because that side has other implementations to serve — see
//! [`crate::export`].

use crate::clock::Ppm;
use crate::led::LedError;
use crate::net::{NetError, NetFaults, NetStats, NodeId};
use crate::scenario::{NodeSpec, Op, Scenario, ScriptedOp};
use crate::storage::StorageError;
use crate::world::{op_tag, EventKind, NodeCore, RunReport, TraceEntry, TraceKind, World};
use lumen_device::Action;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Format version. Bumped when the grammar changes in a way an old file would
/// not survive — a recording that silently parses differently is worse than one
/// that refuses to parse.
pub const FORMAT_VERSION: u32 = 1;

const HEADER: &str = "lumen-sim-recording";

/// Placeholder for an empty byte string. A bare empty token would vanish under
/// whitespace splitting and shift every field after it.
const EMPTY: &str = "-";

/// A complete run: what was asked for, and what happened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Recording {
    pub scenario: Scenario,
    pub report: RunReport,
}

/// Why a recording would not parse.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    pub message: String,
}

impl ParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Two runs that were supposed to be identical, and were not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Divergence {
    /// Index into the trace where they first differ.
    pub index: usize,
    pub recorded: Option<TraceEntry>,
    pub replayed: Option<TraceEntry>,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "replay diverged at trace[{}]: recorded {:?}, replayed {:?}",
            self.index, self.recorded, self.replayed
        )
    }
}

impl std::error::Error for Divergence {}

/// Run a scenario and keep the result.
pub fn record(
    scenario: Scenario,
    factory: impl FnMut(&NodeSpec) -> Box<dyn NodeCore>,
) -> Recording {
    let mut world = World::new(scenario, factory);
    let report = world.run();
    Recording {
        scenario: world.scenario().clone(),
        report,
    }
}

/// Run a recording's scenario again.
///
/// Note what is *not* fed back in: the trace. Replay re-derives every packet
/// loss, every jitter draw and every delivery order from the seed. Replaying
/// the recorded outcomes instead would prove nothing — it would only show that
/// a list can be read twice.
pub fn replay(
    recording: &Recording,
    factory: impl FnMut(&NodeSpec) -> Box<dyn NodeCore>,
) -> RunReport {
    World::new(recording.scenario.clone(), factory).run()
}

/// Replay and check the result matches, byte for byte.
pub fn verify_replay(
    recording: &Recording,
    factory: impl FnMut(&NodeSpec) -> Box<dyn NodeCore>,
) -> Result<RunReport, Divergence> {
    let fresh = replay(recording, factory);
    match recording.report.first_divergence(&fresh) {
        None if recording.report == fresh => Ok(fresh),
        None => Err(Divergence {
            // Traces agree but something outside them (stats, frame digests)
            // does not. Reported at the end rather than pretending to a
            // position, because there is no trace index to blame.
            index: recording.report.trace.len(),
            recorded: None,
            replayed: None,
        }),
        Some((index, recorded, replayed)) => Err(Divergence {
            index,
            recorded,
            replayed,
        }),
    }
}

// ---------------------------------------------------------------- hex helpers

fn to_hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return EMPTY.to_string();
    }
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn from_hex(token: &str, line: usize) -> Result<Vec<u8>, ParseError> {
    if token == EMPTY {
        return Ok(Vec::new());
    }
    if token.len() % 2 != 0 {
        return Err(ParseError::new(line, "odd-length hex"));
    }
    (0..token.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&token[i..i + 2], 16)
                .map_err(|_| ParseError::new(line, format!("bad hex {token:?}")))
        })
        .collect()
}

// ------------------------------------------------------------------ writing

impl Recording {
    /// Serialise to the checked-in text form.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{HEADER} {FORMAT_VERSION}");
        let _ = writeln!(out, "seed {}", self.scenario.seed);
        let _ = writeln!(out, "duration_us {}", self.scenario.duration_us);
        let _ = writeln!(out, "faults {}", faults_to_text(&self.scenario.faults));
        for n in &self.scenario.nodes {
            let _ = writeln!(
                out,
                "node {} {} {} {} {}",
                n.id, n.pixel_count, n.skew_us, n.drift_ppm, n.storage_capacity
            );
        }
        for s in &self.scenario.script {
            let _ = writeln!(out, "op {} {}", s.at_us, op_to_text(&s.op));
        }
        let _ = writeln!(out, "end {}", self.report.end_us);
        let s = &self.report.stats;
        let _ = writeln!(
            out,
            "stats {} {} {} {} {} {} {}",
            s.sent,
            s.delivered,
            s.dropped_loss,
            s.dropped_partition,
            s.dropped_unreachable,
            s.duplicated,
            s.reordered
        );
        for (node, (count, digest)) in &self.report.frames {
            let _ = writeln!(out, "frames {node} {count} {digest:016x}");
        }
        for e in &self.report.trace {
            let _ = writeln!(
                out,
                "trace {} {} {}",
                e.at_us,
                e.node,
                trace_to_text(&e.kind)
            );
        }
        out
    }

    /// Parse the text form.
    pub fn from_text(text: &str) -> Result<Recording, ParseError> {
        let mut seed = None;
        let mut duration_us = None;
        let mut faults = NetFaults::perfect();
        let mut nodes = Vec::new();
        let mut script = Vec::new();
        let mut end_us = 0u64;
        let mut stats = NetStats::default();
        let mut frames: BTreeMap<NodeId, (u64, u64)> = BTreeMap::new();
        let mut trace = Vec::new();
        let mut saw_header = false;

        for (idx, raw) in text.lines().enumerate() {
            let line = idx + 1;
            let raw = raw.trim();
            // Blank lines and `#` comments are skipped so a checked-in
            // recording can be annotated with what it is reproducing.
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let mut t = raw.split_whitespace();
            let keyword = t.next().expect("non-empty after trim");
            match keyword {
                HEADER => {
                    let v: u32 = num(&mut t, line, "version")?;
                    if v != FORMAT_VERSION {
                        return Err(ParseError::new(
                            line,
                            format!("format version {v}, expected {FORMAT_VERSION}"),
                        ));
                    }
                    saw_header = true;
                }
                "seed" => seed = Some(num::<u64>(&mut t, line, "seed")?),
                "duration_us" => duration_us = Some(num::<u64>(&mut t, line, "duration_us")?),
                "faults" => faults = faults_from_text(&mut t, line)?,
                "node" => nodes.push(NodeSpec {
                    id: num(&mut t, line, "id")?,
                    pixel_count: num(&mut t, line, "pixel_count")?,
                    skew_us: num(&mut t, line, "skew_us")?,
                    drift_ppm: num::<Ppm>(&mut t, line, "drift_ppm")?,
                    storage_capacity: num(&mut t, line, "storage_capacity")?,
                }),
                "op" => {
                    let at_us = num(&mut t, line, "at_us")?;
                    script.push(ScriptedOp {
                        at_us,
                        op: op_from_text(&mut t, line)?,
                    });
                }
                "end" => end_us = num(&mut t, line, "end")?,
                "stats" => {
                    stats = NetStats {
                        sent: num(&mut t, line, "sent")?,
                        delivered: num(&mut t, line, "delivered")?,
                        dropped_loss: num(&mut t, line, "dropped_loss")?,
                        dropped_partition: num(&mut t, line, "dropped_partition")?,
                        dropped_unreachable: num(&mut t, line, "dropped_unreachable")?,
                        duplicated: num(&mut t, line, "duplicated")?,
                        reordered: num(&mut t, line, "reordered")?,
                    }
                }
                "frames" => {
                    let node: NodeId = num(&mut t, line, "node")?;
                    let count: u64 = num(&mut t, line, "count")?;
                    let digest = hex_u64(&mut t, line, "digest")?;
                    frames.insert(node, (count, digest));
                }
                "trace" => trace.push(TraceEntry {
                    at_us: num(&mut t, line, "at_us")?,
                    node: num(&mut t, line, "node")?,
                    kind: trace_from_text(&mut t, line)?,
                }),
                other => return Err(ParseError::new(line, format!("unknown keyword {other:?}"))),
            }
        }

        if !saw_header {
            return Err(ParseError::new(1, "missing header line"));
        }
        Ok(Recording {
            scenario: Scenario {
                seed: seed.ok_or_else(|| ParseError::new(1, "missing seed"))?,
                duration_us: duration_us
                    .ok_or_else(|| ParseError::new(1, "missing duration_us"))?,
                nodes,
                faults,
                script,
            },
            report: RunReport {
                trace,
                stats,
                end_us,
                frames,
            },
        })
    }
}

type Tokens<'b> = std::str::SplitWhitespace<'b>;

fn next_token<'b>(t: &mut Tokens<'b>, line: usize, what: &str) -> Result<&'b str, ParseError> {
    t.next()
        .ok_or_else(|| ParseError::new(line, format!("missing {what}")))
}

fn num<T: std::str::FromStr>(t: &mut Tokens<'_>, line: usize, what: &str) -> Result<T, ParseError> {
    let tok = next_token(t, line, what)?;
    tok.parse()
        .map_err(|_| ParseError::new(line, format!("bad {what}: {tok:?}")))
}

fn hex_u64(t: &mut Tokens<'_>, line: usize, what: &str) -> Result<u64, ParseError> {
    let tok = next_token(t, line, what)?;
    u64::from_str_radix(tok, 16).map_err(|_| ParseError::new(line, format!("bad {what}: {tok:?}")))
}

fn bytes(t: &mut Tokens<'_>, line: usize, what: &str) -> Result<Vec<u8>, ParseError> {
    let tok = next_token(t, line, what)?;
    from_hex(tok, line)
}

fn text_field(t: &mut Tokens<'_>, line: usize, what: &str) -> Result<String, ParseError> {
    let raw = bytes(t, line, what)?;
    String::from_utf8(raw).map_err(|_| ParseError::new(line, format!("{what} is not UTF-8")))
}

fn ids(t: &mut Tokens<'_>, line: usize, what: &str) -> Result<Vec<NodeId>, ParseError> {
    let count: usize = num(t, line, what)?;
    (0..count).map(|_| num(t, line, what)).collect()
}

fn faults_to_text(f: &NetFaults) -> String {
    format!(
        "{} {} {} {} {} {}",
        f.loss_permille,
        f.latency_us,
        f.jitter_us,
        f.reorder_permille,
        f.reorder_extra_us,
        f.duplicate_permille
    )
}

fn faults_from_text(t: &mut Tokens<'_>, line: usize) -> Result<NetFaults, ParseError> {
    Ok(NetFaults {
        loss_permille: num(t, line, "loss_permille")?,
        latency_us: num(t, line, "latency_us")?,
        jitter_us: num(t, line, "jitter_us")?,
        reorder_permille: num(t, line, "reorder_permille")?,
        reorder_extra_us: num(t, line, "reorder_extra_us")?,
        duplicate_permille: num(t, line, "duplicate_permille")?,
    })
}

fn op_to_text(op: &Op) -> String {
    let tag = op_tag(op);
    match op {
        Op::Tick(id) | Op::Kill(id) => format!("{tag} {id}"),
        Op::Send { from, to, bytes } => format!("{tag} {from} {to} {}", to_hex(bytes)),
        Op::Multicast { from, group, bytes } => format!("{tag} {from} {group} {}", to_hex(bytes)),
        Op::Join { node, group } => format!("{tag} {node} {group}"),
        Op::Revive { node, wipe_storage } => format!("{tag} {node} {}", u8::from(*wipe_storage)),
        Op::Partition(a, b) | Op::Heal(a, b) => format!("{tag} {a} {b}"),
        Op::HealAll => tag.to_string(),
        Op::Split { left, right } => {
            let mut s = format!("{tag} {}", left.len());
            for id in left {
                let _ = write!(s, " {id}");
            }
            let _ = write!(s, " {}", right.len());
            for id in right {
                let _ = write!(s, " {id}");
            }
            s
        }
        Op::SetFaults(f) => format!("{tag} {}", faults_to_text(f)),
        Op::Skew { node, offset_us } | Op::Discipline { node, offset_us } => {
            format!("{tag} {node} {offset_us}")
        }
        Op::Drift { node, ppm } => format!("{tag} {node} {ppm}"),
        Op::Present { node, level } => format!("{tag} {node} {level}"),
        Op::Store { node, key, value } => {
            format!("{tag} {node} {} {}", to_hex(key.as_bytes()), to_hex(value))
        }
        Op::Erase { node, key } => format!("{tag} {node} {}", to_hex(key.as_bytes())),
    }
}

fn op_from_text(t: &mut Tokens<'_>, line: usize) -> Result<Op, ParseError> {
    let tag = next_token(t, line, "op tag")?;
    Ok(match tag {
        "tick" => Op::Tick(num(t, line, "node")?),
        "send" => Op::Send {
            from: num(t, line, "from")?,
            to: num(t, line, "to")?,
            bytes: bytes(t, line, "bytes")?,
        },
        "multicast" => Op::Multicast {
            from: num(t, line, "from")?,
            group: num(t, line, "group")?,
            bytes: bytes(t, line, "bytes")?,
        },
        "join" => Op::Join {
            node: num(t, line, "node")?,
            group: num(t, line, "group")?,
        },
        "kill" => Op::Kill(num(t, line, "node")?),
        "revive" => Op::Revive {
            node: num(t, line, "node")?,
            wipe_storage: num::<u8>(t, line, "wipe_storage")? != 0,
        },
        "partition" => Op::Partition(num(t, line, "a")?, num(t, line, "b")?),
        "heal" => Op::Heal(num(t, line, "a")?, num(t, line, "b")?),
        "heal_all" => Op::HealAll,
        "split" => Op::Split {
            left: ids(t, line, "left")?,
            right: ids(t, line, "right")?,
        },
        "set_faults" => Op::SetFaults(faults_from_text(t, line)?),
        "skew" => Op::Skew {
            node: num(t, line, "node")?,
            offset_us: num(t, line, "offset_us")?,
        },
        "drift" => Op::Drift {
            node: num(t, line, "node")?,
            ppm: num(t, line, "ppm")?,
        },
        "discipline" => Op::Discipline {
            node: num(t, line, "node")?,
            offset_us: num(t, line, "offset_us")?,
        },
        "present" => Op::Present {
            node: num(t, line, "node")?,
            level: num(t, line, "level")?,
        },
        "store" => Op::Store {
            node: num(t, line, "node")?,
            key: text_field(t, line, "key")?,
            value: bytes(t, line, "value")?,
        },
        "erase" => Op::Erase {
            node: num(t, line, "node")?,
            key: text_field(t, line, "key")?,
        },
        other => return Err(ParseError::new(line, format!("unknown op {other:?}"))),
    })
}

fn trace_to_text(kind: &TraceKind) -> String {
    match kind {
        TraceKind::Op { tag } => format!("op {tag}"),
        TraceKind::Event(EventKind::Tick) => "event tick".to_string(),
        TraceKind::Event(EventKind::Datagram { len }) => format!("event datagram {len}"),
        TraceKind::Event(EventKind::PeerDiscovered { prefix }) => {
            format!("event peer_up {}", hex4(prefix))
        }
        TraceKind::Event(EventKind::PeerLost { prefix }) => {
            format!("event peer_down {}", hex4(prefix))
        }
        TraceKind::Action(Action::SetTimer { in_us }) => format!("action set_timer {in_us}"),
        TraceKind::Action(Action::Send {
            to,
            datagram,
            transport,
        }) => format!(
            // Transport before the bytes: it is one short token and the
            // datagram is unbounded, so a line stays readable when it wraps.
            "action send {} {} {}",
            match to {
                lumen_device::Destination::Mesh => "mesh".to_string(),
                lumen_device::Destination::Peer(p) => hex4(p),
            },
            match transport {
                lumen_device::Transport::Datagram => "datagram",
                lumen_device::Transport::Reliable => "reliable",
            },
            hex_bytes(datagram)
        ),
        TraceKind::Action(Action::DisciplineClock { offset_us }) => {
            format!("action discipline {offset_us}")
        }
        TraceKind::Action(Action::RoleChanged { role, epoch }) => {
            format!("action role {} {epoch}", role_name(*role))
        }
        TraceKind::Action(Action::SyncLost) => "action sync_lost".to_string(),
        TraceKind::Action(Action::SyncAcquired) => "action sync_acquired".to_string(),
        TraceKind::Rx { from, len, digest } => format!("rx {from} {len} {digest:016x}"),
        TraceKind::Frame { digest } => format!("frame {digest:016x}"),
        TraceKind::Net(e) => format!("net_err {}", net_err_tag(e)),
        TraceKind::Store(e) => format!("store_err {}", store_err_tag(e)),
        TraceKind::Led(e) => format!("led_err {}", led_err_tag(e)),
    }
}

fn trace_from_text(t: &mut Tokens<'_>, line: usize) -> Result<TraceKind, ParseError> {
    let kind = next_token(t, line, "trace kind")?;
    Ok(match kind {
        "op" => TraceKind::Op {
            tag: op_tag_from_str(next_token(t, line, "op tag")?, line)?,
        },
        "event" => match next_token(t, line, "event")? {
            "tick" => TraceKind::Event(EventKind::Tick),
            "datagram" => TraceKind::Event(EventKind::Datagram {
                len: num(t, line, "len")?,
            }),
            "peer_up" => TraceKind::Event(EventKind::PeerDiscovered {
                prefix: prefix4(t, line)?,
            }),
            "peer_down" => TraceKind::Event(EventKind::PeerLost {
                prefix: prefix4(t, line)?,
            }),
            other => return Err(ParseError::new(line, format!("unknown event {other:?}"))),
        },
        "action" => match next_token(t, line, "action")? {
            "set_timer" => TraceKind::Action(Action::SetTimer {
                in_us: num(t, line, "in_us")?,
            }),
            "send" => {
                let dest = next_token(t, line, "destination")?;
                let to = if dest == "mesh" {
                    lumen_device::Destination::Mesh
                } else {
                    lumen_device::Destination::Peer(parse_prefix(dest, line)?)
                };
                let transport = match next_token(t, line, "transport")? {
                    "datagram" => lumen_device::Transport::Datagram,
                    "reliable" => lumen_device::Transport::Reliable,
                    // Not defaulted: a replay that guessed the transport would
                    // reproduce a run the recording never described, which is
                    // the one thing a replay must not do.
                    other => {
                        return Err(ParseError::new(
                            line,
                            format!("unknown transport `{other}`"),
                        ))
                    }
                };
                TraceKind::Action(Action::Send {
                    to,
                    datagram: parse_bytes(next_token(t, line, "datagram")?, line)?,
                    transport,
                })
            }
            "discipline" => TraceKind::Action(Action::DisciplineClock {
                offset_us: signed(t, line, "offset_us")?,
            }),
            "role" => {
                let role = parse_role(next_token(t, line, "role")?, line)?;
                TraceKind::Action(Action::RoleChanged {
                    role,
                    epoch: num(t, line, "epoch")?,
                })
            }
            "sync_lost" => TraceKind::Action(Action::SyncLost),
            "sync_acquired" => TraceKind::Action(Action::SyncAcquired),
            other => return Err(ParseError::new(line, format!("unknown action {other:?}"))),
        },
        "rx" => TraceKind::Rx {
            from: num(t, line, "from")?,
            len: num(t, line, "len")?,
            digest: hex_u64(t, line, "digest")?,
        },
        "frame" => TraceKind::Frame {
            digest: hex_u64(t, line, "digest")?,
        },
        "net_err" => TraceKind::Net(match next_token(t, line, "net_err")? {
            "node_down" => NetError::NodeDown,
            "payload_too_large" => NetError::PayloadTooLarge,
            "buffer_too_small" => NetError::BufferTooSmall,
            other => return Err(ParseError::new(line, format!("unknown net_err {other:?}"))),
        }),
        "store_err" => TraceKind::Store(match next_token(t, line, "store_err")? {
            "value_too_long" => StorageError::ValueTooLong,
            "full" => StorageError::Full,
            "buffer_too_small" => StorageError::BufferTooSmall,
            other => {
                return Err(ParseError::new(
                    line,
                    format!("unknown store_err {other:?}"),
                ))
            }
        }),
        "led_err" => TraceKind::Led(match next_token(t, line, "led_err")? {
            "wrong_pixel_count" => LedError::WrongPixelCount,
            "powered_off" => LedError::PoweredOff,
            other => return Err(ParseError::new(line, format!("unknown led_err {other:?}"))),
        }),
        other => {
            return Err(ParseError::new(
                line,
                format!("unknown trace kind {other:?}"),
            ))
        }
    })
}

/// `TraceKind::Op` holds a `&'static str`, so parsing has to map back onto one
/// of the known tags rather than leaking an arbitrary string from a file into a
/// static-lifetime slot.
fn op_tag_from_str(s: &str, line: usize) -> Result<&'static str, ParseError> {
    const TAGS: [&str; 17] = [
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
    ];
    TAGS.into_iter()
        .find(|t| *t == s)
        .ok_or_else(|| ParseError::new(line, format!("unknown op tag {s:?}")))
}

fn net_err_tag(e: &NetError) -> &'static str {
    match e {
        NetError::NodeDown => "node_down",
        NetError::PayloadTooLarge => "payload_too_large",
        NetError::BufferTooSmall => "buffer_too_small",
    }
}

fn store_err_tag(e: &StorageError) -> &'static str {
    match e {
        StorageError::ValueTooLong => "value_too_long",
        StorageError::Full => "full",
        StorageError::BufferTooSmall => "buffer_too_small",
    }
}

fn led_err_tag(e: &LedError) -> &'static str {
    match e {
        LedError::WrongPixelCount => "wrong_pixel_count",
        LedError::PoweredOff => "powered_off",
    }
}

/// Four bytes as hex, for a sender prefix.
fn hex4(bytes: &[u8; 4]) -> String {
    hex_bytes(bytes)
}

fn hex_bytes(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2 + 1);
    if bytes.is_empty() {
        // An empty field would read as a missing token and shift every field
        // after it, which is the classic way a hand-rolled line format loses
        // data silently.
        return "-".to_string();
    }
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn parse_bytes(text: &str, line: usize) -> Result<Vec<u8>, ParseError> {
    if text == "-" {
        return Ok(Vec::new());
    }
    if text.len() % 2 != 0 {
        return Err(ParseError::new(line, "hex field has an odd length"));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let raw = text.as_bytes();
    for pair in raw.chunks(2) {
        let hi = hex_digit(pair[0], line)?;
        let lo = hex_digit(pair[1], line)?;
        out.push(hi * 16 + lo);
    }
    Ok(out)
}

fn parse_prefix(text: &str, line: usize) -> Result<[u8; 4], ParseError> {
    let bytes = parse_bytes(text, line)?;
    bytes
        .try_into()
        .map_err(|_| ParseError::new(line, "a sender prefix is four bytes"))
}

fn prefix4(t: &mut Tokens<'_>, line: usize) -> Result<[u8; 4], ParseError> {
    parse_prefix(next_token(t, line, "prefix")?, line)
}

fn hex_digit(c: u8, line: usize) -> Result<u8, ParseError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ParseError::new(line, "not a hex digit")),
    }
}

fn signed(t: &mut Tokens<'_>, line: usize, what: &str) -> Result<i64, ParseError> {
    next_token(t, line, what)?
        .parse()
        .map_err(|_| ParseError::new(line, format!("{what} is not a signed number")))
}

fn role_name(role: lumen_device::Role) -> &'static str {
    match role {
        lumen_device::Role::Follower => "follower",
        lumen_device::Role::Candidate => "candidate",
        lumen_device::Role::Leader => "leader",
    }
}

fn parse_role(text: &str, line: usize) -> Result<lumen_device::Role, ParseError> {
    Ok(match text {
        "follower" => lumen_device::Role::Follower,
        "candidate" => lumen_device::Role::Candidate,
        "leader" => lumen_device::Role::Leader,
        other => return Err(ParseError::new(line, format!("unknown role {other:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{IdleCore, PeriodicCore};

    fn idle(_: &NodeSpec) -> Box<dyn NodeCore> {
        Box::new(IdleCore)
    }

    fn periodic(_: &NodeSpec) -> Box<dyn NodeCore> {
        Box::new(PeriodicCore::new(1_000))
    }

    /// A scenario that touches every op variant, so the format is exercised
    /// end to end rather than on the three ops a hand-written test remembers.
    fn kitchen_sink() -> Scenario {
        Scenario::new(0xDEC0DE, 200_000)
            .with_node(NodeSpec::new(1, 4).with_clock_error(250, 12))
            .with_node(NodeSpec::new(2, 0).with_storage_capacity(1024))
            .with_node(NodeSpec::new(3, 2))
            .with_faults(NetFaults::lossy_wifi())
            .at(0, Op::Join { node: 2, group: 1 })
            .at(0, Op::Join { node: 3, group: 1 })
            .at(1_000, Op::Tick(1))
            .at(
                2_000,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![0xDE, 0xAD],
                },
            )
            .at(
                2_500,
                Op::Send {
                    from: 1,
                    to: 2,
                    bytes: vec![],
                },
            )
            .at(
                3_000,
                Op::Multicast {
                    from: 1,
                    group: 1,
                    bytes: vec![1, 2, 3],
                },
            )
            .at(
                4_000,
                Op::Store {
                    node: 2,
                    key: "zone/desk".into(),
                    value: vec![9],
                },
            )
            .at(
                4_500,
                Op::Erase {
                    node: 2,
                    key: "zone/desk".into(),
                },
            )
            .at(
                5_000,
                Op::Present {
                    node: 1,
                    level: 700,
                },
            )
            .at(6_000, Op::Partition(1, 2))
            .at(7_000, Op::Heal(1, 2))
            .at(
                8_000,
                Op::Split {
                    left: vec![1],
                    right: vec![2, 3],
                },
            )
            .at(9_000, Op::HealAll)
            .at(10_000, Op::SetFaults(NetFaults::perfect()))
            .at(
                11_000,
                Op::Skew {
                    node: 3,
                    offset_us: -400,
                },
            )
            .at(12_000, Op::Drift { node: 3, ppm: -25 })
            .at(
                13_000,
                Op::Discipline {
                    node: 3,
                    offset_us: 900,
                },
            )
            .at(14_000, Op::Kill(3))
            .at(
                15_000,
                Op::Revive {
                    node: 3,
                    wipe_storage: true,
                },
            )
            .at(16_000, Op::Present { node: 3, level: 5 })
    }

    #[test]
    fn a_recorded_run_replays_identically() {
        let rec = record(kitchen_sink(), periodic);
        let again = verify_replay(&rec, periodic).expect("replay must match");
        assert_eq!(again, rec.report);
        assert_eq!(again.digest(), rec.report.digest());
        assert!(!rec.report.trace.is_empty());
    }

    #[test]
    fn replay_survives_a_round_trip_through_text() {
        let rec = record(kitchen_sink(), periodic);
        let text = rec.to_text();
        let parsed = Recording::from_text(&text).expect("round trip");
        assert_eq!(parsed, rec);
        assert_eq!(parsed.to_text(), text);
        verify_replay(&parsed, periodic).expect("a parsed recording still replays");
    }

    #[test]
    fn the_text_form_is_readable_and_annotatable() {
        let rec = record(
            Scenario::new(1, 10).with_nodes(1, 0).at(1, Op::Tick(1)),
            idle,
        );
        let text = rec.to_text();
        assert!(text.starts_with("lumen-sim-recording 1\n"));
        assert!(text.contains("\nseed 1\n"));
        assert!(text.contains("\nop 1 tick 1\n"));

        let annotated = format!("# reproduces the 3am red room\n\n{text}");
        assert_eq!(Recording::from_text(&annotated).unwrap(), rec);
    }

    #[test]
    fn a_different_seed_gives_a_different_run() {
        let a = record(kitchen_sink(), periodic);
        let mut other = kitchen_sink();
        other.seed += 1;
        let b = record(other, periodic);
        assert_ne!(a.report.digest(), b.report.digest());
    }

    #[test]
    fn a_divergent_replay_is_reported_with_a_position() {
        let mut rec = record(kitchen_sink(), periodic);
        // Corrupt the recording as a stand-in for a nondeterministic core.
        rec.report.trace[3].at_us += 1;
        let err = verify_replay(&rec, periodic).unwrap_err();
        assert_eq!(err.index, 3);
        assert!(err.recorded.is_some() && err.replayed.is_some());
        assert!(format!("{err}").contains("diverged at trace[3]"));
    }

    #[test]
    fn divergence_outside_the_trace_is_still_caught() {
        let mut rec = record(kitchen_sink(), periodic);
        rec.report.stats.delivered += 1;
        let err = verify_replay(&rec, periodic).unwrap_err();
        assert_eq!(err.index, rec.report.trace.len());
        assert!(err.recorded.is_none());
    }

    #[test]
    fn empty_byte_strings_survive_the_format() {
        assert_eq!(to_hex(&[]), EMPTY);
        assert_eq!(from_hex(EMPTY, 1).unwrap(), Vec::<u8>::new());
        assert_eq!(to_hex(&[0x00, 0xFF]), "00ff");
        assert_eq!(from_hex("00ff", 1).unwrap(), vec![0x00, 0xFF]);
    }

    #[test]
    fn bad_hex_is_rejected() {
        assert_eq!(from_hex("abc", 4).unwrap_err().line, 4);
        assert!(from_hex("zz", 4).is_err());
    }

    #[test]
    fn a_missing_header_is_an_error() {
        let err = Recording::from_text("seed 1\nduration_us 2\n").unwrap_err();
        assert!(err.message.contains("header"));
    }

    #[test]
    fn a_wrong_version_is_an_error() {
        let err = Recording::from_text("lumen-sim-recording 99\n").unwrap_err();
        assert!(err.message.contains("format version 99"));
    }

    #[test]
    fn missing_required_fields_are_errors() {
        let err = Recording::from_text("lumen-sim-recording 1\nduration_us 5\n").unwrap_err();
        assert!(err.message.contains("seed"));
        let err = Recording::from_text("lumen-sim-recording 1\nseed 5\n").unwrap_err();
        assert!(err.message.contains("duration_us"));
    }

    #[test]
    fn unknown_keywords_and_tags_are_errors() {
        for (text, needle) in [
            ("lumen-sim-recording 1\nwat 1\n", "unknown keyword"),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\nop 0 wat\n",
                "unknown op",
            ),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\ntrace 0 1 wat\n",
                "unknown trace kind",
            ),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\ntrace 0 1 op wat\n",
                "unknown op tag",
            ),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\ntrace 0 1 event wat\n",
                "unknown event",
            ),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\ntrace 0 1 action wat\n",
                "unknown action",
            ),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\ntrace 0 1 net_err wat\n",
                "unknown net_err",
            ),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\ntrace 0 1 store_err wat\n",
                "unknown store_err",
            ),
            (
                "lumen-sim-recording 1\nseed 1\nduration_us 1\ntrace 0 1 led_err wat\n",
                "unknown led_err",
            ),
        ] {
            let err = Recording::from_text(text).unwrap_err();
            assert!(
                err.message.contains(needle),
                "{text:?} gave {:?}",
                err.message
            );
        }
    }

    #[test]
    fn truncated_and_malformed_lines_are_errors() {
        for text in [
            "lumen-sim-recording\n",
            "lumen-sim-recording x\n",
            "lumen-sim-recording 1\nseed\n",
            "lumen-sim-recording 1\nseed x\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nop\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nop 0 tick\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nfaults 1 2\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nnode 1 2\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nframes 1 2 zz\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nstats 1\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nop 0 split 2 1\n",
            "lumen-sim-recording 1\nseed 1\nduration_us 1\nop 0 store 1 ff ff\nend\n",
        ] {
            assert!(
                Recording::from_text(text).is_err(),
                "{text:?} should not parse"
            );
        }
    }

    #[test]
    fn a_non_utf8_key_is_rejected() {
        let text = "lumen-sim-recording 1\nseed 1\nduration_us 1\nop 0 store 1 ff ff\n".to_string();
        let err = Recording::from_text(&text).unwrap_err();
        assert!(err.message.contains("not UTF-8"));
    }

    #[test]
    fn every_trace_kind_survives_the_round_trip() {
        let kinds = vec![
            TraceKind::Op { tag: "heal_all" },
            TraceKind::Event(EventKind::Tick),
            TraceKind::Action(Action::SetTimer { in_us: 42 }),
            TraceKind::Rx {
                from: 3,
                len: 7,
                digest: 0xABCD,
            },
            TraceKind::Frame { digest: 0x1234 },
            TraceKind::Net(NetError::NodeDown),
            TraceKind::Net(NetError::PayloadTooLarge),
            TraceKind::Net(NetError::BufferTooSmall),
            TraceKind::Store(StorageError::Full),
            TraceKind::Store(StorageError::ValueTooLong),
            TraceKind::Store(StorageError::BufferTooSmall),
            TraceKind::Led(LedError::PoweredOff),
            TraceKind::Led(LedError::WrongPixelCount),
        ];
        for kind in kinds {
            let text = trace_to_text(&kind);
            let mut tokens = text.split_whitespace();
            assert_eq!(trace_from_text(&mut tokens, 1).unwrap(), kind, "{text}");
        }
    }

    #[test]
    fn parse_errors_display_with_a_line_number() {
        let e = ParseError::new(7, "boom");
        assert_eq!(format!("{e}"), "line 7: boom");
        let _: &dyn std::error::Error = &e;
    }
}
