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

### 0.8 Epic close-out scope (2026-05-17, user-directed) — R3 relay + R10/R14/R15 deletions are IN this epic (Amendment A4; A3 RESCINDED)

The §0 epic's broker is **built, deployed (socket-activated host-ns
unit), and VM-proven end-to-end** (Amendments A1 — greetd session-spec
sequencing + pam_putenv/getenvlist-merge; A2 — single calloop
event-loop broker, idle-exit, evict-old reachable). The flagship gate
`session-onehandle` shows REAL `pam_mount` decrypting+mounting a LUKS
`$HOME` at `pam_open_session` using the auth-phase password recovered
from the SAME `pam_handle_t`. Gates: `run-pam-auth`, `session-r5r6`,
`session-onehandle` (all real PAM, no mocks).

**Decision (Amendment A4, controlling):** the compositor→broker relay
(R3) and the R14/R15/R10 close-gate DELETIONS — delete
`crates/halmasuit-pam`, delete the setuid `crates/halmasuit-spawn` +
its `security.wrappers` entry, remove `halmasuit-greetd`'s
`MAX_SESSION_BUILDS_PER_CONNECTION` — are **IN this epic's scope and
ARE preconditions for closing it.**

An earlier same-day decision (A3, "Option 1") deferred these to a
"successor G-layer / compositor-integration epic." **A3 is RESCINDED.**
The deferral was a false division: it leaves `halmasuit-pam`
(in-compositor PAM) and the broker's libpam FFI BOTH linked and live
at once — the forbidden two-libpam-surface anti-pattern — and it makes
the privilege boundary **unreviewable**, because the boundary's whole
security claim (the unprivileged compositor cannot reach the
credential; it only relays) cannot be evaluated when no relaying
consumer exists. Deferring also ships C1 (the in-compositor PAM vuln)
indefinitely under a "deferred" label. "Rip the old path out now
without the relay" was rejected (breaks the compositor, nothing
replacing it). "Harden the in-compositor path" was rejected (its
insecurity is structural, not a bug — the only fix is replacement, =
the broker). The R10/R14/R15 discipline ("deleted atomically WITH the
replacement landing, never before, never left behind after") is
satisfied by making the replacement land HERE.

**Remaining work in THIS epic (tasks created iteratively):**

1. **R3 compositor→broker relay.** `halmasuit-greetd`'s greetd state
   machine stops calling `halmasuit-pam` in-process and instead
   connects to the `halmasuit-session` socket, sends `BeginAuth`, and
   relays `ConvPrompt`/`ConvResponse`/`Success`/`StartSession`/
   `Cancel` over the frozen `halmasuit-session-ipc` contract. This
   makes the broker the LIVE auth/session path. No `_v2`/shim — the
   in-process call site is replaced, not duplicated.
2. **Atomic deletions, the moment #1 lands (R10/R14/R15):** delete
   `crates/halmasuit-pam`; delete `crates/halmasuit-spawn` + its
   `security.wrappers`/setuid install; remove `halmasuit-greetd`'s
   `MAX_SESSION_BUILDS_PER_CONNECTION` +
   `CodecError::SessionBuildLimitExceeded`. Then `cargo tree -p
   halmasuit` shows no `pam-sys`; no world-exec setuid spawn inode
   exists anywhere.
3. **`login-flash` through the new path.** PID-continuity AND
   `assert_frame_continuity` across greeter→session with the broker
   launching the session; `login-flash` stays a HARD gate, unmodified.
4. **R13 docs re-sweep.** The R13 pass done under A3 described the
   helper as "interim / scheduled for deletion by a successor epic."
   With A3 rescinded, re-sweep ARCHITECTURE.md/CLAUDE.md so the broker
   is described as THE path and `halmasuit-pam` + setuid
   `halmasuit-spawn` as DELETED — no interim/scheduled language.
5. R9's compositor-SIGCHLD-reaper clause stays superseded by the
   broker's `AuthSlot`-owned synchronous reaping (independently true);
   it disappears with `halmasuit-pam`'s deletion in step 2.

**This epic closes on:** R3 relay landed (broker is the live path) +
the R10/R14/R15 atomic deletions done + `cargo tree -p halmasuit`
shows no `pam-sys` + no setuid spawn inode + `just check` green +
`just test-vm` green INCLUDING `login-flash` through the
broker-launched session + the three real-PAM broker gates green + R13
docs re-swept + the two-tier `gambit:review` (Tier 1 whole-epic + Tier
2 deep adversarial security) both pass. The review runs only AFTER R3
+ the deletions land — auditing the boundary before its consumer
exists is the failure A4 corrects.

Recorded in epic Task #1 as **Amendment A4** (controlling; A3 marked
RESCINDED). Do NOT re-introduce the A3 deferral.

