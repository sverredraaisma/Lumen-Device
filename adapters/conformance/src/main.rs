//! The conformance adapter for `lumen-device`.
//!
//! Speaks the runner's line protocol on stdin/stdout and forwards each request
//! into the real state machines. Without it the behavioural vectors run only
//! against the reference *fixture*, which answers from the corpus and therefore
//! passes by construction — a suite that cannot fail, which is the one thing a
//! conformance suite must never be.
//!
//! Because the cores are sans-IO, this is a loop around a pair of pure
//! functions and nothing else. No sockets, no threads, no clock: the vector
//! supplies the time.
//!
//! It claims `behavioural` only. The codec lives in `lumen-proto` on the other
//! side of the licence boundary and deserves its own adapter there; claiming
//! `codec` here would mean this GPL binary answering for an Apache crate's
//! conformance, and the runner would report a pass that belonged to neither.

use std::io::{BufRead, Write};

use lumen_conformance::hex;
use lumen_conformance::json::{parse, Json};
use lumen_device::channels::{Channel, ClaimOutcome};
use lumen_device::sources::{Change, PushError, Removal, Source, SourceStack};
use lumen_device::{Action, Destination, Event, Identity, Node, Role};
use lumen_proto::Uuid;
use lumen_vm::q16::Q16;

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut machine = Machine::None;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("lumen-device-adapter: stdin: {e}");
                return;
            }
        };
        let line = line.trim();
        // The diagnostic channel. Ignoring it here is what lets the other side
        // log without corrupting the exchange.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let response = respond(line, &mut machine);
        // Flush after every line without exception: a buffered answer is a
        // deadlock, not a slow one, because the runner blocks on each.
        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            return;
        }
    }
}

/// Whichever machine the current vector asked for.
enum Machine {
    None,
    Node(Box<Node>),
    Sources(Box<SourceStack>),
    Channel(Box<Channel>),
}

fn respond(line: &str, machine: &mut Machine) -> String {
    let (verb, rest) = match line.split_once(' ') {
        Some((v, r)) => (v, r.trim()),
        None => (line, "{}"),
    };
    let body = match parse(rest) {
        Ok(j) => j,
        Err(e) => return format!("error {verb} body is not json: {e}"),
    };

    match verb {
        "hello" => {
            // Codec vectors are not claimed, so the runner reports them as
            // skipped rather than failing this adapter for a half it never
            // offered to run.
            r#"ok {"name":"lumen-device 0.1.0","protocol":2,"kinds":["behavioural"]}"#.to_string()
        }
        "reset" => reset(&body, machine),
        "event" => event(&body, machine),
        "decode" | "encode" => "error this adapter runs behavioural vectors only".to_string(),
        other => format!("error unknown request verb `{other}`"),
    }
}

// ---- reset -----------------------------------------------------------------

fn reset(body: &Json, machine: &mut Machine) -> String {
    let Some(name) = body.get("machine").and_then(Json::as_str) else {
        return "error reset needs a `machine`".to_string();
    };
    let state = match body.get("state") {
        Some(s) => s,
        None => return "error reset needs a `state`".to_string(),
    };

    match name {
        "node" => match build_node(state) {
            Ok(n) => {
                *machine = Machine::Node(Box::new(n));
                "ok {}".to_string()
            }
            Err(e) => format!("error {e}"),
        },
        "sources" => {
            let budget = state.get("budget").and_then(Json::as_u64).unwrap_or(0) as u32;
            let max = state
                .get("max_concurrent")
                .and_then(Json::as_u64)
                .unwrap_or(0) as usize;
            *machine = Machine::Sources(Box::new(SourceStack::new(budget, max)));
            "ok {}".to_string()
        }
        "channel" => {
            let id = state.get("channel_id").and_then(Json::as_u64).unwrap_or(0) as u16;
            let hold_ms = state.get("hold_ms").and_then(Json::as_u64).unwrap_or(0) as u32;
            // A q16 default arrives as its raw i32, which may be negative.
            let default = match state.get("default") {
                Some(Json::Number(t)) => Q16(t.parse::<i64>().unwrap_or(0) as i32),
                _ => Q16::ZERO,
            };
            *machine = Machine::Channel(Box::new(Channel::new(id, hold_ms, default)));
            "ok {}".to_string()
        }
        // Naming a machine this implementation does not have is an `error`, not
        // a `reject`: the adapter failed to answer, rather than the
        // implementation refusing something.
        other => format!("error unknown machine `{other}`"),
    }
}

