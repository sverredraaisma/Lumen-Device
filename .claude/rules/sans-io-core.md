---
paths:
  - "crates/lumen-device/**/*.rs"
---

# The core performs no I/O

Everything in this crate is a state machine of the shape:

```rust
fn on_event(&mut self, now: ShowTimeUs, ev: Event) -> Vec<Action>
```

Events in, actions out. The shell — firmware, daemon, simulator — owns the sockets,
the timers and the flash writes and never lets this crate reach them.

**Banned outright in this crate.** Each of these makes a run unreproducible, and a
distributed bug that cannot be replayed stays unfixed:

- `std::net`, `std::fs`, any socket or file handle
- `std::time::{SystemTime, Instant}` — time arrives as the `now` argument
- `rand`, `getrandom`, any unseeded randomness — entropy arrives through
  `lumen_hal::Entropy`
- `std::thread`, `tokio`, any executor
- `println!` / `eprintln!` — emit a log `Action` instead

If a change seems to need one of these, the logic has landed on the wrong side of
the boundary. Move it to the shell and pass the result in as an `Event`.

## Why this is worth the friction

Three things fall out of it, and none survive if it slips:

1. **Deterministic replay** — record the event stream, feed it back, get an
   identical run.
2. **Tests with no hardware, no network and no waiting** — a 24-hour clock-drift
   scenario runs in milliseconds.
3. **Behavioural conformance** — "given these events, did you emit these actions"
   is the entire contract, so a three-way split brain is just a longer vector file
   in `lumen-spec`.

It is a cheap constraint on new code and an expensive refactor of existing code.

## Every behaviour gets a vector

When you add a behaviour here, write the conformance vector in `lumen-spec` in the
same change. Retrofitting a suite across seven repos is miserable, and a bug
reproduced in `lumen-sim` should leave a vector behind so every implementation
inherits the regression test.

## Check new sources against the four rules

Anything that claims pixels declares a **priority and an expiry**. A source above
the ambient floor with no expiry is a bug — that is how a room ends up stuck red
at 3am. Activation is **scheduled at a named future show time**, never "go now".
Every failure path has a **defined visual outcome**; a device is never dark
because of software.
