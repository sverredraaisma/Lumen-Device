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
- `lumen-device`: sans-IO state machines, no async runtime, no threads; `no_std`
  plus `alloc`, and it builds for bare metal (`riscv32imac-unknown-none-elf`,
  `xtensa-esp32s3-none-elf`) - `std` is switched on for tests only
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

## Rendering on more than one core

`render::Shard` is the seam. A device with two cores builds one shard per core,
gives each its own `Renderer` and the matching run of the output buffer from
`split_at_mut`, and merges the `FrameReport`s afterwards. **No thread appears in
this crate and none may** - the firmware decides how many cores go through the
seam, which is what keeps the rule below intact.

Shards must render exactly what one whole render does, and that is checked
(`shards_render_what_one_whole_does`, over several frames so the per-LED history
is covered) rather than assumed. A two-core device rendering a different show
from a one-core device would break the mesh's agreement with itself, which is
worse than being slow and would stay invisible until two kinds of device shared
a room. Spike S4 measured 2.1x on an ESP32-S3 with byte-identical output:
`lumen-dev/spikes/s4-dual-core/RESULTS.md`.

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

- **The render loop is per pixel, and the tests are four LEDs long.** A linear
  scan to find an LED by index was free in every host test and cost about as
  much per frame as running the effect did at 300 LEDs on real silicon. Anything
  in `render_source`'s inner loop is paid three hundred times a frame, sixty
  times a second, on a chip with no cache to hide it; the per-LED history map is
  the next such cost and has not been dealt with.
- **The header's `budget` is the `pixel` section only.** Charge the `frame`
  section `Program::section_cost(Section::Frame)` instead. Reusing the per-pixel
  figure faults every effect that hoists anything substantial - which is to say
  the well-written ones - and it faulted a shipped corpus effect for exactly
  that reason.

- **The "cannot link / no local coverage" note used to be wrong; both now work.**
  `link.exe` was never missing. What was missing was the **Windows SDK**, so the
  linker had no `kernel32.lib` to link against and Rust reported that as
  "linker `link.exe` not found". Adding the SDK component to the existing VS
  2022 install fixed the MSVC toolchain and `cargo llvm-cov` together. If a
  fresh machine shows this symptom, install the C++ workload rather than
  switching to `windows-gnu`: that workaround builds, which is why nobody
  revisits it, and it silently costs you coverage because the `windows-gnu`
  toolchain ships no profiler runtime.
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
