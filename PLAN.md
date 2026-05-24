# Plan

How halmasuit v2 gets built. Companion to [`ARCHITECTURE.md`](ARCHITECTURE.md)
(what we're building) and [`RESEARCH.md`](RESEARCH.md) (what's been
empirically validated).

## Where we are

**The in-repo compositor is complete.** Six epics have shipped on
`main` (canonical @ `74dfd25`):

1. **Phase A** — rootfs compositor, greetd I/O, DRM master, privilege
   drop, greeter spawn/kill.
2. **Privilege-separation** (HANDOFF §0) — the unified session/pamd
   epic. The OpenSSH/GDM shape: a single privileged
   `halmasuit-session` broker owns one `pam_handle_t` for the entire
   auth→session lifecycle, runs `pam_authenticate` in an ephemeral
   SIGKILL-able fork, and launches the session by forking once and
   dropping privileges in a non-setuid child. `halmasuit-pam` and the
   setuid `halmasuit-spawn` are **deleted** — one libpam surface, no
   setuid inode. The compositor is an unprivileged sans-IO relay to
   the broker. See HANDOFF §0 (decision record + Amendments A1–A9)
   and ARCHITECTURE.md "Authentication and session lifecycle".
3. **Epic #2 Visual G-layer** — wallpaper plane composited from frame
   0, `assert_no_flash_stream` live, real niri as broker-launched
   session, frame-audit instrument with ssimulacra2 goldens.
4. **Epic #5 Hand-rolled libpam FFI** — production halmasuit-session
   links `-lpam` directly; `pam-sys` reduced to dev-deps-only audit
   lever via `tests/pam_ffi_parity.rs`. Zero bindgen/clang-sys/
   libclang at production build time.
5. **Epic #12 VideoBackend** — sandboxed `halmasuit-decoder`
   subprocess (rsmpeg + seccomp-bpf), restart-or-fallback policy,
   frame-0 invariant preserved through video wallpaper.
6. **Wayland-server convergence epic** — the R2-DankGreeter-pixel
   tail Epic #2 explicitly deferred. Delivers `wl_surface.frame`
   callbacks post-vblank, commit aggregation, deferred xdg-shell
   configure, PopupManager, surface enter/leave, full focus
   integration (data-device, primary-selection, text-input),
   libinput pointer + Xcursor render, presentation-feedback,
   linux-dmabuf, and the Phase-B-1..15 protocol surface (viewporter,
   decoration, activation, inhibit pair, foreign-v2, wm-dialog,
   toplevel-icon, data-device, primary-selection-v1, text-input-v3,
   cursor-shape, touch). Real DMS DankGreeter (Quickshell+Qt6) is the
   greeter; the full Qt6 keystroke auth arc reaches `SessionOpened`
   end-to-end.

`tests/login-flash.nix` is GREEN **through the broker-launched
session** (PID + frame continuity across the real greeter→session
transition). `just check` is 336/336 + `r14-gate` + `vis-selftest`;
`just test-vm` is the 24-gate sweep.

**What's left** is two independent tranches:

- **Cross-repo deployment to gnomon:** DankGreeter launcher patch in
  DMS, switchover from `services.greetd.enable` to
  `services.halmasuit.enable` in gnomon's nix-config, and the
  real-hardware shakedown on actual nvidia-drm + seatd + xkb.
- **Phase B (initramfs survival):** drm-master-probe Phases 0–3
  already validated the empirical mechanics. Production wiring is the
  major remaining milestone — `initrd-handoff-probe` research crate,
  `--features initramfs`, `halmasuit-luks`, BGRT-aware first frame,
  NixOS initrd module wiring, `tests/full-boot-flash.nix`, Plymouth
  removal.

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

Status legend: **Done** (landed + tested) · **Cross-repo** (in-repo work
complete; remaining work lives in nix-config / DMS / gnomon) ·
**Deferred to Phase B** (deliberately out of the rootfs compositor —
initramfs + adapter crates).

