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

The compositor exists and is the live system. The
privilege-separated PAM/session epic has landed: the privileged
`halmasuit-session` broker is the **live** auth/session path; the
compositor is an unprivileged sans-IO relay to it; the in-compositor
`halmasuit-pam` and the setuid `halmasuit-spawn` helper are **deleted**
(single libpam surface = `halmasuit-session`; no setuid inode in the
closure). `login-flash` passes **through the broker-launched session**
— PID and frame continuity hold across the real greeter→session
transition. The assertion is never modified to pass; the system under
test changes, the assertion does not.

## Hard rules

These are anti-patterns from `ARCHITECTURE.md` codified as Claude rules.
Do not relax them without explicit user direction:

- **`login-flash` is a GREEN hard gate.** It proves halmasuit's PID
  and `assert_frame_continuity` hold across the real greeter→session
  transition *through the broker-launched session* — no compositor
  restart, no flash. Never weaken, skip, invert, or conditionalize the
  assertion to keep it green; an unexpected FAIL means the flash
  invariant regressed (the thing this project exists to prevent). The
  `just test-vm` exit-code inversion is gone; the CI gate must run it
  as a normal pass/fail check (see CI).
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
  `HANDOFF.md` §0.7–§0.12 (the canonical amendment record A1–A8).
- **`halmasuit-pam` and the setuid `halmasuit-spawn` are DELETED.**
  There is exactly one libpam surface (`halmasuit-session`) and NO
  setuid inode in the closure (Epic R10/R14/R15, landed atomically
  with the broker going live). Do NOT re-introduce an in-compositor
  PAM path, a setuid spawn helper, a `security.wrappers` setuid entry,
  a second libpam consumer, or `halmasuit-greetd`'s
  `MAX_SESSION_BUILDS_PER_CONNECTION`. Privilege-drop+exec exists ONLY
  in the broker's non-setuid session-leader child (fork-then-drop from
  already-root — R7/R11).
- **The compositor retains zero escalation capability.** After its
  in-process privilege drop the bounding set is emptied entirely; it
  keeps only `CAP_KILL` (to signal its greeter child). Do NOT re-add
  `{CAP_SETUID,CAP_SETGID}` (or anything) to the compositor bounding
  set — it execs no setuid helper, so any retained cap is a
  least-authority regression (R15).
- **UID floor is load-bearing — enforced by the broker's
  session-leader child.** Refuse any `uid`/`gid < UID_MIN` (typically
  1000); reject `(uid_t)-1`/overflow. The broker independently
  re-derives identity from PAM (`pam_get_user` → pwent) and never
  trusts a compositor-asserted identity. This is what makes a
  compromised halmasuit *not* a root compromise. Removing it is
  security theater. See ARCHITECTURE.md threat model row 11.
- **Broker SO_PEERCRED authorizes its trusted RELAY peer, not the
  greeter.** In the live topology that peer is the unprivileged
  compositor (greeter→[compositor's greetd greeter-gate]→compositor→
  [broker's relay-peer gate]→broker). `SO_PEERCRED` authenticates the
  peer; it never authorizes the action — identity is independently
  PAM-derived (R8). The env/var is `HALMASUIT_BROKER_PEER_UID` /
  `relay_peer_uid`; `nix/module.nix` sets it to the compositor uid
  when the compositor is enabled, else the greeter uid (standalone
  direct-broker deploys/tests). Do NOT rename it back to "greeter" or
  point the broker gate at the greeter uid in the live path.
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
- **The compositor never blocks the render/calloop thread on broker
  IPC (A7).** greetd's PAM boundary is fully sans-IO
  (emit/suspend/resume); the broker fd is a per-connection NON-blocking
  calloop source. No blocking `recv`/`send`-then-`recv`, with or
  without a timeout; no synchronous `PamSession::step`.
- **One per-greeter-episode object owns the broker socket the whole
  episode (A6/A8).** It is the sole `OwnedFd`; the calloop source is a
  `Generic` over a NON-owning borrowed-fd newtype; the source token is
  removed before the episode drops. No `dup`/`Rc`/`Arc` of the broker
  socket, ever (premature-EOF / least-authority; CVE-2015-6563/6564).
- **Session lifecycle is one-way broker→compositor (A5).** No
  compositor-emitted lifecycle frame (no such `CompositorToBroker`
  variant — keep it type-impossible). The visible greeter→session swap
  is gated on AND(`SessionOpened`, the compositor's own
  first-non-empty session frame) — never `SessionOpened` alone (that
  reintroduces the flash). The broker is the sole reaper; any
  SCM_RIGHTS leader pidfd the compositor holds is poll-only (never
  waitid/reap/signal); no raw leader pid in any frame.
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
- **PAM bindings** — RESOLVED: `pam-sys` (the only libpam consumer,
  in `halmasuit-session`). The pin + re-audit policy lives in the
  workspace `Cargo.toml` comment; do not add a second PAM crate.
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

Listed in `ARCHITECTURE.md` § "Open decisions"; the still-open ones:

1. smithay revision pin (above).
2. Final `org.halmasuit.Compositor1` D-Bus surface.
3. OCR for text-leak detection in frame-capture (tesseract).

(PAM-bindings strategy is RESOLVED — `pam-sys` in `halmasuit-session`.)

Don't invent answers; flag the decision when the relevant code lands.

## CI

GHA on `ubuntu-24.04`, actions pinned to commit SHAs. Consumes (does
not push to) `joshsymonds.cachix.org`. `login-flash` is now a normal
pass/fail gate (it passes through the broker-launched session); the
old `continue-on-error: true` + `expected-fail`-as-success inversion
is the deleted/superseded model and must NOT remain in the workflow —
if any inversion is still present in `.github/`, removing it is a
tracked follow-up.

## Where to look

- `RESEARCH.md` — empirically validated architectural foundations.
- `ARCHITECTURE.md` — full design, threat model, roadmap.
- `HANDOFF.md` §0.7–§0.12 — the canonical privilege-separation epic
  decision record (Amendments A1–A8, with primary-source derivations
  and DO-NOT-REVISIT conditions).
- `Justfile` — every command, local and CI.
- `tests/login-flash.nix` — the canonical no-flash gate; reading it
  end-to-end is the fastest way to understand the testing posture.
- `crates/halmasuit-session/src/broker.rs` — the module doc is the
  authoritative description of the privileged PAM/session lifecycle
  (one handle, killable auth fork, fork-then-drop leader, slot-owned
  reaping, relay-peer SO_PEERCRED gate).
- `crates/halmasuit/src/broker_session.rs` — the compositor's
  per-greeter `BrokerEpisode`: owns the broker `SeqpacketChannel`,
  drives the sans-IO greetd machine, relays to the broker (A6/A7/A8).
