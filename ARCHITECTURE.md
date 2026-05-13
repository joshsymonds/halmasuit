# Halmasuit

A Linux **system compositor** — one long-lived display-server process that owns
the graphics hardware from the moment the user-space graphical stack starts
until the system shuts down, hosting a normal Wayland window manager (niri)
and its shell (DMS) as nested clients. Eliminates the visible flash that
exists today between greetd's greeter and the user's desktop session.

---

## Mission

Make the Linux desktop login experience feel like Windows or macOS:
continuous visual presentation from graphical-target up, with the same
process owning the display across the greeter→session boundary so there is
no black frame, no compositor restart, no visible discontinuity.

The full ambition is BIOS → kernel → initramfs → greeter → desktop visual
continuity, the way macOS goes from Apple logo to login window to desktop
without a single flash. That requires moving halmasuit into the initramfs
and surviving `switch_root`, which is real engineering work parked for a
later milestone. The first milestone is the much smaller and still-novel
problem of making the **greeter → session** transition seamless.

---

## The problem we are solving

Today on a standard Linux desktop:

1. greetd execs the configured greeter binary (`tuigreet`, `regreet`,
   `gtkgreet`, or — in this user's case — `dms-greeter`).
2. That greeter binary typically spawns its own nested Wayland compositor
   (niri / hyprland / labwc) to host its UI as a Wayland client.
3. User types credentials. greetd performs PAM. On success, greetd kills
   the greeter (and its nested compositor), then execs the user's
   session — which spawns *another* fresh compositor.

Two compositor processes run sequentially with a hard kill between them.
The DRM master is released and re-acquired. The framebuffer is cleared.
The user sees a visible black flash, sometimes punctuated by leaked kernel
text on the fbcon console.

This is structural, not a bug. Every greetd-compatible setup on Linux has
this property. No widely deployed Linux desktop avoids it.

Windows and macOS avoid it by having a long-lived system-level compositor
(DWM and WindowServer respectively) that hosts both the login UI and the
user shell as different "desktops" within the same compositor process. The
compositor outlives every shell. Linux has nothing comparable in shipping
form.

---

## High-level architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  systemd (PID 1)                                                │
│   └─ halmasuit.service                                          │
│      └─ halmasuit (as 'compositor' user, holds DRM master)      │
│         ├─ Wayland server   /run/halmasuit/wayland-0            │
│         ├─ greetd-compat    /run/halmasuit/greetd.sock          │
│         ├─ control IPC      /run/halmasuit/control.sock         │
│         ├─ D-Bus consumer   → logind (TakeDevice, sleep events) │
│         ├─ D-Bus server     → org.halmasuit.Compositor1         │
│         └─ child lifecycle:                                     │
│             ├─ PHASE greeter   → DankGreeter / regreet / …      │
│             │                    (as 'greeter' user, Wayland)   │
│             ├─ PHASE session   → niri (as authenticated user)   │
│             │                    └─ DMS, terminals, browser, …  │
│             └─ PHASE locked    → ext-session-lock-v1 client     │
└─────────────────────────────────────────────────────────────────┘
```

Halmasuit replaces greetd entirely. The compositor itself is a system
service running as a dedicated `compositor` system user. It owns DRM
master via logind from the moment graphical.target is reached, and never
exits until shutdown.

---

## The greeter integration

The most important architectural decision: **halmasuit speaks greetd's
wire protocol** ([reference](https://man.sr.ht/~kennylevinsen/greetd/protocol.md))
as a server. Every existing Wayland greeter that greetd already speaks to
(DankGreeter, regreet, tuigreet, gtkgreet, agreety) connects to halmasuit
unchanged. From the greeter's perspective, halmasuit IS greetd.

Wire types and the JSON codec come from upstream — `halmasuit-greetd`
depends on the published [`greetd_ipc`](https://crates.io/crates/greetd_ipc)
crate maintained alongside greetd itself. What we own is the daemon-side
logic: the state machine, PAM glue, and the integration points that swap
halmasuit's foreground Wayland client when auth succeeds. We do not
re-derive the protocol from the spec text — reusing greetd's own types
means we track protocol additions automatically and inherit no drift
bugs. Reusing greetd-the-daemon itself as a library is not feasible: its
privilege model (run as root, `execve` the user session) is incompatible
with halmasuit's (run as `compositor` user, never exec, delegate UID
switching to `halmasuit-spawn`).

Flow:

1. halmasuit launches the configured greeter binary with environment:
   - `WAYLAND_DISPLAY=halmasuit-0` (points at halmasuit's Wayland socket)
   - `GREETD_SOCK=/run/halmasuit/greetd.sock`
   - `XDG_RUNTIME_DIR=/run/user/<greeter-uid>`
2. Greeter binary renders its UI as a Wayland client of halmasuit.
3. User submits credentials. Greeter sends `create_session { username }`
   over the greetd socket.
4. Halmasuit invokes PAM in-process via FFI to libpam. PAM challenges
   flow back to the greeter via `auth_message` and return through
   `post_auth_message_response`.
5. On PAM success, the greeter sends `start_session { cmd: ["niri"] }`.
6. Halmasuit terminates the greeter, invokes `halmasuit-spawn` (the
   setuid privilege-drop helper), niri is execed as the authenticated
   user.
7. Halmasuit's Wayland server now hosts niri as its sole foreground
   client.

DMS patch needed: roughly twenty lines in the `dms-greeter` launcher
script to skip its nested-niri spawn when `WAYLAND_DISPLAY` is already
set by the parent. DankGreeter's QML and Quickshell content run
unmodified.

---

## Process model and UID handoff

- **halmasuit** runs as `compositor` (system user, no shell, no login).
  Owns DRM master via logind, holds the Wayland/greetd/IPC sockets.
- **Greeter process** runs as `greeter` (system user, no shell, no login,
  no FS access beyond its own cache dir). Same posture as greetd's
  greeter user. Sees keystrokes (Wayland input) including typed
  passwords; relays auth via greetd protocol to halmasuit.
- **`halmasuit-spawn`** is a small setuid-root helper (~80 lines,
  `#![forbid(unsafe_code)]`, statically linked, no env propagation except
  an explicit allowlist, no file I/O except opening `XDG_RUNTIME_DIR`).
  Its only job is to drop privileges to a target UID/GID and exec a
  command. Audited like sudo's pre-fork core.
