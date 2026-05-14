# Plan

How halmasuit v2 gets built. Companion to [`ARCHITECTURE.md`](ARCHITECTURE.md)
(what we're building) and [`RESEARCH.md`](RESEARCH.md) (what's been
empirically validated).

## Where we are (2026-05-14)

**Phase A's in-repo contract is complete.** `tests/login-flash.nix` is
GREEN (the v1 baseline assertion of greeter→session PID continuity
holds, measured against halmasuit's PID as the long-lived compositor).
132/132 unit tests pass; the consolidated `halmasuit-vm` integration
test passes in ~15s end-to-end with the full lifecycle exercised. The
`gambit:review` panel returned 15 findings against the integration
milestone; all are implemented or fixed inline. The privilege split
is real: halmasuit drops to a configured compositor system user, retains
exactly `CAP_KILL` over the drop, kills the greeter on session start.

**What's left to put halmasuit on real hardware is cross-repo work in
nix-config / DMS / gnomon:** the DankGreeter launcher patch, the
dms-niri integration switchover, and a real-hardware shakedown.

**Phase B (initramfs survival) hasn't started.** drm-master-probe
Phases 0–3 already validated the empirical mechanics; the production
wiring is the next major milestone.

## Goal

Make `tests/login-flash.nix` go green and ship a *broadly working* v2
that delivers halmasuit's architectural commitment end-to-end: one
process from initramfs through to shutdown, no visible flashes, every
phase transition internal to the running compositor. Components are
implemented at minimum-viable quality; polish ("atomic parts superbly")
happens iteratively under the protection of the green test.

Phase A landed the rootfs half; Phase B will extend halmasuit
backwards into initramfs.

## Approach

**Thin vertical slice first.** Build the smallest end-to-end version
that crosses every architectural boundary the project depends on —
initramfs to rootfs, splash to greeter to session, root to compositor
user. Polish each component within the working integration, in any
order, as the working build keeps catching regressions.

This is the user's stated working preference: "broadly working, then
individual components until beautiful." The order is a preference, not
a contract. The scope below is the contract.

## Build order

One ordering constraint inside the otherwise-flexible "broadly working"
phase: **the structured-event sink lands first, before any compositor
logic.** From `main()` line 1, halmasuit emits NDJSON state events to
stderr through `tracing` + `tracing-subscriber`. journald captures them
across both initramfs and rootfs; humans and agents tail with
`journalctl -u halmasuit -f -o json`.

This is how you see what the spine is doing while you build it. Headless
VM tests assert structure (PID continuity, DRM-master holding) but
`virtio-gpu-pci` paints nothing, and the project is designed to be
developed on any KVM-capable Linux host without a local display (per
[`ARCHITECTURE.md`](ARCHITECTURE.md): "v2 must build and pass tests on
any KVM-capable Linux system, not just gnomon"). Structured event logs
are the agent- and SSH-friendly substitute for a monitor. Instrumenting
after the spine is built wastes the observability exactly when it's
most useful.

The live snapshot surface — `/run/halmasuit/introspect.sock` and
`org.halmasuit.Debug.Introspect.Snapshot()` over D-Bus — lands later,
once the rootfs phase has somewhere stable for `/run/halmasuit/`.
Initramfs gets the event stream only; nobody is querying initramfs
interactively anyway.

Both surfaces are query-only by construction. No `set_*` or `inject_*`
methods, ever. Kept on a separate D-Bus interface from
`org.halmasuit.Compositor1` so control-plane scope can't leak into
observability scope. PAM message text is redacted before emission so
the feed can't become a passphrase side-channel; socket mode 0600,
owned by `compositor`.

## In scope

Status legend: **Done** (landed + tested) · **In flight** (partial,
under active iteration) · **Queued** (Phase A scope, not started) ·
**Deferred to Phase B** (deliberately out of Phase A — initramfs +
adapter crates).

| Component | Status | Current state |
|---|---|---|
| `halmasuit` binary — smithay scaffolding | Done | smithay 0.7 (pinned to niri's rev); `wl_compositor`, `xdg_wm_base`, `wl_seat`, `wl_output`, `wl_shm` advertised. |
| `halmasuit` binary — greetd I/O integration | Done | calloop-wired `Listener` (SO_PEERCRED authz) + per-fd `Connection` + `PamThread` factory; `MAX_GREETD_CONNECTIONS=4` runaway cap; `SpawnRequest` handoff to `halmasuit-spawn`. |
| `halmasuit` binary — DRM master | Done | `DRM_IOCTL_SET_MASTER` on `/dev/dri/card0` (or `HALMASUIT_DRM_DEVICE`) at startup while still root; FD held for process lifetime. Fail-closed if `HALMASUIT_SKIP_DRM_MASTER` set under euid 0. |
| `halmasuit` binary — privilege drop | Done | In-process `setresgid` + `setresuid` to configured compositor uid, with `prctl(PR_SET_KEEPCAPS, 1)` + `capset` to retain exactly `{CAP_KILL}` — needed for the greeter-kill on session start. Fail-closed if compositor uid unset and euid is root. |
| `halmasuit` binary — greeter spawning | Done | Fork+exec the configured greeter via `Command::pre_exec`: sigprocmask reset + setresgid + setresuid into greeter system user, minimal env passthrough (XDG_RUNTIME_DIR, WAYLAND_DISPLAY, GREETD_SOCK, PATH). SIGCHLD reaper claims the zombie when the greeter exits. |
| `halmasuit` binary — greeter kill on `SessionRequested` | Done | `Child::kill()` immediately after the `SessionRequested` event emit, before invoking halmasuit-spawn. CAP_KILL retention crosses the uid boundary (compositor uid 998 → greeter uid 999). `Event::GreeterTerminated { pid }` records the action. |
| `halmasuit-spawn` | Done | Audit-grade ~140 lines, `#![forbid(unsafe_code)]`, UID floor (load-bearing per ARCHITECTURE.md row 11), pwent validation, env allowlist, fuzz harness. `gambit:review` complete (task #9 security fix). |
| `halmasuit-greetd` | Done | Clean-room wire types from the [protocol spec](https://man.sr.ht/~kennylevinsen/greetd/protocol.md), state machine, length-prefixed JSON codec, `Listener` (SO_PEERCRED, world-mode rejection, `accept_authorized` w/ both positive- and rejection-path tests), `Connection` (per-fd driver with `Zeroizing` read buffer, explicit size cap, propagated codec errors). `gambit:review` complete (18 improvements implemented). |
| `halmasuit-pam` | Done | Real libpam FFI quarantined to this crate (`#![deny(unsafe_code)]` + per-block `#[expect]`). `Pam` RAII handle, `bridge_conv` extern "C" callback (catch_unwind, zeroized response buffers, NUL-rejection), `PAM_FAIL_DELAY` no-op, `PamThread` worker with bounded `recv_timeout`. `gambit:review` complete (3 gaps + 14 improvements addressed). |
| `halmasuit-splash` | Deferred to Phase B | Static logo painted via dumb buffer. Phase B (initramfs survival) is where the splash becomes visible; before that there's nothing to paint over. |
| `halmasuit-luks` / `-fsck` / `-emergency` | Deferred to Phase B | Adapter crates that depend on initramfs context. |
| `halmasuit-kms` / `-protocols` / `-ipc` / `-cli` | Stub | Workspace crates exist as placeholders. Concrete code lands as the consuming task arrives — `halmasuit-kms` likely populates during the Phase B DRM-backend work. |
| Introspection surface — NDJSON to journald | Done | `halmasuit-introspect` emits `Event` variants through `tracing` + `tracing-subscriber`'s JSON formatter to stderr; systemd captures into journald. Variants in stable use: `Started`, `PhaseEntered` (Init/DrmMasterAcquired/WaylandReady/GreetdReady/Deprivileged), `GreeterSpawned`, `GreeterTerminated`, `SessionRequested`, `Shutdown`, `Fatal`. |
| Introspection surface — live snapshot socket | Queued | `/run/halmasuit/introspect.sock` + `org.halmasuit.Debug.Introspect.Snapshot()` over D-Bus. Schema details deferred to the implementing task. |
| `sd_notify` / SIGTERM handling | Phase A done | Graceful SIGTERM emits `Shutdown { reason: signal_term }`; SIGCHLD reaper handles zombie children. Phase B adds `SurviveFinalKillSignal=yes` + `execve` re-pivot to rootfs (drm-master-probe Phases 2+3 validated this works). |
| NixOS module | Done | `services.halmasuit.enable = true` with full option surface: `compositorUid`, `greeterUid`, `greeterGroup`, `greeterCommand`, `pamService`, `spawnPackage`, `installPamConfig`. Hardening directives kept that don't imply NoNewPrivileges; security.wrappers for `halmasuit-spawn` (setuid root) unconditional. PAM service auto-installed; `SupplementaryGroups = [ "shadow" ]` so pam_unix's `getspnam` fast-path avoids the `unix_chkpwd` fork. |
| `tests/login-flash.nix` | Done — GREEN | Measures halmasuit's PID continuity across greeter→session (the v1 baseline measured niri's PID, which was greetd-architecture-specific). CI's `continue-on-error` + the Justfile inversion are removed; it's a hard gate. |
| `tests/halmasuit-vm.nix` | Done | Consolidated VM gate: lifecycle events, socket permissions, post-drop process identity, greeter child identity, Wayland globals, full PAM auth + halmasuit-spawn invocation + greeter termination, clean shutdown. ~15s end-to-end. |
| `tests/full-boot-flash.nix` | Deferred to Phase B | Frame-capture continuity from kernel handoff through `SESSION`; depends on initramfs survival being in place. |
| DankGreeter launcher patch | **Cross-repo** | ~20 lines in DMS (nix-config) to skip its nested-niri spawn when `WAYLAND_DISPLAY` is set by halmasuit. |
| dms-niri integration on gnomon | **Cross-repo** | Replace `services.greetd.enable` with `services.halmasuit.enable` in gnomon's host config; declare halmasuit-greeter / halmasuit-compositor users. |
| Real-hardware shakedown on gnomon | **Cross-repo** | Boot halmasuit on actual KMS hardware (not virtio-gpu); will likely surface integration issues VM tests can't see. |

## Out of scope (Phase B or later)

| Item | Reason |
|---|---|
| Animated / shader-driven splash | Logo is sufficient for "didn't brick" signal. The Vulkan/wgpu sizzle version is a polish pass once the static path works. |
| Direct-scanout optimization (v3) | A nested-compositing performance optimization that doesn't change correctness. Adds `ext-halmasuit-host-v1` protocol + niri-side patch. After v2 ships. |
| Crash recovery overlay | Graceful crash recovery (per ARCHITECTURE.md v4+) — paint a recovery scene when niri dies. Adds after v2 ships. |
| Fast user switching | Multi-session hosting, foreground swap. Post-v2. |
| HDR / VRR pass-through | Color management + presentation-timing plumbing through nesting. Post-v2 + depends on v3. |
| Multi-seat | One halmasuit per seat or one serving multiple. Post-v2. |
| Screen casting / remote display infrastructure | Post-v2. |
| Real-hardware verification beyond gnomon | gnomon (daily-driver) is the v2 deployment target; broader hardware coverage comes later. |
| Edge-case adapter UX (LUKS retry, fsck repair Y/N, emergency recovery menu) | Happy paths only for v2. Edge cases are polish passes. |

## Open decisions for the v2 brainstorm

Carried forward from ARCHITECTURE.md "Open decisions" + things this plan
deliberately defers to the epic. **Resolved** entries kept as a record
of the decision; **Open** entries still need answering as the
consuming code lands.

### Resolved during Phase A

- ~~**PAM bindings strategy.**~~ **RESOLVED:** `pam-sys` 1.0.0-alpha5 directly, no wrapper crate. Implemented in `halmasuit-pam`; review-approved.
- ~~**smithay revision pin.**~~ **RESOLVED:** pinned to niri's current git revision (`ff5fa7df...`) in workspace `Cargo.toml`.
- ~~**Build order within "broadly working."**~~ **RESOLVED:** introspection sink first, then `halmasuit-spawn` audit-grade, then the smithay spine, then `halmasuit-greetd` + `halmasuit-pam`, then calloop wiring + DRM master + privilege drop + greeter spawn + login-flash flip.
- ~~**Privilege-drop mechanism + setuid wrapper for `halmasuit-spawn`.**~~ **RESOLVED:** halmasuit drops in-process via `setresgid` + `setresuid` after binding sockets and acquiring DRM master. Retains `{CAP_KILL}` via `prctl(PR_SET_KEEPCAPS, 1)` + `capset`. `halmasuit-spawn` wrapped setuid root via `security.wrappers`. NoNewPrivileges-implying hardening directives are unconditionally off (incompatible with the setuid handoff); `SupplementaryGroups = [ "shadow" ]` so pam_unix avoids the fragile `unix_chkpwd` fork.
- ~~**login-flash measurement target.**~~ **RESOLVED:** measures halmasuit's `MainPID` (the long-lived compositor) across greeter→session, replacing the v1 baseline's niri-PID assertion (greetd-architecture-specific). Same assertion intent — no compositor restart — different process under measurement.

### Open

- **`halmasuit-luks` prompt rendering form.** Replace splash with prompt vs. overlay prompt via subsurface composition. *Decision deferred to Phase B (when the adapter lands).*
- **`org.halmasuit.Compositor1` D-Bus surface.** Method list depends on what desktop-environment integration actually needs. *Decision deferred to the D-Bus implementation task.*
- **`org.halmasuit.Debug.Introspect` surface shape.** Single `Snapshot()` method vs. signal-based event stream over D-Bus vs. both. Exact NDJSON schema (surface roles, geometry units, phase enum). Redaction policy for `pam_message` content. *Decision deferred to the snapshot-socket task; the stderr/tracing half is already shipping and is schema-flexible.*
- **OCR in `full-boot-flash` test.** May defer text-leak detection if tesseract bindings prove fiddly. *Decision deferred to Phase B (when full-boot-flash is built).*
- **Initramfs handoff mechanism.** `SurviveFinalKillSignal=yes` is the validated path (drm-master-probe Phase 2); the optional additional `execve` re-pivot to the rootfs-systemd MainPID (Phase 3) is a refinement. *Decision deferred to the Phase B `halmasuit-in-initrd` task — pick the simplest combination that makes `full-boot-flash` go green.*

## See also

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the design halmasuit implements
- [`RESEARCH.md`](RESEARCH.md) — empirically validated architectural foundations (drm-master-probe Phase 0 + Phase 1)
