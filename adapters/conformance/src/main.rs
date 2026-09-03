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
use lumen_crypto::Ed25519Verifier;
use lumen_device::channels::{Channel, ClaimOutcome};
use lumen_device::gateway::{
    admit, Binding, BindingError, Ingress, Protocol, Refusal, MAX_GATEWAY_PRIORITY,
};
use lumen_device::records::{Authority, Hlc, Record, RecordType, RejectReason, Store};
use lumen_device::render::{Bound, RenderFault, Renderer, Rgb};
use lumen_device::sources::{Change, PushError, Removal, Source, SourceStack};
use lumen_device::zones::{Axis, Clause, CmpOp, DeviceLeds, Led, MapQuality, Predicate, Zone};
use lumen_device::{Action, Destination, Event, Identity, Node, Role};
use lumen_proto::Uuid;
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::NoUniforms;

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
    /// The gateway policy is a pure function rather than a state machine, so
    /// what a vector resets is the binding it is judged against.
    Gateway(Box<Option<Binding>>),
    /// Zone resolution is a pure function of a zone and one device's LEDs, so
    /// a vector resets both together.
    Zone(Box<(Zone, DeviceLeds)>),
    Records(Box<(Store, Authority)>),
    Render(Box<RenderState>),
}

/// What a `render` vector accumulates: a stack, the device's LEDs, and one
/// owned program per bound source.
///
/// The bytecode is owned here because `Bound` borrows a parsed `Program`, which
/// in turn borrows the bytes. Parsing per frame rather than holding the parsed
/// form keeps those lifetimes local to one call.
struct RenderState {
    renderer: Renderer,
    stack: SourceStack,
    leds: DeviceLeds,
    bound: Vec<BoundSource>,
}

struct BoundSource {
    source: Source,
    bytecode: Vec<u8>,
    membership: lumen_device::zones::Membership,
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
        "gateway" => {
            *machine = Machine::Gateway(Box::new(match build_binding(state) {
                Ok(b) => Some(b),
                Err(e) => return format!("error {e}"),
            }));
            "ok {}".to_string()
        }
        "zone" => match build_zone_state(state) {
            Ok(z) => {
                *machine = Machine::Zone(Box::new(z));
                "ok {}".to_string()
            }
            Err(e) => format!("error {e}"),
        },
        "records" => match build_authority(state) {
            Ok(a) => {
                *machine = Machine::Records(Box::new((Store::new(), a)));
                "ok {}".to_string()
            }
            Err(e) => format!("error {e}"),
        },
        "render" => {
            let budget = state
                .get("budget")
                .and_then(Json::as_u64)
                .unwrap_or(u32::MAX as u64) as u32;
            let max = state
                .get("max_concurrent")
                .and_then(Json::as_u64)
                .unwrap_or(8) as usize;
            let count = state.get("led_count").and_then(Json::as_u64).unwrap_or(4) as u16;
            *machine = Machine::Render(Box::new(RenderState {
                renderer: Renderer::new(),
                stack: SourceStack::new(budget, max),
                leds: strip_of(count),
                bound: Vec::new(),
            }));
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
        Machine::Gateway(binding) => gateway_event(binding, at_us, kind, ev),
        Machine::Zone(z) => zone_event(&z.0, &z.1, kind),
        Machine::Records(r) => records_event(&mut r.0, &r.1, kind, ev),
        Machine::Render(r) => render_event(r, at_us, kind, ev),
    }
}

/// A synthetic strip: `count` LEDs in a line, one metre apart.
fn strip_of(count: u16) -> DeviceLeds {
    DeviceLeds {
        device: Uuid([0xDD; 16]),
        // Rough rather than synthetic, so a geometric zone would be meaningful
        // here even though these vectors do not use one.
        quality: MapQuality::Rough,
        leds: (0..count)
            .map(|i| Led {
                index: i,
                world: [Q16::from_int(i as i16), Q16::ZERO, Q16::ZERO],
                local: [Q16::from_int(i as i16), Q16::ZERO, Q16::ZERO],
            })
            .collect(),
    }
}