- **niri** runs as the authenticated user. Inherits the Wayland socket
  via `WAYLAND_DISPLAY` env. Renders its scene to a single Wayland
  surface that halmasuit composites onto the display.

The privilege-drop sequence inside `halmasuit-spawn`:

```
assert target_uid >= UID_MIN (typically 1000)
assert target_gid >= UID_MIN
setresgid(target_gid, target_gid, target_gid)
setgroups(target_supplementary_groups)
setresuid(target_uid, target_uid, target_uid)
prctl(PR_SET_NO_NEW_PRIVS)
execve(cmd, sanitized_argv, sanitized_envp)
```

No intervening syscalls touch user-controlled state between privilege
drop and exec.

The UID-floor refusal is what makes the privilege split *not* security
theater. A compromised halmasuit can invoke `halmasuit-spawn` — that is
in the threat model and not preventable. What the floor prevents is
using spawn to escalate to root or any other system user. With the floor
in place, the worst-case outcome of full code execution in halmasuit is
"spawn arbitrary commands as the currently-logged-in user," not "system
compromise" — reboot to recovery and the persistent damage is in
`$HOME`, not in `/`. Removing the floor turns the split into theater.

---

## Authentication flow

PAM happens in-process in halmasuit. Halmasuit registers a PAM service
file (`halmasuit`) via the NixOS module. The `compositor` user is granted
permission to authenticate other users via `pam_unix` or whatever modules
are configured in the system PAM config.

State machine (in `halmasuit-greetd` crate):

```
IDLE
  ↓ create_session { username }
SESSION_CREATED(id)
  ↓ pam_authenticate() — issues challenges to greeter
  ↓ post_auth_message_response → … → PAM result
AUTH_SUCCESS(id, uid, gid)    AUTH_FAILED(id) → IDLE
  ↓ start_session { cmd }
SPAWNING
  ↓ halmasuit-spawn → execve
SESSION_RUNNING
  ↓ session exits
IDLE
```

Explicitly state-machine-checked: `start_session` is only valid in
`AUTH_SUCCESS`, never in `IDLE` or `SESSION_CREATED`. Property tests
exercise the transitions; cargo-fuzz fuzzes the wire decoder.

Credential material (challenge strings, password responses) is zeroed
via the `zeroize` crate as soon as PAM completes. Halmasuit retains only
the authenticated UID and username — non-sensitive metadata.

