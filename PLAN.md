# Plan

How halmasuit v2 gets built. Companion to [`ARCHITECTURE.md`](ARCHITECTURE.md)
(what we're building) and [`RESEARCH.md`](RESEARCH.md) (what's been
empirically validated).

## Where we are

**Phase A's in-repo contract is complete, and the privilege model has
been rebuilt.** The original Phase A shipped in-compositor PAM
(`halmasuit-pam`) plus a setuid `halmasuit-spawn` helper. The **unified
session/pamd privilege-separation epic** then replaced that model
wholesale with the OpenSSH/GDM shape: a single privileged
`halmasuit-session` broker that owns one `pam_handle_t` for the entire
auth→session lifecycle, runs `pam_authenticate` in an ephemeral
SIGKILL-able fork, and launches the session by forking once and dropping
privileges in a non-setuid child. `halmasuit-pam` and the setuid
`halmasuit-spawn` are **deleted** — one libpam surface, no setuid inode.
The compositor is now an unprivileged sans-IO relay to the broker. See
[`HANDOFF.md`](HANDOFF.md) §0 (the decision record + Amendments A1–A9)
and [`ARCHITECTURE.md`](ARCHITECTURE.md) "Authentication and session
lifecycle".

`tests/login-flash.nix` is GREEN **through the broker-launched session**
(PID + frame continuity across the real greeter→session transition).
`just check` is 244/244 + `r14-gate`; `just test-vm` is the 14-gate
sweep incl. the three real-PAM broker gates (`run-pam-auth`,
`session-r5r6`, `session-onehandle`).

**What's left to put halmasuit on real hardware is cross-repo work in
nix-config / DMS / gnomon:** the DankGreeter launcher patch, the
dms-niri integration switchover, and a real-hardware shakedown. The
in-repo next milestone is the **visual G-layer** (real DankGreeter +
real niri on the broker-launched path — HANDOFF §6).