fn build_node(state: &Json) -> Result<Node, String> {
    let uuid = uuid_field(state, "uuid")?;
    let mesh_id = uuid_field(state, "mesh_id")?;
    let capacity = state
        .get("capacity")
        .and_then(Json::as_u64)
        .ok_or("state.capacity is missing")? as u32;
    let boot_counter = state
        .get("boot_counter")
        .and_then(Json::as_u64)
        .unwrap_or(1) as u32;
    let now_us = state.get("now_us").and_then(Json::as_u64).unwrap_or(0);

    Ok(Node::new(
        Identity { uuid, capacity },
        mesh_id,
        boot_counter,
        now_us,
    ))
}

fn uuid_field(state: &Json, key: &str) -> Result<Uuid, String> {
    let text = state
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("state.{key} is missing"))?;
    let bytes = hex::decode(text).map_err(|e| format!("state.{key}: {e}"))?;
    let arr: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("state.{key} must be 16 bytes, got {}", bytes.len()))?;
    Ok(Uuid(arr))
}

// ---- event -----------------------------------------------------------------

fn event(body: &Json, machine: &mut Machine) -> String {
    let at_us = body.get("at_us").and_then(Json::as_u64).unwrap_or(0);
    let Some(ev) = body.get("event") else {
        return "error event needs an `event`".to_string();
    };
    let Some(kind) = ev.get("event").and_then(Json::as_str) else {
        return "error event.event must name a kind".to_string();
    };

    match machine {
        Machine::None => "error no machine; send `reset` first".to_string(),
        Machine::Node(node) => node_event(node, at_us, kind, ev),
        Machine::Sources(stack) => sources_event(stack, at_us, kind, ev),
        Machine::Channel(ch) => channel_event(ch, at_us, kind, ev),
    }
}

fn producer_of(ev: &Json) -> Result<[u8; 4], String> {
    let text = ev
        .get("producer")
        .and_then(Json::as_str)
        .ok_or("the event needs a `producer`")?;
    let b = hex::decode(text).map_err(|e| format!("producer: {e}"))?;
    b.as_slice()
        .try_into()
        .map_err(|_| "a producer is 4 bytes".to_string())
}

fn channel_event(ch: &mut Channel, at_us: u64, kind: &str, ev: &Json) -> String {
    let mut actions: Vec<String> = Vec::new();
    match kind {
        "claim" => {
            let producer = match producer_of(ev) {
                Ok(p) => p,
                Err(e) => return format!("error {e}"),
            };
            let priority = ev.get("priority").and_then(Json::as_u64).unwrap_or(0) as u8;
            let lease_ms = ev.get("lease_ms").and_then(Json::as_u64).unwrap_or(0) as u32;
            actions.push(match ch.claim(at_us, producer, priority, lease_ms) {
                ClaimOutcome::Taken => r#"{"action":"claimed","outcome":"taken"}"#.to_string(),
                ClaimOutcome::Renewed => r#"{"action":"claimed","outcome":"renewed"}"#.to_string(),
                ClaimOutcome::Preempted { previous } => format!(
                    r#"{{"action":"claimed","outcome":"preempted","previous":"{}"}}"#,
                    hex::encode(&previous)
                ),
                ClaimOutcome::Refused { holder } => format!(
                    r#"{{"action":"claimed","outcome":"refused","holder":"{}"}}"#,
                    hex::encode(&holder)
                ),
            });
        }
        "release" => {
            let producer = match producer_of(ev) {
                Ok(p) => p,
                Err(e) => return format!("error {e}"),
            };
            // A release from anyone but the owner is ignored, not honoured:
            // otherwise one packet from any device knocks the desktop app off
            // the audio channel.
            actions.push(if ch.release(producer) {
                r#"{"action":"released"}"#.to_string()
            } else {
                r#"{"action":"release_ignored"}"#.to_string()
            });
        }
        "publish" => {
            let producer = match producer_of(ev) {
                Ok(p) => p,
                Err(e) => return format!("error {e}"),
            };
            let seq = ev.get("seq").and_then(Json::as_u64).unwrap_or(0) as u16;
            let value = match ev.get("value") {
                Some(Json::Number(t)) => Q16(t.parse::<i64>().unwrap_or(0) as i32),
                _ => Q16::ZERO,
            };
            actions.push(if ch.publish(at_us, producer, seq, value) {
                r#"{"action":"published"}"#.to_string()
            } else {
                r#"{"action":"dropped"}"#.to_string()
            });
        }
        "advance" => {
            if let Some(p) = ch.advance(at_us) {
                actions.push(format!(
                    r#"{{"action":"lease_lapsed","producer":"{}"}}"#,
                    hex::encode(&p)
                ));
            }
        }
        "read" => {
            actions.push(format!(
                r#"{{"action":"value","value":{},"stale":{}}}"#,
                ch.read(at_us).0,
                ch.is_stale(at_us)
            ));
        }
        other => return format!("error unknown event `{other}` for machine `channel`"),
    }
    format!("ok {{\"actions\":[{}]}}", actions.join(","))
}

