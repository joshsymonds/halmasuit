# Status

**As of 2026-05-12.** halmasuit v1 is complete. The project's measurement
instrument — a CI gate that detects the greeter→session compositor flash on
the real desktop stack — ships and works as designed. v2 (the compositor
itself) starts next.

---

## v1 — Complete

Test infrastructure that validates the flash exists on the current stack.

### What ships

- Cargo workspace with 8 placeholder crates for v2 — all `unimplemented!()` /
  empty libs, no compositor code.
- Nix flake + plain `pkgs.mkShell` dev environment, `joshsymonds.cachix.org`
  configured as a read-only substituter.
- Justfile gates: `check`, `lint`, `test`, `test-vm`, `test-vm-drive`
  (agent-drivable interactive VM with QEMU window + FIFO command file).
- GHA workflow on `ubuntu-24.04`, all actions pinned to commit SHAs, cargo
  registry + rustup + target cached between runs.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the full v1→v5+ design, security
  threat model, and anti-patterns.
- `tests/smoke-boot.nix` — substrate health check. Boots `dms-niri` from
  nix-config, asserts the greeter process chain is alive. Passes.
- `tests/login-flash.nix` — **RED by design.** Drives greetd's wire protocol
  from a custom test greeter, captures niri PID + UID before and after
  login, asserts continuity. Fails today (greetd kills the greeter and execs
  fresh niri for the user). The failure message names both PIDs explicitly.
  v2 makes it green.
- `tests/lib/test-user.nix` — shared "intentionally insecure test user"
  module so future tests don't copy-paste the password-less-sudo posture.

### What v1 deliberately does NOT do

- Any compositor code. All binary crates are `unimplemented!()`.
- Visual frame assertions. Deferred — virtio-gpu-pci doesn't expose a GBM
  allocator for niri's smithay backend, so headless screenshots are black.
  PID tracking is graphics-independent and sufficient for the flash signal.
- Initramfs integration, direct scanout, crash isolation, multi-seat, HDR,
  VRR, OpenTelemetry export — all deferred per ARCHITECTURE.md roadmap.

### Where the work lives

- GitHub: <https://github.com/joshsymonds/halmasuit>
- Local: `~/Personal/halmasuit`

### How to verify v1 yourself

```bash
cd ~/Personal/halmasuit
nix develop
just check      # lint + unit. Clean.
just test-vm    # smoke-boot passes; login-flash fails as designed;
                # wrapper translates expected-fail to exit 0.
```

### How it was validated

