# lumen-device

The mesh state machines — discovery, time sync, election, replication, zones, the
source stack, the render loop — plus `lumen-sim`, the simulator that runs whole
meshes with no hardware.

- **Licence:** GPL-3.0. This is the "how to be part of the mesh" side of the
  boundary; the permissive side is `lumen-core`.
- **Main branch:** `main`
- **Status:** W8 (the simulator) done; W5/W6/W7 still owe the state machines.

## Stack

- Rust 1.85+, edition 2021
- `lumen-device`: sans-IO state machines, no async runtime, no threads
- `lumen-sim`: simulated HAL over a simulated clock and network, plus replay
- No third-party dependencies anywhere in the workspace — the PRNG, the text
  recording format and the JSON vector writer are all hand-rolled, because a
  recorded run has to still reproduce after a `cargo update`

## Commands

```bash
cargo test --workspace
cargo test -p lumen-sim                      # scenario suite
cargo clippy --workspace --all-targets       # CI runs with -D warnings
cargo fmt --all -- --check
cargo llvm-cov --workspace --summary-only    # coverage; must be >= 95%
```

## The one architectural rule

Everything in `lumen-device` is `on_event(now, ev) -> Vec<Action>` and performs
**no I/O at all**. No sockets, no clock reads, no randomness, no threads, no
`println!`. The shell (firmware, daemon, simulator) does all of that and passes
results in as events.

Full list of what that bans, and why it is worth the friction:
`.claude/rules/sans-io-core.md` — it auto-loads when you open a file in that crate.

## Hard rules

- **Coverage floor is 95%**, measured on the workspace. Distributed logic that is
  not covered is logic nobody has ever actually run in the failure case.
- **Every behaviour ships with its conformance vector** in `lumen-spec`, in the
  same change. Retrofitting a suite across seven repos is miserable.
- **A bug reproduced in `lumen-sim` becomes a checked-in scenario**, and then a
  vector, so every implementation inherits the regression test.
- **Design for extension.** New roles, source kinds, zone predicates and record
  types should be additions behind an enum or a trait, not edits threaded through
  the render loop. If adding a source kind means touching five files, the seam is
  in the wrong place.
- **No `unsafe`.** `#![forbid(unsafe_code)]` stays.

## Check every change against the four project rules

1. **Priority and expiry on every source.** Nothing permanently captures a pixel;
   a dead publisher releases its claim. A source above the ambient floor with no
   expiry is a bug — that is how a room ends up stuck red at 3am.
2. **Scheduled activation, never "go now."** Changes take effect at a named future
   show time, so the mesh switches together across a network hiccup.
3. **Mapping is a pure upgrade.** Every device always has coordinates — synthetic,
   rough or mapped — so no feature gets an unmapped code path.
4. **Defined degradation.** Every failure has a specified visual outcome. Stale
   channels decay, a corrupt program falls back, a lost network keeps rendering.
   A device is never dark because of software.

## Gotchas

> Living section. Add anything that cost real time.

- **Local coverage does not work on this machine.** The `windows-gnu` toolchain
  ships no profiler runtime, so `cargo llvm-cov` fails with "the compiler may have
  been built without the profiler runtime". The gate that counts runs in CI on
  Linux. Installing the VS Build Tools C++ workload fixes this and the linker.
- **The default toolchain on this machine cannot link.** MSVC's `link.exe` is not
  installed; use `cargo +stable-x86_64-pc-windows-gnu ...`. Inside Git Bash,
  `/usr/bin/link` also shadows it even when it is installed.
- **The render clock never steps.** Disciplining it means slewing. A step is a
  visible glitch, and the wall clock is a separate, optional concern.
- **`lumen-hal` is a path dependency on a sibling checkout**, not a crates.io
  version — nothing is published yet. `../lumen-core/crates/lumen-hal` must
  exist, which is what `lumen-dev/scripts/clone-all.sh` arranges; CI checks the
  sibling out by hand. Both revert to a plain version dep at the first
  `lumen-core` release, and both are marked `TODO(release)`.
- **Nothing may iterate a `HashMap` in `lumen-sim`.** Delivery order to a
  multicast group, the order nodes are stepped in, the order storage enumerates:
  all of them are observable, so all of them use `BTreeMap`/`BTreeSet` or an
  explicit sort. This is what "same seed, same run" is actually made of.

## Specialized guides (loaded on demand — do not preload)

- Sans-IO constraints: `.claude/rules/sans-io-core.md` (auto-loads)
- Design notes: `docs/runtime-model.md`, `docs/data-model.md`, `docs/effects.md`
- Licence boundary and project-wide rules: `CONTRIBUTING.md`

## Compact instructions

Preserve code changes, file paths touched, decisions made, and any measured
number (coverage, sync offset, scenario seed). Drop raw build and test output.