**Phase B (initramfs survival) hasn't started.** drm-master-probe
Phases 0–3 already validated the empirical mechanics; the production
wiring is a later major milestone.

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
| `halmasuit` binary — greetd I/O integration | Done | calloop-wired `Listener` (SO_PEERCRED authz) + per-fd `Connection`; the greetd state machine is sans-IO and relays to the `halmasuit-session` broker (per-greeter `BrokerEpisode`, non-blocking broker calloop source — A6/A7/A8). No in-process PAM; no `SpawnRequest` (the broker fork-then-drops the session leader). |
| `halmasuit` binary — DRM master | Done | `DRM_IOCTL_SET_MASTER` on `/dev/dri/card0` (or `HALMASUIT_DRM_DEVICE`) at startup while still root; FD held for process lifetime. Fail-closed if `HALMASUIT_SKIP_DRM_MASTER` set under euid 0. |
| `halmasuit` binary — privilege drop | Done | In-process `setresgid` + `setresuid` to configured compositor uid. Final capability posture: `CapPrm=CapEff={CAP_KILL}` (signal authority over the greeter for the session-start kill); **bounding set empty** (`CapBnd=0` — the compositor execs no setuid helper, so any retained cap would be a least-authority regression, R15); `CapInh=CapAmb=∅`. Fail-closed if compositor uid unset and euid is root. |
| `halmasuit` binary — greeter spawning | Done | Fork+exec the configured greeter via `Command::pre_exec`: sigprocmask reset + setresgid + setresuid into greeter system user, minimal env passthrough (XDG_RUNTIME_DIR, WAYLAND_DISPLAY, GREETD_SOCK, PATH). SIGCHLD reaper claims the zombie when the greeter exits. |
| `halmasuit` binary — greeter kill on session start | Done | `pidfd_send_signal(greeter, SIGKILL)` on the A5 two-key swap. `CAP_KILL` retention crosses the uid boundary (compositor uid → greeter uid). `Event::GreeterTerminated { pid }` records the action. |
| `halmasuit-session` (privileged broker) | Done | One `pam_handle_t` whole lifecycle; `pam_authenticate` in an ephemeral SIGKILL-able `setrlimit`-bounded fork; fork-then-drop **non-setuid** session leader; identity independently PAM-re-derived (`pam_get_user`→pwent); UID floor in the leader child; `getgrouplist(resolved user)`-only supplementary groups (A9); `pam_getenvlist()`-merged env (A1); single calloop broker, socket-activated, idle-exit, evict-old slot (A2); relay-peer `SO_PEERCRED` gate (R8). `unsafe` confined to `pam_ffi`/`worker`. Two-tier `gambit:review` APPROVED (A9 escalation found + closed). |
| `halmasuit-session-ipc` (pure wire contract) | Done | Types + codec for the compositor↔broker relay; A5 one-way `BrokerToCompositor` lifecycle frames (`SessionOpened`/`SessionEnded`); `#![forbid(unsafe_code)]`. |
| `halmasuit-greetd` | Done | Clean-room wire types from the [protocol spec](https://man.sr.ht/~kennylevinsen/greetd/protocol.md), **fully sans-IO** state machine (emit/suspend/resume, A7), length-prefixed JSON codec, `Listener` (SO_PEERCRED, world-mode rejection). No libpam; relays to the broker. `MAX_SESSION_BUILDS_PER_CONNECTION` removed (R14). |
| `halmasuit-splash` | Deferred to Phase B | Static logo painted via dumb buffer. Phase B (initramfs survival) is where the splash becomes visible; before that there's nothing to paint over. |
| `halmasuit-luks` / `-fsck` / `-emergency` | Deferred to Phase B | Adapter crates that depend on initramfs context. |
| `halmasuit-kms` / `-protocols` / `-ipc` / `-cli` | Stub | Workspace crates exist as placeholders. Concrete code lands as the consuming task arrives — `halmasuit-kms` likely populates during the Phase B DRM-backend work. |
| Introspection surface — NDJSON to journald | Done | `halmasuit-introspect` emits `Event` variants through `tracing` + `tracing-subscriber`'s JSON formatter to stderr; systemd captures into journald. Variants in stable use: `Started`, `PhaseEntered` (Init/DrmMasterAcquired/WaylandReady/GreetdReady/Deprivileged), `GreeterSpawned`, `GreeterTerminated`, `SessionRequested`, `Shutdown`, `Fatal`. |
| Introspection surface — live snapshot socket | Queued | `/run/halmasuit/introspect.sock` + `org.halmasuit.Debug.Introspect.Snapshot()` over D-Bus. Schema details deferred to the implementing task. |
| `sd_notify` / SIGTERM handling | Phase A done | Graceful SIGTERM emits `Shutdown { reason: signal_term }`; SIGCHLD reaper handles zombie children. Phase B adds `SurviveFinalKillSignal=yes` + `execve` re-pivot to rootfs (drm-master-probe Phases 2+3 validated this works). |
| NixOS module | Done | `services.halmasuit.enable = true`; socket-activated host-ns `halmasuit-session` broker unit (no standing root when idle) + the hardened deprivileged `halmasuit.service`. **No `security.wrappers` setuid entry** (the setuid `halmasuit-spawn` is deleted — R15); no setuid inode in the closure. PAM service auto-installed; the broker carries `SupplementaryGroups = [ "shadow" ]` so pam_unix's `getspnam` fast-path avoids the `unix_chkpwd` fork (irrelevant to the session — A9 derives leader groups from the resolved user only). `HALMASUIT_BROKER_PEER_UID`/`relay_peer_uid` set to the compositor uid when the compositor is enabled, else the greeter uid. |
| `tests/login-flash.nix` | Done — GREEN | Measures halmasuit's PID + frame continuity across the real greeter→session transition **through the broker-launched session**. Normal pass/fail hard gate — the old CI `continue-on-error` + Justfile exit-code inversion are deleted and must not return. |
| `tests/halmasuit-vm.nix` | Done | Consolidated VM gate: lifecycle events, socket permissions, post-drop process identity, greeter child identity, Wayland globals, full real-PAM auth through the broker + greeter termination, clean shutdown. |
| `tests/full-boot-flash.nix` | Deferred to Phase B | Frame-capture continuity from kernel handoff through `SESSION`; depends on initramfs survival being in place. |
| DankGreeter launcher patch | **Cross-repo** | ~20 lines in DMS (nix-config) to skip its nested-niri spawn when `WAYLAND_DISPLAY` is set by halmasuit. |
| dms-niri integration on gnomon | **Cross-repo** | Replace `services.greetd.enable` with `services.halmasuit.enable` in gnomon's host config; declare halmasuit-greeter / halmasuit-compositor users. |
| Real-hardware shakedown on gnomon | **Cross-repo** | Boot halmasuit on actual KMS hardware (not virtio-gpu); will likely surface integration issues VM tests can't see. |

## Phase B: initramfs survival

**Starting state (as of Phase A close).** The rootfs compositor is
done end-to-end in VM tests. drm-master-probe Phases 0–3 already
empirically validated the load-bearing mechanics Phase B builds on:
`DRM_IOCTL_SET_MASTER` survives `setresuid` and fork (Phases 0–1),
and survives `switch_root` + `execve` with the same FD remaining
master (Phase 3, `tests/drm-master-probe-phase3.nix`). Phase B is
production wiring on validated foundations.

**First task: `initrd-handoff-probe`** — a research crate analogous
to drm-master-probe, exercising the full halmasuit binary across
the initramfs → rootfs boundary. drm-master-probe-phase3 covers
the bare exec test; this probe extends it to halmasuit's full
Wayland + greetd + pidfd surface area. Should:

1. Run halmasuit from `boot.initrd.systemd.services.halmasuit` —
   acquire DRM master, bind the Wayland socket, emit lifecycle
   events to a sink that survives the pivot (journald cross-pivot
   continuity is the test surface here).
2. Either re-exec across `switch_root` (Phase 3 pattern: new PID,
   FDs preserved by hand) or stay with the same PID via
   `SurviveFinalKillSignal=yes` (Phase 2 pattern). Decide per the
   handoff-mechanism Open Decision below.
3. Assert post-pivot continuity: same DRM master fd usable for
   ioctls, Wayland socket reachable by a fresh client spawned on
   the rootfs side, single NDJSON event stream visible in rootfs
   journald.

This is scaffolding for the production wiring, not production
halmasuit-from-initrd. Stub the greetd + PAM paths for the probe;
the goal is to flush out the cross-pivot mechanics in isolation
before the production halmasuit binary depends on them.

### Phase B production build order (after the probe is green)

| Item | Notes |
|---|---|
| halmasuit binary — `--features initramfs` gate | Per ARCHITECTURE.md: differs from the rootfs path only in DRM open (direct, no logind) and password-agent registration. Same crate; both feature configurations should pass `just check`. |
| `halmasuit-splash` | Static logo painted via dumb-buffer KMS write. Same crate used in `INITRAMFS_SPLASH`, `ROOTFS_SPLASH`, and `SHUTDOWN_SPLASH` phases. The animated/wgpu "sizzle" path is a polish pass deferred until full-boot-flash is reliably green. |
| `halmasuit-luks` | systemd password-agent Wayland client. Runs as root in initramfs by necessity (no user db yet). Highest-risk surface in Phase B — sees passphrases. Decision on rendering form (foreground replacement vs subsurface overlay) lands at this task. |
| NixOS module — initrd wiring | `boot.initrd.systemd.services.halmasuit`, `boot.initrd.kernelModules` for the target GPU driver, Mesa + ICDs in the initramfs closure (~100MB add per ARCHITECTURE.md "Capability and cost"). |
| `tests/full-boot-flash.nix` | The Phase B hard gate. Frame-capture continuity from kernel handoff through SESSION; asserts no all-black frames and no DSSIM jump across any transition. Replaces Plymouth's role in proving "the boot looks right." |
| Plymouth removal on gnomon | Cross-repo. After full-boot-flash is green in VM, switch gnomon's nix-config to drop `boot.plymouth.enable` and add `services.halmasuit.enable` with the new initrd options. |

Open decisions specific to Phase B (initramfs handoff mechanism,
`halmasuit-luks` UI form, OCR in full-boot-flash) are listed in the
**Open decisions** section below.

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

- ~~**PAM bindings strategy.**~~ **RESOLVED:** `pam-sys` directly, no wrapper crate. Originally in `halmasuit-pam`; **superseded by the privilege-separation epic** — `pam-sys` now links in exactly one crate, the privileged `halmasuit-session` broker (`halmasuit-pam` deleted; `r14-gate` enforces the single libpam surface).
- ~~**smithay revision pin.**~~ **RESOLVED:** pinned to niri's current git revision (`ff5fa7df...`) in workspace `Cargo.toml`.
- ~~**Build order within "broadly working."**~~ **RESOLVED** (Phase A): introspection sink first, then the smithay spine, then `halmasuit-greetd`, then calloop wiring + DRM master + privilege drop + greeter spawn + login-flash flip.
- ~~**Privilege-drop mechanism + setuid wrapper.**~~ **SUPERSEDED by the privilege-separation epic.** Phase A's setuid-`halmasuit-spawn` model is **deleted**. The compositor drops in-process via `setresgid`/`setresuid` after binding sockets and acquiring DRM master to a post-drop posture of `CapPrm=CapEff={CAP_KILL}` with an **empty bounding set** (it execs no setuid helper — R15). Privilege-drop+exec exists only in the `halmasuit-session` broker's **non-setuid** fork-then-drop session-leader child (already root — R7/R11). No `security.wrappers` setuid entry; no `NoNewPrivileges` constraint from a setuid handoff (there is none). See HANDOFF §0 / ARCHITECTURE.md "Authentication and session lifecycle".
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