---

## Wayland protocol surface

Halmasuit hosts a small Wayland surface. Its only Wayland client is one
of `{greeter, niri, lock-client}` at a time, so it doesn't need the full
layer-shell ecosystem the inner WM provides.

**Implemented in v2:**
- Core: `wl_display`, `wl_compositor`, `wl_subcompositor`, `wl_seat`,
  `wl_output`, `wl_shm`
- `xdg-shell` (for greeter and niri to map top-level windows)
- `linux-dmabuf-v1` (for niri to share GPU buffers efficiently)
- `presentation-time` (for frame timing)
- `ext-session-lock-v1` (for lock screen clients)
- `wp_viewporter`, `wp_fractional_scale_v1` (modern display support)

**Out-of-scope for v2 (deferred to v3+):**
- A custom `ext-halmasuit-host-v1` protocol that lets the inner
  compositor (niri) signal "I'm rendering fullscreen, direct-scanout my
  buffer." This is what eliminates the nesting GPU tax — but it requires
  protocol design + niri-side patches. See **Roadmap** below.

---

## D-Bus integration

Inbound consumption (halmasuit-as-client):

- `org.freedesktop.login1.Seat.TakeDevice` / `ReleaseDevice` — DRM and
  input FDs delegated by logind.
- `org.freedesktop.login1.Manager.PrepareForSleep` signal — suspend
  preparation hook.
- `org.freedesktop.login1.Manager.Inhibit` — block sleep during
  critical compositor operations.

Outbound service (halmasuit-as-server):

- `org.halmasuit.Compositor1` bus name on the system bus.
- Methods: `Lock()`, `Unlock()`, `RestartInnerWM()`, `Status() → JSON`.
- Authorization via polkit. Default policy: `Lock` is anyone; the rest
  require `wheel` group or interactive auth.

D-Bus libraries: `zbus` (5.x), async, native Rust, no glib dependency.

---

## IPC layer

Local control plane is JSON-RPC 2.0 over a Unix socket at
`/run/halmasuit/control.sock`, mode 0660 owned by the `wheel` group.

CLI client: `halmasuit msg <command>` — modelled on `niri msg`.

Commands:
- `status` — current phase (greeter/session/locked), uptime, current user
- `lock` — initiate lock-screen flow
- `unlock` — release lock (requires re-auth)
- `restart-wm` — kill and respawn the inner WM (clients die in v1)
- `recovery` — show the in-compositor recovery overlay
- `version`

The same surface is exposed via D-Bus (a subset) for desktop-environment
integration; the rest stays local-only.

---

## Observability

Logging and tracing via the `tracing` ecosystem:

- `tracing` for span/event macros throughout the codebase
- `tracing-subscriber` for default sink (JSON to stdout, captured by
  systemd → journald)
- `tracing-journald` for direct journald structured fields
- Per-frame spans: frame_id, output_id, presentation timing
- Per-event spans: input source, processing duration
- IPC request/response spans
- Greetd state-machine transitions as discrete events

**Not in v1:** `tracing-opentelemetry` for OTLP export. Trivial to add
later (one `.with()` call on the subscriber); deferred because we have
no spans worth exporting until v2 compositor code lands.

---

## Security analysis

### Threat model

Attackers, in order of likelihood:

1. **Authenticated user attempting privilege escalation** to root or to
   another user's session.
2. **Unauthenticated user at the keyboard** attempting to bypass the
   greeter.
3. **Malicious software running as the greeter user** (compromised
   greeter binary).
4. **Malicious software running as the compositor user** (compromised
   halmasuit).
5. **Supply-chain attack** on Cargo dependencies or system PAM modules.

### Attack vectors and mitigations

