# Research

Index of empirical research artifacts in this repository. Each probe
below is a runnable proof of an architectural claim halmasuit's v2 design
depends on. The probes exist to *settle* questions of "is this even
possible?" before sinking implementation weeks into v2's compositor.

These artifacts are **not production code**.

- They are not gated by `just check` (run explicitly).
- They have not been through `gambit:review` or any formal review.
- Their assertions mirror the architectural questions that motivated
  them, not the contracts halmasuit production code will need.
- The probe binaries are intentionally separate from the `halmasuit-*`
  crates and must not be code-lifted into v2 (see each crate's epic
  anti-patterns).

They are kept in the repository because:

1. **Runnable proofs.** When doubt about the v2 premise arises, running
   the probe re-establishes ground truth in seconds. Much stronger
   signal than rereading design prose.
2. **Pattern reference.** v2 production code (halmasuit-kms, etc.) is
   written from scratch but against the patterns these probes validated.
3. **Regression canary.** A future nixpkgs / kernel / systemd update
   that breaks one of these probes would surface a real change in the
   kernel/userspace contract halmasuit's architecture rests on — well
   before a v2 user would notice.

## Probes

### `drm-master-probe` — DRM master persistence across the boot pipeline

**Question:** Can a userspace process hold DRM master continuously from
initramfs through `switch_root`, through a privilege drop, into the rootfs
userspace lifetime — with no logind brokerage and no contention?

**Result:** Yes, empirically verified end-to-end on the QEMU
`virtio-gpu-pci` substrate.

**Phase 0 — rootfs-direct master holding.** A userspace process can take
DRM master at boot (without logind) and hold it through
`multi-user.target` with no kernel, systemd, or logind contention.
Test: `tests/drm-master-probe.nix`. Run: `just test-drm-probe`.

**Phase 1 — initramfs + `switch_root` survival via `@argv[0]` + privilege
drop.** The same process can be started in initramfs via
`boot.initrd.systemd.services`, mark itself with `@argv[0]` per
[systemd's ROOT_STORAGE_DAEMONS](https://systemd.io/ROOT_STORAGE_DAEMONS/)
convention, survive `switch_root` with master FD intact, call
`setresuid(0 → 1000)` while continuing to hold master, and resume
heartbeating with tick continuity ≤1s gap. Failure mode encountered
along the way was identified as `SIGTERM`-from-rootfs-systemd reaping
the orphan unit — catchable signal, standard daemon engineering, not an
architectural blocker. Test: `tests/drm-master-probe-phase1.nix`. Run:
`just test-drm-probe-phase1`.

**Phase 2 — same as Phase 1 but using `SurviveFinalKillSignal=yes`.**
Tests whether systemd's v255+ unit directive (in the `[Unit]` section,
not `[Service]` — see "gotchas" below) is a viable replacement for the
`@argv[0]` convention. With the probe binary running
`PROBE_SKIP_ARGV0_MARK=1` (no `__progname_full` write) and the unit
declaring `SurviveFinalKillSignal=yes`, the process passes the same
setresuid + master-held + tick-continuity assertions Phase 1 covers.
Test: `tests/drm-master-probe-phase2.nix`. Run: `just test-drm-probe-phase2`.

**Phase 3 — `execve` across `switch_root` with DRM fd preservation.**
Tests whether halmasuit-in-initramfs can `execve` into the
rootfs-resident binary path BEFORE `switch_root`, with the DRM master
fd surviving the exec. The probe pre-exec branch takes master + paints,
then execs into `/sysroot/nix/.ro-store/<own hash>/bin/drm-master-probe`
with `FD_CLOEXEC` cleared on the DRM fd; the post-exec branch wraps the
inherited fd as a Card, re-issues `SET_MASTER` (idempotent on an fd
that already holds master), re-derives CRTC/FB/connector handles via
`GET_CRTC` + `resource_handles`, and continues the normal Phase 1/2
flow. Composes WITH a survival mechanism (Phase 3 uses
`SurviveFinalKillSignal=yes`) — `execve` preserves PID, so the killall
still applies regardless. Test: `tests/drm-master-probe-phase3.nix`.
Run: `just test-drm-probe-phase3`.

**Code:** `crates/drm-master-probe/`
**Interactive (visual paint verification):** `just test-vm-drive drm-master-probe`, `just test-vm-drive drm-master-probe-phase1`, `just test-vm-drive drm-master-probe-phase2`, `just test-vm-drive drm-master-probe-phase3`

**Production mechanism recommendation:**

