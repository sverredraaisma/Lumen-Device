What the system stores, and how it stays consistent across a mesh with no central server. Follows from the decision that the mesh is **fully autonomous** — apps are editors and remotes, never a dependency.

## Records

Everything is a record with a UUID, a type, a hybrid logical clock timestamp, an author key, and a signature.

| Type | Holds | Lives on |
|---|---|---|
| `device` | name, chip, LED count, output config, power limit, root position and orientation, per-LED coordinates | its own device (authoritative), replicated read-only |
| `zone` | a selector — explicit device/LED ranges, geometric predicates, or both — plus a projection. See [[Runtime Model#Zones]] | replicated |
| `effect` | [[Effect Language]] source text — the canonical form. The node graph is a view over it | replicated |
| `scene` | effect + parameter values + zone binding + priority | replicated |
| `show` | timeline of scene and parameter changes over time | replicated |
| `schedule` | wall-clock or sun-relative rules that activate scenes | replicated |
| `binding` | trigger (event or channel condition) to action | replicated |
| `channel` | declared shared-state channel: id, layout, rate, hold_ms, default | replicated |
| `key` | authorised controller public keys and revocations | replicated |
| `program` | compiled bytecode for a specific effect and device class | pushed, not replicated |

A device's own per-LED coordinates stay authoritative **on that device**. It is the one piece of state the device knows better than anyone, it survives an app reinstall, and it is what lets a device join a mesh already knowing its own shape.

## Signing

**Every record is signed.** Ed25519, by the key that authored it; a device rejects any record that is unsigned or whose signature does not verify against a currently-authorised key. Without this, the shared symmetric mesh key means any paired device — including a cheap bridged node someone tampered with — could forge a `scene`, `schedule` or `binding` and have it replicate as genuine. Programs were already signed; the records deciding *which* program runs, when, and at what priority must be too, or signing the programs achieves little.

Two signing authorities, and the distinction matters:

| Record | Signed by | Because |
|---|---|---|
| `zone`, `effect`, `scene`, `show`, `schedule`, `binding`, `channel`, `key` | a **controller** key | these are authored by a person, and only an authorised controller may write them |
| `device` (own name, coordinates, output config, capabilities) | the **device's own** identity key | a device is authoritative about itself, and it has no controller key — it could not sign otherwise |

A device's self-signed record is accepted **only for its own UUID**, so a compromised device can lie about itself but cannot rewrite anything else. That is a much smaller blast radius than the alternative, and it is the natural boundary.

Cost is about 64 bytes per record and one verify per record on gossip receive. Verify only when a record actually *changes* — the digest exchange compares HLCs, so unchanged records are never re-verified, and steady-state gossip costs nothing.

## Replication

Keepers are **elected, and capped at 5–7** ([[Firmware#Roles]]), ranked by flash size then capacity score. Everyone else pulls the records they need read-only and caches them. A bridged RP2040 is never a keeper.

- **Gossip.** Every 5 s a keeper sends a `STATE_DIGEST` to a random peer: a compact list of record id to HLC. Differences trigger `STATE_PULL` and `STATE_PUSH`.
- **Conflict resolution.** Last writer wins per record, ordered by HLC then author key as a tiebreak. Deliberately simple. Records are small and edits are rare and human-driven, so the pathological cases that motivate real CRDTs do not arise here.
- **No partial records.** A record is replaced whole, never merged field by field. Two people editing the same effect at once loses one edit — acceptable, and the editor should warn if the record changed under you.
- **Quorum.** With three or more keepers, a partition that heals converges on HLC order. With fewer, warn the user: a mesh with one keeper loses the show if that device dies.

> **Open question:** should there be an explicit export/backup to the app, or a designated "primary keeper" that a user is told to keep powered? I would do both — automatic replication, plus a one-tap backup file, because "my lights forgot everything" is the worst failure this system can have.

## Bindings — how status and reactive lighting are configured

A binding is `when <trigger> [and <condition>] then <action>`, stored as a record. This is the layer that makes status lighting a configuration rather than a coding task.

### Evaluation: every keeper, idempotent actions

**Decided: all keepers evaluate every binding, and duplicate actions collapse.** No elected evaluator, so no single point of failure and no gap during re-election — a partition that leaves one keeper reachable still fires your triggers.

The mechanism is a deterministic **action id**:

```
action_id = hash(binding_id, trigger_event_id, quantised_show_time)
```

Every keeper computes the same id for the same trigger, so the five to seven copies of an action are recognisably one action at the point of effect. Consequences that have to be built in rather than assumed:

- **Events need stable unique ids.** `EVENT` currently carries `{source_uuid, kind, value, show_time_us}` — not sufficient, since two keepers may receive the same event with slightly different timestamps and compute different ids. The **producer** must mint the id ([[Protocol#Events and state]]); receivers must never derive it.
- **Source pushes are naturally idempotent.** Pushing a source with an existing action id replaces rather than adds. This is the easy case, and it is most actions.
- **Outbound effects need dedup at the boundary.** Webhooks, MQTT publishes and HA calls go out through `caps=gateway`, so the gateway keeps a short-lived set of recently-seen action ids and drops repeats. One gateway, one send. Where two gateways exist, they must agree — either elect one per integration, or accept that an external system can occasionally see a duplicate and document it.
- **Chained events must not amplify.** A binding whose action emits an event that triggers another binding will otherwise fan out multiplicatively across keepers. An emitted event inherits a derived-but-deterministic id from its causing action id, so the chain stays one logical thread no matter how many keepers walk it. Depth-limit the chain too, or a user can write an infinite loop by accident.

Quantise show time coarsely — a tenth of a second is plenty — so keepers with slightly different arrival times still agree.

| Trigger | Examples |
|---|---|
| Event | IMU tap, button press, motion detected, device joined or left |
| Channel condition | audio band above threshold, temperature over 30, HA entity changed |
| Schedule | wall clock, sunrise/sunset offset, day of week |
| Manual | app, HA scene, MQTT message, HTTP call |

| Action | Effect |
|---|---|
| Activate scene | on a zone, at a priority, with a fade |
| Set parameter | drive an effect parameter directly |
| Push override | a temporary high-priority scene with a timeout |
| Emit event | chain bindings, or notify an external system |

Because overrides carry a priority and a timeout ([[Protocol#Arbitration]]), a status alert is inherently self-clearing: if the thing that raised it dies, the lights return to ambient instead of staying stuck red. Every action above is really "push a source onto a zone's stack" — see [[Runtime Model#The source stack]], which is the single mechanism all of this reduces to.

## Storage layout on device

```
/nvs      uuid, keys, mesh key, wifi credentials, capacity calibration
/fs       replicated records (LittleFS, JSON or CBOR)
/prog     program pool, sized by available RAM, plus a factory-default fallback
/coords   per-LED coordinates, compact binary
```

The factory-default program sits outside the pool and is never evicted or overwritten. If every pool slot fails verification, a device falls back to it and shows a defined "unconfigured" pattern. A device should never be dark because of a software problem — being dark and being broken should look different.

## Open questions

- Per-LED coordinates are **relative to the device root** (decided — a device can then be moved without remapping). World coordinates and zone membership are recomputed together, on the same trigger, **once the device has stopped moving**: `Settling` in `zones.rs` holds the policy.

  A root change and a mapping change both wait half a second of quiet before anything is recomputed, because both arrive from an AR session that emits them continuously while somebody points a phone at the device. A zone record change applies at once, because it is somebody's deliberate act and they are watching the lights to see whether it worked.

  Debounced rather than rate limited, which is the part worth keeping. A rate limit would recompute periodically *during* a move, and every intermediate answer is wrong — showing them in sequence is the flicker the rule exists to prevent. Waiting for quiet shows the old membership throughout the move and the new one once.
- Does a show need versioning and undo history, or is that only in [[Desktop Application]] and never replicated?
