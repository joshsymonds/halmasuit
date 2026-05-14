# Plan

How halmasuit v2 gets built. Companion to [`ARCHITECTURE.md`](ARCHITECTURE.md)
(what we're building) and [`RESEARCH.md`](RESEARCH.md) (what's been
empirically validated).

## Goal

Make `tests/login-flash.nix` go green and ship a *broadly working* v2
that delivers halmasuit's architectural commitment end-to-end: one
process from initramfs through to shutdown, no visible flashes, every
phase transition internal to the running compositor. Components are
implemented at minimum-viable quality; polish ("atomic parts superbly")
happens iteratively under the protection of the green test.

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

| Component | Status | Minimum bar / current state |
|---|---|---|
| `halmasuit` binary — smithay scaffolding | Done | smithay 0.7 (pinned to niri's rev) scaffolding; `wl_compositor`, `xdg_wm_base`, `wl_seat`, `wl_output`, `wl_shm` advertised; lifecycle events emit through `halmasuit-introspect`. Tested via `tests/halmasuit-introspect.nix` (NixOS VM). |
| `halmasuit` binary — greetd I/O integration | **Queued (next task)** | calloop wiring: `halmasuit_greetd::server::Listener` bound at `/run/halmasuit/greetd.sock`, per-connection state with `halmasuit_pam::PamThread` factory, `SpawnRequest` handoff to `halmasuit-spawn` invocation. Emits `Phase::GreetdReady`. |
| `halmasuit` binary — DRM master + setresuid | Queued | Direct `DRM_IOCTL_SET_MASTER` on `/dev/dri/card0` (no logind). Drop to `compositor` user via `setresuid`. Phase A does NOT include initramfs survival (`SurviveFinalKillSignal=yes` / `@argv[0]`) — that's Phase B; v2 Phase A runs from `multi-user.target` only. |
| `halmasuit-spawn` | Done | Audit-grade ~140 lines, `#![forbid(unsafe_code)]`, UID floor (load-bearing per ARCHITECTURE.md row 11), pwent validation, env allowlist, fuzz harness. Reviewed (task #9 security fix). |
| `halmasuit-greetd` | Done | Wire types (clean-room re-derivation from the [protocol spec](https://man.sr.ht/~kennylevinsen/greetd/protocol.md); upstream `greetd_ipc` is GPL-3-only and would force halmasuit's binary to GPL), state machine, length-prefixed JSON codec, `Listener` (SO_PEERCRED, world-mode rejection, `accept_authorized`), `Connection` (per-fd driver with `Zeroizing` read buffer + explicit size cap + propagated codec errors). 49 unit + proptests. `gambit:review` complete (18 improvements implemented). |
| `halmasuit-pam` | Done | Real libpam FFI quarantined to this crate (`#![deny(unsafe_code)]` + per-block `#[expect]`). `Pam` RAII handle, `bridge_conv` extern "C" callback (catch_unwind, zeroized response buffers, NUL-rejection), `PAM_FAIL_DELAY` no-op, `PamThread` worker driving `halmasuit_greetd::PamSession` with bounded `recv_timeout`. 27 tests. `gambit:review` complete (3 gaps + 14 improvements all addressed). |
| `halmasuit-splash` | Queued | **Static logo** painted via dumb buffer. NOT animated, NOT shader-driven. A single image, just enough that "the system didn't brick" is visually obvious. The Vulkan/wgpu "sizzle" splash is a Phase B polish pass. |
| `halmasuit-luks` / `-fsck` / `-emergency` | Deferred to Phase B | Adapter crates that depend on initramfs (`luks`, `fsck`) or recovery flows (`emergency`). Phase A is rootfs-only. |
| `halmasuit-kms` / `-protocols` / `-ipc` / `-cli` | Stub | Workspace crates exist but bodies are placeholders. Concrete code lands as the integration centerpiece needs each piece. `halmasuit-kms` will likely populate during the DRM master task. |
| Introspection surface | In flight | **Done:** NDJSON events to stderr/journald from `main()` line 1 via `tracing` + `tracing-subscriber` (`halmasuit-introspect` crate; lifecycle events `Started`/`PhaseEntered`/`Shutdown`/`Fatal`). **Queued:** live snapshot socket at `/run/halmasuit/introspect.sock` + `org.halmasuit.Debug.Introspect.Snapshot()`; redaction policy for `pam_message` content. |
| `sd_notify` / SIGTERM handling | In flight (Phase A scope is partial) | **Done:** graceful SIGTERM handler in halmasuit (lifecycle test asserts `Shutdown { reason: signal_term }`). **Phase B:** `SurviveFinalKillSignal=yes` for switch_root survival (validated by drm-master-probe Phase 2) plus `execve` re-pivot + `sd_notify` registration with rootfs systemd (Phase 3 validated). Phase A doesn't ship initramfs survival, so neither lands here. |
| NixOS module | In flight | **Done:** `services.halmasuit.enable = true` with `RuntimeDirectory=halmasuit`, hardening (NoNewPrivileges, ProtectKernel*, SystemCallFilter, etc.), setuid wrapper for `halmasuit-spawn`. **Queued:** PAM service file `/etc/pam.d/halmasuit`, `compositor` + `greeter` system users, replacement of greetd in `dms-niri`, `boot.initrd.kernelModules`. |
| DankGreeter launcher patch | Queued | ~20 lines in DMS to skip the nested-niri spawn when `WAYLAND_DISPLAY` is set by halmasuit. Last step before the login-flash flip. |
| `tests/login-flash.nix` | Queued (final step) | Currently inverted (RED-by-design until v2 ships). Flips to a GREEN gate when the integration above is complete; CI's `continue-on-error: true` + `expected-fail` interpretation gets removed at the same time. |
| `tests/full-boot-flash.nix` | Deferred to Phase B | Frame-capture continuity from kernel handoff through to `SESSION` phase; depends on initramfs survival being in place. |

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

- ~~**PAM bindings strategy.**~~ **RESOLVED:** `pam-sys` 1.0.0-alpha5 directly, no wrapper crate. Implemented in `halmasuit-pam`; review-approved. See ARCHITECTURE.md "Open decisions" row 1 for the full rationale.
- ~~**smithay revision pin.**~~ **RESOLVED:** pinned to niri's current git revision (`ff5fa7df...`) in workspace `Cargo.toml`.
- ~~**Build order within "broadly working."**~~ **RESOLVED:** introspection sink landed first (per the stated constraint), then `halmasuit-spawn` audit-grade, then the smithay spine, then `halmasuit-greetd` + `halmasuit-pam`. The integration-centerpiece task (calloop wiring) ties them together next.
- **`halmasuit-luks` prompt rendering form.** Replace splash with prompt vs. overlay prompt via subsurface composition. *Decision deferred to Phase B (when the adapter lands).*
- **`org.halmasuit.Compositor1` D-Bus surface.** Method list depends on what desktop-environment integration actually needs. *Decision deferred to the D-Bus implementation task.*
- **`org.halmasuit.Debug.Introspect` surface shape.** Single `Snapshot()` method vs. signal-based event stream over D-Bus vs. both. Exact NDJSON schema (surface roles, geometry units, phase enum). Redaction policy for `pam_message` content. *Decision deferred to the snapshot-socket task; the stderr/tracing half is already shipping and is schema-flexible.*
- **OCR in `full-boot-flash` test.** May defer text-leak detection if tesseract bindings prove fiddly. *Decision deferred to Phase B (when full-boot-flash is built).*

## See also

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the design halmasuit implements
- [`RESEARCH.md`](RESEARCH.md) — empirically validated architectural foundations (drm-master-probe Phase 0 + Phase 1)
