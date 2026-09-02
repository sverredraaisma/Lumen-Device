# lumen-device

What makes a device a device: discovery, time sync, election, replication, the
source stack, zones and the render loop — plus the simulator that exercises all
of it without hardware.

**GPL-3.0.** This is the deliberate side of the licence boundary. A third-party
controller needs the codec, the compiler and the VM (all Apache, in
`lumen-core`); it does not need any of this. Someone selling a device that runs
these state machines publishes their changes.

| Crate | Contents |
|---|---|
| `lumen-device` | the sans-IO mesh state machines |
| `lumen-sim` | simulated HAL over a simulated clock and network, scenario harness, deterministic replay |

## Sans-IO

The core performs **no I/O at all**:

```rust
fn on_event(&mut self, now: Instant, ev: Event) -> Vec<Action>
```

There is no `rand()` to accidentally call and no socket to accidentally open,
because the core cannot reach them — determinism is enforced by the type system
rather than by code review. Three things fall out of that:

- **Deterministic replay.** Record the event stream, feed it back, get an
  identical run.
- **Tests without hardware, network or real time.** A 24-hour clock-drift
  scenario runs in milliseconds.
- **Behavioural conformance is possible at all.** "Given these events, did you
  emit these actions" is the entire contract, so a three-way split brain is
  just a longer vector file in `lumen-spec`.

## Why the simulator lives here

Not in the desktop app: this is the repo where election and replication bugs
live, so this is the repo that most needs the harness. Its CI runs whole-mesh
scenario tests with fault injection on every PR.

A bug reproduced here should be exported as a conformance vector to
`lumen-spec`, so every implementation inherits the regression test.

## Development

```
cargo test --workspace
```