| # | Vector | Mitigation |
|---|---|---|
| 1 | Another process connects to `/run/halmasuit/greetd.sock` and impersonates the greeter | Socket mode 0600 owned by `greeter`. SO_PEERCRED check on accept — connecting PID must be in the spawned greeter process tree. |
| 2 | Greeter client sends `start_session` without completing PAM | Explicit state machine in `halmasuit-greetd`. `start_session` requires a session_id only obtainable via successful PAM. Property-tested + fuzzed. |
| 3 | Compromised greeter exfiltrates passwords | Same risk class as greetd today; structural property of GUI password entry. Defense in depth: greeter runs as `greeter` user with no FS access beyond cache dir, no setuid, no PAM ability of its own. A keylogger-as-greeter has no escalation path. |
| 4 | Second Wayland client connects during greeter phase to screenshot/inject input | Halmasuit enforces single-foreground-client. Wayland socket SO_PEERCRED check; reject any connection outside the spawned greeter/session process tree. |
| 5 | TOCTOU during privilege drop in `halmasuit-spawn` | UID/GID passed as integers via argv. No path lookups in the helper. Privilege-drop syscalls execute in lockstep with no intervening user-controlled-state syscalls. Standard sudo hardening. |
| 6 | Bug in `halmasuit-spawn` itself → privilege escalation | Helper is microscopic (~80 lines), `#![forbid(unsafe_code)]`, statically linked, no env propagation except allowlist, no file I/O except `XDG_RUNTIME_DIR` open. Audited on every change. |
| 7 | Credential material lingers in halmasuit memory after auth | `zeroize` crate clears PAM challenge/response buffers immediately after auth completes. Only authenticated UID is retained. |
| 8 | DRM master takeover by another process | logind enforces single-master-per-seat. Halmasuit registers as seat master via logind D-Bus; logind rejects competing requests. |
| 9 | D-Bus method abuse — random user calls `RestartInnerWM` | polkit rules shipped with the NixOS module. Privileged methods require `wheel` group or interactive authentication. |
| 10 | Lock-screen bypass | Lock screen is an `ext-session-lock-v1` client. Halmasuit refuses to release the lock until the client demonstrates a successful PAM round-trip. Same flow as greeter, in-session. |
| 11 | Compromised halmasuit invokes `halmasuit-spawn` with `target_uid=0` (or any system UID) to escalate to root | **UID floor**: spawn refuses any `target_uid < UID_MIN` (typically 1000); same for `target_gid`. A compromised compositor can still invoke spawn for legitimate session UIDs but cannot reach root or other system users. Worst case bounded to session-as-currently-logged-in-user. **This is the load-bearing security property of the privilege split** — without it, the split is theater. |

### Posture vs current greetd

Halmasuit's posture is **strictly better** than greetd's. greetd-the-daemon
runs as **root** — full compromise of greetd is full compromise of the
system, with no privilege boundary to fall back on. halmasuit factors
that privilege into one ~80-line setuid helper (`halmasuit-spawn`) and
leaves the compositor itself unprivileged. Concretely:

- **Bug-class delta.** Most exploitable bugs (info leaks, OOB reads,
  partial heap overwrites, Wayland state-machine errors, smithay surface
  bugs) become non-fatal when the process has no privileges to escalate.
  In a root greetd or root halmasuit, the same bug class is a
  kernel-attack primitive — root can `ptrace` arbitrary processes, write
  `/proc/*/mem`, open `/dev/mem`, `init_module`, etc.
- **Full-RCE bound.** Even on full code-execution in halmasuit, the UID
  floor in `halmasuit-spawn` (row 11 above) caps the blast radius at the
  currently-logged-in user. greetd has no equivalent cap because it *is*
  root.
- **Audit ratio.** The privileged surface goes from "all of greetd plus
  its deps" to ~80 lines of `#![forbid(unsafe_code)]`, statically-linked
  Rust. That fits on a whiteboard and is reviewable by eye on every
  commit.

The pattern is standard split-privilege design, the same one OpenSSH
uses (privileged `sshd` + unprivileged per-connection child) and the
same one Windows uses (privileged `winlogon`/`lsass` + DWM running as a
virtual service account, not SYSTEM). The privilege isn't hidden — it's
**factored**.

`halmasuit-spawn`, the Wayland socket peer-credential check, and the
greetd state machine are the three things that must have fuzz tests and
property tests from v1 onwards.

---

## Rust toolchain and ecosystem

As of project start (2026-05):

| Item | Version | Notes |
|---|---|---|
| Rust stable | 1.95.0 (2026-04-16); 1.96 due 2026-05-28 | Pin via `rust-toolchain.toml` |
| Rust edition | **2024** | Bleeding-edge; editions are 3-year cadence (2015/2018/2021/2024, next ~2028). No 2027 edition exists. |
| `wayland-server` | 0.31.13 | Stable |
| `calloop` | 0.14.4 | Smithay's event loop |
| `tracing` | 0.1.44 | |
| `zbus` | 5.15.0 | |
| `drm` | 0.15.0 | |
| `input` (libinput) | 0.10.0 | |
| `wlcs` | 1.8.1 | Canonical-maintained, active |