### 0.9 Broker→compositor session-lifecycle signalling (2026-05-17, user-directed) — Amendment A5

R3 (the compositor→broker relay) raised one design point not pinned by
the immutable requirements: under R7 the broker (not the compositor)
owns and parents the session leader, so the unprivileged compositor
must learn "session ready" / "session ended" to do the flash-free
`wl_client` swap and revert — without being the leader's parent or the
credential holder. Resolved like A1/A2: three independent BLIND
primary-source agents, different angles (privsep-login lineage
greetd/GDM/OpenSSH; system-compositor lineage Mir+USC/LightDM/
gamescope; kernel + systemd-logind + pidfd threat model). Strong
convergence + one load-bearing sharpening only the system-compositor
agent surfaced.

**Decision (Amendment A5):**

1. **One-way trust, broker→compositor only.** Session-lifecycle
   frames exist ONLY in `BrokerToCompositor`, NEVER in
   `CompositorToBroker`. The compositor emits no lifecycle frame
   ever; it is a pure sink. Structural (the type only deserializes
   on the privileged-emit side — greetd `SessionChildToParent` /
   GDM private worker→daemon bus), SO_PEERCRED-gated to the root
   broker peer (authenticates, never authorizes). This structurally
   kills (a) a compromised greeter/client forging a force-logout and
   (b) forging "session ready" to phish the post-login surface.
2. **Typed, outcome-bearing frame pair** added to
   `halmasuit-session-ipc` `BrokerToCompositor`: `SessionOpened` and
   `SessionEnded { Exited(code) | Signaled(signo) }`. Keep the
   crash-vs-clean distinction (GDM `SESSION_EXITED`/`SESSION_DIED`;
   do NOT collapse like greetd) — the compositor uses it for revert
   UX/policy. The frame carries NO raw pid.
3. **Two-key flash-free swap (load-bearing — the project's reason to
   exist).** `SessionOpened` *authorizes/names* the session; the
   actual VISIBLE greeter→session swap fires only on
   AND(`SessionOpened` received, compositor's own in-process
   observation of the session Wayland client's first committed
   buffer of non-zero size). The greeter stays visible underneath
   until that first real frame. Swapping on `SessionOpened` alone
   reintroduces the exact flash halmasuit deletes (Mir/USC
   `is_session_ready_for_display = session && ready`,
   `WindowWlSurfaceRole` first-non-empty-buffer gate). The trust
   model (who has authority) is unchanged by this; it governs only
   WHEN the swap becomes visible.
