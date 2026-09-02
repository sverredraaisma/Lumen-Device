The authoring model. An effect *is* [[Effect Language]] source text; the **node graph and timeline are views over it**. This note describes the conceptual model — nodes, layers, masks, compilation — and the language note describes the form it is actually stored in. Effects compile to [[Bytecode VM]] programs that every device runs independently against the shared show clock.

## Original notes (kept)

- Volumetric effects, making use of the location mapping for LED coords
- Effect layering
	- more efficient with dedicated mask layers and effects
	- mask effects are simple and fast to run and only output 1 or 0 for a pixel
	- if a mask is 0, none of that folder/effect's layers or calculations run, reduces load on controllers

Both survive intact. Masks become the `MASKTEST` instruction with a compile-time skip distance; volumetric effects fall out of every LED knowing its own world coordinates.

## The green/amber rule

This is the central idea, and it comes straight from the decision to render on-device.

- A node whose output depends only on **`(x, y, z, i, t, parameters)`** is **green**: it compiles into the per-pixel kernel and every device computes it identically, for free, with zero network traffic.
- A node that needs anything else — audio, a shared simulation, a sensor, an external value — is **amber**: it becomes a **channel** ([[Protocol#Rendering]]), a small blob broadcast each frame that the kernel reads as a uniform.

The editor should colour nodes this way and show the resulting per-frame bandwidth *before* you press play. It turns an invisible architectural constraint into something you can see and reason about while patching.

## Node catalogue

**Sources (green)** — time, position, index, normalised position, constant, random-per-pixel, noise 1D/2D/3D/4D, gradient, distance field, plane, sphere, radial, ramp, pulse train.

**Sources (amber)** — audio bands, audio beat/onset, IMU, temperature, external value (HA/MQTT/HTTP), simulation output, timeline value.

**Shaping** — math ops, curve/easing, remap, quantise, threshold, smoothstep, sample-and-hold, slew limiter.

**Space** — translate/rotate/scale coordinates, mirror, tile, polar, project onto axis or plane, nearest-point-on-path.

**Colour** — palette lookup, HSV, colour temperature, gamma, blend modes (normal, add, multiply, screen, overlay, max), fade.

**Mask** — any node producing 0/1 or coverage; attaches to a layer or group and gates it.

**Composite** — layer stack, group, crossfade, priority select.

**Stateful (green with a local buffer)** — decay/trail, feedback, blur along strip, motion blur. These use the `prev` register, the per-pixel history buffer.

**Stateful (amber, needs the sim channel)** — particles, comets crossing device boundaries, boids, fluid, physics. The sim master runs these once and broadcasts state; every device renders that state against its own LED coordinates.

## Layers as sugar

A layer stack is a graph shape, not a separate system: an ordered chain of composite nodes, each with an optional mask. The editor should let you work in a layer list for ambient and status work and drop into the graph only when you need to, so simple things stay simple. Both views edit the same record.

## The timeline

The timeline **automates parameters**; it does not render. That keeps it cheap: a timeline compiles to a keyframe table evaluated once per frame in the `frame` section, or — if it drives a device-independent value — broadcast as an automation channel.

Tracks can hold:
- Parameter keyframes with easing
- Scene changes — a **source push at a scheduled show time**, not a raw `ACTIVATE`. `ACTIVATE` is the lower-level program-slot switch and is an implementation detail of a push that happens to need a program the device is not currently holding
- Marker/cue points, triggerable by event or manually
- An audio reference track, for aligning a choreographed show to a known piece of music

Because activation is scheduled against `show_time_us` rather than sent as "go now", a choreographed show stays sample-accurate across the mesh even if the network hiccups a second before a cue.

## Compilation pipeline

1. **Validate** the graph — types, cycles (only permitted through `prev` or a channel), unbound inputs.
2. **Partition** into green and amber, allocating a channel per amber source.
3. **Hoist** everything invariant across pixels into the `frame` section, and everything invariant across frames into `once`.
4. **Fold** constants, resolve palettes and curves into tables.
5. **Order masks first** and compute skip distances for `MASKTEST`.
6. **Emit** per device class, since a device with 3 output channels and no white LED gets different emit code.
7. **Budget check** per device against measured capacity score ([[Bytecode VM#Budgets]]).
8. **Sign and upload** into a free pool slot, then `ACTIVATE` all devices at a common future show time.

Step 6 means the artefact is per device *class*, not per device — devices with the same chip, LED type and channel count share a program and share the upload.

## Effect library and expandability

Ship the effect library as ordinary `effect` records — source text like any other — not as firmware built-ins. A user effect and a shipped effect must be the same kind of thing, or "easily expandable" is not true. That implies:

- Effects are **exportable and importable** as a single file, with their palettes and curves embedded.
- Graphs can be **encapsulated as reusable nodes** with declared inputs, so a complex effect becomes a building block.
- Effects declare **required capabilities** (audio, mapped coordinates, RGBW) so the app can say "this needs a mapped device" rather than rendering something wrong.

**Decided:** the canonical form of an effect is text, not graph data — see [[Effect Language]]. The node editor is a view over it. An expression tree *is* a DAG, so the two are renderings of the same structure rather than one being a lossy export of the other.

## Audio-reactive specifics

All four audio sources you selected feed the same channel format, so effects never know or care where the audio came from.

| Source | Latency | Notes |
|---|---|---|
| I2S mic on a device | lowest | fully standalone, room acoustics apply |
| Line-in / dedicated analyser node | lowest, cleanest | best for a permanent install |
| Desktop loopback | low | exact signal, and can look ahead for beat prediction |
| Phone mic | highest | convenient, most variable |

**Analysis happens at the source**, once, and only the result is broadcast — never raw audio. Standard channel layout:

```
32 log-spaced band magnitudes (u8, AGC-normalised)
overall level, smoothed level
onset flag, beat phase 0..1, estimated BPM, confidence
```

Publishing beat *phase* rather than beat *events* is what makes this work over a lossy network: a receiver that misses a packet can still extrapolate where in the bar it is, so an effect stays on beat instead of stuttering.

Multiple audio sources hand over automatically by claim-and-lease ([[Protocol#Channel ownership]]) — the desktop preempts the room mic when you plug it in, and its lease lapsing hands control back. Note this is a *different* mechanism from the source-stack priority rule, which governs sources on a zone rather than producers of a channel, even though both are priority-and-lease shaped.

## Open questions

- Should the sim master's state be **snapshot** (full state each frame, simple, stateless receivers) or **delta**? Snapshot at 512 bytes and 60 Hz is 30 KB/s, which is fine. Start snapshot.
- Should an effect be able to request a *specific* audio producer rather than "whatever owns the channel"? Probably not — it would break the property that effects never know where audio came from — but a permanent install with two analyser nodes in different rooms is a real case that this does not currently serve. Two channel ids might be the better answer than a producer selector.
