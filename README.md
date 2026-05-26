# halmasuit

A Linux **system compositor**: one long-lived Wayland-server process that
owns the GPU from `multi-user.target` through to shutdown and hosts both
the greeter and the user's session (niri) as nested Wayland clients of
itself. Eliminates the visible flash that exists today between greetd's
greeter and the user desktop on every greetd-based Linux setup.

## Status

**v2 in-repo contract complete (Phase A + Phase B). Not yet running
on real hardware.**

The compositor exists and works in NixOS VM tests. The auth/session
path is privilege-separated: a single privileged `halmasuit-session`
broker owns one `pam_handle_t` for the whole auth→session lifecycle,
runs `pam_authenticate` in an ephemeral SIGKILL-able fork, and launches
the session by forking once and dropping privileges in a non-setuid
child. The compositor is an unprivileged sans-IO relay to it; there is
exactly one libpam surface and **no setuid binary in the closure** (the
old in-compositor `halmasuit-pam` and setuid `halmasuit-spawn` are
deleted). `tests/login-flash.nix` is GREEN **through the broker-launched
session**: halmasuit holds its PID and frame continuity across the real
greeter→session transition, the property that proves no restart and
therefore no flash.

**Phase B (initramfs survival) shipped.** halmasuit runs from the
initramfs systemd, survives `switch_root` via
`SurviveFinalKillSignal=yes`, unlocks LUKS volumes via the
`halmasuit-luks` password-agent Wayland client, and reaches paths
in the rootfs view post-pivot via the broker's SCM_RIGHTS root-fd
handoff (the rootfs `chroot` fd travels over the broker socket so
the surviving initramfs process can `fchdir` + `chroot` into the
rootfs mount-ns view). 6/6 visual matrix cells green
(`tests/visual-phase-b-{side,enc}-{image,shader,video}.nix` — 2
LUKS shapes × 3 wallpaper variants); `tests/full-boot-flash.nix`
green; `tests/luks-unlock.nix` green.

`just check` is 377/377; `just test-vm` is a 49-gate sweep.
Empirical foundations for Phase B are in [RESEARCH.md](RESEARCH.md)
(drm-master-probe Phases 0–3 on DRM master persistence across
`setresuid`, fork, and `switch_root` + `execve`).

**What's left before halmasuit boots a real machine** — all
cross-repo, in [nix-config](https://github.com/joshsymonds/nix-config):

- ~20-line DankGreeter launcher patch in DMS: detect halmasuit's
  `WAYLAND_DISPLAY=wayland-0` and skip the nested-niri spawn that
  exists for the greetd model.
- dms-niri integration switchover: replace `services.greetd.enable`
  with `services.halmasuit.enable` +
  `services.halmasuit.fromInitrd.enable` in gnomon's host config;
  declare the halmasuit-greeter and halmasuit-compositor system
  users; remove `boot.plymouth.enable`.
- Real-hardware shakedown on gnomon. Likely to surface integration
  issues VM tests can't see (real KMS instead of virtio-gpu, real
  DankGreeter rendering, real niri-as-session-client).

## Architecture

### Crate map

