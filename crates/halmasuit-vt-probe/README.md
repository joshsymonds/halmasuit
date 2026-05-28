# halmasuit-vt-probe

Research probe for **Epic #71 Phase 0**: empirically validates whether
the compositor (unprivileged, no `CAP_SYS_TTY_CONFIG`) can drive the
entire VT cooperative-switching state machine — `TIOCSCTTY` +
`VT_SETMODE` + `VT_RELDISP` (both arg variants) — on a VT fd it owns.

Not production code. The production VT-switching path lives in
`halmasuit`'s `vt_switch.rs` (the home-VT model — R-honest.7).

## Verdict

**PASS — an unprivileged compositor can own its VT and drive the
cooperative handshake without `CAP_SYS_TTY_CONFIG`.**

Captured in NixOS test VM (`tests/vt-probe-phase0.nix`) running:

- **Kernel**: Linux 6.18.29 (NixOS unstable, May 2026)
- **Compositor user**: `halmasuit-compositor` (uid 998, gid 998)
- **Bounding set at test time**: empty (no `CAP_SYS_TTY_CONFIG`, no
  anything — verified by the probe before the load-bearing calls)

All four load-bearing kernel calls succeeded without
`CAP_SYS_TTY_CONFIG`:

| ioctl | Required permission | Path satisfied via | Result |
|---|---|---|---|
| `TIOCSCTTY` | Session leader, no controlling TTY | `setsid()` immediately before | ✅ success |
| `VT_SETMODE PROCESS` | `perm = 1` (controlling TTY or cap) | TIOCSCTTY made tty the controlling TTY | ✅ success |
| `VT_RELDISP(VT_ACKACQ)` | `perm = 1` | same | ✅ success |
| `VT_RELDISP(VT_RELDISP_PERMIT)` | `perm = 1` | same | ✅ success |

The kernel's `perm` check in `drivers/tty/vt/vt_ioctl.c`:
```c
if (current->signal->tty == tty || capable(CAP_SYS_TTY_CONFIG))
    perm = 1;
```
The first arm matches once `TIOCSCTTY` makes the inherited fd's TTY
the calling process's controlling TTY. The cap is not required.

## Implication: the home-VT model

This result is the empirical foundation of the production **home-VT
model** (`halmasuit/src/vt_switch.rs`, R-honest.7): because the
unprivileged compositor can drive `TIOCSCTTY`/`VT_SETMODE`/`VT_RELDISP`
itself, it owns its own ("home") VT directly and the broker has NO VT
role at all.

The compositor handles the entire cooperative-switching state machine
in-process:

1. Open `/dev/tty<home>` in its **root startup window** (the same
   window that opens the DRM master fd), then `setsid()` + `TIOCSCTTY`
   + `VT_SETMODE PROCESS` + `VT_ACTIVATE` to bring it to the
   foreground. The controlling-TTY designation survives the privilege
   drop, so the VT ioctls keep working unprivileged thereafter (this
   probe's result).
2. A switch is a local `VT_ACTIVATE(target)` — the target VT (a getty)
   is never grabbed.
3. relsig/acqsig are **realtime signals** (`SIGRTMIN+4`/`+5`, not
   SIGUSR1/2 — those are stolen by Mesa/EGL threads, freedesktop
   #87322), delivered on a dedicated `signalfd` calloop source.
   relsig → `drm.pause()` + `VT_RELDISP(release)`; acqsig →
   `drm.resume()` + `VT_RELDISP(VT_ACKACQ)`.

The broker is not involved in VT switching. Liveness is enforced by a
systemd watchdog (`WatchdogSec` + `sd_notify` from the calloop loop), so
a hung compositor is SIGKILLed by systemd and the kernel's `reset_vc`
reverts the VT to `VT_AUTO` — never a broker-side concern.

## What the probe does NOT validate

- The drop-master-then-`VT_RELDISP` ordering (systemd #21388 lesson).
  The probe doesn't open a DRM device; the production
  `handle_vt_relsig` enforces it (pause master, then ack).
- The getty-collision behavior the home-VT model exists to avoid:
  grabbing a getty's VT gets the fd revoked by `vhangup()`. The
  probe runs against tty2 in a test VM; production owns a getty-free
  home VT (`HALMASUIT_HOME_VT`, e.g. tty8).

## How to run

The probe is gated behind a NixOS VM test, not behind `just check`'s
unit-test sweep. Same shape as the other research probes:

```sh
nix build .#checks.x86_64-linux.vt-probe-phase0
```

The test sets up a single-user NixOS VM, invokes the probe under
`systemd-run` as a transient unit, then drives VT switches from the
test driver. The probe logs to `/tmp/vt-probe.log`; the test asserts
a `VERDICT:` line.

## Lineage

This probe joins three siblings in the halmasuit research-probe
tradition:

- `drm-master-probe` (Epic #2 / #4 / #5): DRM master persistence
  across the initramfs→rootfs pivot.
- `halmasuit-shutdown-probe` (Epic #47 R2): same-PID + DRM master
  survival through systemd-shutdown's pivot to `/run/initramfs`.
- `halmasuit-vt-probe` (Epic #71): inherited-fd VT-switching without
  `CAP_SYS_TTY_CONFIG`.

Each probe answers a single load-bearing kernel-API question with a
minimal binary + NixOS VM test, before the production code commits
to the design assumption the probe verifies.
