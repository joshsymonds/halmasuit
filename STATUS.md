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

Implement the halmasuit compositor binary that makes `login-flash` go green.

**The fundamental thing v2 builds:** one long-lived display-server process
that hosts both the greeter and the user session as nested Wayland clients
of itself. greetd is replaced entirely. From the greeter's perspective,
halmasuit IS greetd (same wire protocol).

### Concrete scope

| Crate / file | What it does in v2 |
|---|---|
| `crates/halmasuit` | smithay-based compositor binary. Owns DRM master from `graphical.target` to shutdown. Runs as the `compositor` system user. |
| `crates/halmasuit-greetd` | greetd wire-protocol server. DankGreeter, regreet, tuigreet, gtkgreet — all greetd-compatible greeters work unchanged against this socket. |
| `crates/halmasuit-ipc` | JSON-RPC types for the local control plane. |
| `crates/halmasuit-cli` | `halmasuit msg ...` CLI for status, lock, restart-wm, recovery. |
| `crates/halmasuit-spawn` | Setuid-root privilege-drop helper. ~80 lines. `#![forbid(unsafe_code)]`. Audited like sudo's pre-fork core. |
| `crates/halmasuit-protocols` | Wayland XML + wayland-rs codegen. Hosts our `ext-halmasuit-host-v1` once we add it (v3 territory). |
| `nix/module.nix` (new) | NixOS module `services.halmasuit.enable = true;` replacing greetd, wiring the setuid helper, polkit policies, etc. |
| DankGreeter launcher patch (~20 lines in DMS) | Skip the nested-niri spawn when `WAYLAND_DISPLAY` is already set by halmasuit. |

### v2 done condition

- `just test-vm` runs `login-flash` and it **passes**. The test never
  changed; the system under test did.
- gnomon runs halmasuit as the daily-driver greeter via the NixOS module.
- The greetd→niri visible flash is gone in practice (verifiable by eye and,
  formally, by the green test).

### Open decisions for v2 (per ARCHITECTURE.md)

- **PAM bindings** — `pam-client` and `pam-sys` are stale (2022 and 2023);
  decide on use-as-is / fork / thin FFI when the auth code lands.
- **smithay pin** — track niri's or cosmic-comp's current git revision; not
  bleeding-edge.
- **D-Bus surface** — `org.halmasuit.Compositor1` final method list depends
  on what desktop-environment integration actually needs.

Estimated scope: 4–8 weeks of evening work. Biggest single chunk is the
compositor's own greeter UI — text + button + logo via direct smithay
rendering, since DMS isn't available at this layer.

---

## After v2

Brief sketch; see [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full roadmap.

- **v3** — Direct-scanout optimization. Custom Wayland protocol
  (`ext-halmasuit-host-v1`) + niri client patch. Removes the nesting GPU
  tax; frame latency matches running niri natively.
- **v4** — Initramfs integration. BIOS firmware splash → halmasuit splash →
  greeter → session with zero visible discontinuities. The macOS/Windows
  end state.
- **v5+** — Crash isolation with client preservation, fast user switching,
  HDR + VRR pass-through across nesting, multi-seat, graphical recovery
  mode.

---

## Bookkeeping

- License: Apache-2.0
- Cache: `joshsymonds.cachix.org` — public, read-only consumption from CI.
  Gnomon doesn't currently push to it; setting up auto-push is a small,
  optional productivity step (~90s of browser work for a personal auth
  token).
- The Mir/Lomiri ecosystem proves this architecture works in production
  (Ubuntu Touch / PinePhone since 2013). halmasuit ports it to the
  wlroots/smithay world for desktop Linux.