fn node_event(node: &mut Node, at_us: u64, kind: &str, ev: &Json) -> String {
    // The datagram's bytes have to outlive the borrow inside `Event`.
    let bytes;
    let event = match kind {
        "tick" => Event::Tick,
        "datagram" => {
            let Some(text) = ev.get("bytes").and_then(Json::as_str) else {
                return "error datagram event needs `bytes`".to_string();
            };
            bytes = match hex::decode(text) {
                Ok(b) => b,
                Err(e) => return format!("error datagram bytes: {e}"),
            };
            Event::Datagram { bytes: &bytes }
        }
        "peer_discovered" | "peer_lost" => {
            let Some(prefix) = ev.get("prefix").and_then(Json::as_str) else {
                return format!("error {kind} needs a `prefix`");
            };
            let decoded = match hex::decode(prefix) {
                Ok(b) => b,
                Err(e) => return format!("error prefix: {e}"),
            };
            let Ok(p): Result<[u8; 4], _> = decoded.as_slice().try_into() else {
                return "error prefix must be 4 bytes".to_string();
            };
            if kind == "peer_discovered" {
                Event::PeerDiscovered { prefix: p }
            } else {
                Event::PeerLost { prefix: p }
            }
        }
        other => return format!("error unknown event `{other}` for machine `node`"),
    };

    let actions = node.on_event(at_us, event);
    let rendered: Vec<String> = actions.iter().map(render_node_action).collect();
    format!("ok {{\"actions\":[{}]}}", rendered.join(","))
}

