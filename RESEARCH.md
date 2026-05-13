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

**Phase 1 — initramfs + `switch_root` survival + privilege drop.** The
same process can be started in initramfs via
`boot.initrd.systemd.services`, mark itself with `@argv[0]` per
[systemd's ROOT_STORAGE_DAEMONS](https://systemd.io/ROOT_STORAGE_DAEMONS/)
convention, survive `switch_root` with master FD intact, call
`setresuid(0 → 1000)` while continuing to hold master, and resume
heartbeating with tick continuity ≤1s gap. Failure mode encountered
along the way was identified as `SIGTERM`-from-rootfs-systemd reaping
the orphan unit — catchable signal, standard daemon engineering, not an
architectural blocker. Test: `tests/drm-master-probe-phase1.nix`. Run:
`just test-drm-probe-phase1`.

**Code:** `crates/drm-master-probe/`
**Interactive (visual paint verification):** `just test-vm-drive drm-master-probe`, `just test-vm-drive drm-master-probe-phase1`

**Validated mechanisms (no upstream patches needed):**

- `@argv[0]` survival via glibc's `__progname_full` data symbol works on
  NixOS's systemd-in-initramfs.
- DRM master is per-fd, not per-uid; `setresuid` to a non-root UID
  retains mastership.
- `IgnoreOnIsolate = true` + `DefaultDependencies = false` keep the
  initramfs systemd unit from being stopped at `initrd-switch-root.target`
  isolation.
- `boot.initrd.kernelModules = [ "virtio_gpu" ]` (and the equivalent for
  the user's actual GPU driver) must be set so `/dev/dri/card0` exists
  in initramfs.

**v2 engineering carryover:**

- Rootfs systemd discovers the orphan service (its unit name lives only
  in the now-dead initramfs systemd) and sends `SIGTERM` ~1 second
  post-`switch_root`. halmasuit-real needs one of:
  - `sd_notify` to register with rootfs systemd as a tracked unit
  - graceful `SIGTERM` handler with proper resource release
  - explicit detachment from systemd's process tracking
- The actual unit-name reconciliation across the boundary is a real
  problem to solve in v2 production, but it has nothing to do with
  whether the kernel-level boundary crossings work — which is what
  the probe proves.