| Component | Status | Current state |
|---|---|---|
| `halmasuit` binary — smithay scaffolding | Done | smithay 0.7 (pinned to niri's rev); `wl_compositor`, `xdg_wm_base`, `wl_seat`, `wl_output`, `wl_shm` advertised. |
| `halmasuit` binary — greetd I/O integration | Done | calloop-wired `Listener` (SO_PEERCRED authz) + per-fd `Connection`; the greetd state machine is sans-IO and relays to the `halmasuit-session` broker (per-greeter `BrokerEpisode`, non-blocking broker calloop source — A6/A7/A8). No in-process PAM; no `SpawnRequest` (the broker fork-then-drops the session leader). |
| `halmasuit` binary — DRM master | Done | DRM open via `LibSeatSession`/seatd (`backend_session_libseat` smithay feature) — seatd is the registered master; halmasuit holds the DRM fd via libseat. Fail-closed if compositor uid unset and euid is root. |
| `halmasuit` binary — privilege drop | Done | In-process `setresgid` + `setresuid` to configured compositor uid. Final capability posture: `CapPrm=CapEff={CAP_KILL}` (signal authority over the greeter for the session-start kill); **bounding set empty** (`CapBnd=0` — the compositor execs no setuid helper, so any retained cap would be a least-authority regression, R15); `CapInh=CapAmb=∅`. |
| `halmasuit` binary — greeter spawning | Done | Fork+exec the configured greeter via `Command::pre_exec`: sigprocmask reset + setresgid + setresuid into greeter system user, minimal env passthrough (XDG_RUNTIME_DIR, WAYLAND_DISPLAY, GREETD_SOCK, PATH). SIGCHLD reaper claims the zombie when the greeter exits. |
| `halmasuit` binary — greeter kill on session start | Done | `pidfd_send_signal(greeter, SIGKILL)` on the A5 two-key swap. `CAP_KILL` retention crosses the uid boundary (compositor uid → greeter uid). `Event::GreeterTerminated { pid }` records the action. |
| `halmasuit-session` (privileged broker) | Done | One `pam_handle_t` whole lifecycle; `pam_authenticate` in an ephemeral SIGKILL-able `setrlimit`-bounded fork; fork-then-drop **non-setuid** session leader; identity independently PAM-re-derived (`pam_get_user`→pwent); UID floor in the leader child; `getgrouplist(resolved user)`-only supplementary groups (A9); `pam_getenvlist()`-merged env (A1); single calloop broker, socket-activated, idle-exit, evict-old slot (A2); relay-peer `SO_PEERCRED` gate (R8). Hand-rolled libpam FFI in `crates/halmasuit-session/src/pam_sys.rs` following sudo-rs's pattern; production links `-lpam` directly with zero bindgen / clang-sys / libclang at build time (Epic #5). `unsafe` confined to `pam_ffi`/`worker`. `pam-sys` retained as dev-deps-only audit lever via `tests/pam_ffi_parity.rs` (asserts struct layouts + constants + symbol resolution match bindgen against build-host libpam headers). |
| `halmasuit-session-ipc` (pure wire contract) | Done | Types + codec for the compositor↔broker relay; A5 one-way `BrokerToCompositor` lifecycle frames (`SessionOpened`/`SessionEnded`); `#![forbid(unsafe_code)]`. |
| `halmasuit-greetd` | Done | Clean-room wire types from the [protocol spec](https://man.sr.ht/~kennylevinsen/greetd/protocol.md), **fully sans-IO** state machine (emit/suspend/resume, A7), length-prefixed JSON codec, `Listener` (SO_PEERCRED, world-mode rejection). No libpam; relays to the broker. `MAX_SESSION_BUILDS_PER_CONNECTION` removed (R14). |
| Wallpaper plane — WallpaperEngine + 3 backends | Done | The `WallpaperEngine` owns a pluggable `WallpaperBackend` trait surface; three backends share it. `ImageBackend` decodes the configured PNG and composites it as the bottom-most internal plane from frame 0 (no separate client). `ShaderBackend` wires a live GLSL pipeline with declared-uniforms config. `VideoBackend` (Epic #12) drives a sandboxed `halmasuit-decoder` subprocess (rsmpeg + seccomp-bpf) over a SEQPACKET relay, with restart-or-fallback policy on decoder death — frame-0 invariant preserved through video. Config is the `services.halmasuit.wallpaper = { type = "image" | "shader" | "video"; ... }` discriminated union. |
| `halmasuit-decoder` / `halmasuit-decoder-ipc` | Done (Epic #12) | Sandboxed subprocess + pure wire contract crate. Decoder is forked + dup2'd to fd 3 by halmasuit, decodes via rsmpeg behind a seccomp-bpf syscall allowlist (33 syscalls, KillProcess default, openat read-only via SeccompCondition + MaskedEq), runs as the compositor uid 998 (no extra capabilities). mmap-backed custom AVIO; PTS-based pacing via `ppoll`; loop-on-EOF works through fresh AVIO context against the same memory region. Production builds bindgen-free via `FFMPEG_BINDING_PATH` (checked-in `ffmpeg_binding.rs`); only `just regenerate-decoder-bindings` invokes bindgen. |
| Wayland protocol surface | Done (convergence epic) | `wl_surface.frame` callbacks emitted post-vblank (R2); `is_sync_subsurface` aggregation in commit (R3); initial xdg_surface.configure deferred to commit handler (R4); smithay PopupManager + positioner-driven popup geometry (R5); `wl_surface.enter/leave` for xdg-toplevels (R6); focus integration with data-device / primary-selection / text-input (R7); libinput pointer events routed to `wl_pointer` (R8a); visible cursor render via Xcursor theme (R8b); `wp_presentation_feedback` with per-VBlank emission (R9); `zwp_linux_dmabuf_v1` with renderer-derived tranche (R10); Phase-B-1..15 advertise-and-delegate globals (`wp_viewporter`, `xdg_decoration_manager_v1`, `xdg_activation_v1`, idle-inhibit, keyboard-shortcuts-inhibit, `xdg-foreign-v2`, `xdg-wm-dialog`, `xdg-toplevel-icon`, `wl_data_device_manager`, `primary-selection-v1`, `text-input-v3`, `wp_cursor_shape_manager_v1`, `wl_touch`). Real toolkits (GTK4, Qt6/Quickshell) render and accept input end-to-end. |
| `halmasuit-luks` / `-fsck` / `-emergency` | Deferred to Phase B | Adapter crates that depend on initramfs context. |
| `halmasuit-kms` / `-protocols` / `-ipc` / `-cli` | Stub | Workspace crates exist as placeholders. Concrete code lands as the consuming task arrives — `halmasuit-kms` likely populates during the Phase B DRM-backend work. |
| Introspection surface — NDJSON to journald | Done | `halmasuit-introspect` emits `Event` variants through `tracing` + `tracing-subscriber`'s JSON formatter to stderr; systemd captures into journald. Variants in stable use: `Started`, `PhaseEntered` (Init/DrmMasterAcquired/WaylandReady/GreetdReady/Deprivileged), `GreeterSpawned`, `GreeterTerminated`, `SessionRequested`, `Shutdown`, `Fatal`, `FrameRendered` (frame_audit-gated). |
| Introspection surface — frame snapshot (`halmasuit-debug` only) | Done | `org.halmasuit.Debug.Introspect.Snapshot(path)` lives in `halmasuit-debug` (gated on `--features frame_audit`); visual VM tests consume it for byte-exact frame assertions. Deliberately NOT in the production binary (arbitrary-file-write surface). No production snapshot socket is planned: visual defects in halmasuit's domain are transients (sub-frame transitions), which a polled snapshot cannot catch. Field observability for transients, if/when needed, lands as an extension of the NDJSON event stream — `Snapshot()` is not the right shape for it. |
| `sd_notify` / SIGTERM handling | Done | Graceful SIGTERM emits `Shutdown { reason: signal_term }`; SIGCHLD reaper handles zombie children. Phase B adds `SurviveFinalKillSignal=yes` + `execve` re-pivot to rootfs (drm-master-probe Phases 2+3 validated this works). |
| NixOS module | Done | `services.halmasuit.enable = true`; socket-activated host-ns `halmasuit-session` broker unit (no standing root when idle) + the hardened deprivileged `halmasuit.service`. **No `security.wrappers` setuid entry** (the setuid `halmasuit-spawn` is deleted — R15); no setuid inode in the closure. PAM service auto-installed; the broker carries `SupplementaryGroups = [ "shadow" ]` so pam_unix's `getspnam` fast-path avoids the `unix_chkpwd` fork (irrelevant to the session — A9 derives leader groups from the resolved user only). `HALMASUIT_BROKER_PEER_UID`/`relay_peer_uid` set to the compositor uid when the compositor is enabled, else the greeter uid. |
| `tests/login-flash.nix` | Done — GREEN | Measures halmasuit's PID + frame continuity across the real greeter→session transition **through the broker-launched session**. Normal pass/fail hard gate. |
| `tests/halmasuit-vm.nix` | Done | Consolidated VM gate: lifecycle events, socket permissions, post-drop process identity, greeter child identity, Wayland globals, full real-PAM auth through the broker + greeter termination, clean shutdown. |
| `tests/visual-niri-session.nix` | Done | Real upstream niri (`nix-config.inputs.niri-flake.packages.niri-unstable`) nested as the broker-launched session. Software-rendered headless (llvmpipe). Real greetd auth → broker → niri `xdg_toplevel` fullscreen foreground; PID-continuity + `assert_no_flash_stream` across the swap. |
| `tests/visual-dankgreeter.nix` / `-dankgreeter-auth.nix` | Done | Real DMS DankGreeter (Quickshell+Qt6) as halmasuit's `greeterCommand` over the wallpaper, no-flash invariant intact. The `-auth` variant drives the full Qt6 keystroke auth arc end-to-end: real DMS UI → broker → real `pam_unix` → `SessionOpened` → broker-launched real niri. |
| `tests/visual-{frame-callbacks,sync-subsurface,deferred-configure,popup,gtk4-smoke}.nix` | Done | Per-contract protocol gates from the wayland-server convergence epic. Real GTK4 client smoke test (`visual-gtk4-smoke`) for cross-toolkit validation. |
| `tests/visual-wallpaper-video.nix` | Done | Epic #12 gate: real `halmasuit-decoder` sandbox + crash-recovery + budget-exhaustion + login-flash continuity under video wallpaper. |
| `tests/full-boot-flash.nix` | Deferred to Phase B | Frame-capture continuity from kernel handoff through `SESSION`; depends on initramfs survival being in place. |
| DankGreeter launcher patch | **Cross-repo** | ~20 lines in DMS (nix-config) to skip its nested-niri spawn when `WAYLAND_DISPLAY` is set by halmasuit. The VM `visual-dankgreeter-auth` gate exercises the module via `${nix-config}/modules/desktop/dms-niri.nix`, so the patch should be additive and small. |
| dms-niri integration on gnomon | **Cross-repo** | Replace `services.greetd.enable` with `services.halmasuit.enable` in gnomon's host config; declare halmasuit-greeter / halmasuit-compositor users. |
| Real-hardware shakedown on gnomon | **Cross-repo** | Boot halmasuit on actual KMS hardware (RTX 5070 Ti / nvidia-drm); will likely surface integration issues VM tests can't see. |

## Phase B: initramfs survival

**Starting state.** The rootfs compositor is done end-to-end — six
in-repo epics shipped, real DMS DankGreeter Qt6 auth arc green
end-to-end. drm-master-probe Phases 0–3 already empirically
validated the load-bearing mechanics Phase B builds on:
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
| Wallpaper plane — initramfs path | The internal wallpaper plane (already composited by `halmasuit` in rootfs) extended to the dumb-buffer KMS write needed in the `INITRAMFS_SPLASH` phase; carried through `ROOTFS_SPLASH` and `SHUTDOWN_SPLASH`. The animated/wgpu "sizzle" path is a polish pass deferred until full-boot-flash is reliably green. |
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

- ~~**PAM bindings strategy.**~~ **RESOLVED:** hand-rolled libpam FFI in `crates/halmasuit-session/src/pam_sys.rs` (Epic #5), following sudo-rs's pattern. Production halmasuit-session links `-lpam` directly with zero bindgen / clang-sys / libclang at build time. Originally in `halmasuit-pam` with `pam-sys` as the binding crate; **superseded twice** — first by the privilege-separation epic (which moved the FFI surface to the privileged `halmasuit-session` broker; `halmasuit-pam` deleted; `r14-gate` enforces the single libpam surface), then by Epic #5 (which replaced the `pam-sys` dependency with the hand-rolled extern block; pam-sys retained as `[dev-dependencies]`-only audit lever via `tests/pam_ffi_parity.rs`).
- ~~**smithay revision pin.**~~ **RESOLVED:** pinned to niri's current git revision (`ff5fa7df...`) in workspace `Cargo.toml`.
- ~~**Build order within "broadly working."**~~ **RESOLVED** (Phase A): introspection sink first, then the smithay spine, then `halmasuit-greetd`, then calloop wiring + DRM master + privilege drop + greeter spawn + login-flash flip.
- ~~**Privilege-drop mechanism + setuid wrapper.**~~ **SUPERSEDED by the privilege-separation epic.** The setuid-`halmasuit-spawn` model is deleted. The compositor drops in-process via `setresgid`/`setresuid` after binding sockets and acquiring DRM master to a post-drop posture of `CapPrm=CapEff={CAP_KILL}` with an empty bounding set (it execs no setuid helper). Privilege-drop+exec exists only in the `halmasuit-session` broker's non-setuid fork-then-drop session-leader child (already root). No `security.wrappers` setuid entry; no `NoNewPrivileges` constraint from a setuid handoff (there is none). See the "Privilege-separation decision record" below and ARCHITECTURE.md "Authentication and session lifecycle".
- ~~**login-flash measurement target.**~~ **RESOLVED:** measures halmasuit's `MainPID` (the long-lived compositor) across greeter→session, replacing the v1 baseline's niri-PID assertion (greetd-architecture-specific). Same assertion intent — no compositor restart — different process under measurement.

### Open

- **`halmasuit-luks` prompt rendering form.** Replace splash with prompt vs. overlay prompt via subsurface composition. *Decision deferred to Phase B (when the adapter lands).*
- **`org.halmasuit.Compositor1` D-Bus surface.** Method list depends on what desktop-environment integration actually needs. *Decision deferred to the D-Bus implementation task.*
- **`org.halmasuit.Debug.Introspect` surface shape.** Single `Snapshot()` method vs. signal-based event stream over D-Bus vs. both. Exact NDJSON schema (surface roles, geometry units, phase enum). Redaction policy for `pam_message` content. *Decision deferred to the snapshot-socket task; the stderr/tracing half is already shipping and is schema-flexible.*
- **OCR in `full-boot-flash` test.** May defer text-leak detection if tesseract bindings prove fiddly. *Decision deferred to Phase B (when full-boot-flash is built).*
- **Initramfs handoff mechanism.** `SurviveFinalKillSignal=yes` is the validated path (drm-master-probe Phase 2); the optional additional `execve` re-pivot to the rootfs-systemd MainPID (Phase 3) is a refinement. *Decision deferred to the Phase B `halmasuit-in-initrd` task — pick the simplest combination that makes `full-boot-flash` go green.*

## Privilege-separation decision record

The locked-in shape of halmasuit's PAM/session boundary. CLAUDE.md
codifies the hard rules; this section records *why* they are the way
they are and the rejected alternatives that must not be revisited.
The amendments (A1–A9) name specific decisions that were made during
the broker epic and stabilized via primary-source research.

### Core invariant

**One `pam_handle_t`, owned by the broker, never split across processes
and never `execve`'d between auth and session.** Credential-passing PAM
modules (`pam_mount` unlocking encrypted `$HOME`, `pam_gnome_keyring`,
`pam_krb5`) require the same handle to span `pam_authenticate` →
`pam_open_session`. greetd's `worker.rs` is the canonical statement of
this; the research is unanimous (OpenSSH, GDM, SDDM). A two-handle
design silently breaks: `$HOME` stays locked, the keyring is empty, no
error surfaces. The "killable, SIGKILL-anytime" property is satisfied
by running `pam_authenticate` in an ephemeral SIGKILL-able
`setrlimit`-bounded fork that reports back to the handle-owning parent
— not by making the handle-owner killable.

### Vulnerability cured

C1: the pre-broker compositor ran each PAM attempt in a detached worker
thread, and libpam has no cancellation point. A malicious greeter
looping CreateSession/CancelSession (including disconnect/reconnect to
reset any per-connection cap) accumulated unbounded uncancellable
workers. `MAX_SESSION_BUILDS_PER_CONNECTION` was per-`Connection` —
reconnect-churn defeats it. The killable-subprocess + global single-slot
model bounds the flood to O(1) regardless of churn, also fixes a hung
network-PAM module wedging a thread forever, and makes CancelSession
actually cancel.

### Locked decisions

1. **Separate worker binary** (`halmasuit-pam-worker`), not re-exec-self
   — keeps smithay/compositor code out of the auth process address space.
2. **Supervisor-owned teardown:** the broker holds a
   `WorkerHandle{pid,pidfd}`; the greetd state machine stays a pure
   `step()` machine that never learns a pid exists. Kill via
   `pidfd_send_signal`, reap via the existing R4 reaper. Process control
   never lives in the pure protocol crate. Teardown is NOT `Drop`-based
   (`Drop` cannot `waitpid` — std and tokio both document this; reaping
   in `Drop` produces zombies).
3. **`SOCK_SEQPACKET` socketpair** for the broker IPC — kernel message
   boundaries; serde-framed typed messages; challenge/response buffers
   `Zeroizing` on both sides; `wire_format_*` roundtrip drift tests pin
   the JSON shape.
4. **No `MAX_SESSION_BUILDS_PER_CONNECTION`.** The single-slot model
   makes it redundant, and its presence implies a defense it doesn't
   provide.
5. **SIGKILL directly, no SIGTERM grace, for the auth worker.** The
   worker is blocked in `pam_authenticate`; it has no session/children
   to wind down (auth-only, no `pam_open_session`). Destroying the
   address space is stronger credential hygiene than zeroize-then-
   continue. greetd's SIGTERM→grace→SIGKILL is for post-auth *sessions*,
   which the auth worker is not.
6. **Single-slot scope: GLOBAL.** One seat, one greeter, one auth at a
   time. Mirrors greetd's single `current` slot. Bounds live workers to
   O(1) across any churn including disconnect/reconnect (the C1 attack)
   with no per-connection counter. "New CreateSession evicts in-flight"
   is the correct single-slot semantic.

### Rejected — DO NOT REVISIT unless the cited condition changes

- **Re-exec-self PAM worker.** Larger auth-process address space; the
  project idiom is a separate microscopic binary.
- **RAII `Drop`-kills-worker teardown.** `Drop` cannot `waitpid` →
  zombies; puts a kill primitive in the pure crate.
- **`cancel()` threaded through the protocol state machine.** Spreads
  process control into the pure protocol crate.
- **SIGTERM→grace→SIGKILL for the auth worker.** That is the session
  teardown pattern; an auth worker has nothing to wind down.
- **Per-connection single-slot.** Reconnect-churn defeats it (the C1
  attack); only GLOBAL is the bound.
- **Pure rate-limiter instead of the structural cure.** A limiter only
  paces it; process-isolation + single-slot eliminates the class.
- **Two-handle / cross-process-handle PAM design.** Silently breaks
  credential-passing modules (locked `$HOME`, no error).
- **Mechanism A: `setns(/proc/1/ns/mnt)` from a sandboxed compositor
  spawning the session.** Needs `CAP_SYS_ADMIN`+`CAP_SYS_CHROOT` in the
  spawner; breaks the forbid-unsafe and minimal-caps posture; inverts
  the UID-floor threat model. The shipped resolution is Mechanism D
  (the deliberately-unsandboxed privileged broker unit in the host
  mount namespace — `run0`/`machinectl`/logind precedent), so this is
  not an open avenue.

### Amendments (the rules CLAUDE.md cites)

- **A1 — env merge.** Session leader's `execve` env is
  `pam_getenvlist()` MERGED with the fixed allowlist, never a blind
  env-replace. Blind replace clobbers pam_env, pam_systemd, pam_mount.
- **A2 — single calloop broker, idle-exit, evict-old slot.** One
  event loop in the broker; socket-activated so there's no standing
  root daemon when idle; a new CreateSession evicts the in-flight one.
- **A4 — atomic deletion of the old path.** The R3 compositor→broker
  relay landing is what deletes `halmasuit-pam` + the setuid
  `halmasuit-spawn` + `MAX_SESSION_BUILDS_PER_CONNECTION`. They are
  not deleted before the replacement lands (breaks the compositor),
  and not left behind after it lands (forbidden two-libpam-surface
  anti-pattern). The deferral pattern ("ship the broker, delete the
  old path later") is RESCINDED.
- **A5 — session lifecycle is one-way broker→compositor.** Lifecycle
  frames exist only in `BrokerToCompositor`, never in
  `CompositorToBroker`. The compositor emits no lifecycle frame; it
  is a pure sink. Structurally kills (a) a compromised greeter
  forging a force-logout, and (b) forging "session ready" to phish
  the post-login surface. The visible greeter→session swap is gated
  on AND(`SessionOpened`, the compositor's own first-non-empty session
  frame) — never `SessionOpened` alone (that reintroduces the flash;
  Mir/USC `is_session_ready_for_display = session && ready`,
  `WindowWlSurfaceRole` first-non-empty-buffer gate). The broker is
  the sole reaper; any SCM_RIGHTS leader pidfd the compositor holds is
  poll-only.
- **A6 — single owner of the broker socket.** The compositor's
  per-greeter-connection `BrokerEpisode` owns the broker
  `SeqpacketChannel` (`OwnedFd`) for the whole episode. Auth state is
  a sans-IO payload-only enum that owns nothing transport. No `dup`,
  `Rc<RefCell>`, or `Arc<Mutex>` of the privilege-crossing fd:
  `dup` reintroduces premature-/no-EOF + a second writer +
  reused-fd double-close (OpenSSH CVE-2015-6563 / CVE-2015-6564 class);
  Rc/Arc reintroduce the premature-close hazard.
- **A7 — fully sans-IO greetd boundary; no blocking IPC on the
  compositor calloop thread.** The compositor never does a blocking
  `recv` on the broker socket — not brief, not with a timeout. A
  timeout only trades unbounded stall for bounded multi-vblank stall
  and adds an `SO_RCVTIMEO`/`EINTR` hazard (Mir hit this exactly;
  upstream fix branch was named `no-ipc-on-compositor-threads`). The
  broker SEQPACKET fd is a per-connection non-blocking calloop source.
  greetd's PAM boundary emits/suspends/resumes (h11 `NEED_DATA` /
  rustls `read_tls`+`process_new_packets` pattern). A synchronous
  `PamSession::step` that does send + blocking recv internally is the
  forbidden sans-IO anti-pattern.
- **A8 — calloop source over a non-owning borrowed-fd newtype.**
  calloop's `Generic<F: AsFd>` takes `F` by value and closes via its
  destructor; the episode is the sole `OwnedFd`, so the calloop source
  is a `Generic` over `struct …Fd(RawFd); impl AsFd { unsafe
  BorrowedFd::borrow_raw }` with no `Drop`. `insert_source`'s bound is
  `S: EventSource + 'l` (not `'static`); a `RawFd`-holding newtype
  satisfies it. Close-exactly-once: the `RegistrationToken` is
  `loop_handle.remove`d BEFORE the episode is dropped — asserted by
  test, not assumed. SEQPACKET + `MSG_DONTWAIT` both directions makes
  the no-block invariant structural; no outbox/Ping/idle machinery is
  needed.
- **A9 — supplementary groups from PAM-resolved user, never the
  handle-owner's `getgroups()`.** The session leader's supplementary
  groups are `getgrouplist(PAM-resolved username, PAM-resolved primary
  gid)` ONLY, computed in the fork-drop child from the PAM-derived
  identity. The handle-owner's own `getgroups()` is NEVER a source.
  Primary sources: `pam_setcred(3)` man page (verbatim: "credentials
  should be established, by the application, prior to a call to this
  function… `initgroups(2)` (or equivalent) should have been
  performed"); OpenSSH `do_setusercontext()`; util-linux `login(1)`
  `init_groups()`; GDM `gdm-session-worker.c`. The opposite ("capture
  the daemon's `getgroups()` and graft it onto the user") is
  CVE-2021-41617 (OpenSSH ≤8.7) / sddm #1159 — a named CVE class. The
  R7/R11 "getgrouplist-MERGE the established supplementary set" clause
  is RESCINDED by A9; `initgroups`-equivalent from the resolved
  identity is the correct, mandated behavior. `pam_group`/`group.conf`
  conditional grants are out of scope under the one-handle-in-parent
  architecture.

### Why a privileged broker (not a sandboxed spawner)

niri (or any real session), as a child of the hardened
`halmasuit.service`, would inherit its `ProtectHome=true` /
`ProtectSystem=strict` / `PrivateTmp=true` mount namespace. `setresuid`
does not change the mount namespace; fork/exec does not escape it.
`/home/$USER` and `/run/user/$UID` — created correctly on the host by
`pam_systemd`/logind — would be invisible to the session. Four
research agents (sshd, systemd-logind/pam_systemd, greetd/GDM/SDDM,
namespace mechanics) converged: `pam_systemd`/logind is NOT a
namespace-escape mechanism (it creates `/run/user/$UID` in the PAM
caller's existing mount namespace; "register with logind" alone does
NOT fix this). Every existing DM/sshd avoids the problem by not
sandboxing the spawner; greetd's own source comments that
`pam_open_session`/`pam_close_session` must run as root in the host
ns. The shipped resolution is Mechanism D: a deliberately-unsandboxed
privileged unit (`halmasuit-session`) that PID 1 socket-activates →
host namespace. Hardened halmasuit asks it, over SO_PEERCRED-
authenticated IPC, only after the greetd state machine reaches
PAM-verified `start_session`; that unit runs the `pam_systemd` tail
and `execve`s the session in the host ns. halmasuit stays fully
hardened; the privileged surface stays tiny and separately auditable;
no `CAP_SYS_ADMIN` anywhere.

## See also

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the design halmasuit implements
- [`RESEARCH.md`](RESEARCH.md) — empirically validated architectural foundations (drm-master-probe Phase 0 + Phase 1)