fn render_node_action(a: &Action) -> String {
    match a {
        Action::SetTimer { in_us } => {
            format!(r#"{{"action":"set_timer","in_us":{in_us}}}"#)
        }
        Action::Send { to, datagram } => {
            let dest = match to {
                Destination::Mesh => "mesh".to_string(),
                Destination::Peer(p) => hex::encode(p),
            };
            format!(
                r#"{{"action":"send","to":"{dest}","datagram":"{}"}}"#,
                hex::encode(datagram)
            )
        }
        Action::DisciplineClock { offset_us } => {
            format!(r#"{{"action":"discipline","offset_us":{offset_us}}}"#)
        }
        Action::RoleChanged { role, epoch } => {
            let name = match role {
                Role::Leader => "leader",
                Role::Follower => "follower",
                // No vector spells this one yet, so the name is chosen here.
                // Emitting it rather than collapsing it into "follower" keeps
                // a vector that ever does distinguish them honest.
                Role::Candidate => "candidate",
            };
            format!(r#"{{"action":"role","role":"{name}","epoch":{epoch}}}"#)
        }
        Action::SyncLost => r#"{"action":"sync_lost"}"#.to_string(),
        Action::SyncAcquired => r#"{"action":"sync_acquired"}"#.to_string(),
    }
}

fn sources_event(stack: &mut SourceStack, at_us: u64, kind: &str, ev: &Json) -> String {
    let mut changes = Vec::new();
    let mut refused: Option<String> = None;

    match kind {
        "push" => {
            let source = match build_source(ev, at_us) {
                Ok(s) => s,
                Err(e) => return format!("error {e}"),
            };
            if let Err(e) = stack.push(at_us, source, &mut changes) {
                refused = Some(render_push_error(&e));
            }
        }
        "pop" => {
            let id = match uuid_field(ev, "id") {
                Ok(u) => u,
                Err(e) => return format!("error {e}"),
            };
            // `pop` answers whether it found anything. Popping something
            // already gone is refused rather than silently accepted: a caller
            // that pops twice has lost track of its own source, and telling it
            // so is the difference between a bug it can see and one it cannot.
            if !stack.pop(at_us, id, &mut changes) {
                refused = Some(r#"{"action":"refused","reason":"not_found"}"#.to_string());
            }
        }
        "advance" => stack.advance(at_us, &mut changes),
        other => return format!("error unknown event `{other}` for machine `sources`"),
    }

    let mut rendered: Vec<String> = changes.iter().map(render_change).collect();
    // A refusal is the answer to the push, so it comes after whatever the push
    // managed to change — which for a refused push is nothing.
    if let Some(r) = refused {
        rendered.push(r);
    }
    format!("ok {{\"actions\":[{}]}}", rendered.join(","))
}

fn build_source(ev: &Json, at_us: u64) -> Result<Source, String> {
    Ok(Source {
        id: uuid_field(ev, "id")?,
        zone: uuid_field(ev, "zone")?,
        scene: uuid_field(ev, "scene")?,
        priority: ev
            .get("priority")
            .and_then(Json::as_u64)
            .ok_or("push needs a `priority`")? as u8,
        // `null` and an absent key both mean "no expiry", which is the whole
        // point of the ambient floor.
        expires_at_us: ev.get("expires_at_us").and_then(Json::as_u64),
        fade_in_ms: ev.get("fade_in_ms").and_then(Json::as_u64).unwrap_or(0) as u16,
        fade_out_ms: ev.get("fade_out_ms").and_then(Json::as_u64).unwrap_or(0) as u16,
        cost: ev.get("cost").and_then(Json::as_u64).unwrap_or(0) as u32,
        // Not a field of the vector: it is when the push arrived, which the
        // step already states as its own time.
        pushed_at_us: at_us,
    })
}

fn render_push_error(e: &PushError) -> String {
    match e {
        PushError::NoExpiry { priority } => {
            format!(r#"{{"action":"refused","reason":"no_expiry","priority":{priority}}}"#)
        }
        PushError::AlreadyExpired { expires_at_us } => format!(
            r#"{{"action":"refused","reason":"already_expired","expires_at_us":{expires_at_us}}}"#
        ),
        PushError::NoRoom => r#"{"action":"refused","reason":"no_room"}"#.to_string(),
    }
}

fn render_change(c: &Change) -> String {
    match c {
        Change::Admitted(id) => {
            format!(r#"{{"action":"admitted","id":"{}"}}"#, hex::encode(&id.0))
        }
        Change::Rejected { id, cost, spare } => format!(
            r#"{{"action":"rejected","id":"{}","cost":{cost},"spare":{spare}}}"#,
            hex::encode(&id.0)
        ),
        Change::Removed { id, reason } => {
            let reason = match reason {
                Removal::Expired => "expired",
                Removal::Popped => "popped",
                Removal::Superseded => "superseded",
            };
            format!(
                r#"{{"action":"removed","id":"{}","reason":"{reason}"}}"#,
                hex::encode(&id.0)
            )
        }
        Change::FadeFinished(id) => {
            format!(
                r#"{{"action":"fade_finished","id":"{}"}}"#,
                hex::encode(&id.0)
            )
        }
    }
}
