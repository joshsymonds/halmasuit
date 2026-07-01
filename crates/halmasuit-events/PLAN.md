# halmasuit-events — PLAN

A pub/sub event bus that lets halmasuit's components and any
nested compositor share **system state events** — lifecycle
phases, compositor signals (window open/close, perspective switch,
fullscreen), system-health signals (CPU, idle), and user-defined
signals. The wallpaper engine is the first major consumer; the
reactive-wallpaper-on-boot feature depends on this crate
existing.

This document is a plan for a crate that does not yet exist.
When implementation lands, it should be supplemented or replaced
by a normal README.md describing the shipped crate.

The **v1 epic** below is the authoritative, approved contract for
the first landing (the in-process reactive-wallpaper slice). The
remainder of this document — sockets, the CLI, `UserSignal`,
system-health publishers, the compositor/DMS consumers — is the
broader vision and the roadmap for later acts; it is NOT in v1
scope and its types are not added until their publishers exist.

---

## v1 epic — in-process reactive-wallpaper slice (approved 2026-05-28)

The smallest vertical slice that ships reactive wallpaper
end-to-end: a calloop-native in-process bus carrying the events
halmasuit already emits, wired to the wallpaper's existing
`EventTime` / `EventValue` uniform bindings. No socket, no CLI, no
new privileged surface.

### Requirements (IMMUTABLE)

1. New library crate `crates/halmasuit-events/` (dual
   MIT-OR-Apache) exposing `BusEvent` and the publish/subscribe
   primitives. It depends on `halmasuit-introspect`, `serde`,
   `serde_json` ONLY — NO calloop, smithay, wayland, or tokio.
2. `BusEvent` embeds `halmasuit_introspect::Event` as a single
   `Lifecycle(Event)` variant and adds only genuinely-new
   bus-native signals: `BootStage(u8)`, `Idle { duration_seconds:
   u64 }`, `ActivityResumed`, `LuksPrompt`. `introspect::Event` is
   NOT restated as a `SystemPhase`, and `Phase::as_u32`
   discriminants are NOT renumbered.
3. `introspect::Event` gains `Deserialize` + `Clone` (it already
   derives `Serialize`). `halmasuit-introspect` gains a
   runtime-agnostic in-process subscriber registry of
   `Box<dyn Fn(&Event) + Send + Sync>` callbacks. `emit()` logs to
   tracing/journald FIRST (byte-for-byte unchanged), THEN fans out
   to registered callbacks. With no callback registered, `emit()`
   behaves exactly as today.
4. The calloop adapter lives in `halmasuit`, NOT in the bus crate:
   one registered introspect callback wraps each `Event` as
   `BusEvent::Lifecycle(_)` and pushes it onto a
   `calloop::channel::Sender`. New bus-native signals publish onto
   the same channel from their emission sites.
5. A consumer subscribes by registering its own
   `calloop::channel::Channel<BusEvent>` as a loop source; its
   handler runs with `&mut HalmasuitState`. No synchronous registry
   invoked inside `publish()` (the `&mut state` reentrancy trap).
6. The wallpaper `ShaderBackend`'s existing `EventTime { event }`
   and `EventValue { event }` bindings are wired: on a matching
   `BusEvent`, `EventTime` writes the transition's `current_time`
   into the named uniform; `EventValue` writes the event's payload.
   The "wallpaper bus not yet connected" warning in `shader.rs` is
   DELETED.
7. Every `BusEvent` has a canonical, stable, dotted-lowercase name
   (e.g. `halmasuit.session.opened`, `halmasuit.foreground.session`,
   `halmasuit.phase.scanout_active`, `halmasuit.boot.stage`),
   matching the convention already used in `wallpaper/config.rs`
   tests. That name is the match key against
   `EventTime`/`EventValue`'s `event` field.
8. `BootStage(N)` is emitted at a small, configurable set (2–3) of
   startup milestones. `LuksPrompt` is defined but NOT emitted in
   v1 (it is a Phase-B initramfs state; Phase B is not started).
   Greeter / Session / Shutdown reactions require NO new emission
   points — they flow via the wrapped `ForegroundChanged`,
   `SessionOpened`, `SessionClientFirstFrame`, `SessionEnded`, and
   `Shutdown` lifecycle events.
