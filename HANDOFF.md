# halmasuit epic — HANDOFF

Status as of **2026-05-15**, branch **`feature/visual-compositor`**, HEAD
**`1586aaa`**. Working tree clean. This document is the cold-start brief for
whoever (or whichever session) resumes the "renderer → real session, no-flash
visual proof" epic. It is self-contained: read this and you know the why, the
original scope, exactly how far we got, the one thing blocking the finish, and
what remains.

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

`just check` = **150/150** green. **11 VM gates** green via `just test-vm`
(plus the `drm-master-probe` phase probes). Every layer below is committed on
`feature/visual-compositor` and has a passing gate.

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

**This decision is being made in a separate, PAM-adjacent session.** It is a
CLAUDE.md security-review event and it intersects two other in-flight
threads — memory `pam-killable-subprocess-direction` (PAM must be a killable
subprocess) and the hardening reconciliation on
`fold/visual-compositor-hardening` (memory `fvc-port-reconciliation`). The
session-launch path is common to all three; **design Mechanism D with those in
view, not in isolation.** Recommended to run it through `gambit:brainstorming`
given its size and cross-cutting/threat-model nature.

---

## 6. Remaining work to land the epic

Strict order; each blocked by the previous:

1. **[BLOCKER] Session-spawn namespace handoff** (Mechanism D). Its own task,
   decided/owned by the PAM-adjacent session. Reshapes the
   halmasuit-spawn/PAM/session boundary. Until this lands, G1–G4 cannot run on
   the real path. *Recommended: do this before any G1 carve-out, so the
   flagship proof runs on the true production path with no synthetic
   `ProtectHome=false` to later unwind.*
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

Task-tool state: epic **#1** open; #2–#12,#14–#17 completed; **#13** pending
(layer-G acceptance, blockedBy #18); **#18** pending (G1, redo from scratch,
blocked on the handoff). G2/G3/G4 not yet created — create them iteratively
(gambit:executing-plans style) as G1 lands, not all upfront.

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
