# halmasuit epic — HANDOFF

Canonical branch **`main`** @ `17ae692`, pushed to `origin/main`. This
document is the self-contained cold-start brief for resuming halmasuit.

**State.** The unified **session/pamd privilege-separation epic** (§0) is
**SHIPPED** — 45 commits, fast-forward-merged to `main`, two-tier
`gambit:review` APPROVED (round 4, zero gaps; it caught and closed a real
privilege escalation, Amendment A9 / §0.13). The privileged
`halmasuit-session` broker is the **live** auth/session path; the
in-compositor `halmasuit-pam` and the setuid `halmasuit-spawn` are
**deleted**. Gates green: `just check` 244/244 + `r14-gate`;
`just test-vm` 14/14 incl. `login-flash` through the broker-launched
session and the three real-PAM broker gates (`run-pam-auth`,
`session-r5r6`, `session-onehandle`).

**Read order.** §0 is now the **decision record** for the shipped
privilege-separation work — the rationale and the DO-NOT-REVISIT
conditions (CLAUDE.md cites §0.7–§0.13 as canonical). §§1–4 describe what
halmasuit is and the visual-compositor layers already landed (A–F2, G0).
§5 records that the namespace-handoff blocker is **resolved** (Mechanism
D shipped *as* the broker). **§6 is the live work queue: the visual
G-layer (G1–G4) is what remains** — it was the thing §0 unblocked.

**Branch topology.** Collapsed and resolved. `main` carries everything;
`feature/session-pamd` was merged and deleted, `feature/visual-compositor`
/ `fold/...` superseded. **`harden/phase-a-review` is retained** for one
unique commit, `aa93f4f` (a login-flash DRM-master-continuity assertion).
That implementation is wrong for v2 — it greps debugfs for the compositor
PID as DRM master, false-by-construction under the libseat/seatd model
(seatd is the registered master; halmasuit holds the DRM fd *via*
libseat). The concept is valuable; the rewrite is the §6 sidecar and the
branch stays the reference impl until it lands.

---

## 0. Decision record — Unified session/pamd epic (SHIPPED)

**Status: SHIPPED** and merged to `main` @ `17ae692`. This section is the
canonical record of *why* the privileged broker is shaped the way it is
and *what must not be revisited*. The locked decisions (§0.3) and
Amendments A1–A9 (§0.8–§0.13) are immutable unless their stated
DO-NOT-REVISIT condition changes; CLAUDE.md's hard rules are the
enforced subset. The narrative below is preserved as the derivation — do
not re-derive it.

### 0.1 The core realization (the expensive insight — do not re-derive)

