# halmasuit-vt-probe

Research probe for **Epic #71 Phase 0**: empirically validates whether
the compositor (unprivileged, no `CAP_SYS_TTY_CONFIG`) can drive the
entire VT cooperative-switching state machine — `TIOCSCTTY` +
`VT_SETMODE` + `VT_RELDISP` (both arg variants) — using only an
inherited fd whose source process is the privileged broker.

Not production code. The production VT-switching path lives in
`halmasuit-session` (broker) and `halmasuit` (compositor).

## Verdict

**PASS — broker-passes-fd model is fully viable.**

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

## Implication for Epic #71's broker design

The broker (`halmasuit-session`) only needs to:

1. Open `/dev/ttyN` (it has the necessary group access — root, or
   member of the `tty` group).
2. Pass the fd to the compositor over the existing SCM_RIGHTS-capable
   socket.
3. Receive the compositor's "I dropped DRM master, ack" message.
4. Call `VT_ACTIVATE(target)` — this is the only ioctl the broker
   itself needs to make (and it does require `CAP_SYS_TTY_CONFIG`,
   which the broker holds as part of its capability set).

The compositor handles the rest of the cooperative-switching state
machine entirely in-process:

- `setsid()` after privilege drop.
- `TIOCSCTTY` on the inherited fd.
- `VT_SETMODE PROCESS` with relsig=SIGUSR1, acqsig=SIGUSR2.
- Signal handling via `signalfd` (matches halmasuit's existing
  calloop-driven signal model).
- `VT_RELDISP(VT_ACKACQ)` on SIGUSR2 (kernel switched TO our VT).
- `VT_RELDISP(1)` on SIGUSR1 (kernel switching AWAY from our VT).

No SCM_RIGHTS-back-pass of the fd. No broker-mediated `VT_RELDISP`
calls. The protocol surface between broker and compositor stays
narrow: open + fd-pass + the existing PAM-broker message bus.

## What the probe does NOT validate

- The production broker protocol (the specific message types,
  rate-limiting, timeout enforcement). All of that lands in
  Epic #71's R-series implementation tasks.
- The drop-master-then-VT_ACTIVATE ordering (systemd #21388 lesson).
  That's enforced at the broker; the probe doesn't open a DRM device.
- The watchdog for a compositor that hangs in its SIGUSR1 handler.
  Also a broker-side concern.
- Behavior when `/dev/ttyN` already has another session's controlling
  process (e.g., a getty). The probe runs in a test VM where tty2
  is not actively claimed; the production broker should pick a VT
  number that's documented as available or detect the collision.

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
