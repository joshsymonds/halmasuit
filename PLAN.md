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

| Component | Minimum bar for v2 |
|---|---|
| `halmasuit` binary | smithay-based Wayland server. Takes DRM master directly (`DRM_IOCTL_SET_MASTER`, not via logind). Starts in initramfs, survives `switch_root` via `SurviveFinalKillSignal=yes` in the unit's `[Unit]` section (validated by drm-master-probe Phase 2; `@argv[0]` from Phase 1 is the documented fallback for systemd < v255), drops to `compositor` user via `setresuid`, hosts one foreground `wl_client` at a time. |
| `halmasuit-spawn` | Correct from day one. ~80 lines, `#![forbid(unsafe_code)]`, UID floor (load-bearing per ARCHITECTURE.md threat model row 11), audit-grade. |
| `halmasuit-greetd` | Full greetd wire protocol via upstream [`greetd_ipc`](https://crates.io/crates/greetd_ipc) crate. State machine + real PAM in-process. Accepts existing greetd-compatible greeters (DankGreeter, regreet, tuigreet, gtkgreet, agreety) unchanged. |
| `halmasuit-splash` | **Static logo** painted via dumb buffer. NOT animated, NOT shader-driven. A single image, just enough that "the system didn't brick" is visually obvious. The Vulkan/wgpu "sizzle" splash is a Phase B polish pass. |
| `halmasuit-luks` | systemd password-agent. Happy path: render prompt, accept passphrase, submit via the agent socket protocol. No retry UI, no advanced options. |
| `halmasuit-fsck` | systemd-fsckd progress. Happy path: progress display. No repair-decision Y/N flow yet. |
| `halmasuit-emergency` | `emergency.target` adapter. Happy path: graphical PAM-as-root prompt, exec a terminal. No recovery-menu UX. |
| `halmasuit-kms`, `halmasuit-protocols`, `halmasuit-ipc`, `halmasuit-cli` | Backing crates per ARCHITECTURE.md workspace layout. |
| Introspection surface | NDJSON events to stderr/journald from `main()` line 1 via `tracing` + `tracing-subscriber`. Live snapshot via `/run/halmasuit/introspect.sock` and `org.halmasuit.Debug.Introspect.Snapshot()` once rootfs is reached. Socket mode 0600, owned by `compositor`. PAM message text redacted before emission. Query-only; separate D-Bus interface from `org.halmasuit.Compositor1`. Lands first in build order (see "Build order" above). |
| `sd_notify` / SIGTERM handling | drm-master-probe Phase 1 identified that rootfs systemd sends SIGTERM to the orphan unit ~1s post-`switch_root`. v2 Phase A handles this via a graceful SIGTERM handler in halmasuit (independent of the killall survival mechanism — Phase 2's `SurviveFinalKillSignal=yes` covers the killall; the post-pivot orphan-unit SIGTERM is separate). The cleaner alternative — `execve` at switch_root + `sd_notify` registration with rootfs systemd (Phase 3 validated this works) — is a v3 polish that avoids the SIGTERM handler entirely. (Empirical findings in RESEARCH.md Phases 1, 2, 3.) |
| NixOS module | `services.halmasuit.enable = true;` replaces **both** Plymouth and greetd. Wires initramfs + rootfs systemd units, setuid bit on `halmasuit-spawn`, PAM service file `halmasuit`, `compositor` and `greeter` system users, polkit policies. Includes `boot.initrd.kernelModules` for the target hardware's GPU driver. |
| DankGreeter launcher patch | ~20 lines in DMS to skip the nested-niri spawn when `WAYLAND_DISPLAY` is set by halmasuit. |
| `tests/login-flash.nix` | Flipped from expected-RED to GREEN gate; the v1 measurement v2 satisfies. |
| `tests/full-boot-flash.nix` | New test: frame-capture continuity from kernel handoff through to `SESSION` phase. Asserts no all-black frame and no DSSIM jump across any transition. |

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
deliberately defers to the epic:

- **PAM bindings strategy.** `pam-client` and `pam-sys` are both stale (2022/2023). Decision: use-as-is / fork / thin-FFI-via-bindgen. Decide when the auth code lands.
- **smithay revision pin.** Pin to whatever niri or cosmic-comp is currently on. Decide at v2 start.
- **`halmasuit-luks` prompt rendering form.** Replace splash with prompt vs. overlay prompt via subsurface composition. Decide when the adapter is implemented.
- **`org.halmasuit.Compositor1` D-Bus surface.** Method list depends on what desktop-environment integration actually needs.
- **`org.halmasuit.Debug.Introspect` surface shape.** Single `Snapshot()` method vs. signal-based event stream over D-Bus vs. both. Exact NDJSON schema (surface roles, geometry units, phase enum). Redaction policy for `pam_message` content. Decide as the auth code and first introspection consumer land — the stderr/tracing half lands earlier and is schema-flexible by virtue of being unstructured `tracing` events at first.
- **OCR in `full-boot-flash` test.** May defer text-leak detection to a later test if tesseract bindings prove fiddly.
- **Build order within "broadly working."** User preference is the thin-spine approach. Open question: start with `halmasuit-spawn` (small, security-critical, standalone) before the spine, or start directly with the spine and add `halmasuit-spawn` as the first integration dep? Brainstorm decides.

## See also

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the design halmasuit implements
- [`RESEARCH.md`](RESEARCH.md) — empirically validated architectural foundations (drm-master-probe Phase 0 + Phase 1)