fn render_event(st: &mut RenderState, at_us: u64, kind: &str, ev: &Json) -> String {
    match kind {
        "bind" => {
            let bytecode = match hex::decode(ev.get("program").and_then(Json::as_str).unwrap_or(""))
            {
                Ok(b) => b,
                Err(e) => return format!("error program: {e}"),
            };
            let source = match build_source(ev, at_us) {
                Ok(s) => s,
                Err(e) => return format!("error {e}"),
            };
            // The membership comes from resolving a real zone rather than
            // being assembled here: `Bounds` is computed by that resolution and
            // is not the adapter's to invent, and a hand-built one would let a
            // projection be normalised over a range no zone ever produced.
            let from = ev.get("led_from").and_then(Json::as_u64).unwrap_or(0) as u16;
            let to = ev
                .get("led_to")
                .and_then(Json::as_u64)
                .unwrap_or(st.leds.leds.len() as u64) as u16;
            let zone = Zone {
                id: source.zone,
                include: vec![Clause::Device {
                    device: st.leds.device,
                    leds: Some((from, to)),
                }],
                exclude: Vec::new(),
                projection: lumen_device::zones::Projection::Strip,
            };
            let membership = zone.resolve(&st.leds);

            let mut changes = Vec::new();
            let refused = st.stack.push(at_us, source, &mut changes).err();
            st.bound.push(BoundSource {
                source,
                bytecode,
                membership,
            });

            let mut actions: Vec<String> = changes.iter().map(render_change).collect();
            if let Some(e) = refused {
                actions.push(render_push_error(&e));
            }
            format!("ok {{\"actions\":[{}]}}", actions.join(","))
        }
        "frame" => {
            let t = match ev.get("t") {
                Some(Json::Number(x)) => Q16(x.parse::<i64>().unwrap_or(0) as i32),
                _ => Q16::ZERO,
            };

            // Parse every program first: `Bound` borrows them, so they have to
            // outlive the render call.
            let mut programs = Vec::new();
            for b in &st.bound {
                match Program::parse(&b.bytecode) {
                    Ok(p) => programs.push(p),
                    Err(e) => return format!("error a bound program does not parse: {e:?}"),
                }
            }
            let bound: Vec<Bound<'_>> = st
                .bound
                .iter()
                .zip(programs.iter())
                .map(|(b, p)| Bound {
                    source: b.source,
                    program: p,
                    membership: &b.membership,
                    projection: lumen_device::zones::Projection::Strip,
                })
                .collect();

            // The buffer starts black so the vector can tell "nothing wrote
            // here" apart from "something wrote black".
            let mut out = alloc_pixels(st.leds.leds.len());
            let report = st.renderer.render(
                at_us,
                t,
                &st.leds,
                &st.stack,
                &bound,
                &mut NoUniforms,
                &mut out,
            );

            let pixels: Vec<String> = out
                .iter()
                .map(|p| format!("[{},{},{}]", p.r.0, p.g.0, p.b.0))
                .collect();
            let mut actions: Vec<String> = Vec::new();
            actions.push(format!(
                r#"{{"action":"pixels","rgb":[{}]}}"#,
                pixels.join(",")
            ));
            for f in &report.faults {
                let RenderFault::Program { source, .. } = f;
                actions.push(format!(
                    r#"{{"action":"faulted","source":"{}"}}"#,
                    hex::encode(&source.0)
                ));
            }
            format!("ok {{\"actions\":[{}]}}", actions.join(","))
        }
        "advance" => {
            let mut changes = Vec::new();
            st.stack.advance(at_us, &mut changes);
            let actions: Vec<String> = changes.iter().map(render_change).collect();
            format!("ok {{\"actions\":[{}]}}", actions.join(","))
        }
        other => format!("error unknown event `{other}` for machine `render`"),
    }
}

fn alloc_pixels(n: usize) -> Vec<Rgb> {
    (0..n)
        .map(|_| Rgb::new(Q16::ZERO, Q16::ZERO, Q16::ZERO))
        .collect()
}

fn key32(j: &Json, key: &str) -> Result<[u8; 32], String> {
    let text = j
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("`{key}` is missing"))?;
    let b = hex::decode(text).map_err(|e| format!("{key}: {e}"))?;
    b.as_slice()
        .try_into()
        .map_err(|_| format!("`{key}` must be 32 bytes"))
}