- `gambit:executing-plans` drove three subtasks to completion (#11, #12, #13).
- `gambit:review` dispatched four reviewers (conformance, security, quality,
  performance) followed by a dedicated verifier sub-agent. 19 candidate
  findings → 18 confirmed, 1 refuted, 0 gaps. All 18 confirmed improvements
  were implemented in commit `48ef130` before approval.

---

## v2 — Next

Implement the halmasuit compositor binary that makes `login-flash` go green
**and** makes the whole boot sequence flash-free from kernel handoff to
session start.

**The fundamental thing v2 builds:** one long-lived halmasuit process,
started in the initramfs, surviving `switch_root` via re-exec, persisting
until shutdown. Owns DRM master continuously from initramfs through to
power-off. Hosts the splash, the greeter, niri, and the lock screen — each
as a single foreground `wl_client`. Replaces **both Plymouth and greetd**.

Earlier drafts of the roadmap split this into v2 (greetd replacement) and
v4 (initramfs integration). They are one milestone because splitting them
is what causes the boot flash — shipping a v2 that only exists after
`graphical.target` would be a worse Plymouth and solve nothing of the
project's stated mission.

### Concrete scope

| Crate / file | What it does in v2 |
|---|---|
| `crates/halmasuit` | smithay-based compositor binary. Runs in both initramfs (as root) and rootfs (as `compositor` user). Owns DRM master continuously from `INITRAMFS_SPLASH` to `SHUTDOWN_SPLASH`. Re-execs itself across `switch_root`. |
| `crates/halmasuit-kms` | DRM/KMS direct-scanout core. Open device, become master, atomic modeset, primary plane management. Handles `simpledrm` → real-KMS driver migration. |
| `crates/halmasuit-splash` | GPU-accelerated `wl_client` doing shader-driven rendering via Vulkan (`wgpu` or `ash`). Runs continuously from `INITRAMFS_SPLASH` through to power-off as the system's visual signature — fading behind the greeter, behind the lock screen, returning to foreground at shutdown. See ARCHITECTURE.md "Visual identity" for the aesthetic ambition; not "a logo on a background." Ships ~100 MB of Mesa+Vulkan into the initramfs to make this possible. |
| `crates/halmasuit-luks` | systemd password-agent adapter, runs as `wl_client` during initramfs. Required for any encrypted-rootfs system to boot through halmasuit without dropping to TTY. |
| `crates/halmasuit-greetd` | greetd wire-protocol server (in-process). DankGreeter, regreet, tuigreet, gtkgreet — all greetd-compatible greeters connect unchanged. |
| `crates/halmasuit-spawn` | Setuid-root privilege-drop helper. ~80 lines. `#![forbid(unsafe_code)]`. Audited like sudo's pre-fork core. |
| `crates/halmasuit-protocols` | Wayland XML + wayland-rs codegen. Hosts our `ext-halmasuit-host-v1` once we add it (v3 territory). |
| `crates/halmasuit-ipc` | JSON-RPC types for the local control plane. |
| `crates/halmasuit-cli` | `halmasuit msg ...` CLI for status, lock, restart-wm, recovery. |
| `nix/module.nix` (new) | NixOS module wiring halmasuit into **both** the initramfs (`boot.initrd.systemd.services.halmasuit`) and the rootfs (`systemd.services.halmasuit`). Replaces Plymouth AND greetd. Installs the setuid helper, polkit policies, PAM service `halmasuit`, `compositor` and `greeter` system users. |
| DankGreeter launcher patch (~20 lines in DMS) | Skip the nested-niri spawn when `WAYLAND_DISPLAY` is already set by halmasuit. |
| `tests/full-boot-flash.nix` (new) | Frame-capture from kernel handoff through `SESSION` phase, asserts no all-black frame and no DSSIM jump above threshold across any transition. |

Explicitly **deferred** from v2 (per ARCHITECTURE.md "Out of scope"):

- `halmasuit-fsck` and `halmasuit-emergency` adapters (same pattern as
  `halmasuit-luks`; can be added incrementally after v2 lands).
- Direct-scanout optimization (v3).
- True client preservation across niri crashes (explicitly never).
- Multi-seat, HDR, VRR pass-through.

### v2 done condition

- `just test-vm` runs `login-flash` and it **passes**.
- `just test-vm` runs `full-boot-flash` and it **passes**.
- gnomon boots with halmasuit and with both Plymouth and greetd
  removed from the system entirely.
- Verifiable by eye: no flash from kernel handoff through to session.

### Open decisions for v2 (per ARCHITECTURE.md)

- **PAM bindings** — `pam-client` and `pam-sys` are stale (2022 and 2023);
  decide on use-as-is / fork / thin FFI when the auth code lands.
- **smithay pin** — track niri's or cosmic-comp's current git revision; not
  bleeding-edge.
- **`switch_root` re-exec mechanism** — same-process `execve` with fd
  inheritance vs. fork+exec with `SCM_RIGHTS`. Decision when the NixOS
  initramfs unit is wired.
- **`halmasuit-luks` UI form** — replace splash with prompt vs. overlay
  prompt via subsurface. Decide when the adapter is implemented.
- **D-Bus surface** — `org.halmasuit.Compositor1` final method list
  depends on what desktop-environment integration actually needs.

Estimated scope is larger than the previous v2-without-initramfs framing:
likely 8–16 weeks of evening work. The risky part is the boundary work
(switch_root survival, simpledrm migration, DRM-master-across-privilege-
drop); the integration work (greetd protocol, PAM, smithay setup) is mostly
lifting/adapting existing patterns from greetd, anvil, and niri.

---

## After v2

Brief sketch; see [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full roadmap.

- **v3** — Direct-scanout optimization. Custom Wayland protocol
  (`ext-halmasuit-host-v1`) + niri client patch. Removes the nesting GPU
  tax; frame latency matches running niri natively.
- **v4 and beyond** — Graceful crash recovery (halmasuit survives niri
  death, paints recovery UI; apps not preserved). Fast user switching.
  Multi-seat. HDR/VRR. Screen casting infrastructure. `halmasuit-fsck`
  and `halmasuit-emergency` adapters if not already shipped post-v2.

**Explicitly not pursued:** *true* client preservation across niri
restart. Would require forking niri into a non-compositor policy daemon;
cost not justified by the gain.

---

## Bookkeeping

- License: Apache-2.0
- Cache: `joshsymonds.cachix.org` — public, read-only consumption from CI.
  Gnomon doesn't currently push to it; setting up auto-push is a small,
  optional productivity step (~90s of browser work for a personal auth
  token).
- The Mir/Lomiri ecosystem proves this architecture works in production
  (Ubuntu Touch / PinePhone since 2013). Valve's gamescope (the
  Steam Deck compositor, MIT-licensed) proves the "thin compositor
  hosting one foreground client" model at consumer-device scale.
  Halmasuit borrows the structural shape from both, ports it to
  Rust/smithay, and applies it to the desktop boot pipeline that
  neither targets — the explicit goal is to eliminate the process
  boundary at `graphical.target` that even gamescope-on-Steam-Deck still
  has (where Plymouth and gamescope are separate processes with a
  visually-masked but structurally-real handoff).