9. A new `introspect::Event::WallpaperUniformApplied { event_name,
   uniform }` is emitted when the wallpaper writes a bus-driven
   value into a uniform. It is the journald observability signal
   the headless VM gate keys off (pixels are unobservable under
   `virtio-gpu-pci`).

### Success criteria (MUST ALL BE TRUE)

- [ ] `just check` green (rustfmt, clippy `-D warnings`,
      cargo-deny, cargo-machete, typos, nextest).
- [ ] Unit tests cover: bus fan-out to multiple subscribers; the
      introspect callback registry (fan-out, and no-op when empty);
      `BusEvent` serde round-trip; `BusEvent` → canonical-name
      mapping; `EventTime`/`EventValue` binding lookup writes the
      correct uniform value.
- [ ] A targeted VM test drives a real lifecycle transition (to
      session-active) and asserts the matching
      `wallpaper_uniform_applied` marker via `introspect_events`.
- [ ] `login-flash` and the full `just test-vm` sweep stay green
      (no regression to the flash gate).
- [ ] `emit()`'s journald output is unchanged for every existing
      event (the `introspect_events` parsing contract holds).

### Anti-patterns (FORBIDDEN)

- NO tokio / `tokio::broadcast` / any async runtime (reason: the
  daemon is pure calloop; a runtime is the wrong fit and dead
  weight).
- NO `SystemPhase` parallel to `Phase`, and NO renumbering
  `Phase::as_u32` (reason: one vocabulary; the discriminants are a
  frozen wire contract — append-only).
- NO Unix socket, SO_PEERCRED auth, `halmasuit-event` CLI,
  `UserSignal`, or system-health publishers in this slice (reason:
  out of scope; they land WITH their publishers, never
  speculatively).
- NO future variants (`UserSignal`, `CpuLoad`, `MemoryPressure`,
  `NetworkChange`, compositor events) added before their publishers
  exist (reason: dead enum arms rot).
- NO calloop / smithay / wayland / tokio dependency in
  `halmasuit-events` (reason: it is a pure types + fan-out layer;
  runtime coupling defeats reuse by kushuh / external consumers).
- NO making `emit()` fan out BEFORE it logs to tracing (reason: the
  journald observability contract — `login-flash`,
  `assert_no_flash_stream` — must never sit behind a fallible
  in-process step).
- NO blocking the render/calloop thread on bus delivery; NO
  bounded-channel drop logic in v1 (reason: in-process, low-volume,
  single-thread; backpressure is a socket-era concern).
- NO mocking the bus in the VM test (reason: real fan-out, real
  wallpaper, real journald marker — same posture as the rest of the
  suite).

### Approach

The bus is fed from the single `emit()` fan-out point rather than
by dual-publishing at every call site, so "the wiring lives in one
place" is literally true: `halmasuit-introspect` gains a
runtime-agnostic callback registry, `emit()` logs to journald then
fans out, and `halmasuit` registers one callback that marshals each
`Event` onto a `calloop::channel` as `BusEvent::Lifecycle`. Every
lifecycle event already emitted thus reaches the bus for free, and
the load-bearing journald contract is untouched because logging
still happens first.

The taxonomy is hybrid: `BusEvent` wraps `introspect::Event` for
everything already emitted and adds only the genuinely-new
bus-native signals. `introspect::Event` stays the frozen journald
state-transition schema; the new signals (`BootStage`, `Idle`,
`LuksPrompt`) live at the bus layer, not inside it.

The wallpaper subscribes by registering a `calloop::channel`
source whose handler runs with `&mut HalmasuitState` — the same
pattern `PrepareForShutdown` already uses, which sidesteps the
`&mut state` reentrancy a synchronous registry would hit. On each
`BusEvent` it resolves the event's canonical dotted name, finds
matching `EventTime`/`EventValue` bindings, and writes the
transition time or payload into the uniform, emitting a
`WallpaperUniformApplied` marker for the VM gate.

### Approaches considered

#### Separate `SystemEvent` / `SystemPhase` taxonomy (PLAN.md as written) — REJECTED
REJECTED BECAUSE: duplicates `Phase` + `Foreground` + the session
lifecycle events already emitted; forces the wallpaper to reconcile
two vocabularies against one set of real transitions.
DO NOT REVISIT UNLESS: the bus is split out for a non-halmasuit
consumer that genuinely cannot depend on `halmasuit-introspect`.