C1 ("PAM must be a killable subprocess") and Mechanism D ("session-spawn
namespace handoff", §5) were **not two problems — they are two ends of
one `pam_handle_t` lifecycle**:

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
They were co-designed as one epic.

### 0.2 The crux — RESOLVED: one shared handle, owned by the broker (R1)

Credential-passing PAM modules (pam_mount unlocking an encrypted `$HOME`,
pam_gnome_keyring, pam_krb5) require **the same `pam_handle_t` to span
`authenticate` → `open_session`** (greetd's `worker.rs` comment is the
canonical statement; confirmed by the research).

**Resolved: one shared handle.** The privileged `halmasuit-session`
broker owns ONE `pam_handle_t` for the whole lifecycle (auth → setcred →
open_session → … → close_session → pam_end) and never `execve`s between
auth and session. The "killable, SIGKILL-anytime" property of C1 is
satisfied **not** by making the handle-owner killable but by running
`pam_authenticate` in an **ephemeral SIGKILL-able `setrlimit`-bounded
privileged fork** that reports the result back to the handle-owning
parent (R5/R7). The two-handle option was rejected: it silently breaks
credential-passing modules (locked `$HOME`, no error). This is the
greetd/OpenSSH/GDM/su shape. Codified as **R1** in CLAUDE.md's hard
rules; DO NOT REVISIT unless R1's one-handle-in-parent model is itself
rescinded (see §0.13/A9.5).

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

### 0.7 What shipped (the realized crate layout)

The epic landed exactly the locked decisions of §0.3 with Amendments
A1–A9. As built:

- **`halmasuit-session`** (NEW, privileged) — the host-ns broker. Owns
  the one `pam_handle_t` (R1); runs `pam_authenticate` in an ephemeral
  SIGKILL-able `setrlimit`-bounded privileged fork (R5); forks once and
  drops privileges in a **non-setuid** session-leader child (R7/R11,
  already root — no setuid binary); single calloop event-loop broker,
  socket-activated, idle-exits, evict-old slot (A2); relay-peer
  `SO_PEERCRED` gate (R8). `unsafe` confined to its `pam_ffi`/`worker`
  modules. Module doc in `src/broker.rs` is the authoritative spec.
- **`halmasuit-session-ipc`** (NEW, pure) — the frozen wire contract
  (types + codec only) for the compositor↔broker relay, incl. the A5
  one-way `BrokerToCompositor` lifecycle frames.
- **`halmasuit-greetd`** — its greetd state machine is now fully sans-IO
  (emit/suspend/resume, A7); no in-process PAM; `MAX_SESSION_BUILDS_PER_
  CONNECTION` + `CodecError::SessionBuildLimitExceeded` removed (R6/R14).
- **`halmasuit`** — `broker_session.rs` (the per-greeter `BrokerEpisode`,
  sole `OwnedFd` owner of the broker socket, A6/A8), `broker_relay.rs`
  (pure relay phase machine), `swap_gate.rs` (the A5 two-key flash-free
  greeter→session swap). Post-drop bounding set empty; keeps only
  `CAP_KILL` (R15).
- **DELETED:** `crates/halmasuit-pam`, the setuid `crates/halmasuit-spawn`
  + its `security.wrappers` entry, `tests/halmasuit-spawn.nix` (R10/R14/
  R15, atomic with the broker going live).
- **`nix/module.nix`** — the socket-activated `halmasuit-session` unit;
  no setuid inode in the closure; `r14-gate` in `just check` fails the
  build if the compositor ever transitively links `pam-sys`.

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
   `assert_no_flash_stream` across greeter→session with the broker
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

### 0.13 Session-leader supplementary groups derive from the PAM-resolved user, never the handle-owner's `getgroups()` (2026-05-17, user-directed) — Amendment A9 (CONTROLLING; SUPERSEDES the R7/R11 "getgrouplist-MERGE the established supplementary set / blind initgroups is forbidden" clause)

**Surfaced by:** the epic close-gate two-tier `gambit:review` (round 3),
Security reviewer + verifier (high confidence), then resolved with two
independent blind primary-source research agents that converged with
zero divergence.

**The defect.** The handle-owning broker process runs with its own
supplementary group `shadow` (deliberate and load-bearing — lets
pam_unix's `getspnam` fast-path read `/etc/shadow` in-process, avoiding
the fragile `unix_chkpwd` helper fork under the sandboxed unit; see the
standing `project-pam-unix-shadow-group` rationale). The shipped code
(`session.rs` `nix::unistd::getgroups()` → `merged_groups` →
`worker.rs` `setgroups`) derived the fork-drop child's supplementary
set from the **broker's own post-`setcred` `getgroups()`** and unioned
it, unfiltered, onto the session leader. Net: **every authenticated
user session received the `shadow` group → world-readable `/etc/shadow`
→ offline crack of all password hashes incl. root.** This negates the
epic's core threat-model promise ("a compromised/abused halmasuit is
not a root compromise"). The flagship gate #24
(`tests/session-onehandle.nix`) did not catch it — it *asserted it as
correct* (`grep -qw shadow /tmp/oh/leader-id`), having been written on
the same wrong model.

**Why R7/R11's group clause was wrong (primary sources).** R7/R11 said
the leader's groups must be the "getgrouplist-MERGE of the
PAM-established supplementary set" and that "blind `initgroups`" is the
forbidden anti-pattern. The PAM group contract is the inverse:

- `pam_setcred(3)` (man7.org, verbatim): *"these properties (along
  with the default supplementary groups of which the user is a member)
  are credentials that should be set directly by the application and
  not by PAM. Such credentials should be established, by the
  application, prior to a call to this function. For example,
  `initgroups(2)` (or equivalent) should have been performed."* PAM
  assigns the supplementary-group base to the **application, via
  `initgroups`/`getgrouplist`, from the user's identity** — and there
  is **no `pam_getgrouplist`** (no group analogue of
  `pam_getenvlist`); module-added groups are only a side effect on the
  calling process's credential set.