| Crate | Role |
|---|---|
| `halmasuit` | The compositor binary. smithay-based Wayland server; drives the greetd protocol via calloop; spawns the greeter as a child; relays the auth/session lifecycle to the `halmasuit-session` broker (per-greeter `BrokerEpisode`, non-blocking broker calloop source). Holds no PAM, no credentials, no escalation capability post-drop (keeps only `CAP_KILL`). Runs in both initramfs (via `services.halmasuit.fromInitrd.enable`) and rootfs. |
| `halmasuit-luks` | systemd password-agent Wayland client. Registers with `/run/systemd/ask-password/`, prompts the user for LUKS passphrases via a layer-shell surface over halmasuit's wallpaper plane, returns the answer to systemd-cryptsetup. Runs as root in the initramfs (no userdb yet) — highest-risk Phase B surface because it sees passphrases. |
| `halmasuit-session` | **The single privileged surface.** Host-ns PAM/session broker: one `pam_handle_t` whole-lifecycle, `pam_authenticate` in an ephemeral SIGKILL-able `setrlimit`-bounded fork, non-setuid fork-then-drop session leader, identity independently PAM-re-derived, UID floor in the leader child, socket-activated with idle-exit, relay-peer `SO_PEERCRED` gate. `unsafe` confined to its `pam_ffi`/`worker` modules. |
| `halmasuit-session-ipc` | Pure wire contract (types + codec) for the compositor↔broker relay, incl. the one-way `BrokerToCompositor` session-lifecycle frames. `#![forbid(unsafe_code)]`. |
| `halmasuit-introspect` | Schema-stable `Event` enum + `tracing-subscriber` JSON sink. Every state transition lands in journald as one JSON line. `#![forbid(unsafe_code)]`. |
| `halmasuit-greetd` | Clean-room greetd wire protocol + fully sans-IO state machine + `Listener` (SO_PEERCRED authz, 0660 mode) + per-fd `Connection`. Links no libpam; relays to the broker. `#![forbid(unsafe_code)]`. |
| `halmasuit-vm-client` | Test-only greetd-protocol CLI driver. Used by the VM tests to exercise auth flows deterministically. |
| `drm-master-probe` | Research crate, not v2 production. Validated the empirical foundations of Phase A + Phase B (`DRM_IOCTL_SET_MASTER` survival across `setresuid`, fork, and `switch_root` + exec). |

The remaining workspace members (`halmasuit-protocols`,
`halmasuit-kms`, `halmasuit-ipc`, `halmasuit-cli`, `halmasuit-test`)
are placeholder stubs that will populate as the consuming tasks land.
`halmasuit-fsck` and `halmasuit-emergency` adapters are explicitly
out of scope for v2 (edge-case UX — happy-path mandate).

### Runtime flow

```
multi-user.target → halmasuit.service starts as root
        │
        ├─ open /dev/dri/card0, DRM_IOCTL_SET_MASTER  → Phase::DrmMasterAcquired
        ├─ smithay state, bind /run/halmasuit/wayland-0 → Phase::WaylandReady
        ├─ bind /run/halmasuit/greetd.sock (0660, SO_PEERCRED gated) → Phase::GreetdReady
        ├─ fork+exec the configured greeter as the greeter system user
        │  via Command::pre_exec → setresgid + setresuid (still root)
        ├─ setresgid + setresuid into the compositor system user,
        │  capset permitted=effective={CAP_KILL}, bounding set EMPTY
        │  → Phase::Deprivileged   (no setuid helper is ever exec'd)
        ↓
main loop (calloop) — unprivileged compositor
        │
        ├─ greeter (DankGreeter) connects to wayland-0 + greetd.sock
        ├─ per-greeter BrokerEpisode opens a SEQPACKET to the
        │  socket-activated halmasuit-session broker (SO_PEERCRED both ways)
        │  └─ relays the greetd auth conversation, length-bounded,
        │     sans-IO (compositor never blocks the render loop)
        │
        │   halmasuit-session (privileged, host-ns):
        │     ├─ ONE pam_handle_t: pam_start → authenticate → acct →
        │     │  setcred → open_session → … → close_session → pam_end
        │     ├─ pam_authenticate runs in an ephemeral SIGKILL-able
        │     │  setrlimit-bounded fork (credentials never persist)
        │     ├─ identity re-derived from PAM (pam_get_user → pwent),
        │     │  UID floor enforced; getgrouplist(resolved user) groups
        │     └─ fork ONCE; the non-setuid child drops privileges
        │        (setgroups → setresgid → setresuid) and execve(niri,…)
        │        as the authenticated user, env = pam_getenvlist()+allowlist
        │
        ├─ broker → compositor: SessionOpened / SessionEnded (one-way)
        ├─ two-key swap: AND(SessionOpened, first non-empty session frame)
        │     → pidfd_send_signal(greeter, SIGKILL) → GreeterTerminated
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
                     # cargo-machete + typos + nextest (377)
just test-vm         # NixOS VM gates (49): smoke-boot, halmasuit-vm,
                     # initrd-survival, full-boot-flash, luks-unlock,
                     # visual-initrd-pixmap, visual-phase-b-* (6-cell
                     # matrix), run-pam-auth, session-r5r6,
                     # session-onehandle, login-flash, halmasuit-input,
                     # visual-* — all hard
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