halmasuit production should use **`SurviveFinalKillSignal=yes`** as the
survival mechanism (Phase 2). It's the systemd-upstream-supported path.
`@argv[0]` works today (Phase 1) but the
[systemd ROOT_STORAGE_DAEMONS](https://systemd.io/ROOT_STORAGE_DAEMONS/)
documentation is explicit that the convention applies to "storage
technology only, not to daemons with any other (non-storage related)
purposes" — halmasuit is non-storage. Recent regressions
([systemd #37700](https://github.com/systemd/systemd/issues/37700),
[#40933](https://github.com/systemd/systemd/issues/40933)) suggest the
convention may legitimately tighten in future systemd releases.

**Optional layer: `execve` for clean handoff (Phase 3).** Stack on top
of `SurviveFinalKillSignal=yes` to get post-pivot canonical binary
paths and a foundation for `sd_notify` registration with rootfs systemd
— avoiding the orphan-unit `SIGTERM` reaper that Phase 1/2 currently
handles with a signal-ignore handler. Not required for v2 Phase A;
landing-target for v3 or later.

**Validated mechanisms (no upstream patches needed):**

- `SurviveFinalKillSignal=yes` in the unit's `[Unit]` section keeps the
  process alive across `systemd-shutdown`'s killall at the `pivot_root`
  boundary. systemd implements this via the cgroup xattr
  `user.survive_final_kill_signal`, set by PID 1 when it sees the
  directive.
- `@argv[0]` survival via glibc's `__progname_full` data symbol works on
  NixOS's systemd-in-initramfs (Phase 1) — but is constrained-use per
  upstream policy.
- `execve` from initramfs binary to rootfs binary path preserves the
  DRM master fd when `FD_CLOEXEC` is cleared. PID is preserved by
  `execve`. Mastery is per-fd at the kernel level, not per-process-image.
  Re-issuing `SET_MASTER` on the same fd post-exec is idempotent.
- DRM master is per-fd, not per-uid; `setresuid` to a non-root UID
  retains mastership.
- `IgnoreOnIsolate = true` + `DefaultDependencies = false` keep the
  initramfs systemd unit from being stopped at `initrd-switch-root.target`
  isolation. (Separate concern from the killall — these prevent PID 1
  from *stopping* the unit; the killall is `systemd-shutdown` doing
  process-level cleanup.)
- `boot.initrd.kernelModules = [ "virtio_gpu" ]` (and the equivalent for
  the user's actual GPU driver) must be set so `/dev/dri/card0` exists
  in initramfs.

**Gotchas surfaced by the empirical work:**

- **`SurviveFinalKillSignal=` belongs in `[Unit]`, not `[Service]`.**
  systemd's `load-fragment-gperf.gperf.in` registers it as
  `Unit.SurviveFinalKillSignal`. A misplaced directive is silently
  dropped with a `journalctl` warning `Unknown key 'SurviveFinalKillSignal'
  in section [Service], ignoring` — the unit proceeds as if the
  directive were absent. The Phase 2 + Phase 3 testScripts assert the
  absence of this warning BEFORE asserting any survival behavior, so
  the failure mode is impossible to misread as success.
- **The `SIGTERM` rootfs systemd sends ~1s post-`switch_root`** is a
  *separate* concern from the killall. Rootfs systemd discovers the
  orphan unit (its name lives only in the now-dead initramfs systemd)
  and tries to stop what it doesn't recognize. This SIGTERM is
  independent of which survival mechanism kept the probe alive during
  the killall.
- **NixOS's initramfs view of the rootfs nix store** is
  `/sysroot/nix/.ro-store/` and `/sysroot/nix/.rw-store/` (the
  read-only and writable layers). The overlay `/sysroot/nix/store` is
  composed by rootfs systemd POST-pivot. Pre-pivot, paths into the
  rootfs binary must target `.ro-store` directly. Post-pivot,
  `/proc/<pid>/exe` shows whichever path the exec was resolved through
  — typically the `.ro-store` path because the overlay didn't exist
  when exec ran.
- **`after = "sysroot.mount"` is insufficient** if you need the rootfs
  nix store available. Use `requires = "initrd-fs.target"; after =
  "initrd-fs.target"` — the systemd-documented "all rootfs filesystem
  mounts complete" target.

**v2 engineering carryover:**

- Production halmasuit uses `SurviveFinalKillSignal=yes` (Phase 2) as
  the survival mechanism — systemd-upstream-supported, no
  storage-only policy risk.
- Whether to ALSO use `execve` (Phase 3) for clean rootfs-systemd
  handoff is a v3 polish decision. Phase A ships without it; the
  orphan-unit `SIGTERM` is handled via a graceful handler, same as
  Phase 1/2 baseline.
- Rootfs systemd discovers the orphan service (its unit name lives only
  in the now-dead initramfs systemd) and sends `SIGTERM` ~1 second
  post-`switch_root`. halmasuit-real needs one of:
  - `sd_notify` to register with rootfs systemd as a tracked unit
    (requires Phase 3 exec, since the post-exec process can start with
    a clean rootfs-systemd-known identity)
  - graceful `SIGTERM` handler with proper resource release
  - explicit detachment from systemd's process tracking
- The actual unit-name reconciliation across the boundary is a real
  problem to solve in v2 production, but it has nothing to do with
  whether the kernel-level boundary crossings work — which is what
  the probes prove.

## Phase 4 — libseat/seatd session survival across `setresuid`

**Question.** Epic layer E (`#11`) adopts libseat for input. The
canonical smithay/anvil libseat pattern makes `Session::open()`
REPLACE halmasuit's self-acquired `SET_MASTER`: seatd (a tiny root
daemon) brokers the DRM + input fds and *owns* DRM master. This
inverts the model Phases 0–3 validated. Does a seatd-brokered libseat
session — DRM master, libinput device fds, session-active — survive a
process's `setresuid` to a non-root uid?

**Probe.** `drm-master-probe --features phase4` (`PROBE_PHASE=seatd`),
`tests/drm-master-probe-phase4.nix`. Starts as root, `LibSeatSession::
new()` (seatd backend, `LIBSEAT_BACKEND=seatd`), `session.open()` the
DRM node, master-only modeset (`set_crtc`), libinput via
`LibinputSessionInterface<LibSeatSession>` + `udev_assign_seat`, then a
BARE `setresuid(0→1000)` (zero retained caps — strictly stricter than
halmasuit's `CAP_KILL`-retaining drop, so a pass is a-fortiori valid
for halmasuit), then re-assert: master-only `set_crtc` again, an
injected keystroke through libinput, `session.is_active()`.

**Result — PASS.** Post-`setresuid(→1000)`:
`phase4 post-drop: master=OK input=OK active=true`. The master-only
`set_crtc` succeeds on the seatd-brokered fd after the drop; an
injected keystroke (`KeyCode(38)`) is delivered by libinput after the
drop; the session stays active. seatd logged the client connecting as
uid 0 and being added to seat0 *before* the drop; the brokered fds and
the seatd socket connection are unaffected by the subsequent uid
change (they are connection-/fd-scoped, not re-authorized per-op).

**Key architectural finding for `#11`.** Under libseat, DRM master is
held by **seatd**, NOT by the compositor. `/sys/kernel/debug/dri/0/
clients` shows `seatd` as master (`y`); the compositor never appears
as debugfs-master and never issues `SET_MASTER` itself. This is the
intended, *improved* privilege posture: halmasuit no longer needs the
DRM-master ioctl nor (for devices) its own root window — seatd is the
only root component touching raw devices. The Phase-0–3
"is-the-probe-PID-debugfs-master" assertion is therefore the WRONG
check for the libseat model; the correct master proof is a master-only
ioctl on the brokered fd succeeding (which it does, before and after
the drop).

**Directive for `#11` (production rewire).**
- Replace `open_and_set_master()` with `LibSeatSession` +
  `session.open()` for the DRM node; do NOT call drm-rs
  `acquire_master_lock()` anymore (seatd owns master).
- libinput via `LibinputSessionInterface<LibSeatSession>` +
  `udev_assign_seat(session.seat())`; insert the
  `LibSeatSessionNotifier` and `LibinputInputBackend` as calloop
  sources.
- halmasuit may keep starting as root (to reach the seatd socket and
  to bind sockets under `/run/halmasuit`) and `setresuid` to the
  compositor uid afterwards exactly as today — the libseat session
  survives that drop. Force `LIBSEAT_BACKEND=seatd` (no logind
  session exists for a system service; removes autodetect ambiguity).
- NixOS module: `services.seatd.enable = true`. The compositor
  connects to the seatd socket while still root, before the drop.
- This does NOT regress the UID-floor / privilege split: seatd is the
  sole root device broker; halmasuit gains no new privileged-open
  surface and in fact sheds the `SET_MASTER` one. drm-master-probe
  Phases 0–3 (self-master across `switch_root`) remain the validated
  basis for the *initramfs* survival path; Phase 4 covers the
  *rootfs* libseat model layer E actually uses.

The lean Phases 0–3 probe closure is unchanged: Phase 4's
smithay/libseat/libinput deps are behind the `phase4` cargo feature
and only the separate `drm-master-probe-phase4` package/test build
with it.
