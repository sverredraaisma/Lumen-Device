What is actually rendering at any given moment, and how everything that wants to change the lights competes for them. [[Data Model]] says what is *stored*; this says what *happens*.

## The source stack

**There is exactly one mechanism.** Every zone holds a stack of active sources. Anything that wants to affect lights pushes a source; the highest-priority live source renders. Shows, schedules, alerts, the app, Home Assistant and Art-Net are all just things that push.

A source is:

```
source {
  id        who pushed it
  zone      what it covers
  scene     effect + parameter values
  priority  0..255
  expires   show time, or never
  fade_in   ms
  fade_out  ms
}
```

Resolution per pixel, every frame: take the highest-priority source covering it; on equal priority, the most recently pushed wins. A source is removed when it expires, when its pusher explicitly pops it, or when its pusher's lease lapses.

The payoff is that a hard question — "an alert fires during a scheduled show while I'm manually controlling the lights, what happens?" — has no special case. Three sources are on the stack at three priorities, the alert renders, and when it expires the lights fall back to manual, then to the show. Nothing had to be coordinated, and nothing gets permanently stuck.

| Band | Typical pusher | Expiry |
|---|---|---|
| 0–63 | default / ambient scene | never — this is the floor |
| 64–127 | schedules, shows, audio-reactive | at show end |
| 128–191 | app manual control, live preview | lease, ~30 s renewed while the app is open |
| 192–223 | system self-health — see [[#Status lighting]] | short timeout, always |
| 224–255 | user status and alerts | short timeout, always |

**Every source above the floor must expire.** A source with no expiry at priority >0 is a bug — it is how a system ends up stuck red at 3am with nobody knowing why. The editor and the API should refuse it.

### Leases

An app pushing manual control renews its source every ~10 s. Close the app, drop the WiFi, or kill the process, and the source lapses within 30 s and the room returns to its schedule on its own. No cleanup code, no disconnect handler, no stuck state.

### Transitions

Fades happen at the *compositor*, not inside effects, so any two scenes can cross-fade without either knowing about the other. A source fading in is composited over what is below it at increasing weight — weighted blending of separately rendered buffers.

### Concurrency: dynamic, admission-controlled

**Decided: no fixed slot count.** A device renders as many concurrent sources as its RAM and per-frame instruction budget allow, admitting them until it cannot afford more. The original "two program slots" was sized for A/B switching, before the source stack existed; it is replaced by a pool.

Each concurrent source costs one resident program, one render buffer, and its kernel's share of the frame budget. A device admits sources **highest priority first**, so what gets dropped under pressure is always the least important thing:

- A source that cannot be admitted is **not rendered at all** — never rendered at reduced quality, which would look like a bug rather than a limit.
- Rejection is **reported as an event**, so the app can say "the ceiling strip is ignoring the ambient scene because the alert and the show are using its budget" instead of leaving you to notice a dark strip.
- Admission is re-evaluated whenever the stack changes, so a source drops back in as soon as something above it expires.

The obvious risk of a dynamic limit is that **the same show behaves differently on a C3 and an S3**, which is exactly the kind of inconsistency that is miserable to debug. Two rules contain it:

1. **A guaranteed floor.** Every `render` device must support at least **two concurrent sources plus one cross-fading out**. A device that cannot meet the floor at its configured LED count and frame rate must reduce frame rate until it can — the floor is not negotiable, because below it an alert cannot appear over an ambient scene, and that is the whole point of the stack.
2. **The compiler reports worst-case concurrency per device**, not just per-effect budget. The publish log should state, for each device, how many sources it can carry — so a mixed mesh's weakest member is visible at authoring time rather than discovered during a show.

> **Open question:** does a fade need to be synchronised across devices to the millisecond, or is starting within a frame or two enough? Millisecond sync is nearly free given the show clock, so probably just do it.

## Zones

A zone is a **selector**, evaluated on each device against its own LEDs — never resolved centrally into a pixel list. Both forms are supported and can be combined:

```
zone "desk" {
  include device "strip-a" leds 0..59
  include device "strip-b"
  exclude where z > 1.4
}

zone "floor level" {
  include where z < 0.3
}

zone "reading corner" {
  include where dist(x, y, z, 2.1, 0.4, 1.0) < 1.5
}
```

Explicit sets are predictable and are what you want for "this specific strip". Geometric predicates survive rewiring and automatically pick up new devices, which is what you want for "the bottom of the room". Being able to union and subtract them is what makes both usable together — an explicit set minus a geometric exclusion covers most real cases.

Zones may **overlap**; the source stack resolves per pixel, so overlap is well-defined and does not need to be prevented.

**Geometric predicates require trustworthy coordinates.** Every device has coordinates, but a `synthetic` device's are arbitrary — so a predicate like `z < 0.3` would select it essentially at random, which is worse than not selecting it. A geometric clause therefore only matches LEDs whose `mapq` is `rough` or better; a synthetic device is never selected geometrically. An explicit `include device ...` clause still selects it, because naming a device is an unambiguous statement of intent regardless of where it thinks it is.

The UI must show this, or "why is that strip dark" becomes a recurring mystery. A zone view should list devices excluded *because* they are unmapped, with a one-tap route to placing them roughly.

**Decided: zone membership is evaluated on-device.** Each device tests its own LEDs against the predicate, so zones self-update with no publish step and a device moved to a new position joins and leaves zones by itself.

That needs a small predicate evaluator in firmware — a cut-down [[Bytecode VM]] program, since a predicate is just an expression returning a boolean and the machinery already exists. A zone record replicates as its selector, not as a resolved list, which also means zone definitions stay tiny regardless of how many LEDs they cover.

Two things this makes necessary rather than optional:

- **The apps must be able to show what a zone currently resolves to**, by querying devices, or the system is undebuggable. "Why is that strip dark" must have a visible answer, and with distributed evaluation nobody holds the answer centrally by default.
- **Re-evaluation needs a defined trigger.** Recompute on device root change, mapping change, or zone record change — not every frame. A device that has just been moved should re-evaluate once and settle, not flicker between zones while an AR session is refining its position.

An explicit `include device ... leds ...` clause resolves trivially on-device too, since a device just checks whether it is named. So both zone forms use one mechanism.

## Projections

An effect written for a 1D strip must still work on a mapped 3D device, or half the community's effects are unusable in your best feature. A zone therefore carries a **projection** defining what `u` means for its pixels:

| Projection | `u` becomes |
|---|---|
| `strip` | index along the physical strip (default for unmapped) |
| `axis(v)` | position projected onto a vector, normalised over the zone bounds |
| `radial(p)` | distance from a point |
| `path(...)` | distance along an authored curve through the space |
| `angle(axis)` | angle around an axis — good for rings and ceilings |

So a 1D comet effect becomes a comet sweeping along whatever axis the zone declares, with no change to the effect. `x, y, z` stay world coordinates regardless, so genuinely volumetric effects ignore projections entirely.

**`u` is per-source, not per-pixel.** Zones may overlap and two overlapping zones may declare different projections, so there is no single global answer to "what is `u` for this LED". `u` is evaluated in the context of *the source being rendered* — a source targets one zone, and that zone's projection defines `u` for that source's kernel. A pixel covered by three sources has three different `u` values in the same frame, and that is correct. This also means the projection is baked in at compile time per (effect, zone) pair, so it costs nothing at runtime.

For matrices and panels the zone also exposes a **2D projection**, giving effects a `uv` pair and a declared width and height. That is what makes text, images, scrolling and 2D patterns possible without a separate subsystem — a panel is a zone with a `grid` projection, and a set of strips arranged in a rectangle can declare the same projection and run the same effects.

## Unmapped devices: mapping is pure upgrade

**Everything except genuinely volumetric effects must work on day one with nothing mapped.** Mapping 50 devices is a serious investment, and a system that feels broken until that chore is done will not survive contact with a real installation. Mapping adds capability; it never gates it.

The mechanism that makes this cheap: **every device always has coordinates.** An unmapped device is given *synthetic* coordinates — its declared topology laid out along an arbitrary axis at an arbitrary origin, flagged `synthetic`.

| | Unmapped (synthetic coords) | Mapped |
|---|---|---|
| `u`, `i`, `n` | correct, from the strip itself | correct |
| `x, y, z` | present but arbitrary | true world position |
| 1D and 2D effects | work exactly as intended | work exactly as intended |
| Volumetric effects | run, and look plausible but arbitrary | look right |
| Geometric zones | do not select it — its coordinates are arbitrary, so matching would be random | select it |
| Zones by device/LED range | work | work |

So nothing crashes, no effect needs an unmapped code path, and no feature has to be disabled. Effects that genuinely depend on real geometry declare `requires mapped` ([[Effect Language]]) and are simply shown as unavailable until the device is mapped, with the app saying why.

The upgrade path has a useful middle step: a **rough manual placement** — "this strip runs along the desk, roughly 2 m, starting here" — costs a few seconds per device, replaces the synthetic coordinates with approximately true ones, and makes volumetric effects work well enough for most purposes. AR mapping then refines it later. Between synthetic, rough and AR-mapped there is a smooth gradient of effort against fidelity, and the user can stop wherever they like.

Devices should therefore carry a **mapping quality** flag — `synthetic`, `rough`, `mapped` — with per-LED confidence where AR-mapped ([[App]]). The app uses it to explain what an effect will look like, and to suggest where more mapping effort would pay off.

## Show control

You want both cue-based operation and live parameter control, so both are first-class rather than bolted on.

### Cue stacks

A **cue list** is an ordered set of looks; `go` advances to the next, with a per-cue fade time. Each cue is just a push onto the source stack, so cues compose with schedules and alerts exactly like everything else.

| Concept | Meaning |
|---|---|
| Cue | a scene on a zone, with fade in/out and an optional auto-follow delay |
| Go / Back | advance or retreat; Back is a re-push, not an undo |
| Auto-follow | a cue that triggers the next after *n* seconds — how a timeline and a cue list are the same thing underneath |
| Blackout / hold | a global source at a high priority, obviously reversible |

Cue lists can be driven from the desktop, the phone, a MIDI or OSC controller, a physical button on a control-surface device, or an incoming event. **Cue triggers must be schedulable against the show clock** ([[Protocol#Rendering]]), so a `go` fires simultaneously on every device rather than rippling across the room.

> A pre-programmed timeline and a manual cue list should be *the same data*: a timeline is a cue list where every cue auto-follows. Building them as one thing avoids two editors and two mental models.

### Live parameters

Any `param` in an effect ([[Effect Language]]) can be bound to a live control — an on-screen fader, a MIDI CC, an OSC address, or an encoder on a control-surface device.

Live parameter changes travel as an **automation channel**, not as a program recompile. That is the whole reason parameters are separate from the graph: turning a knob is a few bytes per frame, and it must never trigger a compile. Latency budget from knob to light should be under ~50 ms, which multicast comfortably allows.

- Bindings are stored per control surface, so a physical panel keeps its assignment across scenes.
- A "snapshot" captures current live values back into a scene, so a good improvised look can become a saved one.
- Live values need the same lease behaviour as manual control: if the controller vanishes, parameters return to their scene defaults rather than freezing at whatever they last were.

## Status lighting

All four of your cases are the same mechanism — a binding pushes a source — but they need different plumbing on the input side.

**Discrete state (home automation, dev status).** An event or a state change fires a binding that pushes a scene at 224+ with a timeout. Needs Home Assistant and MQTT (already specified) plus an **HTTP endpoint and a small CLI**, so `lumen alert build-failed` from a CI script is a one-liner. That CLI is the difference between this being used for dev status and not.

**Continuous values (temperature, air quality, power).** These are not events; they are channels. An external value becomes a `channel` an effect reads, mapped to colour through a palette. Best expressed as an ambient effect *at the floor priority* rather than an alert — the room is subtly tinted by air quality rather than interrupted by it. That is a different and better use of a light than a notification, and the system should make it as easy.

**System self-health.** The firmware raises these itself:

| Condition | Suggested indication |
|---|---|
| Device left the mesh | remaining devices pulse briefly at its last known position |
| Power limit engaged | a slow amber breathe, so you know why it looks dim |
| Program failed to verify | the factory fallback pattern, which is deliberately unlike any real effect |
| WiFi weak | only on request, via an "identify/diagnose" mode |
| Time sync not converged | suppress rendering of tightly-synced shows rather than showing them wrong |

Self-health indication needs a **volume control**. Lights that constantly report on themselves are irritating; the default should be "show me only what I can act on", with a diagnostic mode in the app that turns everything on.

Self-health lives in its own band, **192–223**, above manual control but below user alerts at 224+. The earlier suggestion of "cap it below 192" was wrong: that would put the system's own complaints in the same band as the user's manual control, so a device grumbling about weak WiFi could override someone deliberately setting the lights. Self-health must outrank ordinary operation — you need to see it — but must never mask a real alert.

## Provisioning

**QR to start, BLE where available, SoftAP as fallback.**

1. User scans the QR on the device. It carries the device UUID, a provisioning token, and which methods the device supports.
2. Phone connects over **BLE** if the chip has it — no network switching, and it works even where the user's WiFi is a captive-portal mess.
3. Otherwise the phone joins the device's **SoftAP** and does the same exchange.
4. Credentials handed over, X25519 pairing completed, mesh key delivered ([[Protocol#Security]]).
5. Device joins WiFi, appears in discovery, and **identifies itself by blinking** so the user can confirm which physical object they just configured.

Step 5 matters at your ~50 device target. Configuring twelve identical strips and then working out which is which is miserable; making identification part of provisioning means the device gets named while the user is still standing next to it.

The QR should also work for a device already provisioned — scanning it jumps straight to that device's settings, and it is the same code the AR mapping in [[App]] uses for identification. One sticker, three jobs.

> **Open question:** batch provisioning. Twelve new strips is twelve rounds of this flow. Worth considering a mode where one configured device shares credentials with a new one over BLE or ESP-NOW, so only the first device needs a phone.