- OpenSSH `do_setusercontext()`: `initgroups(pw->pw_name, pw->pw_gid)`
  — user identity only, never the daemon's `getgroups()`.
- util-linux `login(1)` `init_groups()`:
  `initgroups(cxt.username, pwd->pw_gid)`, with the in-source comment
  *"This should be done before pam_setcred, because PAM modules might
  add groups during that call."* Root → `setgroups(0, NULL)`, never
  inherits login's own set.
- GDM `gdm-session-worker.c`: `initgroups(worker->username, gid)`
  before `pam_setcred`. Same pattern.
- **CVE-2021-41617 (OpenSSH ≤8.7)** and **sddm #1159** are this exact
  bug — a privileged daemon's supplementary groups leaking into a
  process running as another user, classified as privilege escalation.
  sddm maintainers, verbatim: *"`getgroups` simply retrieves the
  groups of the current user, which is `root` for `sddm-helper`."*
  The sanctioned fix in both: derive from the **target user's
  identity** via `initgroups`/`getgrouplist`.

"Capture the handle-owner's `getgroups()` post-`setcred` and graft it
onto the user" is not a pattern with tradeoffs — it is a named CVE
class. `pam_group`-via-`setcred` conditional grants land in whichever
process runs `pam_setcred`; under R1 that is the privileged broker
parent (which carries `shadow`), so those grants are *inseparable* from
the broker's own groups and cannot be preserved without leaking. They
are therefore out of scope (no test or stated requirement needs them;
the only thing that ever rode that path was the leak). The base-layer
principle is the same one R8 already enforces for uid/gid/username: **a
privileged launcher derives a principal's credentials from that
principal's identity, never from its own process state.** A9 brings
group derivation back into line with that invariant; it is the
canonical prior-art shape, not a patch.

**Decision (Amendment A9, controlling — SUPERSEDES R7/R11 group
clause):**

A9.1 — The session leader's supplementary groups are
`getgrouplist(PAM-resolved username, PAM-resolved primary gid)` ONLY,
computed in the fork-drop child from the R8 PAM-derived identity. The
handle-owner's own `getgroups()` is NEVER a source. `setgroups` of that
user-derived list happens in the child while still privileged, before
`setresuid` (the existing R7 ordering slot that previously applied the
merged list).

A9.2 — R7/R11's "getgrouplist-MERGE the PAM-established supplementary
set" requirement and the "blind `initgroups` is the forbidden
anti-pattern" framing are RESCINDED. `initgroups`-equivalent
(`getgrouplist(user)` + `setgroups`) from the resolved identity is the
*correct, mandated* behavior. `pam_group`/`group.conf` conditional
grants are explicitly OUT OF SCOPE under the one-handle-in-parent
architecture (R1).

A9.3 — The broker continuing to carry `shadow` is, with A9.1 in place,
structurally irrelevant to the session (the child never reads the
parent's group set). Eliminating `shadow` from the broker (e.g.
reverting to the `unix_chkpwd` helper) is a SEPARATE, OPTIONAL
defense-in-depth question with its own cost (helper-fork fragility
under the sandboxed unit — the original reason `shadow` was added) and
is explicitly NOT part of this fix and NOT a blocker.

