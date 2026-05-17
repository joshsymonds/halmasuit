# halmasuit — Claude instructions

Project-specific guidance. The global `~/.claude/CLAUDE.md` preferences
still apply (terse, no `_v2` suffixes, fix all lints in one pass, etc.).

## What this project is

A Linux **system compositor**: one long-lived display-server process owning
the GPU from `graphical.target` to shutdown, hosting the greeter and the
user session as nested Wayland clients of itself. Replaces greetd entirely.
Eliminates the visible black flash that exists today between greeter and
session on every greetd-based Linux desktop.

Read **`ARCHITECTURE.md`** for the design, threat model, and roadmap.
Read **`PLAN.md`** for v2's implementation scope and "in / out" decisions.
Read **`RESEARCH.md`** for the empirically validated architectural
foundations (drm-master-probe Phase 0 + Phase 1).

The terms `v1`, `v2`, `v3`, `v4`, `v5+` in this repo always refer to the
roadmap milestones in `ARCHITECTURE.md`, not crate versions.

## Where the project is

**v1 is complete.** v1 is *test infrastructure that proves the flash
exists*; the compositor itself does not exist yet. All 7 crates are
stubs (~83 lines of Rust workspace-wide). The interesting code in v1
lives in `tests/login-flash.nix`, not in `crates/`.

**v2 is the next milestone.** v2 writes the compositor that makes
`login-flash` go green. That test must not be modified to make it pass —
the system under test changes; the assertion does not.

## Hard rules

These are anti-patterns from `ARCHITECTURE.md` codified as Claude rules.
Do not relax them without explicit user direction:

- **`login-flash` is RED BY DESIGN until v2 ships.** `just test-vm`'s
  wrapper inverts its exit code on purpose. Do not "fix" the failure by
  weakening the assertion, skipping the test, or making it conditional.
  An unexpected pass is a CI error, not a success.
- **The privileged surface is the `halmasuit-session` broker.** It
  owns ONE `pam_handle_t` for the whole lifecycle (auth → setcred →
  open_session → … → close_session → pam_end), runs `pam_authenticate`
  in an ephemeral SIGKILL-able `setrlimit`-bounded privileged fork,
  and launches the session by forking once and dropping privileges in
  a **non-setuid child** (it is already root — greetd/OpenSSH/GDM/su
  shape). It is a single socket-activated unit with no standing root
  when idle. `unsafe` is confined to its `pam_ffi`/`worker` modules;
  every change to this crate's privileged boundary is a
  security-review event — surface diffs prominently. See
  ARCHITECTURE.md "Authentication and session lifecycle" and
  `HANDOFF.md` §0.8.
- **`halmasuit-spawn` + `halmasuit-pam` are the INTERIM in-compositor
  path, scheduled for deletion by the successor (compositor→broker /
  G-layer) epic** — atomically when the compositor relays to the
  broker (the broker is built + VM-proven but not yet the *live*
  compositor path; deleting them now would break the compositor with
  no replacement). Until then they stay load-bearing: `halmasuit-spawn`
  must stay microscopic (~80 lines, `#![forbid(unsafe_code)]`,
  statically linked, env allowlist only, no file I/O except
  `XDG_RUNTIME_DIR`); every change to it is a security-review event.
  Do NOT delete them, the `security.wrappers` entry, or
  `halmasuit-greetd`'s `MAX_SESSION_BUILDS_PER_CONNECTION` in this
  epic; do NOT add any *new* setuid spawn path or second libpam
  consumer (HANDOFF §0.8).
- **UID floor is load-bearing — enforced by the broker (and the
  interim `halmasuit-spawn`).** Refuse any `uid`/`gid < UID_MIN`
  (typically 1000); reject `(uid_t)-1`/overflow. The broker
  independently re-derives identity from PAM (`pam_get_user` → pwent)
  and never trusts a compositor-asserted identity (`SO_PEERCRED`
  authenticates the peer, never authorizes the action). This is what
  makes a compromised halmasuit *not* a root compromise. Removing it
  is security theater. See ARCHITECTURE.md threat model row 11.
- **One `pam_handle_t`, never split, owner never `execve`s between
  auth and session.** No two-handle / cross-process-handle design
  (pam_mount/keyring/krb5 silently break — locked `$HOME`, no error).
  No blind `initgroups`/env-replace in the session leader (clobbers
  pam_group/pam_systemd/pam_mount/pam_env — must `getgrouplist`- and
  `pam_getenvlist`-MERGE).
- **No `start_session` path that bypasses PAM success.** The greetd
  state machine in `halmasuit-greetd` enforces this; never add a code
  path that side-steps it.
- **No PAM in the compositor's address space.** libpam links in
  exactly one crate (`halmasuit-session`); the compositor relays only
  length-bounded conversation frames.
- **No mocking PAM in VM / integration tests.** Real PAM, real users.
  Unit tests inside `halmasuit-greetd` may mock individual modules.
- **No running the compositor as root.** halmasuit runs as the
  `compositor` system user and holds no handle. The broker is the
  privileged process; it idle-exits (no standing root daemon).
- **No trusting Wayland client PIDs from message contents.** Always
  `SO_PEERCRED` on the socket.