#### Reuse + extend `introspect::Event` directly (append new variants to it) — REJECTED
REJECTED BECAUSE: the new bus-native signals (`Idle`, future
`UserSignal`/health) are not compositor state-transitions and don't
belong in the frozen journald schema; `Lifecycle(Event)` wraps
without polluting it.
DO NOT REVISIT UNLESS: a specific new signal is genuinely a
compositor state-transition worth journald emission on its own.

#### tokio broadcast channel (PLAN.md open question) — REJECTED
REJECTED BECAUSE: there is no tokio in the daemon; it is pure
calloop. DO NOT REVISIT.

#### Explicit dual-publish (`emit()` + `bus.publish()` at each site) — REJECTED
REJECTED BECAUSE: call-site churn and easy to miss a site; the
`emit()` fan-out centralizes the wiring in one place.
DO NOT REVISIT UNLESS: the callback-registry approach proves
unworkable in practice.

#### Synchronous subscriber registry invoked inside `publish()` — REJECTED
REJECTED BECAUSE: the consumer needs `&mut HalmasuitState`, which
`publish()` already borrows — a reentrancy/borrow conflict.
`calloop::channel` defers delivery to the next loop turn and
sidesteps it idiomatically. DO NOT REVISIT.

### First task

**Scaffold `halmasuit-events` + `BusEvent` + introspect
`Deserialize`/`Clone` (pure types, no I/O).** PLAN steps 1–2.

- Study `crates/halmasuit-introspect/src/lib.rs` (the `Event`
  enum, derives, serde tagging) and `crates/halmasuit/Cargo.toml`
  (workspace dep conventions, `[workspace.lints]`).
- `cargo new --lib crates/halmasuit-events`; wire it into the
  workspace `Cargo.toml` members; `[lints] workspace = true`;
  license/edition/rust-version from `.workspace`.
- Add `Deserialize` + `Clone` to `introspect::Event` and the types
  it carries (`Phase`, `Foreground`, `LayerRole`, `ShutdownReason`,
  `SessionExit`); keep existing `Serialize` derives. Update the
  existing round-trip tests to also exercise deserialize.
- Define `BusEvent` with the `Lifecycle(Event)`, `BootStage(u8)`,
  `Idle { duration_seconds: u64 }`, `ActivityResumed`, `LuksPrompt`
  variants; derive `Debug, Clone, Serialize, Deserialize`.
- Implement `BusEvent::canonical_name(&self) -> Option<String>`
  (or `&'static str` + formatted) with the dotted scheme; unit-test
  the mapping for every variant, asserting `halmasuit.session.opened`
  matches the string already used in `wallpaper/config.rs` tests.
- No transport, no fan-out, no calloop yet — pure types + names.

Success criteria for the first task:
- [ ] `cargo build -p halmasuit-events` and the introspect crate
      both build.
- [ ] `BusEvent` serde round-trips for every variant (unit test).
- [ ] `canonical_name` covers every variant and matches the
      established dotted convention (unit test).
- [ ] `just check` green.

---

## Why this exists

halmasuit already tracks rich internal state — lifecycle phases
(boot, greeter, session-active, shutdown, post-pivot), system
health, user activity. Some of that is needed by other components:
the wallpaper engine wants to morph its uniforms when the system
transitions phases; a nested compositor wants to publish compositor
events that the wallpaper engine can also react to; the greeter or
session might want to know when long-idle starts.

Today this coordination is ad-hoc — calloop callbacks, direct
function calls between halmasuit modules, scattered match arms on
lifecycle enums. Adding a new consumer (the reactive wallpaper)
means adding another set of wiring everywhere events originate.

**`halmasuit-events` makes the bus a first-class component.**
Publishers push events; consumers subscribe to a filtered stream.
The wiring lives in one place. Adding a new consumer (DMS in the
future, a script publishing user signals, kushuh when it exists)
is a subscribe call, not a refactor.

---

## Scope

In:
- A typed event taxonomy (Rust enum) covering halmasuit's
  lifecycle, system health, user signals, and a generic compositor-
  event subset.
- A transport layer that supports in-process subscribers (most
  consumers) and out-of-process subscribers (external scripts,
  the nested compositor as a separate process).