fn build_authority(state: &Json) -> Result<Authority, String> {
    let mut a = Authority::new();
    if let Some(arr) = state.get("controllers").and_then(Json::as_array) {
        for c in arr {
            a.authorise_controller(uuid_field(c, "id")?, key32(c, "key")?);
        }
    }
    if let Some(arr) = state.get("devices").and_then(Json::as_array) {
        for d in arr {
            a.register_device(uuid_field(d, "id")?, key32(d, "key")?);
        }
    }
    Ok(a)
}

fn build_record(j: &Json) -> Result<Record, String> {
    let kind = RecordType::from_u8(j.get("kind").and_then(Json::as_u64).unwrap_or(255) as u8)
        .ok_or("unknown record kind")?;
    let sig_bytes = hex::decode(
        j.get("signature")
            .and_then(Json::as_str)
            .ok_or("the record needs a `signature`")?,
    )
    .map_err(|e| format!("signature: {e}"))?;
    let signature: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| "a signature is 64 bytes".to_string())?;
    Ok(Record {
        id: uuid_field(j, "id")?,
        kind,
        hlc: Hlc(j.get("hlc").and_then(Json::as_u64).unwrap_or(0)),
        author: uuid_field(j, "author")?,
        body: hex::decode(j.get("body").and_then(Json::as_str).unwrap_or(""))
            .map_err(|e| format!("body: {e}"))?,
        signature,
    })
}

fn reject_name(r: &RejectReason) -> &'static str {
    match r {
        RejectReason::BadSignature => "bad_signature",
        RejectReason::UnknownAuthor => "unknown_author",
        RejectReason::NotItsOwnDeviceRecord => "not_its_own_device_record",
        RejectReason::WrongAuthority => "wrong_authority",
        RejectReason::Superseded => "superseded",
    }
}