**smithay** (Wayland compositor toolkit): we will pin to a git
revision matching niri's or cosmic-comp's current pin, not crates.io
(0.7.0 is from June 2024 and master has had substantial breaking changes
since: DnD refactor, surface state overhaul, `delegate_dispatch2!` macro
collapse, framebuffer-effect render elements). This is the standard
smithay-downstream pattern.

**PAM bindings ecosystem is stale.** `pam-client` last touched 2022;
`pam-sys` 2023. Options for v2: (a) use them as-is — the C API is
stable; (b) fork one with updates; (c) write thin FFI via `bindgen`.
Decision deferred to v2.

---

## Cargo workspace layout

```
halmasuit/
├── Cargo.toml                  # workspace root with shared [workspace.lints]
├── flake.nix                   # dev shell + package + NixOS module export
├── devenv.nix                  # devenv-managed dev environment
├── .envrc                      # direnv → use flake / use devenv
├── Justfile                    # check/lint/test/test-vm/mutants/fuzz/miri/loom
├── rust-toolchain.toml         # pin: stable, edition 2024
├── rustfmt.toml
├── deny.toml                   # cargo-deny: license allowlist + advisory check
├── typos.toml
├── clippy.toml
├── ARCHITECTURE.md             # this file
├── README.md
├── LICENSE                     # Apache-2.0
├── .github/
│   └── workflows/
│       └── ci.yml              # nix flake check on ubuntu-24.04 + cachix
├── crates/
│   ├── halmasuit/              # compositor binary (v2)
│   ├── halmasuit-protocols/    # Wayland XML + wayland-rs codegen (v2)
│   ├── halmasuit-greetd/       # greetd wire-protocol server impl (v2)
│   ├── halmasuit-ipc/          # JSON-RPC control plane types (v2)
│   ├── halmasuit-cli/          # halmasuit msg CLI (v2)
│   ├── halmasuit-spawn/        # setuid privilege-drop helper (v2)
│   └── halmasuit-test/         # NixOS test harness helpers (v1: live)
├── protocols/                  # vendored Wayland XML (v2)
├── nix/
│   ├── module.nix              # services.halmasuit.* NixOS module
│   └── package.nix             # Nix derivation
├── tests/
│   ├── seamless-boot.nix       # the v1 deliverable
│   ├── login-flow.nix          # (v2+)
│   └── crash-recovery.nix      # (v2+)
└── xtask/                      # build/release automation
```

Monorepo with Cargo workspace. Atomic commits across crates; one CI
pipeline; one issue tracker; one release process. Matches niri,
cosmic-comp, smithay.

---

## Development environment

Per savecraft.gg convention:

