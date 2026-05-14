# halmasuit

A Linux **system compositor**: one long-lived Wayland-server process that
owns the GPU from `multi-user.target` through to shutdown and hosts both
the greeter and the user's session (niri) as nested Wayland clients of
itself. Eliminates the visible flash that exists today between greetd's
greeter and the user desktop on every greetd-based Linux setup.

## Status

**v2 Phase A: in-repo contract complete. Not yet running on real
hardware.**

The compositor exists and works in NixOS VM tests. `tests/login-flash.nix`
— the test that empirically measured the flash — is GREEN: it now
holds halmasuit's PID continuous across the greeter→session
transition, which is the architectural property that proves no
restart and therefore no flash. `tests/halmasuit-vm.nix` is the
end-to-end gate (~16 s, six suites): lifecycle events, socket
permissions and ownership, the in-process privilege drop with
capability posture pinned to `CapEff={CAP_KILL}`/
`CapBnd={CAP_SETUID, CAP_SETGID}`, greeter child identity,
Wayland globals, full PAM auth + setuid `halmasuit-spawn`
invocation + greeter SIGKILL on session start, clean shutdown.

**What's left before halmasuit boots a real machine** — all
cross-repo, in [nix-config](https://github.com/joshsymonds/nix-config):

- ~20-line DankGreeter launcher patch in DMS: detect halmasuit's
  `WAYLAND_DISPLAY=wayland-0` and skip the nested-niri spawn that
  exists for the greetd model.
- dms-niri integration switchover: replace `services.greetd.enable`
  with `services.halmasuit.enable` in gnomon's host config; declare
  the halmasuit-greeter and halmasuit-compositor system users.
- Real-hardware shakedown on gnomon. Likely to surface integration
  issues VM tests can't see (real KMS instead of virtio-gpu, real
  DankGreeter rendering, real niri-as-session-client).

**Phase B (initramfs survival) hasn't started.** halmasuit running
from initramfs, surviving `switch_root` with its DRM master fd
intact, replacing Plymouth so the splash and the compositor are
the same process from KMS init onward. The empirical foundations
are validated (drm-master-probe Phases 0–3, documented in
[RESEARCH.md](RESEARCH.md)); production wiring is the next major
milestone.

## Architecture

### Crate map

| Crate | Role |
|---|---|
| `halmasuit` | The compositor binary. smithay-based Wayland server; drives the greetd protocol via calloop; spawns the greeter as a child; invokes `halmasuit-spawn` to launch the user session post-PAM. |
| `halmasuit-introspect` | Schema-stable `Event` enum + `tracing-subscriber` JSON sink. Every state transition lands in journald as one JSON line. `#![forbid(unsafe_code)]`. |
| `halmasuit-greetd` | Clean-room greetd wire protocol + state machine + `Listener` (SO_PEERCRED authz, 0660 mode) + per-fd `Connection`. PAM-abstracted via `PamSessionFactory`. `#![forbid(unsafe_code)]`. |
| `halmasuit-pam` | Real libpam FFI quarantined here. `Pam` RAII handle, conv-callback bridge with zeroize'd response buffers, `PamThread` worker driving the greetd state machine round-by-round. `#![deny(unsafe_code)]` with per-block `#[expect]`. |
| `halmasuit-spawn` | The only setuid-root binary. ≤80 lines, `#![forbid(unsafe_code)]`, audit-grade. UID floor (≥1000) on target users is the load-bearing security property of the privilege split — a compromised compositor cannot use spawn to reach root or any system user. |
| `halmasuit-vm-client` | Test-only greetd-protocol CLI driver. Used by the VM tests to exercise auth flows deterministically. |
| `drm-master-probe` | Research crate, not v2 production. Validated the empirical foundations of Phase A + Phase B (`DRM_IOCTL_SET_MASTER` survival across `setresuid`, fork, and `switch_root` + exec). |

The remaining workspace members (`halmasuit-protocols`, `halmasuit-kms`,
`halmasuit-ipc`, `halmasuit-cli`, `halmasuit-test`) are placeholder
stubs that will populate as the consuming tasks land.

### Runtime flow

```
multi-user.target → halmasuit.service starts as root
        │
        ├─ open /dev/dri/card0, DRM_IOCTL_SET_MASTER  → Phase::DrmMasterAcquired
        ├─ smithay state, bind /run/halmasuit/wayland-0 → Phase::WaylandReady
        ├─ bind /run/halmasuit/greetd.sock (0660, SO_PEERCRED gated) → Phase::GreetdReady
        ├─ fork+exec the configured greeter as the greeter system user
        │  via Command::pre_exec → setresgid + setresuid (still root)
        ├─ shrink bounding to {CAP_SETUID, CAP_SETGID}, KEEP_CAPS,
        │  setresgid + setresuid into the compositor system user,
        │  capset permitted=effective={CAP_KILL} → Phase::Deprivileged
        ↓
main loop (calloop)
        │
        ├─ greeter (DankGreeter) connects to wayland-0 + greetd.sock
        ├─ PAM auth completes in-process (halmasuit-pam)
        │
        ├─ on greetd start_session:
        │     emit Event::SessionRequested
        │     pidfd_send_signal(greeter, SIGKILL) → Event::GreeterTerminated
        │     fork+exec /run/wrappers/bin/halmasuit-spawn
        │         halmasuit-spawn (setuid root):
        │             enforce UID_MIN floor
        │             validate pwent matches argv
        │             setresgid + initgroups + setresuid
        │             execve(niri, …) as the authenticated user
        │
        ├─ niri runs as a nested Wayland client of halmasuit
        │  ↑ same halmasuit PID. No restart. No flash.
        │
        └─ SIGTERM → graceful Shutdown event, exit 0
```

## Build and develop

Requires Nix with flakes enabled.

```bash
nix develop          # rust + cargo tooling + qemu + lints
just check           # rustfmt + clippy -D warnings + cargo-deny +
                     # cargo-machete + typos + nextest (133 tests)
just test-vm         # NixOS VM gates: smoke-boot + halmasuit-vm +
                     # halmasuit-spawn + login-flash (all hard gates)
```

For interactive debugging of a VM test with an attached QEMU
window: `just test-vm-drive <name>`. See `Justfile` for the rest.

## Deeper documentation

- **[ARCHITECTURE.md](ARCHITECTURE.md)** — design, threat model,
  capability posture, roadmap. Read this to understand *why*
  halmasuit looks the way it does.
- **[PLAN.md](PLAN.md)** — implementation plan, current in-scope
  status table, resolved + open decisions.
- **[RESEARCH.md](RESEARCH.md)** — empirically-validated foundations
  (DRM master persistence across `setresuid` and `switch_root` +
  `execve`), via the `drm-master-probe` Phase 0–3 experiments.
- **[CLAUDE.md](CLAUDE.md)** — repo-specific instructions for AI
  agents working in this codebase: hard rules, working conventions,
  ecosystem caveats.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. This matches the Rust-Wayland infrastructure tier
(smithay, wlroots, Weston). The choice to re-derive the greetd
wire protocol locally — rather than depend on upstream `greetd_ipc`,
which is GPL-3.0-only — preserves this posture; see
[`crates/halmasuit-greetd/src/lib.rs`](crates/halmasuit-greetd/src/lib.rs)
for the protocol-spec citation and clean-room rationale.