4. **Broker is the sole reaper.** It parents the leader, `waitpid`s
   it, then `close_session`/`pam_end` (R7/R9/#16/#25). The
   compositor is never the parent, holds no pid, never reaps
   (`waitid` would `ECHILD`).
5. **Revert** to greeter on (`SessionEnded` frame) OR (session
   Wayland client disconnect). **Socket-close** = secondary
   broker-crash backstop only, never the primary signal (loses the
   outcome; conflates "broker died" with "session ended").
6. **pidfd backstop (IN scope, user-directed).** The broker passes a
   **poll-only** pidfd of the leader to the compositor via
   SCM_RIGHTS; the compositor adds it to its calloop set and treats
   `EPOLLIN` purely as a zero-latency, pid-reuse-immune,
   broker-crash-resilient hint to start the revert. The compositor
   MUST NOT `waitid`/reap it (not its child; `ECHILD`) and MUST NOT
   `pidfd_send_signal` it. Authoritative signal is still the
   `SessionEnded` frame; the pidfd is a latency/robustness
   accelerator. Matches the existing `project-pidfd-over-raw-kill`
   memory.
7. **logind D-Bus `SessionRemoved` is NOT the compositor's cue** —
   unspoofable but trailing-edge (emitted in `session_finalize`
   after full scope teardown) and pulls in a zbus/D-Bus dep the
   project constrains. Broker-internal corroboration at most.

This is a minimal, primary-source-justified extension of the
otherwise-frozen `halmasuit-session-ipc` contract, in-scope under A4.
The R3 step-1 pure adapter (task #27) is built anticipating the two
new inbound `BrokerToCompositor` variants; the two-key swap +
SCM_RIGHTS pidfd land in the later R3 socket-wiring step. Recorded in
epic Task #1 as **Amendment A5**.

### 0.10 Broker-socket ownership across the auth→session boundary (2026-05-17, user-directed) — Amendment A6

R3 step 3 surfaced a structural question the requirements did not pin:
greetd's `Connection` is generic and, at `SessionState::Spawning`,
DROPS the per-auth `PamSession` and yields `SpawnRequest{cmd,env}` —
designed for the old model where in-process PAM auth is *finished* at
that point. But the broker holds ONE PAM handle across auth AND
session (R1) and, per A1, is BLOCKING to read the session spec on the
same SEQPACKET socket *after* auth-success. So the broker socket must
live the whole episode (BeginAuth → conv → Success → StartSession →
SessionOpened → SessionEnded), past the point greetd tears its
per-auth object down. Resolved like A1/A2/A5: three independent BLIND
primary-source agents, different angles (privsep login daemons
greetd/OpenSSH/GDM; the sans-IO doctrine + rustls/quinn/h2; kernel
fd-ownership/close-EOF semantics + the OpenSSH-privsep CVE record).
**Unanimous, strong convergence — and it overturned the initial
dup-fd lean.**

**Decision (Amendment A6):**

A6.1 — **Single owner.** The one unprivileged↔broker SEQPACKET socket
is owned by the compositor's per-greeter-connection EPISODE object
(`ConnState`-scoped) for the ENTIRE episode (auth → `StartSession` →
`SessionOpened`/`SessionEnded` → close). Exactly one owner; exactly
one writer to the privilege-crossing channel at a time; one audit
point.

A6.2 — **Sans-IO auth driver; borrow, never own.** The greetd auth
state machine is sans-IO and owns NOTHING transport. The per-auth
driver is *fed* / *borrows* the broker channel; its drop at the
auth→session boundary is inert because it holds no fd. "A per-auth
object owns the privileged socket and is dropped at auth-success" is
the canonical sans-IO anti-pattern (the literal httplib-baked-socket
mistake) and is FORBIDDEN. Prior art is unanimous: greetd reuses ONE
`Session` object `mem::swap`'d across the boundary (never
dropped/recreated; auth state is a payload-only enum owning no fd);
OpenSSH models auth→postauth as a dispatch-table swap on ONE
persistent monitor channel; GDM's long-lived object is *named*
"conversation" yet is session-scoped (the naming trap).

A6.3 — **`dup`, `Rc<RefCell>`, `Arc<Mutex>` for this socket are
REJECTED.** `dup` avoids the premature-EOF (last-close semantics) but
adds a second writable fd to a privilege-crossing channel + a
double-close obligation (least-authority violation). `Rc`/`Arc`
reintroduce the premature-close hazard (last drop closes → broker
gets EOF mid-PAM-handle → locked session, no diagnostic — the §0.2
split-lifecycle failure). Single-owner-borrow makes that failure
*structurally impossible*, not runtime-contingent. Backed by
last-close EOF semantics (`close(2)`/`unix(7)`) and the OpenSSH PAM-
monitor lifetime/authority CVE pair **CVE-2015-6563 / CVE-2015-6564**
(unprivileged-side object lifetime must never implicitly control the
privileged PAM handle's fate).

A6.4 — **greetd seam consequence (in scope for R3).** Because
greetd's `PamSessionFactory → 'static Box<dyn PamSession + Send>`
cannot carry a borrow, the clean fix is a small greetd-seam change so
the auth driver is *fed* the transport by `&mut` (the episode layer
owns it) rather than owning a boxed session. This makes greetd MORE
sans-IO — it strengthens R3/R12 (the generic/pure posture), it does
not weaken them. The frozen `halmasuit-session-ipc` contract is
unaffected (this is F-internal ownership only).

A6.5 — Consequences: task #29's `BrokerSession` (which OWNS the
`SeqpacketChannel`) is the identified smell; it is revised to a
sans-IO auth driver the episode layer feeds (borrow, not own). R3
step 3 (#30, the atomic make-live + R10/R14/R15 deletions) inherits
the single-owner discipline. invariant: **transport lifetime ≥
PAM-handle lifetime ≥ auth-state lifetime**; the shortest-lived
(auth) owns the least (nothing transport).

Recorded in epic Task #1 as **Amendment A6**. DO NOT reintroduce a
per-auth/`PamSession`-object that owns the broker fd, nor
dup/Rc/Arc multi-ownership of that socket.

### 0.11 Compositor↔broker auth must not block the render loop — fully sans-IO (2026-05-17, user-directed) — Amendment A7

R3 step 3 surfaced a fork the requirements/A6 did not pin: A6 fixed
socket *ownership* (episode owns it; auth driver borrows). It did NOT
fix the *I/O integration*: greetd's `PamSession::step` is a
SYNCHRONOUS contract, so `BrokerAuthDriver::step` does a BLOCKING
`recv` on the privileged broker socket — and that recv is reached
from `handle_connection_ready` on the SAME single calloop
`EventLoop` that owns the DRM `VBlank`/render source. That stalls the
continuously-presenting compositor on a privileged-peer round-trip —
against the project's defining no-flash invariant. Resolved like
A1/A2/A5/A6: three independent BLIND primary-source agents, different
angles (privsep login daemons greetd-daemon/GDM/LightDM event-loop ↔
auth integration; the sans-IO suspend/resume doctrine +
h11/h2/quinn-proto/rustls; continuous-display robustness + the
kernel recv/timeout/EOF semantics + Mir/USC/gamescope/wlroots/calloop
source). **Unanimous, strong convergence — and it OVERTURNED the
"brief blocking recv is probably fine" lean.**

**Decision (Amendment A7):**

A7.1 — **The compositor calloop thread NEVER does a blocking `recv`
on the broker socket — not brief, not with a timeout.** A recv
timeout only trades an unbounded stall for a bounded multi-vblank
stall; it does not classify peer state and adds an
`SO_RCVTIMEO`/`EINTR` hazard. Mir hit exactly this (synchronous
protocol IPC on the compositor thread, ~1 ms/op measured); the
upstream fix branch was literally named
`no-ipc-on-compositor-threads` (Launchpad #1395421). libwayland/
wlroots/**calloop** all enforce "one poll point; every fd a
non-blocking source."

A7.2 — **The broker SEQPACKET fd is a per-greeter-connection calloop
source** (set non-blocking, exactly like the greetd listener already
is). Episode-owned (A6). Readable → ONE non-blocking framed recv →
feed the relay → resume the greetd state machine. The auth
conversation is driven by the loop multiplexing {greeter fd, broker
fd}, never by a blocking call.

A7.3 — **greetd's PAM boundary becomes fully sans-IO
(emit/suspend/resume).** `Connection`/`SessionState` no longer take a
blocking `&mut dyn PamSession`. When a PAM round is needed the state
machine EMITS the outbound broker frame and SUSPENDS (returns a
"awaiting broker response" signal + the bytes to send to the broker);
the compositor episode loop owns BOTH the greeter fd and the broker
fd as sources and shuttles bytes; the state machine RESUMES when the
broker response is fed in. This is the canonical sans-IO shape
(h11 `NEED_DATA` / h2 `receive_data`+`data_to_send` / quinn-proto
`handle_event`+`poll_transmit` / rustls `read_tls`+
`process_new_packets`); a synchronous `step()` that does send +
blocking recv internally is the explicitly-named sans-IO
anti-pattern ("httplib baked socket calls into the parser"). This
SUPERSEDES the A6-era greetd seam (synchronous fed
`process(&mut dyn PamSession)`): A6 relocated ownership, A7 makes the
effect itself non-blocking. The factory/cap removal already done
stands; the `process` *contract* goes further.

A7.4 — **Killable-peer-as-source-event.** The broker is evicted/
SIGKILLed BY DESIGN (R5). Under A7 peer death is a readable→EOF
event on the broker source → existing `WireError::Closed` →
fail-closed greetd auth failure, handled identically to any other
source event (NOT a blocking-recv return value). This makes the
unprivileged render loop structurally robust against a slow / wedged
/ killed PRIVILEGED peer — the lower-risk, more auditable shape.

A7.5 — **`BrokerAuthDriver`'s synchronous `PamSession::step`
(send + blocking recv) is REMOVED.** `BrokerRelay` (#27, already a
pure phase machine) is retained as the pure translation, driven by
the two calloop sources; `BrokerRelay::poison` (the fail-closed
latch) is retained. greetd/GDM/LightDM are unanimous: multiplex
where there's other work (the compositor — rendering), block only in
the dedicated worker that has nothing else to do (the broker's
ephemeral auth fork — already correct).

A7.6 — Consequences (tasks adapt): #30 is rescoped to the sans-IO
greetd boundary; the A6-era greetd `process(&mut dyn PamSession)`
seam is superseded by the emit/suspend/resume contract; #29's
`BrokerAuthDriver` is removed (its job moves into the loop-driven
relay). The no-flash invariant is now STRUCTURALLY protected — the
compositor cannot stall on the broker.

Recorded in epic Task #1 as **Amendment A7**. DO NOT put a blocking
`recv`/`send`-then-`recv` on the compositor render/calloop thread for
broker IPC, with or without a timeout; DO NOT re-introduce a
synchronous blocking `PamSession::step` effect.

### 0.12 Broker fd as a calloop source without dup/Rc/Arc — borrowed-fd source + episode-owned channel (2026-05-17, user-directed) — Amendment A8

R3 #31/S1 bite 3 surfaced an unpinned integration fork A6/A7 did not
resolve: calloop's `Generic<F: AsFd>` takes `F` BY VALUE and (only)
the value's destructor closes the fd — so making the episode the sole
owner of the broker socket (A6) while calloop watches it for readiness
(A7) is not the default `Generic<OwnedFd>` shape. Resolved like
A1/A2/A5/A6/A7: three independent BLIND primary-source agents
(calloop API/source + `NoIoDrop`/`insert_source` bounds; production
Smithay/anvil/niri per-connection-fd idiom; the cross-source +
privilege-boundary close/dup discipline with sans-IO doctrine).
**Strong convergence, and it surfaced an in-repo defect.**

**Decision (Amendment A8):**

A8.1 — **The episode (`ConnState`/`BrokerEpisode`) is the SOLE owner
of the broker `SeqpacketChannel` (`OwnedFd`) for the whole episode
(A6).** calloop watches it via a `Generic` over a NON-OWNING borrowing
newtype: `struct …Fd(RawFd); impl AsFd { unsafe BorrowedFd::borrow_raw }`,
**no `Drop`**. calloop verified to only ever call `as_fd()` for
register/reregister/unregister and to NEVER `close(2)` (its `Drop`
only `poller.delete`s). `insert_source`'s bound is `S: EventSource +
'l` (NOT `'static`); a `RawFd`-holding newtype satisfies it trivially.
This is permitted by calloop's contract but NOT in its cookbook → it
is a code-review/test invariant, not a type guarantee.

A8.2 — **Close-exactly-once discipline:** the borrowing source's
`RegistrationToken` is `loop_handle.remove`d BEFORE the `ConnState`
(hence the `OwnedFd`) is dropped. That ordering is what makes the
`unsafe BorrowedFd::borrow_raw` sound (epoll never references a
closed/stale fd) and guarantees the single `close(2)` is the episode's
`OwnedFd::drop`. Asserted by test, not assumed.

A8.3 — **NO `dup`/`try_clone_to_owned`/`Rc`/`Arc` of the
privilege-crossing fd** (A6 restated, now with the calloop mechanism
that obviates it). `dup` of a privsep socket = premature-/no-EOF +
second writer + reused-fd double-close (close(2)/dup(2) semantics;
OpenSSH privsep rationale; CVE-2015-6563/6564 class). The borrowing
source needs no dup.

A8.4 — **Cross-source coupling goes through the shared `&mut State`,
never source-to-source.** Both calloop sources (greeter
`Generic<UnixStream>`, broker borrowed-fd `Generic`) carry
`&mut HalmasuitState` and reach `state.connections[id]`; the greeter
callback's `Demand::Pam` does the one non-blocking `chan.send`
directly on the episode-owned channel; the broker callback does the
one non-blocking `chan.recv`. SEQPACKET = one datagram per logical
message ⇒ NO outbox / `Ping` / `insert_idle` / combined-source
machinery (those solve partial-write / cross-thread / non-fd problems
this shape does not have). `MSG_DONTWAIT` both directions makes the
no-block invariant (A7) structural even though the channel lives in
the episode (the sans-IO "pure machine, reactor syscalls" ideal is
not required here — calloop-reported readiness + SEQPACKET atomicity +
`MSG_DONTWAIT` is the defensible, idiomatic shape; pattern (a)).

A8.5 — **Consequence: #31/S1 bite 2's `BrokerEpisode` is CORRECT
as-is** (owns the channel, `MSG_DONTWAIT` I/O in the drive methods);
bite 3 must register the broker fd via the borrowing newtype (NOT move
the channel into `Generic`, NOT dup).

A8.6 — **In-repo defect found (broker side, separate task):** the
already-landed `crates/halmasuit-session/src/broker.rs` (#22 calloop
broker loop) registers the calloop readiness source over a
`try_clone_to_owned()` **dup of the privilege-crossing worker fd**
(and the greeter fd) — the exact A8.3/A6 anti-pattern, on the
PRIVILEGED side. Fix tracked as a dedicated task (borrowed-fd source;
single `InFlight`-owned close), in scope for this epic's two-tier
review at minimum.

Recorded in epic Task #1 as **Amendment A8**. DO NOT dup/Rc/Arc the
broker fd for the calloop source; DO NOT move the episode's
`SeqpacketChannel` into `Generic`; DO NOT add an outbox/Ping/idle for
the SEQPACKET write path; DO remove the source token before the
episode/`OwnedFd` drops (assert it).

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