- **`flake.nix`** at repo root provides the dev shell and the package.
  Consumers (e.g., a user's nix-config) pull halmasuit in as a flake
  input.
- **`devenv.nix`** layers a richer devenv-managed environment with
  pre-commit hooks, ad-hoc tooling, language servers.
- **`.envrc`** has `use flake` (or `use devenv`) so `direnv` auto-loads
  the shell on `cd`.
- **`Justfile`** is the canonical entrypoint for any human-or-CI
  invocation. Same commands locally and in CI.

Justfile targets (mirroring savecraft's organization):

```
just check           # lint + test (the fast local gate; <30s)
just lint            # clippy + cargo-deny + typos + cargo-machete + rustfmt --check
just test            # cargo nextest + llvm-cov ≥ 80% on critical crates
just test-vm         # NixOS VM tests (smoke-boot must pass; login-flash expected RED in v1)
just test-vm-drive name  # agent-drivable interactive VM (QEMU window + FIFO cmd file)
just test-vm-interactive name   # interactive driver for a specific test
just fmt             # rustfmt --edition 2024
just mutants         # cargo-mutants (nightly target)
just miri            # cargo +nightly miri nextest run (nightly target)
just fuzz minutes=10 # cargo +nightly fuzz on protocol decoders (nightly target)
just loom            # RUSTFLAGS="--cfg loom" cargo test --test loom_models
just audit           # cargo audit (CVE check)
just semver          # cargo semver-checks (release PRs)
```

---

## Test strategy

Tiered, with each tier gated and reported separately.

| Tier | Tools | When | Speed |
|---|---|---|---|
| **Format / lint** | rustfmt, clippy `-D warnings`, cargo-deny, typos, cargo-machete | Every commit | seconds |
| **Unit** | `cargo nextest` + `insta` snapshots + `proptest` | Every commit | seconds |
| **Coverage** | `cargo-llvm-cov` ≥ 80% on critical crates | Every commit | seconds |
| **Wayland conformance** | `wlcs` 1.8 against the compositor (v2+) | Every commit | ~1 min |
| **Protocol fuzz** | `cargo-fuzz` on greetd-protocol decoder, Wayland decoder | Every commit (5 min budget); extended nightly | minutes |
| **VM integration** | NixOS tests with virtio-gpu + frame capture | Every commit | ~1 min each |
| **Mutation** | `cargo-mutants` 27 on critical paths | Nightly | hour |
| **UB** | `miri` on unsafe-heavy crates | Nightly | nightly |
| **Concurrency** | `loom` on state-machine paths | Nightly | nightly |
| **Real hardware** | NixOS test running on a physical machine via PiKVM/HDMI capture | Per release | manual |

VM integration tests use the NixOS Python test framework with QEMU +
virtio-gpu. Frame capture via `qmp screendump` at ~60Hz, saved to a
shared directory, asserted in the test script. Assertions include:

- **No black frames** (avg pixel value of any frame > threshold).
- **No leaked text** (OCR via tesseract on sampled frames — possibly
  deferred to v1.5).
- **No visual jumps** (DSSIM between consecutive frames < threshold
  during transitions).
- **Compositor process lifetime** (no PID change across login).
- **Logo continuity** (template match against a known frame at expected
  times).

Failed runs upload all captured frames as GHA artifacts for human review.

---

## CI

GitHub Actions on `ubuntu-24.04` runners (KVM-capable since late 2023).

```
job flake-check:
  uses: cachix/install-nix-action
  uses: cachix/cachix-action  (binary cache)
  run: nix flake check -L --print-build-logs
  on failure: upload-artifact all VM screenshots
```

cachix binary cache keeps build times tractable within GHA's 6-hour
job ceiling. Without it, every PR rebuilds nixpkgs derivations and
times out.

---

## Roadmap

### v1 (current scope) — Test infrastructure validates the existing problem

**Deliverable:** A NixOS VM test (`tests/seamless-boot.nix`) that boots a
system mirroring the user's current desktop (greetd + DankGreeter +
niri + DMS), captures every frame during the BIOS → greeter → desktop
pipeline, and asserts the absence of black frames + leaked text + visual
jumps.

**Expected result:** the test **fails** today, with screenshots showing
the flash. That failure is the project's baseline. Every subsequent
milestone is measured against making this test pass.

No halmasuit binary code in v1. The compositor crates exist as empty
placeholders in the workspace, ready to be filled in v2.

This is TDD applied to systems work: build the measurement instrument
before the thing it measures.

### v2 — Halmasuit compositor that passes the v1 test

- smithay-based compositor binary.
- greetd protocol server (the `halmasuit-greetd` crate).
- PAM integration (decision on bindings made here).
- `halmasuit-spawn` setuid helper.
- NixOS module that replaces greetd: `services.halmasuit.enable = true;`.
- DankGreeter launcher patch (~20 lines).
- Inner-niri lifecycle.
- D-Bus integration (logind + `org.halmasuit.Compositor1`).
- Ship to gnomon as the daily-driver greeter.

When the v1 NixOS test starts passing against the halmasuit-enabled
system, v2 is done.

### v3 — Direct-scanout optimization

- Design and publish `ext-halmasuit-host-v1` Wayland protocol for
  inner-compositor direct-scanout handshake.
- niri-side client implementation (likely as a fork patch initially,
  upstream attempt later).
- Atomic-modeset code in halmasuit to put niri's dmabuf directly on a
  CRTC plane when conditions allow.
- Falls back to normal nested composition when an overlay is up
  (recovery menu, lock screen).

Removes the GPU tax that v2 pays for nested rendering. Frame latency
matches running niri natively.

### v4 — Initramfs integration

- Build halmasuit into the initramfs image.
- Start halmasuit as one of the first userspace processes.
- Handle the simpledrm → real KMS driver handover gracefully (hold last
  frame across, atomic flip to new device).
- Survive `switch_root` (binary in both initramfs and rootfs; re-exec
  pattern à la Plymouth).
- Continue running through systemd target traversal.
- Reach `graphical.target` and accept the first greeter client.

When done: BIOS firmware splash → halmasuit splash → greeter → session
with zero visible discontinuities.

### v5 and beyond

- **Crash isolation with client preservation.** When niri crashes,
  clients survive (currently they die). Requires the "good version" of
  the architecture: halmasuit hosts the clients directly, niri provides
  only window-management policy via a custom protocol. Significant
  protocol work. May or may not be worth pursuing.
- **Fast user switching.** Two user sessions live concurrently;
  foreground swaps without exit.
- **HDR + VRR pass-through across nesting.** Requires plumbing color
  management and presentation timing through the direct-scanout path.
- **Multi-seat.** A single halmasuit process serving multiple seats.
- **Recovery mode.** A graphical recovery UI accessible even when the
  user session won't start. Halmasuit is already running; just paint a
  different scene.
- **Screen casting / remote display infrastructure.** Owned by the
  long-lived compositor, stable across session changes.

---

## Out of scope (explicit non-goals for v1 and v2)

These exist so the scope cannot creep without an explicit decision.

- **NOT in v1:** any halmasuit compositor code. The compositor crates are
  empty placeholders. v1 is purely test infrastructure.
- **NOT in v2:** initramfs integration. v2 ships as a normal display
  manager replacement that starts at `graphical.target`.
- **NOT in v2:** direct-scanout / single-composition optimization. v2
  pays the double-composition GPU cost. Acceptable trade-off for
  scope/risk; v3 fixes it.
- **NOT in v2:** client preservation across inner-WM restart. v2
  restarts niri cleanly and clients die. Same UX as a Hyprland/niri
  crash today.
- **NOT in v2:** multi-seat, HDR, VRR pass-through.
- **NOT in v2:** OpenTelemetry export. Adding `tracing-opentelemetry`
  later is a one-line subscriber change; not needed until we have spans
  worth exporting.

---

## Anti-patterns (forbidden)

Things we will not do regardless of pressure:

- **NO running halmasuit as root.** Compositor runs as `compositor` user;
  setuid `halmasuit-spawn` is the only privileged code path, and it is
  microscopic and audited.
- **NO inheriting environment from the caller** in `halmasuit-spawn`.
  Explicit env allowlist; everything else dropped.
- **NO `unsafe` in `halmasuit-spawn`.** `#![forbid(unsafe_code)]`.
- **NO trusting Wayland client PIDs from message contents.** Always use
  SO_PEERCRED on the socket connection.
- **NO `start_session` permitted without prior PAM success.** State
  machine enforces this with property tests and fuzz coverage.
- **NO credential material kept in memory past PAM completion.**
  `zeroize` immediately after auth.
- **NO `--no-verify` on commits, no `--no-gpg-sign`, no skipping hooks.**
  Quality gates are non-negotiable.
- **NO test that asserts the absence of bugs via "did it not crash."**
  Tests assert observable system properties: pixel values, frame counts,
  process lifetimes.
- **NO mocking PAM in integration tests.** Real PAM, real users, real
  failure modes. (Unit tests can mock individual modules but
  integration / VM tests do not.)
- **NO single-host assumptions.** v2 must build and pass tests on any
  KVM-capable Linux system, not just gnomon.

---

## Open decisions deferred to implementation milestones

These are explicit "we know this needs deciding, just not yet":

1. **PAM bindings** (v2). `pam-client` and `pam-sys` are stale (2022 and
   2023 respectively). Decide between fork, write-thin-FFI, or use-as-is
   when we start writing the auth code.
2. **smithay revision** (v2). Pin to whatever niri or cosmic-comp is on
   when v2 begins. Update on a deliberate cadence; not bleeding-edge.
3. **Exact Wayland protocol surface** for v2 (vs additions in v3+). The
   v2 list above is the minimum; we may add `wp_drm_lease_v1` or
   `linux-explicit-synchronization-v1` if useful.
4. **OCR in v1 test pipeline.** May defer text-leak detection to v1.5
   if tesseract bindings prove fiddly; v1 ships with black-frame and
   DSSIM-jump detection only.
5. **D-Bus surface details.** The method list above is a starting set;
   final surface depends on what desktop-environment integration
   requires in practice.