fn records_event(store: &mut Store, authority: &Authority, kind: &str, ev: &Json) -> String {
    let action = match kind {
        "accept" => {
            let rj = ev.get("record").ok_or(()).map_err(|_| ());
            let Ok(rj) = rj else {
                return "error accept needs a `record`".to_string();
            };
            let record = match build_record(rj) {
                Ok(r) => r,
                Err(e) => return format!("error {e}"),
            };
            let wall_ms = ev.get("wall_ms").and_then(Json::as_u64).unwrap_or(0);
            // A real verifier, not a stub. The byte order a signature covers is
            // the normative part, and a vector checked against a stub would pin
            // none of it.
            match store.accept(record, authority, &Ed25519Verifier, wall_ms) {
                Ok(()) => r#"{"action":"accepted"}"#.to_string(),
                Err(r) => format!(r#"{{"action":"rejected","reason":"{}"}}"#, reject_name(&r)),
            }
        }
        "digest" => {
            let entries: Vec<String> = store
                .digest()
                .iter()
                .map(|e| format!(r#"{{"id":"{}","hlc":{}}}"#, hex::encode(&e.id.0), e.hlc.0))
                .collect();
            format!(r#"{{"action":"digest","entries":[{}]}}"#, entries.join(","))
        }
        "wanted" => {
            let theirs: Vec<lumen_device::records::DigestEntry> = ev
                .get("theirs")
                .and_then(Json::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            Some(lumen_device::records::DigestEntry {
                                id: uuid_field(e, "id").ok()?,
                                hlc: Hlc(e.get("hlc").and_then(Json::as_u64)?),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let ids: Vec<String> = store
                .wanted(&theirs)
                .iter()
                .map(|i| format!("\"{}\"", hex::encode(&i.0)))
                .collect();
            format!(r#"{{"action":"wanted","ids":[{}]}}"#, ids.join(","))
        }
        other => return format!("error unknown event `{other}` for machine `records`"),
    };
    format!("ok {{\"actions\":[{action}]}}")
}

fn q16_of(j: &Json, key: &str) -> Q16 {
    match j.get(key) {
        Some(Json::Number(t)) => Q16(t.parse::<i64>().unwrap_or(0) as i32),
        _ => Q16::ZERO,
    }
}

fn triple(j: &Json, key: &str) -> [Q16; 3] {
    let Some(arr) = j.get(key).and_then(Json::as_array) else {
        return [Q16::ZERO; 3];
    };
    let mut out = [Q16::ZERO; 3];
    for (i, v) in arr.iter().take(3).enumerate() {
        if let Json::Number(t) = v {
            out[i] = Q16(t.parse::<i64>().unwrap_or(0) as i32);
        }
    }
    out
}

fn build_predicate(j: &Json) -> Result<Predicate, String> {
    match j.get("predicate").and_then(Json::as_str) {
        Some("compare") => {
            let axis = match j.get("axis").and_then(Json::as_str) {
                Some("x") => Axis::X,
                Some("y") => Axis::Y,
                Some("z") => Axis::Z,
                other => return Err(format!("unknown axis {other:?}")),
            };
            let op = match j.get("op").and_then(Json::as_str) {
                Some("lt") => CmpOp::Lt,
                Some("le") => CmpOp::Le,
                Some("gt") => CmpOp::Gt,
                Some("ge") => CmpOp::Ge,
                other => return Err(format!("unknown comparison {other:?}")),
            };
            Ok(Predicate::Compare {
                axis,
                op,
                value: q16_of(j, "value"),
            })
        }
        Some("near") => Ok(Predicate::Near {
            point: triple(j, "point"),
            radius: q16_of(j, "radius"),
        }),
        Some("not") => {
            let inner = j.get("of").ok_or("`not` needs an `of`")?;
            Ok(Predicate::Not(alloc_box(build_predicate(inner)?)))
        }
        Some(k @ ("all" | "any")) => {
            let arr = j
                .get("of")
                .and_then(Json::as_array)
                .ok_or("needs an `of` array")?;
            let parts: Result<Vec<Predicate>, String> = arr.iter().map(build_predicate).collect();
            let parts = parts?;
            Ok(if k == "all" {
                Predicate::All(parts)
            } else {
                Predicate::Any(parts)
            })
        }
        other => Err(format!("unknown predicate {other:?}")),
    }
}

fn alloc_box(p: Predicate) -> Box<Predicate> {
    Box::new(p)
}

fn build_clause(j: &Json) -> Result<Clause, String> {
    match j.get("clause").and_then(Json::as_str) {
        Some("device") => {
            let leds = j.get("leds").and_then(Json::as_array).map(|a| {
                let g = |i: usize| a.get(i).and_then(Json::as_u64).unwrap_or(0) as u16;
                (g(0), g(1))
            });
            Ok(Clause::Device {
                device: uuid_field(j, "device")?,
                leds,
            })
        }
        Some("where") => Ok(Clause::Where(build_predicate(j)?)),
        other => Err(format!("unknown clause {other:?}")),
    }
}

fn build_zone_state(state: &Json) -> Result<(Zone, DeviceLeds), String> {
    let zj = state.get("zone").ok_or("state.zone is missing")?;
    let clauses = |key: &str| -> Result<Vec<Clause>, String> {
        match zj.get(key).and_then(Json::as_array) {
            Some(a) => a.iter().map(build_clause).collect(),
            None => Ok(Vec::new()),
        }
    };
    let zone = Zone {
        id: uuid_field(zj, "id")?,
        include: clauses("include")?,
        exclude: clauses("exclude")?,
        // Membership does not depend on the projection, and these vectors are
        // about membership. Strip is the default and the one an unmapped device
        // uses, so it is the honest placeholder rather than an arbitrary one.
        projection: lumen_device::zones::Projection::Strip,
    };

    let dj = state.get("device").ok_or("state.device is missing")?;
    let quality = match dj.get("quality").and_then(Json::as_str) {
        Some("synthetic") => MapQuality::Synthetic,
        Some("rough") => MapQuality::Rough,
        Some("mapped") => MapQuality::Mapped,
        other => return Err(format!("unknown mapping quality {other:?}")),
    };
    let leds = dj
        .get("leds")
        .and_then(Json::as_array)
        .ok_or("state.device.leds is missing")?
        .iter()
        .map(|l| Led {
            index: l.get("index").and_then(Json::as_u64).unwrap_or(0) as u16,
            world: triple(l, "world"),
            local: triple(l, "local"),
        })
        .collect();

    Ok((
        zone,
        DeviceLeds {
            device: uuid_field(dj, "device")?,
            quality,
            leds,
        },
    ))
}

fn zone_event(zone: &Zone, dev: &DeviceLeds, kind: &str) -> String {
    match kind {
        "resolve" => {
            let m = zone.resolve(dev);
            let leds: Vec<String> = m.leds.iter().map(|i| i.to_string()).collect();
            format!(
                "ok {{\"actions\":[{{\"action\":\"selected\",\"leds\":[{}]}}]}}",
                leds.join(",")
            )
        }
        "why_excluded" => format!(
            "ok {{\"actions\":[{{\"action\":\"excluded_for_mapping\",\"excluded\":{}}}]}}",
            zone.excluded_for_mapping(dev)
        ),
        other => format!("error unknown event `{other}` for machine `zone`"),
    }
}

fn protocol_of(name: &str) -> Result<Protocol, String> {
    Ok(match name {
        "artnet" => Protocol::ArtNet,
        "e131" => Protocol::E131,
        "ddp" => Protocol::Ddp,
        "mqtt" => Protocol::Mqtt,
        "http" => Protocol::Http,
        other => return Err(format!("unknown protocol `{other}`")),
    })
}

fn build_binding(state: &Json) -> Result<Binding, String> {
    let protocol = protocol_of(
        state
            .get("protocol")
            .and_then(Json::as_str)
            .ok_or("state.protocol is missing")?,
    )?;
    let from = state.get("pixel_from").and_then(Json::as_u64).unwrap_or(0) as u16;
    let to = state.get("pixel_to").and_then(Json::as_u64).unwrap_or(0) as u16;
    Ok(Binding {
        id: uuid_field(state, "id")?,
        protocol,
        zone: uuid_field(state, "zone")?,
        pixels: (from, to),
        priority_ceiling: state
            .get("priority_ceiling")
            .and_then(Json::as_u64)
            .ok_or("state.priority_ceiling is missing")? as u8,
    })
}

fn render_binding_error(e: &BindingError) -> String {
    match e {
        BindingError::CeilingTooHigh { asked, max } => format!(
            r#"{{"action":"bad_binding","reason":"ceiling_too_high","asked":{asked},"max":{max}}}"#
        ),
        BindingError::EmptyRange => {
            r#"{"action":"bad_binding","reason":"empty_range"}"#.to_string()
        }
    }
}

fn gateway_event(binding: &Option<Binding>, at_us: u64, kind: &str, ev: &Json) -> String {
    let Some(binding) = binding else {
        return "error no binding; send `reset` first".to_string();
    };
    match kind {
        "ingress" => {
            let ingress = Ingress {
                offset: ev.get("offset").and_then(Json::as_u64).unwrap_or(0) as u16,
                count: ev.get("count").and_then(Json::as_u64).unwrap_or(0) as u16,
                // Absent means the protocol carries no priority at all, which
                // is not the same as asking for zero.
                priority: ev.get("priority").and_then(Json::as_u64).map(|v| v as u8),
            };
            let id = match uuid_field(ev, "source_id") {
                Ok(u) => u,
                Err(e) => return format!("error {e}"),
            };
            let action = match admit(binding, &ingress, id, at_us) {
                Ok(a) => format!(
                    concat!(
                        r#"{{"action":"admitted","pixel_from":{},"pixel_to":{},"clipped":{},"#,
                        r#""priority":{},"priority_clamped":{},"expires_at_us":{}}}"#
                    ),
                    a.pixels.0,
                    a.pixels.1,
                    a.clipped,
                    a.source.priority,
                    a.priority_clamped,
                    // Every gateway source gets a lease whether it asked for one
                    // or not, so this is never absent.
                    match a.source.expires_at_us {
                        Some(v) => v.to_string(),
                        None => "null".to_string(),
                    }
                ),
                Err(Refusal::OutsideBinding) => {
                    r#"{"action":"refused","reason":"outside_binding"}"#.to_string()
                }
                Err(Refusal::BadBinding(e)) => render_binding_error(&e),
            };
            format!("ok {{\"actions\":[{action}]}}")
        }
        "program" => {
            // Not a question with two answers. A program is signed code that
            // runs on every device, and taking one from an unauthenticated
            // channel would make the signing pointless.
            let g = lumen_device::gateway::Gateway::new();
            format!(
                "ok {{\"actions\":[{{\"action\":\"program_accepted\",\"accepted\":{}}}]}}",
                g.accepts_programs()
            )
        }
        "ceiling" => format!(
            "ok {{\"actions\":[{{\"action\":\"max_priority\",\"max\":{MAX_GATEWAY_PRIORITY}}}]}}"
        ),
        other => format!("error unknown event `{other}` for machine `gateway`"),
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
