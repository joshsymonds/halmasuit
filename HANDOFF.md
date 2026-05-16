# halmasuit epic — HANDOFF

Status as of **2026-05-16**, canonical branch **`main`** @ `677d0ac` (the
live line converged here — see "Branch topology" below). This document is
the cold-start brief for whoever (or whichever session/context — work is
moving to **vermissian**) resumes halmasuit. It is self-contained.

**Read order: §0 first.** The original epic was the "renderer → real
session, no-flash visual proof" work (§§1–7 below). That work is **paused
behind one prerequisite**: the unified **session/pamd epic** in §0. Do §0
first; then resume the visual G-layer work (§6).

**Branch topology (resolved 2026-05-16).** The earlier two-branch split
(`feature/visual-compositor` for visual; `fold/visual-compositor-hardening`
for hardening) was a coordination mistake and is collapsed. `main` @
`677d0ac` now carries everything, gate-verified (`just check` 171/171,
`just test-vm` 11/11):
- v2 visual compositor layers A–F2 (was on `feature/visual-compositor`),
- the hardening fold R1–R6 + C3 (reviewed clean via `gambit:review`
  epic #19 — 4 fresh reviewers, zero findings),
- the two security fixes the live line was missing, ported forward:
  `dfae047` (PAM-resolved-username provenance into AuthSuccess, F1/MEDIUM,
  orig `ab1eb13`) and `6f0e744` (halmasuit-spawn static-musl +
  no-PT_INTERP, F3, orig `f8e1321`).
`feature/visual-compositor`'s role is done; `fold/...` superseded.
**`harden/phase-a-review` is retained (NOT deleted)** — 9/10 of its
commits are superseded re-ports, but `aa93f4f` (a login-flash
DRM-master-continuity assertion) is unique. It was attempted as a port
and **reverted**: as written it greps debugfs for the compositor PID as
DRM master, which is false-by-construction under v2's libseat/seatd model
(seatd is the registered master; halmasuit holds the DRM fd *via*
libseat — there is no flash). The concept is valuable; the rewrite is a
scoped item (§6) and the branch is the reference impl until it lands.

---

## 0. START HERE — Unified session/pamd epic (PARKED, do this FIRST)

**Status: PARKED.** Fully brainstormed (decisions locked below); NOT
implemented. It is the prerequisite for the visual G-layer (§6) — G1+
need a real session with a real `$HOME`/`$XDG_RUNTIME_DIR`, which is
exactly what this epic delivers. Work parked because the user is
context-switching to **vermissian**; resume here.

### 0.1 The core realization (the expensive insight — do not re-derive)

C1 ("PAM must be a killable subprocess") and Mechanism D ("session-spawn
namespace handoff", §5) are **not two problems — they are two ends of one
`pam_handle_t` lifecycle**:

- **Auth phase** — `pam_start`/`pam_authenticate`/`pam_acct_mgmt`. "Is
  this person who they claim to be." This is what C1 moves into a
  killable subprocess. Needs no privilege/host-ns.
- **Session phase** — `pam_open_session`/`pam_close_session` (pam_systemd
  → logind + `/run/user/$UID`; pam_mount → `$HOME`). §5's research
  proved this **must** run as root in the host mount namespace.
- **The seam between them is the `start_session` transition the
  `halmasuit-greetd` state machine gates** — the single most
  security-critical interface in the project, a CLAUDE.md
  security-review event on both sides.

Designing C1 and D independently produces two incompatible models of who
holds `pam_handle_t`, discovered at integration, at the worst place.
**They must be co-designed as one epic.**

### 0.2 THE unresolved crux (headline open decision for the epic)

Credential-passing PAM modules (pam_mount unlocking an encrypted `$HOME`,
pam_gnome_keyring, pam_krb5) require **the same `pam_handle_t` to span
`authenticate` → `open_session`** (greetd's `worker.rs` comment is the
canonical statement; cited by the research). So:

> **One shared handle** (greetd-style): the killable auth subprocess must
> itself *be / become* the privileged host-ns session owner — which
> contradicts "unprivileged, auth-only, SIGKILL-anytime worker."
> **vs. Two handles**: a separate unprivileged killable auth worker + a
> separate privileged host-ns session unit (Mechanism D), accepting that
> credential-passing modules break **unless** halmasuit constrains its
> supported PAM stack, or re-runs a constrained auth in the session unit.

This is **the** decision the epic must make first; everything else below
is settled scaffolding around it. Do NOT just pick one — it is a
threat-model + architecture decision (the user's standing rule: "when
uncertain about architecture, stop and ask"; "I don't want to just vibes
it").

### 0.3 Locked design decisions (brainstormed + research-backed; immutable unless 0.2 forces otherwise)

Four forks, locked by the user after research:

1. **Worker entrypoint:** a separate `halmasuit-pam-worker` binary (NOT
   re-exec-self) — mirrors halmasuit-spawn's auditable-microscopic-helper
   philosophy; smithay/compositor code never in the auth process.
2. **Teardown owner (research-locked Option A, unanimous across greetd /
   OpenSSH / GDM / pidfd man-pages / std+tokio):** the binary owns a
   supervisor `WorkerHandle{pid,pidfd}`; `halmasuit-greetd` stays a pure
   `step()` state machine that never learns a pid exists; kill via
   `pidfd_send_signal`, reap via the **existing R4 reaper**
   (`classify_reaped_child` gains a `PamWorker` child class). Process
   control must NOT live in the pure protocol crate; teardown must NOT be
   `Drop`-based (std/tokio explicitly refuse Drop-reaping → zombies).
3. **IPC:** `SOCK_SEQPACKET` socketpair (kernel message boundaries,
   mirrors greetd's datagram model); serde-framed typed messages;
   challenge/response buffers `Zeroizing` on BOTH sides; add
   `wire_format_*` roundtrip drift tests like the existing greetd codec.
4. **`MAX_SESSION_BUILDS_PER_CONNECTION`:** remove it (the per-connection
   cap C1 flagged). Global single-slot makes it redundant and its
   presence implies a defense it doesn't provide. Delete cap +
   `CodecError::SessionBuildLimitExceeded` + its tests **as part of the
   epic** — NOT before (it is correct and load-bearing until the
   subprocess+single-slot replacement lands; it is intentionally still
   present on `main`).

Two sub-decisions, locked:

- **Kill signal: SIGKILL directly, no SIGTERM grace.** The auth worker is
  blocked in `pam_authenticate`; it has no session/children to wind down
  (auth-only, no `pam_open_session`). SIGKILL is deterministic against a
  wedged/malicious libpam module and destroying the address space is
  *stronger* credential hygiene than zeroize-then-continue. (greetd's
  SIGTERM→grace→SIGKILL is for post-auth *sessions*, which a pure auth
  worker is not.)
- **Single-slot scope: GLOBAL — one PAM worker process system-wide.** One
  seat, one greeter, one auth at a time (logically true for a system
  compositor; mirrors greetd's single `current` slot). Bounds live
  workers to O(1) across ANY churn incl. disconnect/reconnect (the exact
  C1 attack) with no per-connection counter. There is no concurrent-auth
  scenario to lose: auth at a single seat is inherently serialized; "new
  CreateSession evicts in-flight" is the correct single-slot semantic and
  what greetd does.

### 0.4 Why (the vulnerability this cures)

halmasuit currently runs each PAM attempt in a **detached worker thread**;
libpam has no cancellation point, so a malicious/buggy greeter looping
CreateSession/CancelSession (incl. disconnect/reconnect to reset any
per-connection cap) accumulates unbounded uncancellable workers
(`MAX_SESSION_BUILDS_PER_CONNECTION` is per-`Connection` → reconnect-churn
defeats it; confirmed as **C1** by `gambit:review`'s verifier). The
killable-subprocess + global-single-slot model bounds the flood to O(1)
regardless of churn, also fixes a hung network-PAM module wedging a
thread forever, and makes CancelSession actually cancel. Failure-cost
(auth-fail delay/lockout) stays delegated to the PAM stack
(pam_faillock/pam_faildelay) — NOT reimplemented in-app. The
detached-thread model is the one design the ecosystem specifically
avoids.

### 0.5 ab1eb13 interaction (already on main — build on it)

`dfae047` (orig `ab1eb13`) reshaped the auth path: `PamStep::Success`,
`PamOutcome::Success`, `SessionState::AuthSuccess`, and the
`SpawnRequest` now carry **PAM's canonical resolved username** (not the
pre-auth client string), so `initgroups(3)` in halmasuit-spawn keys on
the PAM-resolved identity. The killable-subprocess worker **must return
that resolved username across the SEQPACKET boundary too** — the wire
protocol's success message carries `{username, uid, gid}`. PAM-name
provenance is in-domain for this epic; do not regress it.

### 0.6 Research corpus + rejected alternatives

Backed by 3 primary-source research agents (greetd `worker.rs`/`context.rs`/
`interface.rs`; OpenSSH privsep monitor + `auth-pam.c`; GDM
`gdm-session-worker`; systemd `(sd-pam)`; util-linux `login`; `pidfd_*`
man pages; std/tokio `Child` Drop docs; OTP/calloop/`signal-safety(7)`;
OWASP MaxStartups/CWE-770/NIST-800-63B). Full detail in memory
[[pam-killable-subprocess-direction]] and (Mechanism D)
`session-spawn-namespace-handoff`.

**Rejected — DO NOT REVISIT unless the cited condition changes:**
- Re-exec-self PAM worker — rejected: larger auth-process address space;
  separate microscopic binary is the project idiom.
- RAII `Drop`-kills-worker teardown — rejected: `Drop` can't `waitpid`
  (std/tokio document this) → zombies; puts a kill primitive in the pure
  crate.
- `cancel()` threaded through the protocol state machine — rejected:
  spreads process control into the pure protocol crate.
- SIGTERM→grace→SIGKILL for the auth worker — rejected: that is the
  *session* teardown pattern; an auth worker has nothing to wind down.
- Per-connection single-slot — rejected: reconnect-churn defeats it (the
  C1 attack); only GLOBAL is the bound.
- Keeping `MAX_SESSION_BUILDS_PER_CONNECTION` as defense-in-depth —
  rejected: dead security theater once single-slot lands.
- Mechanism A: `setns(/proc/1/ns/mnt)` in halmasuit-spawn — rejected
  (see §5): needs CAP_SYS_ADMIN+CAP_SYS_CHROOT, breaks forbid-unsafe,
  inverts the UID-floor threat model. DO NOT REVISIT.
- A pure rate-limiter instead of the structural cure — rejected: the
  research showed process-isolation + single-slot eliminates the class;
  a limiter only paces it.

### 0.7 First moves when resuming

1. Resolve §0.2 (one-handle vs two) — this likely needs another short
   `gambit:brainstorming` round with the user (it's the threat-model
   decision deliberately not vibed). Mechanism D (§5) is the same
   decision wearing the session-phase hat — decide them together.
2. Then run `gambit:brainstorming` → epic Task with §0.3 as immutable
   requirements + §0.6 as anti-patterns → `gambit:executing-plans`, TDD,
   real-PAM VM tests only (no mocks — CLAUDE.md hard rule), per-task
   checkpoints. Worktree off `main`.
3. Crates in play: `halmasuit-pam` (becomes the parent-side lib + new
   `halmasuit-pam-worker` binary), `halmasuit-greetd` (`server.rs`
   Connection state machine; remove the R6 cap here), `halmasuit`
   (`main.rs`: supervisor `WorkerHandle`, reaper integration, factory),
   plus the Mechanism-D privileged session unit (§5) + `nix/module.nix`.
   `halmasuit-spawn` stays microscopic + untouched unless deliberately
   scoped.

---

---

## 1. What halmasuit is, and why this epic exists

halmasuit is a Linux **system compositor**: one long-lived display-server
process that owns the GPU from `graphical.target` to shutdown and hosts both
the greeter and the user session as **nested Wayland clients of itself**. It
replaces greetd entirely.

The problem it eliminates: on every greetd-based desktop today there is a
visible **black flash** at login, because the greeter compositor exits and the
session compositor starts — a process-identity discontinuity on the GPU. The
project's thesis is that a single persistent compositor, with the greeter and
session as nested clients swapped *underneath* it, removes that discontinuity.
The epic's job is to build that compositor and **prove the no-flash claim
empirically**, both visually and over a per-frame event stream, across the
*real* greeter→session transition with the *real* software the user runs.

Design constraints that matter for everything below (full text in
`ARCHITECTURE.md`, `CLAUDE.md`):

- halmasuit composites; it never paints its own UI. The background is a
  separate client (`halmasuit-splash`); the greeter and niri are unmodified
  upstream clients.
- Only `halmasuit-spawn` (a microscopic setuid helper) is privileged.
  `#![forbid(unsafe_code)]`, ~80 lines, capability bounding set locked to
  `{CAP_SETUID, CAP_SETGID}`, load-bearing UID floor (refuses uid OR gid
  < 1000). Every change to it is a security-review event.
- halmasuit itself runs as a hardened, deprivileged systemd service
  (`ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true` — i.e. a
  private mount namespace). This hardening is deliberate and is the crux of
  the current blocker (§5).
- No PAM bypass; no mocking PAM/renderer/DRM/input/clients in VM tests; goldens
  are human-eyeball-inspected, never CI-regenerated; the no-flash invariant is
  asserted over 100% of the `FrameRendered` stream, not sampled screenshots.

The canonical sources: `ARCHITECTURE.md` (design, threat model, roadmap),
`PLAN.md` (scope/in-out), `RESEARCH.md` (validated foundations), `CLAUDE.md`
(hard rules), `Justfile` (every command).

---

## 2. Original epic scope (immutable requirements)

Epic task **#1**. The compositor is built in layers **A → B → C → D → E → F →
G**, each gated by the previous. The immutable requirements (never watered
down):

- **Renderer**: real DRM scanout via `GlesRenderer` + `DrmCompositor`;
  z-ordered `wlr-layer-shell` compositing; brand clear-color `#0a0014` before
  any client commits.
- **`halmasuit-splash`**: separate wl_client, wgpu, one shader, a PNG as a
  fullscreen layer-shell BACKGROUND surface; image via `HALMASUIT_SPLASH_IMAGE`
  / `services.halmasuit.splashImage`.
- **Visual proof**: `frame_audit` Cargo feature gates per-frame
  `Event::FrameRendered` + the `Snapshot()` D-Bus method (production halmasuit
  has zero audit code — verified by `cargo tree`). Goldens in
  `tests/goldens/*.png`, compared with `ssimulacra2_rs` ≥ 90.0, human-inspected.
  Continuity invariant: from the first `ClientFirstFrame{Background}` onward
  every frame has `backdrop_coverage > 0.95` and none has `mean_luminance <
  0.01` — and this must hold across the **real** greeter→niri transition.
- **Input** (core, not feature-gated): real libinput via **libseat/seatd**
  (`LibSeatSession`), `wl_seat` keyboard+pointer, focus routed to the
  foreground client. No synthesized input in VM tests.
- **xdg-shell**: real `xdg_toplevel` fullscreen compositing (the session is an
  xdg toplevel).
- **Foreground state machine**: at most one foreground client above the
  persistent splash; greeter → (PAM success) → session; driven by the greetd
  lifecycle, not process identity; PID-continuity + no-flash preserved on the
  **real** path.
- **Real greeter + real session**: unmodified **DankGreeter** as
  `greeterCommand`; unmodified **niri** as the post-auth session, nested as a
  halmasuit xdg-toplevel client. Missing protocols are added to halmasuit —
  the clients are never patched.
- **Interactive bootable proof**: a documented `just` scenario, QEMU GTK
  window, boot → splash (user's image) → DankGreeter → typed credentials →
  niri, halmasuit-PID-continuous, no flash. Human-eyes, distinct from the
  automated gates.

Pin decision (resolved 2026-05-15, memory `g-layer-pins`): layer G uses the
user's **forks**, not upstream. `nix-config` pinned to `main`
(`github:joshsymonds/nix-config/main`); it transitively provides the real
stack — `niri-unstable = joshsymonds/niri`, `dms =
joshsymonds/DankMaterialShell` — via its `modules/desktop/dms-niri.nix`. The
`josh/integration` work lives on the *DMS/niri* repos, consumed through
nix-config's own inputs (not a nix-config branch).

---

## 3. How far we got — layer by layer

`just check` = **171/171** green (was 150/150 before the hardening fold +
security ports). **11 VM gates** green via `just test-vm` (plus the
`drm-master-probe` phase probes). Every layer below, the R1–R6+C3
hardening fold (reviewed clean — `gambit:review` epic #19), and the
`ab1eb13`/`f8e1321` security ports are committed on **`main`** @
`677d0ac` (see "Branch topology" in the header) and gate-verified.

| Layer | What | Gate(s) | Commit | Task |
|---|---|---|---|---|
| A | Visual-test infra: `visual.py`, `ssimulacra2_rs`, Snapshot()-based capture | (infra) | `b3f100d`… | #2 ✓ |
| B | Renderer: DRM+GBM+GlesRenderer+DrmCompositor, layer-shell, wl_shm, `#0a0014`, `frame_audit` split | `visual-halmasuit-clear/-layer/-splash` | `1a01f63`→`35c521e` | #3–8 ✓ |
| C | `halmasuit-splash` v1 (wgpu PNG layer-shell BACKGROUND) | `visual-halmasuit-splash` | `996f7c4` | #9 ✓ |
| D | `visual-backdrop.nix` 4 stand-in scenes + FrameRendered continuity invariant | `visual-backdrop` | `6b73459` | #10 ✓ |
| E1 | DRM acquisition via `LibSeatSession`/seatd | `drm-master-probe-phase4`, all visual | `482dc1c` (+`2b58e10`) | #11,#14 ✓ |
| E2 | libinput + `wl_seat` keyboard/pointer + focus routing | `halmasuit-input` | `745e8e9` | #15 ✓ |
| F1 | Real `xdg_toplevel` fullscreen compositing over splash | `visual-halmasuit-toplevel` | `64a18e0` | #12 ✓ |
| F2 | greeter→session foreground state machine; no-flash across the **real greetd** transition | `visual-foreground` | `a7078c2` | #16 ✓ |
| G0 | Pin nix-config→main (user's forks); real DMS stack boots | `smoke-boot` (real greetd+DankGreeter+niri+quickshell) | `1586aaa` | #17 ✓ |
| **G1** | Real niri nested as the session | — | **popped (see §5)** | #18 pending |
| G2–G4 | Real DankGreeter; full real-auth arc; interactive proof | — | not started | #13 (+ to-create) |

What F2 already proved (the no-flash thesis, on the real mechanism): real
greetd full-auth → `ForegroundChanged{session}` → halmasuit-spawn execs the
session → **halmasuit PID continuous across the real greeter→session swap**,
`FrameRendered` continuity OK across the whole transition, both scene goldens
matched. The only thing F2 used a stand-in for is the *content* of the two
endpoints (solid-color clients); the entire mechanism is real.

---

## 4. G1 attempt — what we learned (knowledge preserved, code discarded)

G1 (real niri as the session) was attempted and the WIP **deliberately
discarded** (`git restore`/clean — the test file is cheap to recreate and its
session-environment section will be rewritten by the §5 decision anyway). The
*findings* are the value and are durable in memory
(`session-spawn-namespace-handoff`, `g-layer-pins`) and here:

- **Real niri works nested under halmasuit.** niri's winit/GL backend
  connected to halmasuit and halmasuit composited it:
  `xdg_toplevel mapped as fullscreen foreground w:1280 h:800`. halmasuit's
  E/F protocol surface is sufficient for real niri's render path. **No
  halmasuit protocol gap** — the biggest rendering unknown of layer G is
  retired.
- niri runs correctly as the authenticated user (uid/euid/gid 1001) via
  halmasuit-spawn. The privilege drop is fine.
- Incidental, already understood: the session wrapper runs with **no PATH**
  (halmasuit-spawn's minimal env — by design); pass niri its config via
  `--config`, not via shell `mkdir`/`cp`. niri (a compositor even when nested)
  binds its **own** client socket in `XDG_RUNTIME_DIR` and needs a writable
  `$HOME`.

Then the blocker (§5) stopped it.

---

## 5. THE BLOCKER — session↔compositor namespace handoff

niri, as a child of the hardened `halmasuit.service`, inherits its
`ProtectHome=true` / `ProtectSystem=strict` / `PrivateTmp=true` **mount
namespace**. So `/home/alice` and `/run/user/1001` — created correctly on the
host — are invisible to the session. `setresuid` does not change the mount
namespace; fork/exec does not escape it. This is kernel-enforced, not a test
artifact. F2 never hit it because a solid-color stand-in needs neither a HOME
nor a runtime dir; the *real* session does.

This is exactly the **"production session→compositor handover" that
ARCHITECTURE/PLAN explicitly parked as a layer-G open decision.** Four research
agents (sshd, systemd-logind/pam_systemd, greetd/GDM/SDDM, namespace mechanics)
converged with source-level citations (full detail in memory
`session-spawn-namespace-handoff`):

1. **`pam_systemd`/logind is NOT a namespace-escape mechanism.**
   `CreateSession` does cgroup/scope placement and creates `/run/user/$UID`
   *in the PAM caller's existing mount namespace*. "Register with logind"
   alone would **not** fix this — `/run/user/$UID` would be created in the
   host ns, still invisible to a sandboxed niri.
2. **Every existing DM/sshd avoids the problem by not sandboxing the
   spawner** — they run as unsandboxed root in the host mount ns; only the
   leaf `execve`'d process is uid-dropped. greetd's own source comments that
   `pam_open_session`/`pam_close_session` *must* run as root in the host ns.
3. **Mechanism ranking** for halmasuit's constraints (keep the compositor
   hardened; keep `halmasuit-spawn` microscopic/forbid-unsafe/minimal-caps):
   - **D (recommended): split the unit.** A separate, deliberately
     *unsandboxed* privileged unit (`halmasuit-sessiond` /
     `halmasuit-session@<uid>.service`) that PID 1 starts → host namespace.
     Hardened halmasuit asks it, over SO_PEERCRED-authenticated IPC, **only
     after** the greetd state machine reaches PAM-verified `start_session`;
     that unit runs the `pam_systemd` tail and `execve`s niri in the host ns.
     Matches the `run0`/`machinectl`/logind precedent. halmasuit stays fully
     hardened; the privileged surface stays tiny and separately auditable; no
     `CAP_SYS_ADMIN` anywhere.
   - C: `StartTransientUnit` `.service` (not `.scope`) on the system bus —
     PID1-forked → host ns. Works; splits auth from spawn; complicates the
     greeter-kill / no-flash timing (session becomes PID 1's child).
   - B: don't sandbox halmasuit — rejected (it owns the GPU forever).
   - **A: `setns(/proc/1/ns/mnt)` in `halmasuit-spawn` — REJECTED.** Needs
     `CAP_SYS_ADMIN`+`CAP_SYS_CHROOT` in the helper, breaks forbid-unsafe,
     inverts the UID-floor threat model. Do not revisit.

**This is no longer a separate thread — it has been folded into §0.** The
realization (see §0.1) is that Mechanism D *is* the session-phase half of
the same `pam_handle_t` lifecycle as C1's auth-phase killable subprocess;
the §0.2 crux (one shared handle vs two) is the decision that resolves
*both*. Mechanism D's options C/D and the rejected-A `setns` ranking above
remain valid input to §0; **do not design Mechanism D in isolation — it is
§0.7 step 1, co-decided with the one-handle-vs-two crux.** Memory:
[[pam-killable-subprocess-direction]], `session-spawn-namespace-handoff`,
[[fvc-port-reconciliation]] (branch topology, now resolved).

---

## 6. Remaining work to land the epic

Strict order; each blocked by the previous:

1. **[BLOCKER] The entire §0 unified session/pamd epic.** Mechanism D
   (session-spawn namespace handoff) is its session-phase half; C1
   (killable PAM subprocess) its auth-phase half; the §0.2 crux resolves
   both. This subsumes what was previously listed here as a standalone
   handoff task. Until §0 lands, G1–G4 cannot run on the real path
   (the real session needs a real `$HOME`/`$XDG_RUNTIME_DIR`, which only
   the §0 handoff provides). *Do §0 before any G1 carve-out, so the
   flagship proof runs on the true production path with no synthetic
   `ProtectHome=false` to later unwind.*

   **Sidecar (independent of §0; can be done anytime):
   libseat-aware DRM-master-continuity assertion on `login-flash`.** The
   current `login-flash` proves *PID* continuity across greeter→session;
   it does NOT prove the compositor never internally drops/re-acquires
   DRM master (a real flash vector PID-continuity cannot see). The
   concept + a reference implementation live in commit `aa93f4f` on the
   retained `harden/phase-a-review` branch, BUT that implementation is
   wrong for v2: it greps debugfs for the *compositor PID* as DRM master,
   which never holds under libseat/seatd — **seatd** is the registered
   master and halmasuit holds the DRM fd *via* libseat. The rewrite must
   instead assert, across the transition: (a) `seatd` continuously holds
   DRM master, and (b) halmasuit's libseat session is never closed/
   reopened (no `CloseSession`/`OpenSession` churn in the seatd log, and
   halmasuit's seatd client id is stable). It modifies the canonical
   `login-flash` (CLAUDE.md hard-rule territory) — additive only, never
   weaken the existing PID assertion; re-gate full `just test-vm`. Delete
   `harden/phase-a-review` once this lands.
2. **G1 — real niri nested as the session** (task **#18**, reset to pending,
   redo from scratch). Rebuild `tests/visual-niri-session.nix` from
   `tests/visual-foreground.nix` (swap `sessionCmd` → real niri `--config
   <minimal kdl>`; niri pkg =
   `nix-config.inputs.niri-flake.packages.x86_64-linux.niri-unstable`; wire
   flake.nix check + Justfile). Session env flows through the decided handoff
   (no carve-out if D landed). Assert: real greetd auth → niri xdg_toplevel
   fullscreen foreground; niri marker (`pgrep -x niri`); PID-continuity across
   greeter→niri; `assert_frame_continuity`; Snapshot `niri-session` golden,
   Read-inspected before commit. Greeter stays the layer-shell stand-in.
3. **G2 — real DankGreeter as `greeterCommand`** (replaces the stand-in layer
   client). Talks greetd to halmasuit's socket; enumerate the wayland
   protocols it binds and add any halmasuit lacks (never patch DankGreeter).
   Golden inspected; real keyboard input → DankGreeter.
4. **G3 — full real-auth arc gate**: boot → splash(user image) → real
   DankGreeter over splash → emulated keystrokes type the test-user password →
   PAM success → halmasuit-spawn execs real niri → niri foreground.
   Continuity invariant + PID-continuity across the **real** DankGreeter→niri
   transition; niri marker; goldens (splash → DankGreeter → niri) inspected.
5. **G4 — interactive bootable proof + docs**: `just` recipe, QEMU GTK window,
   user's image, the full arc by hand; documented (how to pass the image, what
   you should see). Plus re-verify `cargo tree -p halmasuit` (no features)
   clean / production halmasuit zero `frame_audit`.
6. **Epic close**: task #13 is the layer-G acceptance gate (currently blocked
   by #18; will gain #-G2/G3/G4 blockers as those are created). When all G
   subtasks are green → `gambit:review` → `finishing-branch`. Do **not** flip
   any `login-flash` CI inversion sloppily — it is already green-by-design
   under v2; keep it a hard gate.

Task-tool state: those `#1/#13/#18` IDs belong to the *visual epic's*
task universe from the originating session — task IDs do NOT carry across
sessions, so a fresh context will not resolve them; treat the §3 table +
§6 list as the authoritative state, not the IDs. Conceptually: visual
epic open; layers A–F2 done; G1 (real niri) pending, redo from scratch,
blocked on §0; G2/G3/G4 not yet created (create iteratively as G1 lands).
**§0 (unified session/pamd epic) has no tasks yet — it is parked;
create its epic + first task via `gambit:brainstorming` on resume per
§0.7.** The integration that converged everything onto `main` was
tracked as epic #19 (this session) — reviewed clean and closed.

---

## 7. Pointers

- **Memory** (`/home/joshsymonds/.claude/projects/-home-joshsymonds-Personal-halmasuit/memory/`):
  `session-spawn-namespace-handoff` (the blocker + research + mechanism
  ranking), `g-layer-pins` (forks/main pin), `eyes-on-goldens`,
  `visual-gate-snapshot-decision`, `epic-expanded-greeter-session`,
  `pam-killable-subprocess-direction`, `fvc-port-reconciliation`.
- **Tests/gates**: `tests/*.nix`; the F2 harness `tests/visual-foreground.nix`
  is the template for G1–G3 (real greetd full-auth + Snapshot + continuity).
  `tests/smoke-boot.nix` shows how the real DMS stack is brought up via
  `${nix-config}/modules/desktop/dms-niri.nix` + `node.specialArgs.inputs`.
- **Commands**: `just check` (rustfmt+clippy+deny+machete+typos+nextest);
  `just test-vm` (the 11-gate sweep); `just test-vm-drive <name>` (interactive
  QEMU for debugging); `just update-goldens <name>` (human-in-the-loop golden
  regen — inspect every PNG before commit).
- **Spawn/PAM boundary** (the code the handoff reshapes):
  `crates/halmasuit-spawn/src/main.rs` (the comment block is its spec),
  `crates/halmasuit-greetd/` (the greetd state machine — the PAM-success
  gate), `nix/module.nix` (the hardened `halmasuit.service` —
  `ProtectHome`/`ProtectSystem`/`PrivateTmp` at ~324).
- **Hard rules** (`CLAUDE.md`): no PAM bypass; no client patching (fix
  halmasuit); halmasuit-spawn stays microscopic + forbid-unsafe + minimal
  caps + UID floor; no running halmasuit as root; goldens human-inspected; no
  weakening/skipping structural tests; no `--no-verify`/`--no-gpg-sign`.