- A pub/sub API with reasonable backpressure (slow consumers don't
  stall publishers).
- A wire format for out-of-process messages.

Out:
- General-purpose IPC for arbitrary services (use DBus or direct
  sockets for that).
- Replicating systemd journal — events are ephemeral by default;
  consumers see live events from the moment they subscribe.
- Authentication beyond Unix-socket peer creds (halmasuit-session
  is the privileged surface; this bus is unprivileged
  coordination).

---

## Position

Lives at `crates/halmasuit-events/` in the halmasuit workspace.
Library crate (no binary). Produces `libhalmasuit_events.rlib`,
consumed by:

- `halmasuit` (the daemon): publishes lifecycle phase events;
  hosts the bus's socket; subscribes for diagnostics.
- `halmasuit-session` (the broker): publishes broker-side events
  (greeter spawn, session opened, session closed).
- `halmasuit-wallpaper` (extracted, eventually): subscribes to
  drive its reactive state machine. Until extraction, the wallpaper
  module inside `halmasuit` subscribes from there.
- `kushuh` (future): publishes compositor events; may subscribe to
  system events for its own UI chrome.
- External scripts / `halmasuit-event` CLI: publish user signals.

The bus does NOT depend on smithay, wayland, or any compositor-
related types. It's a pure pub/sub layer. Compositor-specific
event variants are payloads, not coupling.

License: dual MIT-OR-Apache, matching the rest of the workspace.

---

## Event taxonomy