- **No credential material kept past PAM completion.** `zeroize`
  challenge/response buffers immediately.
- **No `--no-verify`, `--no-gpg-sign`, or skipping hooks.** Quality
  gates are non-negotiable.

## Working conventions

- **Entry point: `just`.** `just check` = `lint test` (rustfmt + clippy
  `-D warnings` + cargo-deny + cargo-machete + typos + nextest). Run
  this before reporting success, per the user's global workflow rule.
- **VM tests: `just test-vm`** runs the full headless gate.
  `just test-vm-drive <name>` opens an interactive QEMU window driven
  via a FIFO command file — use this when debugging a VM test, not
  bare `nix build`. The drive shape is documented inline in the
  `Justfile`.
- **Workspace lints live in `[workspace.lints]`.** Add per-crate
  `#![allow]` only with a `// reason: …` comment justifying it; do not
  add per-crate lint overrides in `Cargo.toml` to silence noise.
- **Edition 2024, Rust 1.95 pin** (see `rust-toolchain.toml`). Don't
  bump the channel without coordinating; CI caches key on it.
- **Cargo workspace, monorepo, atomic commits.** A change touching the
  greetd protocol + the CLI + the IPC types lands in one commit.
- **Test user is `tests/lib/test-user.nix`.** New VM tests import it;
  do not copy-paste the insecure user config.

## Ecosystem caveats

These are the easy traps to fall into when adding real deps:

- **`halmasuit-greetd` owns its wire types locally.** The upstream
  `greetd_ipc` crate is GPL-3.0-only; linking it would force
  halmasuit's binary to GPL, conflicting with the workspace's dual
  MIT-OR-Apache posture (which matches the Rust-Wayland infrastructure
  tier: smithay, wlroots, Weston). The wire types in
  `crates/halmasuit-greetd/src/lib.rs` are a clean-room
  reimplementation derived from the public protocol spec at
  <https://man.sr.ht/~kennylevinsen/greetd/protocol.md>, NOT from
  `greetd_ipc`'s source. Drift mitigation is the
  `wire_format_*` canonical-payload roundtrip tests in that crate —
  they pin the JSON shape against payloads from the spec, so if
  upstream ever changes the format we notice. Reusing
  greetd-the-daemon as a library is also not feasible (incompatible
  privilege model: greetd runs as root and `execve`s; halmasuit is
  unprivileged and never execs).
- **smithay** — pin to a `git` revision matching niri's or
  cosmic-comp's current pin, never crates.io 0.7.0 (June 2024,
  pre-DnD-refactor, pre-`delegate_dispatch2!`). Standard
  smithay-downstream pattern.
- **PAM bindings** — `pam-client` and `pam-sys` are both stale
  (2022 / 2023). C API is stable so they probably work as-is; the
  alternatives are fork or thin-FFI-via-`bindgen`. Decide when auth
  code lands; do not silently pick one.
- **D-Bus** — `zbus` 5.x. Do not pull in glib.
- **wayland-server** + **calloop** are smithay's, follow smithay's
  pin.

## Test-VM rendering gotcha

Headless NixOS VM tests use `virtio-gpu-pci` (no GBM allocator) — niri
runs but paints nothing, screenshots are solid black. PID-tracking still
works, which is enough for v1's flash assertion. Interactive driver mode
swaps to `virtio-vga-gl` + `-display gtk,gl=on` and pixels actually
render. If you need visual frame capture in CI later, you'll need to
either solve the headless GL backend or shell out to a real GPU runner.
Do not pretend the headless screenshots are valid — they are
deliberately not.

## Open decisions parked for v2 implementation

Listed in `ARCHITECTURE.md` § "Open decisions"; the load-bearing ones:

1. PAM bindings strategy (above).
2. smithay revision pin (above).
3. Final `org.halmasuit.Compositor1` D-Bus surface.
4. OCR for text-leak detection in v1.5 frame-capture (tesseract).

Don't invent answers; flag the decision when the relevant code lands.

## CI

GHA on `ubuntu-24.04`, actions pinned to commit SHAs. Consumes (does
not push to) `joshsymonds.cachix.org`. `login-flash` is run with
`continue-on-error: true`; the next step interprets `expected-fail` as
success. When v2 makes it green, *flip the gate* — do not leave the
inversion in place.

## Where to look

- `PLAN.md` — v2 implementation plan: what's in scope, what's deferred.
- `RESEARCH.md` — empirically validated architectural foundations.
- `ARCHITECTURE.md` — full design, threat model, roadmap.
- `Justfile` — every command, local and CI.
- `tests/login-flash.nix` — the canonical v1 deliverable; reading it
  end-to-end is the fastest way to understand the project's testing
  posture.
- `crates/halmasuit-session/src/broker.rs` — the module doc is the
  authoritative description of the privileged PAM/session lifecycle
  (one handle, killable auth fork, fork-then-drop leader, slot-owned
  reaping). `HANDOFF.md` §0.8 is the canonical epic close-out /
  deferral record.
- `crates/halmasuit-spawn/src/main.rs` — the interim in-compositor
  setuid helper (scheduled for deletion by the successor epic;
  superseded by the broker's non-setuid fork-then-drop child).