A9.4 — Gate #24 (`tests/session-onehandle.nix`): the
`grep -qw shadow /tmp/oh/leader-id` assertion and its rationale block
are DELETED and replaced with the regression invariant — `shadow` is
**absent** from the leader, the user's static NSS groups are
**present**. The flagship purpose of #24 (the real pam_mount LUKS
one-handle proof + the A1.3 `LANG` env survival) is UNCHANGED; only the
wrong secondary group assertion flips.

A9.5 — R1 (one handle, one process, owner never execs), R7's
fork-then-drop child syscall sequence, R8 (PAM-resolved + re-verified
identity), A1's env-merge, and the §0.2 one-handle pam_mount proof are
ALL unchanged. A9 is surgical: it corrects one requirement clause that
was written on a wrong model of the PAM group contract.

Recorded in epic Task #1 as **Amendment A9**. DO NOT derive the session
leader's supplementary groups from the handle-owner's `getgroups()`;
DO NOT reintroduce a "merge the broker's established set" path; DO NOT
treat `initgroups`/`getgrouplist(user)` as a forbidden anti-pattern
(A9 makes it mandatory). DO NOT REVISIT unless the architecture stops
running `pam_setcred` in a process distinct from the session leader
(i.e. only if R1's one-handle-in-parent model is itself rescinded).

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

- halmasuit composites. The background is its internal witness plane —
  halmasuit decodes the configured PNG and composites it itself from
  frame 0 (no separate client); the greeter and niri are unmodified
  upstream clients.
- The single privileged surface is the `halmasuit-session` broker (§0):
  it owns one `pam_handle_t` for the whole lifecycle, runs
  `pam_authenticate` in an ephemeral SIGKILL-able fork, and launches the
  session by forking once and dropping privileges in a **non-setuid**
  child (already root — no setuid binary anywhere). `unsafe` is confined
  to its `pam_ffi`/`worker` modules; the load-bearing UID floor (refuses
  uid OR gid < ~1000) lives in its session-leader child. Every change to
  its privileged boundary is a security-review event.
- halmasuit itself runs as a hardened, deprivileged systemd service
  (`ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true` — i.e. a
  private mount namespace) holding no credentials and no escalation
  capability (post-drop bounding set empty; keeps only `CAP_KILL`). This
  hardening is what made the namespace handoff a problem; §0's broker
  (host-ns, separate unit) resolved it (§5).
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
- **Witness plane**: halmasuit decodes a configured PNG and composites
  it itself as its bottom-most internal background plane from frame 0
  (no separate client); image via `HALMASUIT_WITNESS_IMAGE` /
  `services.halmasuit.witnessImage`.
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

`just check` = **244/244** green incl. `r14-gate`. **14 VM gates** green
via `just test-vm` (plus the `drm-master-probe` phase probes). Every
layer below, plus the shipped §0 privilege-separation epic, is committed
on **`main`** @ `17ae692` and gate-verified.

| Layer | What | Gate(s) | Commit | Task |
|---|---|---|---|---|
| A | Visual-test infra: `visual.py`, `ssimulacra2_rs`, Snapshot()-based capture | (infra) | `b3f100d`… | #2 ✓ |
| B | Renderer: DRM+GBM+GlesRenderer+DrmCompositor, layer-shell, wl_shm, `#0a0014`, `frame_audit` split | `visual-halmasuit-clear/-layer/-splash` | `1a01f63`→`35c521e` | #3–8 ✓ |
| C | Witness plane: halmasuit composites the configured PNG internally as its bottom-most background from frame 0 | `visual-halmasuit-splash` | `996f7c4` | #9 ✓ |
| D | `visual-backdrop.nix` 4 stand-in scenes + FrameRendered continuity invariant | `visual-backdrop` | `6b73459` | #10 ✓ |
| E1 | DRM acquisition via `LibSeatSession`/seatd | `drm-master-probe-phase4`, all visual | `482dc1c` (+`2b58e10`) | #11,#14 ✓ |
| E2 | libinput + `wl_seat` keyboard/pointer + focus routing | `halmasuit-input` | `745e8e9` | #15 ✓ |
| F1 | Real `xdg_toplevel` fullscreen compositing over splash | `visual-halmasuit-toplevel` | `64a18e0` | #12 ✓ |
| F2 | greeter→session foreground state machine; no-flash across the **real greetd** transition | `visual-foreground` | `a7078c2` | #16 ✓ |
| G0 | Pin nix-config→main (user's forks); real DMS stack boots | `smoke-boot` (real greetd+DankGreeter+niri+quickshell) | `1586aaa` | #17 ✓ |
| §0 | Privilege-separation epic: `halmasuit-session` broker is the live auth/session path; `halmasuit-pam` + setuid `halmasuit-spawn` deleted | `run-pam-auth`, `session-r5r6`, `session-onehandle`, `login-flash` | `17ae692` | SHIPPED ✓ |
| **G1** | Real niri nested as the broker-launched session | — | **next (§6); unblocked by §0** | pending |
| G2–G4 | Real DankGreeter; full real-auth arc; interactive proof | — | not started | to-create |

What F2 proved (the no-flash thesis, on the real mechanism): real greetd
full-auth → `ForegroundChanged{session}` → the session is launched →
**halmasuit PID continuous across the real greeter→session swap**,
`FrameRendered` continuity OK across the whole transition, both scene goldens
matched. F2 used a stand-in only for the *content* of the two endpoints
(solid-color clients); the mechanism is real. Post-§0, `login-flash`
proves the same continuity **through the broker-launched session** (the
session leader is the broker's fork-then-drop child, not a setuid exec).

---

## 4. G1 attempt — what we learned (knowledge preserved, code discarded)

An early G1 (real niri as the session) attempt was **deliberately
discarded** (`git restore`/clean — the test file is cheap to recreate and
its session-environment section is rewritten for the broker-launched
session anyway). The *findings* are the value and are durable in memory
(`session-spawn-namespace-handoff`, `g-layer-pins`) and here:

- **Real niri works nested under halmasuit.** niri's winit/GL backend
  connected to halmasuit and halmasuit composited it:
  `xdg_toplevel mapped as fullscreen foreground w:1280 h:800`. halmasuit's
  E/F protocol surface is sufficient for real niri's render path. **No
  halmasuit protocol gap** — the biggest rendering unknown of layer G is
  retired.
- niri runs correctly as the authenticated user. The privilege drop is
  fine (now the broker's non-setuid fork-then-drop session-leader child).
- Incidental, still relevant for G1: the session env is minimal —
  `pam_getenvlist()` merged with a fixed allowlist (A1), not the caller's
  environment. Pass niri its config via `--config`, not via shell
  `mkdir`/`cp`. niri (a compositor even when nested) binds its **own**
  client socket in `XDG_RUNTIME_DIR` and needs a writable `$HOME` — which
  the broker now provides via `pam_open_session` (pam_systemd +
  pam_mount) in the host mount namespace.

Then the namespace blocker (§5) stopped that attempt; §0 has since
resolved it.

---

## 5. The namespace-handoff blocker — RESOLVED (Mechanism D shipped as the broker)

**This was the layer-G blocker; §0 resolved it.** Recorded here as the
problem statement and the research that picked Mechanism D.

The problem: niri, as a child of the hardened `halmasuit.service`,
inherited its `ProtectHome=true` / `ProtectSystem=strict` /
`PrivateTmp=true` **mount namespace**. So `/home/alice` and
`/run/user/1001` — created correctly on the host — were invisible to the
session. `setresuid` does not change the mount namespace; fork/exec does
not escape it. Kernel-enforced, not a test artifact. F2 never hit it
because a solid-color stand-in needs neither a HOME nor a runtime dir;
the *real* session does.

This was the **"production session→compositor handover"** the design
explicitly parked as a layer-G open decision. Four research agents (sshd,
systemd-logind/pam_systemd, greetd/GDM/SDDM, namespace mechanics)
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

**Resolved as Mechanism D, shipped as `halmasuit-session`.** Mechanism D
*is* the session-phase half of the same `pam_handle_t` lifecycle as C1's
auth-phase killable subprocess (§0.1); the §0.2 crux (one shared handle)
resolved both. The shipped broker is the deliberately-unsandboxed
host-ns privileged unit Mechanism D describes: PID 1 socket-activates it,
it runs the `pam_open_session` tail (pam_systemd + pam_mount) and
fork-then-drops the session leader in the host mount namespace; the
hardened compositor relays to it over `SO_PEERCRED`-authenticated IPC
only after PAM-verified `start_session`. No `CAP_SYS_ADMIN`; no setuid;
the rejected-A `setns` path was never taken (DO NOT REVISIT — §0.6).
Memory: [[pam-killable-subprocess-direction]],
`session-spawn-namespace-handoff`, [[fvc-port-reconciliation]].

---

## 6. Remaining work — the visual G-layer (live queue)

§0 is shipped; the namespace blocker is gone. The visual G-layer is what
remains: prove the no-flash compositor on the *real* greeter→session path
with the *real* software, now running through the broker-launched
session.

**Mechanism foundation (complete).** The in-repo G-layer instrument is
done and gated: halmasuit composites the locked witness internally from
frame 0 (the `halmasuit-splash` client is deleted; config is
`witnessImage`/`HALMASUIT_WITNESS_IMAGE`); `assert_no_flash_stream` is
frame-0-anchored and pinned by the no-VM `just vis-selftest` synthetic
proof; the offscreen GLES readback gives headless deterministic
pixel-exact assertion (`visual-halmasuit-clear` vs the human-inspected
witness golden). Full `just test-vm` is green incl. `login-flash` and
the three broker gates. This instrument is the Phase-B-prepend
foundation — see ARCHITECTURE.md "### Phase-B foundation (the in-repo
instrument)". What remains below is the *real-software* tranche on top
of it. Strict order, each blocked by the previous, **except the
sidecar** (independent — do it anytime):

1. **G1 — real niri nested as the broker-launched session.** Build
   `tests/visual-niri-session.nix` from `tests/visual-foreground.nix`
   (swap `sessionCmd` → real niri `--config <minimal kdl>`; niri pkg =
   `nix-config.inputs.niri-flake.packages.x86_64-linux.niri-unstable`;
   wire flake.nix check + Justfile). Session env + `$HOME`/
   `$XDG_RUNTIME_DIR` now flow through the broker's `pam_open_session`
   (no `ProtectHome=false` carve-out — that is the whole point of §0).
   Assert: real greetd auth (through the broker) → niri `xdg_toplevel`
   fullscreen foreground; niri marker (`pgrep -x niri`); PID-continuity
   across greeter→niri; `assert_no_flash_stream`; Snapshot
   `niri-session` golden, Read-inspected before commit. Greeter stays
   the layer-shell stand-in.
2. **G2 — real DankGreeter as `greeterCommand`** (replaces the stand-in
   layer client). Talks greetd to halmasuit's socket; enumerate the
   Wayland protocols it binds and add any halmasuit lacks (never patch
   DankGreeter). Golden inspected; real keyboard input → DankGreeter.
3. **G3 — full real-auth arc gate**: boot → splash(user image) → real
   DankGreeter over splash → emulated keystrokes type the test-user
   password → PAM success **through the broker** → broker fork-then-drops
   real niri → niri foreground. Continuity invariant + PID-continuity
   across the **real** DankGreeter→niri transition; niri marker; goldens
   (splash → DankGreeter → niri) inspected.
4. **G4 — interactive bootable proof + docs**: `just` recipe, QEMU GTK
   window, user's image, the full arc by hand; documented (how to pass
   the image, what you should see). Plus re-verify `cargo tree -p
   halmasuit` (no features) clean / production halmasuit zero
   `frame_audit`.
5. **Epic close**: layer-G acceptance gate. When all G subtasks are green
   → `gambit:review` → `finishing-branch`. `login-flash` stays a GREEN
   hard gate (it already passes through the broker-launched session); the
   CI inversion is gone — keep it a normal pass/fail check.

**Sidecar (independent — anytime): libseat-aware DRM-master-continuity
assertion on `login-flash`.** Today `login-flash` proves *PID* continuity
across greeter→session; it does NOT prove the compositor never internally
drops/re-acquires DRM master (a real flash vector PID-continuity cannot
see). A reference impl lives in commit `aa93f4f` on the retained
`harden/phase-a-review` branch, BUT it is wrong for v2: it greps debugfs
for the *compositor PID* as DRM master, which never holds under
libseat/seatd — **seatd** is the registered master and halmasuit holds
the DRM fd *via* libseat. The rewrite must instead assert, across the
transition: (a) `seatd` continuously holds DRM master, and (b)
halmasuit's libseat session is never closed/reopened (no `CloseSession`/
`OpenSession` churn in the seatd log, and halmasuit's seatd client id is
stable). It modifies the canonical `login-flash` (CLAUDE.md hard-rule
territory) — additive only, never weaken the existing PID assertion;
re-gate full `just test-vm`. Delete `harden/phase-a-review` once it
lands.

Task-tool state: task IDs do NOT carry across sessions — treat the §3
table + this list as the authoritative state. The visual G-layer has no
gambit tasks yet; create its epic + first task (G1) via
`gambit:brainstorming`. Conceptually: layers A–F2 + G0 done; §0
privilege-separation epic SHIPPED; G1 next (unblocked); G2/G3/G4 created
iteratively as G1 lands.

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
  Broker gates: `tests/{run-pam-auth,session-r5r6,session-onehandle}.nix`
  (all real PAM, no mocks); `tests/login-flash.nix` is the canonical
  no-flash gate (through the broker-launched session).
- **Commands**: `just check` (rustfmt+clippy+deny+machete+typos+nextest
  + `r14-gate`); `just test-vm` (the 14-gate sweep); `just test-vm-drive
  <name>` (interactive QEMU for debugging); `just update-goldens <name>`
  (human-in-the-loop golden regen — inspect every PNG before commit).
- **Privileged PAM/session boundary** (the shipped code):
  `crates/halmasuit-session/src/broker.rs` (module doc = authoritative
  spec: one handle, killable auth fork, fork-then-drop leader, slot-owned
  reaping, relay-peer `SO_PEERCRED` gate); `session_leader.rs`/`worker.rs`
  (the fork-then-drop syscall sequence, A9 group derivation);
  `crates/halmasuit/src/broker_session.rs` (the compositor's per-greeter
  `BrokerEpisode`, A6/A7/A8); `crates/halmasuit-greetd/src/server.rs`
  (sans-IO greetd state machine — the PAM-success gate); `nix/module.nix`
  (the socket-activated `halmasuit-session` unit + the hardened
  `halmasuit.service`).
- **Hard rules** (`CLAUDE.md`): no PAM bypass; no client patching (fix
  halmasuit); one `pam_handle_t` in the broker, never split/`execve`'d;
  no setuid inode / no second libpam surface; UID floor in the broker's
  session-leader child; compositor never blocks the render loop on broker
  IPC; no running the compositor as root; goldens human-inspected; no
  weakening/skipping structural tests; no `--no-verify`/`--no-gpg-sign`.