A single Rust enum with serde derive for wire-format support:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SystemEvent {
    /// halmasuit's overall lifecycle phase changed.
    Phase(SystemPhase),

    /// CPU load fraction in [0, 1], averaged over the last
    /// monitor-tick interval. Published periodically by halmasuit's
    /// system-monitor when load changes by more than a threshold.
    CpuLoad(f32),

    /// Memory pressure fraction in [0, 1]. Same pattern as CpuLoad.
    MemoryPressure(f32),

    /// Network online/offline transition. Source: NetworkManager or
    /// systemd-networkd, polled via DBus by halmasuit.
    NetworkChange { online: bool },

    /// User has been idle for the given duration. Source: the idle
    /// timer in halmasuit-session or the nested compositor's idle
    /// notification. Published once per threshold crossing
    /// (5min, 10min, 30min, etc.).
    Idle { duration_seconds: u64 },

    /// User activity resumed after idle.
    ActivityResumed,

    /// Compositor — published by the nested compositor (kushuh or
    /// a niri with a published IPC plugin), all optional / best-
    /// effort. Not all sources will emit all events.
    WindowOpen { app_id: String, role: Option<String> },
    WindowClose { app_id: String },
    Fullscreen { enabled: bool, app_id: String },
    Focus { app_id: String },
    PerspectiveSwitch { from: String, to: String },

    /// Free-form user-published event. Scripts, weather daemons,
    /// pomodoro timers, etc. publish these.
    ///   name:    a stable identifier ("focus-mode", "lunch-break",
    ///            "raining")
    ///   payload: arbitrary JSON the consumer can interpret
    UserSignal { name: String, payload: serde_json::Value },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemPhase {
    /// Kernel up, halmasuit daemon starting. The wallpaper is
    /// usually rendered in this phase with a "dormant" palette
    /// (black hexrain, etc.).
    Boot,
    /// Multi-stage boot — halmasuit emits Boot then Phase1 then
    /// Phase2 etc. for the wallpaper to step through palettes.
    /// Stage count is configurable per theme.
    BootStage(u8),
    /// LUKS / disk decryption prompt visible.
    LuksPrompt,
    /// Greeter / sign-in screen visible.
    Greeter,
    /// User has signed in; session compositor is running.
    SessionActive,
    /// Long user idle entered.
    Idle,
    /// `PrepareForShutdown` received from logind, graceful tear-
    /// down in progress.
    Shutdown,
    /// Past the rootfs→shutdownRamfs pivot. Liveness lines write
    /// to /dev/kmsg from here until kernel halt.
    PostPivot,
}
```

Event types are versioned via additive enum variants. Removing or
renaming variants is a breaking change; adding new variants is not
(consumers match exhaustively or use a default arm).

---

## Transport mechanism

Two transports, single API:

### In-process (default for halmasuit-internal consumers)

A `tokio::broadcast::Sender<SystemEvent>` (or a calloop-friendly
channel — TBD; depends on which runtime halmasuit's daemon settles
on). Subscribers get a `Receiver`. Backpressure: each receiver is
bounded; slow consumers see dropped events (logged with a counter)
rather than blocking the publisher. Latency: sub-millisecond, just
a channel send + memcpy.

### Out-of-process (for nested compositor, external scripts)

A Unix socket at `$XDG_RUNTIME_DIR/halmasuit/events.sock`. halmasuit
hosts the socket. Wire format: newline-delimited JSON
(`{"event": "...", ...}`), one event per line. The Unix socket
peer credentials (SO_PEERCRED) gate which clients can publish what
(see [Authorization](#authorization)).

The library exposes a unified `EventBus` handle:

```rust
let bus = EventBus::connect()?;            // auto-detects
                                            // in-process or socket
let mut rx = bus.subscribe(filter)?;
while let Some(event) = rx.next().await {
    handle(event);
}

// or:
bus.publish(SystemEvent::UserSignal {
    name: "focus-mode".into(),
    payload: json!({}),
})?;
```

Internal consumers (linked into halmasuit's daemon) skip the socket
and use the in-process channel directly. External consumers
(`halmasuit-event` CLI, kushuh, scripts) talk to the socket.

---

## Pub/sub semantics

- **Live-only by default.** Consumers see events that arrive
  *after* they subscribe. No replay of history. This keeps the
  bus stateless and the implementation simple.
- **Filtered subscriptions.** A subscribe call takes a filter
  predicate (which event variants to deliver). Saves the consumer
  from match-arm spam on irrelevant events. Filter is applied at
  delivery time, so a busy bus doesn't wake every consumer for
  every event.
- **At-most-once delivery.** No retry. If a consumer's channel
  buffer fills (slow consumer), events are dropped. Dropped counts
  are logged + exposed as a metric so we can detect the failure.
- **Total order is per-publisher, not global.** Events from
  publisher A arrive in the order A published them; events from
  publishers A and B may interleave differently in different
  consumers. For halmasuit's use case (lifecycle events from one
  source, compositor events from another) this is sufficient.
- **No request/response.** This is pub/sub, not RPC. If a consumer
  needs to ask halmasuit a question, it does that via DBus or a
  separate socket, not via the event bus.

---

## Publishers

Who publishes what:

| Publisher | Events |
|---|---|
| `halmasuit` daemon | `Phase(*)`, `Idle`, `ActivityResumed`, `CpuLoad`, `MemoryPressure`, `NetworkChange` |
| `halmasuit-session` broker | the broker-side lifecycle events (currently routed through `halmasuit-session-ipc`; may also flow through the bus as `Phase` events) |
| Nested compositor (`kushuh`, or niri with a plugin/script) | `WindowOpen`, `WindowClose`, `Fullscreen`, `Focus`, `PerspectiveSwitch` |
| External scripts | `UserSignal { name, payload }` only |
| `halmasuit-event` CLI | facade for external scripts; serializes JSON and writes to the socket |

Authorization (next section) gates which connections can publish
which variants.

---

## Authorization

Unix socket peer credentials (SO_PEERCRED) classify each connecting
client by UID:

| UID | Can publish |
|---|---|
| `compositor` system user (halmasuit's compositor UID) | Compositor events (`WindowOpen`, `WindowClose`, `Fullscreen`, `Focus`, `PerspectiveSwitch`) |
| `halmasuit-session` broker UID | Lifecycle events (`Phase`, `Idle`) — typically redundant with halmasuit-daemon's own publishing, kept available for the broker's own consumption |
| The logged-in user (uid ≥ 1000) | `UserSignal` only |
| Any other UID | Subscribe-only, no publish |

The classification is conservative: a malicious client with the
user's UID can spam `UserSignal` events, but cannot forge
lifecycle phase or compositor events. The damage from spammed
UserSignals is bounded by what the wallpaper / consumers do in
response (typically: animations the user could trigger anyway by
publishing the signal legitimately).

halmasuit-session's privileged surface is unaffected — the bus is
not a path to privilege escalation. The broker remains the only
gate on UID-floor-bound operations.

---

## Wire format

Newline-delimited JSON, one event per line:

```json
{"type":"Phase","phase":"Greeter"}
{"type":"PerspectiveSwitch","from":"code","to":"meeting"}
{"type":"UserSignal","name":"focus-mode","payload":{}}
```

JSON because:
- Trivial for external scripts to publish (`echo '{"type":...}' | nc -U`)
- Trivial to debug (`tail -f` the socket, more or less)
- serde derives handle it without extra dependencies
- The volume is low (lifecycle events, occasional compositor
  events) so the parse cost is negligible

The internally-used in-process channel skips JSON entirely; it
just sends the Rust enum directly.

Versioning: the JSON form has a `"type"` discriminant. Adding new
variants is forward-compatible; consumers that don't recognize a
variant log + drop it. Removing variants is breaking and requires
a major version bump.

---

## Persistence, replay, ordering

**No persistence.** Events are not durable. If halmasuit's daemon
restarts, in-flight events are lost. Consumers that need to
catch up after restart query the relevant component directly
(e.g., DMS asks halmasuit for current phase on connect).

**No replay buffer.** Consumers see live events from the moment
they subscribe. The wallpaper engine starts in its boot-state
config and only updates as new events arrive. This is fine for
the reactive-wallpaper use case (boot is a known-starting-state
scenario; later transitions are observable in flight).

**No event log.** Diagnostics use tracing as today; the bus is
for runtime coordination, not for audit trails. If the user later
wants an event audit log, that's a separate consumer that
subscribes to all events and writes to a journal.

---

## Integration sequence

1. **Crate scaffold.** `cargo new --lib halmasuit-events`; pin to
   the workspace's MSRV and edition; minimal `Cargo.toml` with
   `serde`, `serde_json`, and the chosen async runtime.
2. **Event type definitions.** `SystemEvent`, `SystemPhase`,
   `EventBus`, `Subscriber`, `Publisher`. Pure types, no I/O yet.
3. **In-process broadcast channel.** Internal `EventBus`
   implementation backed by a broadcast channel; subscribe + publish
   work in-process. Cover with unit tests.
4. **Wire halmasuit daemon as publisher.** halmasuit's existing
   lifecycle code (graceful_shutdown, PrepareForShutdown
   subscription, the boot-phase progression) publishes
   `Phase(*)` events on the bus. No external consumer yet; just
   verify the events flow.
5. **Wire wallpaper engine as consumer.** The wallpaper module
   (inside halmasuit-the-daemon for now) subscribes to phase
   events. Implement the reaction state machine in a stubbed form:
   subscribe, log, no behavior change. Verify events arrive.
6. **Wire reactive hexrain.** The wallpaper state machine actually
   reacts — pushes new palettes through the existing
   `colorPrimaryNext` etc. uniforms when phase events fire. The
   boot progression (black → phase-1 → phase-2 → greeter →
   session-active) becomes visible end-to-end. This is the
   milestone where the bus pays off — reactive wallpaper works.
7. **Unix socket transport.** Add the socket at
   `$XDG_RUNTIME_DIR/halmasuit/events.sock`; route external
   publishers through it. SO_PEERCRED-based authorization.
8. **`halmasuit-event` CLI.** A small binary that wraps the socket
   for shell scripts to publish UserSignals.
9. **Compositor publishing.** Once `kushuh` exists, it publishes
   compositor events. Until then, the WindowOpen / Fullscreen /
   PerspectiveSwitch arms are unused (niri doesn't publish without
   patches we don't intend to write).
10. **DMS consumer (later).** A small DMS module subscribes to
    Phase events to show "system phase" in its status chrome —
    optional, depends on whether the user wants the indicator.

Steps 1-6 are the minimum to ship reactive wallpaper end-to-end.
Steps 7-10 are incremental polish.

---

## Wallpaper-extraction dependency

The reactive wallpaper consumer of this bus naturally lives in
`halmasuit-wallpaper`. That crate is currently a module inside
`halmasuit` (`crates/halmasuit/src/wallpaper/`) and will be
extracted to its own crate eventually for code organization.
**That work is tracked separately and can land in either order
relative to this crate.** The in-process subscriber API works
identically whether the consumer is a module of halmasuit or its
own crate; nothing here forces the extraction.

---

## Open design questions

### Async runtime

halmasuit currently uses calloop (smithay-aligned). tokio is the
broader Rust async ecosystem default. The bus could use either:

- **calloop** — already in halmasuit; no new dependency; native
  fit for the daemon's loop.
- **tokio broadcast channel** — easier ergonomics; but adds a
  runtime; awkward when integrated into a calloop-driven daemon.

Default leaning: **calloop**, with a small internal channel
abstraction so out-of-process clients can use a tokio-flavored
adapter if they want. To be confirmed during implementation.

### Multi-monitor vs single-bus

For events that have a monitor scope (WindowOpen on which monitor,
PerspectiveSwitch on which output), the event variants carry the
monitor name as a payload field. The bus is single (one socket,
one in-process broadcast); subscribers filter by monitor if they
care. No per-monitor sub-buses.

### Idle thresholds

`SystemPhase::Idle` requires choosing thresholds. Should the bus
emit one event at each crossing (5min, 10min, 30min), or one
event per second with the duration as payload? Probable default:
**threshold-crossing only**, configurable per-installation.

### Boot phase granularity

How many `BootStage(N)` events does halmasuit emit during the
boot sequence? Initial guess: 2-3 stages, matching the user's
"phase 1 / phase 2 / etc." design intent. Exact count and timing
depends on what the wallpaper themes want; should be tunable.

### What about replay for late subscribers?

If a consumer subscribes 2 seconds after `Phase::SessionActive`
fires, it misses the event and won't react. For lifecycle phases
specifically, the consumer can query halmasuit's current phase on
subscribe (a one-shot "what's the current state?" call). Should
the bus offer this as a built-in primitive (`bus.current_phase()`),
or should consumers know to query halmasuit-the-daemon directly?
Probable default: **built-in for SystemPhase only** (it's the only
event type with a meaningful "current" state); other events are
genuinely live-only.

---

## Anti-patterns (forbidden)

- **No JSON-event sprawl as the only API.** The Rust enum is the
  source of truth; JSON is its serialization at the socket
  boundary. In-process consumers never touch JSON. Don't end up
  with a "you must serialize to JSON to publish" architecture.
- **No general-purpose IPC.** This bus is for system-state event
  coordination. Anything that needs request/response, file
  transfer, or arbitrary RPC goes elsewhere (DBus, direct sockets,
  halmasuit-session-ipc).
- **No bypassing the broker for privileged operations.** The
  event bus is unprivileged. Anything that needs root or PAM
  goes through `halmasuit-session`, period.
- **No leaking the bus's API surface as a public protocol.** This
  crate is workspace-internal infrastructure. Other projects may
  consume it (via the socket) but the semver contract is between
  halmasuit's components. Breaking changes are acceptable when
  needed.
- **No making events a substitute for state.** Events describe
  changes. If a component needs to know "what's the current
  phase?" it queries halmasuit directly. Events are notifications,
  not the database.
- **No persistence creep.** This is a runtime-coordination bus.
  Audit/log requirements get a separate consumer (probably the
  existing tracing infrastructure), not a feature here.

---

## Why this is worth building

1. **It unblocks reactive wallpaper.** That's the killer feature
   for halmasuit's "wallpaper as system state" philosophy. The
   wallpaper engine has the visual primitives ready (hexrain
   already has palette transitions, sun counts, all the uniforms);
   it just needs a stream of events to react to. The bus is the
   stream.

2. **It removes ad-hoc coordination.** Today, every new "thing X
   should happen when state Y changes" requires wiring callback
   chains across modules. With the bus, it's a subscribe + match.
   The wiring delta for a new reaction drops by an order of
   magnitude.

3. **It's the integration point for the nested compositor.**
   `kushuh` will publish compositor events; halmasuit subscribes
   to drive reactive wallpaper on perspective switches, window
   opens, fullscreen transitions. This is how kushuh participates
   in halmasuit's visual story.

4. **It enables third-party scripting.** Pomodoro timers, weather
   daemons, calendar-aware "morning" / "evening" triggers, focus-
   mode toggles — all become small shell scripts that publish a
   UserSignal. The wallpaper reacts. The user has built a
   programmable ambient feedback channel without writing any code
   beyond the publish line.

5. **Scope is bounded.** This is a small crate — maybe 800-1500
   LOC including tests. Closer to a week than an epic. The
   architectural payoff is disproportionate to the implementation
   cost.
