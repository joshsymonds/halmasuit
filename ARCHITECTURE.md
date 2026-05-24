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

The ambition is BIOS → kernel → initramfs → greeter → desktop visual
continuity, the way macOS goes from Apple logo to login window to desktop
without a single flash. v1 has shipped the test infrastructure that
measures one boundary of this (`tests/login-flash.nix`, the greeter→session
flash). v2 is the implementation that delivers the *whole* visual
continuity — **one halmasuit process from initramfs through to shutdown**,
with no userspace owner of the display ever exiting and restarting after
the kernel hands off.

---

## The architectural commitment

**One halmasuit process, started in the initramfs, never exits until shutdown.**

The entire boot flash exists because of one upstream choice everywhere on
Linux: Plymouth dies at `graphical.target`, then a display manager
(greetd / GDM / SDDM / etc.) starts from scratch. Two userspace owners of
the display, with a hard process boundary between them, and a kernel-level
DRM-master release/acquire across that boundary. The flash is the direct,
unavoidable consequence of that process split.

Halmasuit's design deletes the split. The same `halmasuit` binary that
paints the splash during initramfs survives `switch_root` via systemd's
`SurviveFinalKillSignal=yes` unit directive (systemd v255+, declared in
the `[Unit]` section), which excludes the process from
`systemd-shutdown`'s killall at the `pivot_root` boundary. Same PID,
same memory mapping, same DRM master file descriptor on both sides of
the boundary; no FD-passing dance is needed. The process continues
painting through systemd target traversal, brings up its Wayland server
when the system reaches a ready state, hosts the greeter as a
`wl_client`, swaps to niri after PAM success, and persists until
power-off. DRM master is held continuously from initramfs through
shutdown. The CRTC is modeset exactly once — or twice if `simpledrm`
migrates to a real KMS driver mid-boot. Every visible transition is
internal: either a buffer swap on the primary plane, or a `wl_client`
swap inside halmasuit's own scenegraph. Nothing visible to the user
crosses a process boundary.

The legacy `@argv[0]` storage-daemon convention (write `@` into
`argv[0][0]` via glibc's `__progname_full` data symbol per
[systemd ROOT_STORAGE_DAEMONS](https://systemd.io/ROOT_STORAGE_DAEMONS/))
also works and is retained as a documented fallback. It's NOT
production halmasuit's primary path because the upstream documentation
is explicit: the convention applies "to storage technology only, not to
daemons with any other (non-storage related) purposes." halmasuit is
non-storage, and recent systemd regressions in the convention's
implementation make it brittle to depend on long-term.

This mechanism has been **empirically validated end-to-end** — see
[`RESEARCH.md`](RESEARCH.md) for the `drm-master-probe` Phase 0, 1, 2,
and 3 artifacts. The probes are runnable: `just test-drm-probe`,
`just test-drm-probe-phase1`, `just test-drm-probe-phase2`,
`just test-drm-probe-phase3` re-establish ground truth in seconds.

This is what nothing else in the wlroots/smithay ecosystem ships. Plymouth,
gdm-wayland, SDDM-wayland — every one of them dies and restarts the display
owner at least once during boot. Halmasuit is the architecture that doesn't.

The Mir / Lomiri stack (Ubuntu Touch, PinePhone — in production since 2013)
demonstrates the model works: one long-lived display server hosting the
lockscreen, greeter, and user shell as nested clients. Halmasuit ports that
model into the wlroots/smithay world and into desktop Linux's boot pipeline.

### What halmasuit *itself* implements

Halmasuit is intentionally thin. It owns three things only:

1. **The display surface.** DRM master, KMS modeset, the primary plane.
2. **A Wayland server hosting one foreground `wl_client` at a time.** No
   inner window management, no shell, no taskbar, no anything else a
   normal compositor does — those live in niri.
3. **Phase transitions.** Internal state machine that swaps which
   `wl_client` is foreground, and the orchestration needed to make
   that swap happen: the greetd state machine, relaying the PAM
   conversation to the privileged `halmasuit-session` broker (libpam
   never runs in the compositor's address space — see "Authentication
   and session lifecycle"), and the logind D-Bus calls.

Everything else is a `wl_client` of halmasuit, including the splash, the
greeter, the lock screen, the user session, the LUKS prompt, and the
shutdown splash. **Halmasuit hosts UI; it does not implement UI.**

### Related work and validation

**`lomiri-system-compositor`** (Canonical / UBports — Ubuntu Touch,
PinePhone — in production since 2013, ~13 years and counting) is the
*one* shipping example of halmasuit's exact architecture: one
long-lived userspace process owning the GPU continuously, hosting the
greeter and the user shell as nested clients, never releasing the seat
between them. Source:
[gitlab.com/ubports/core/lomiri-system-compositor](https://gitlab.com/ubports/core/lomiri-system-compositor).
Built on Mir.

halmasuit is a **Rust + smithay + vanilla-Wayland port of USC's
architectural ideas**, not a divergence from them. Reading the USC
source end-to-end is the highest-leverage technical artifact for
de-risking halmasuit v2 — the lifecycle, privilege model, and
greeter→session swap mechanism encode 13 years of hard-won lessons.

USC isn't a substrate halmasuit builds on, for reasons specific and
mostly language-secondary:

- **Tight coupling to the Lomiri (Unity8) shell.** USC's non-skeleton
  code exists to host Lomiri's specific UI components. Hosting a
  greetd-compatible greeter + niri instead means modifying code that
  isn't designed for that.
- **LightDM lifecycle.** USC is spawned by LightDM at greeter-time and
  doesn't span boot → shutdown. Adopting USC means re-adopting the
  display-manager layer halmasuit exists to delete.
- **Mir protocols, not vanilla Wayland.** Niri is a vanilla-Wayland
  compositor. Mir 2.x has a Wayland frontend but its surface is
  incomplete (Lomiri itself is still on Mir 1.x with limited Wayland
  support).
- **Mobile / convergent target.** Touch input, rotation, mobile-shell
  assumptions are baked in.
- **License** (GPL-3+) and **NixOS packaging burden** (Mir's transitive
  dependency graph isn't packaged for NixOS) are real but secondary.
- **Language posture.** halmasuit leans hard on Rust's memory-safety
  guarantees — `#![forbid(unsafe_code)]` on every crate except the one
  privileged broker, where `unsafe` is confined to its auditable
  `pam_ffi`/`worker` modules. C++ in skilled hands is fine, but the
  project's broader posture is consistently Rust-first.

Forking USC would mean tearing out everything in that bullet list —
leaving an architectural skeleton of a few hundred lines. The
substantive choice was always **language + toolkit** (Rust + smithay
vs. C++ + Mir), not fork-vs-build. Both involve writing halmasuit
fresh; we picked the Rust ecosystem on the merits.

**`gamescope`** (Valve, [github.com/ValveSoftware/gamescope](https://github.com/ValveSoftware/gamescope),
MIT) is structurally close to what halmasuit *is* during the `SESSION`
phase — a thin wlroots-based Wayland compositor hosting one foreground
client (Steam Big Picture, or a game) and direct-scans-out when possible.
Notable distinction: gamescope is **not a system compositor** on Steam
Deck. The actual SteamOS boot chain is Plymouth (initramfs + through
`graphical.target`) → SDDM (system display manager, autologin
configured via `/etc/sddm.conf.d/zz-steamos-autologin.conf`) → user
session → `gamescope-session-plus@.service` (a **user** systemd unit
under `graphical-session.target`) → gamescope → Steam. Three
display-owning userspace processes (Plymouth, SDDM, gamescope) with
process boundaries between each. Valve has masked the visual flashes
via a tuned Plymouth theme matching Steam aesthetics, hidden SDDM
(zero-timeout autologin), and fast boot timings — but they have not
eliminated the process boundaries. Halmasuit's structural premise is
"delete the boundaries, don't mask them."

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

## Boot timeline and phases

The halmasuit process traverses internal phases over the lifetime of the
machine. Each phase transition is a state change inside the running
process — *not* a process restart.

```
Phase                  Foreground wl_client          Adapters active
─────────────────────────────────────────────────────────────────────────
INITRAMFS_SPLASH       halmasuit (internal           halmasuit-luks (if
                        wallpaper plane)                cryptsetup needs a
                                                      passphrase),
                                                      halmasuit-fsck (if
                                                      fsck needs interaction)

ROOTFS_SPLASH          halmasuit (internal           (none — just waiting
                        wallpaper plane;                 for system readiness)
                        re-attached post-
                        re-exec; same surface
                        on screen as before)

GREETER                DankGreeter / regreet / …     halmasuit-greetd
                       (as 'greeter' user)            (in-process)

SESSION                niri                          (none — niri owns
                       (as authenticated user,         everything in-session
                        launched by the                via its own protocols)
                        halmasuit-session broker)

LOCKED                 ext-session-lock-v1 client    (lock client itself
                       (swaylock / hyprlock / …)      drives re-auth via PAM
                                                      brokered by halmasuit)

SHUTDOWN_SPLASH        halmasuit (internal           (none — awaits poweroff)
                        wallpaper plane;
                        different scene)
```

Transitions:

- **`INITRAMFS_SPLASH → ROOTFS_SPLASH`**: triggered by systemd's
  `initrd-switch-root.target`. halmasuit's initramfs unit declares
  `SurviveFinalKillSignal=yes` (systemd v255+, in the `[Unit]` section
  — NOT `[Service]`, which systemd silently rejects), excluding the
  process from `systemd-shutdown`'s killall at the `pivot_root`
  boundary. Same PID, same memory mapping, same DRM master fd on both
  sides — no FD passing. The unit also uses `DefaultDependencies = false`
  + `IgnoreOnIsolate = true` so PID 1 doesn't *stop* the unit when
  `initrd-switch-root.target` is isolated (separate concern from the
  killall — these prevent systemd's normal stop sequence, the killall
  is `systemd-shutdown`'s process-level cleanup). Once rootfs is
  mounted, halmasuit drops privileges from root to the `compositor`
  system user via `setresuid` (DRM master is per-fd; mastery survives
  the privilege drop), and resumes painting the same surface. The user
  sees no visible change. **Optional v3 layer:** an `execve` into the
  rootfs-resident binary path before the killall (Phase 3 probe
  validates this) gives the post-pivot process a canonical rootfs
  `/proc/<pid>/exe` and sets up for clean `sd_notify` registration
  with rootfs systemd — sidestepping the orphan-unit-`SIGTERM`-reaper
  problem. v2 Phase A ships without the exec layer; the orphan
  `SIGTERM` is handled via a graceful handler. **Note:** rootfs
  systemd discovers the orphan unit (its name lives only in the
  now-dead initramfs systemd) and sends `SIGTERM` ~1s
  post-`switch_root`; halmasuit must either `sd_notify` to register
  with rootfs systemd as a tracked unit (requires the v3 exec layer),
  install a graceful SIGTERM handler, or detach from systemd's process
  tracking. **Fallback mechanism:** for systems running systemd <
  v255, the `@argv[0]` storage-daemon convention also works (Phase 1
  probe validates this), though upstream documents the convention as
  "storage technology only" — production halmasuit prefers the
  supported `SurviveFinalKillSignal=yes` directive. See
  [`RESEARCH.md`](RESEARCH.md) for empirical evidence (Phases 0, 1, 2,
  and 3).
- **`ROOTFS_SPLASH → GREETER`**: triggered by halmasuit reaching an
  internal-ready state (D-Bus session bus available, `XDG_RUNTIME_DIR`
  populated for the greeter user, halmasuit-greetd socket bound).
  halmasuit-greetd spawns the configured greeter as a `wl_client` running
  as the `greeter` user. As the greeter's first surface commit arrives,
  halmasuit crossfades from the splash buffer to the greeter surface (~250
  ms alpha blend).
- **`GREETER → SESSION`**: triggered by PAM success in the privileged
  `halmasuit-session` broker (the compositor relays the conversation;
  libpam never runs in the compositor). halmasuit kills the greeter
  `wl_client`; the broker — already root, holding the one
  `pam_handle_t` for the whole lifecycle — forks once and the
  non-setuid child drops privileges and execs niri as the
  authenticated user, while the broker parent keeps the handle open
  for `pam_close_session` at logout. halmasuit composites niri's
  surface in the greeter's place. No DRM activity; halmasuit's mastery
  is unchanged.
- **`SESSION ↔ LOCKED`**: triggered by `loginctl lock-session` (D-Bus
  signal halmasuit subscribes to), by an explicit `halmasuit msg lock`,
  or by an idle timeout the session configures. halmasuit spawns the
  configured `ext-session-lock-v1` client, makes it foreground; niri keeps
  running in the background but is hidden. On successful re-auth, lock
  client exits and niri returns to foreground.
- **`SESSION → SHUTDOWN_SPLASH`**: triggered by `PrepareForShutdown` signal
  from logind. halmasuit asks niri to exit; once niri has, halmasuit
  presents its internal wallpaper plane again with a "shutting down" scene
  and awaits the logind-driven power-off.

Two invariants hold across every transition:

1. **DRM master is never released.** Held continuously from
   `INITRAMFS_SPLASH` to the final frame of `SHUTDOWN_SPLASH`. logind
   exists for *session management* (PAM session → `user@.service`,
   `XDG_RUNTIME_DIR` setup, polkit context) but is **not in the DRM
   brokerage path** for halmasuit's seat. logind starts up after halmasuit
   has already become master; nothing else asks logind for the device, so
   no contention arises.
2. **The CRTC is modeset at most twice over the entire boot** — once when
   halmasuit takes DRM in initramfs, optionally once more during the
   `simpledrm` → real-KMS migration when the GPU driver loads. After
   that, every visual change is a buffer swap on the primary plane.
   Atomic flips guarantee no black frame.

The login-flash test from v1 measures one specific transition
(`GREETER → SESSION`) by asserting PID continuity of the niri-rendering
process. v2 will add a sibling test that asserts frame continuity for the
whole boot pipeline — frames captured from kernel handoff through to
`SESSION` phase, asserting no all-black frame and no DSSIM jump above
threshold across any transition.

---

## High-level architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│ INITRAMFS                                                            │
│   systemd (in initramfs, PID 1)                                       │
│    └─ halmasuit.service                                              │
│       └─ halmasuit (as root — no userdb yet in initramfs)            │
│          • opens /dev/dri/card0, becomes DRM master directly         │
│          • Wayland server   /run/halmasuit/wayland-0                 │
│          • foreground: halmasuit (internal wallpaper plane)            │
│          • adapter listening: halmasuit-luks (systemd password agent)│
└──────────────────────────────────────────────────────────────────────┘
                              │
                              │  initrd-switch-root.target
                              │  → halmasuit re-execs from rootfs path
                              │    (DRM fd + wl-socket fd inherited
                              │     across exec; PID changes; mastery
                              │     retained; setresuid → compositor)
                              ▼
┌──────────────────────────────────────────────────────────────────────┐
│ ROOTFS                                                                │
│   systemd (PID 1)                                                     │
│    └─ halmasuit.service                                              │
│       └─ halmasuit (as 'compositor' system user — DRM master         │
│                    retained across re-exec)                          │
│          • Wayland server   /run/halmasuit/wayland-0                 │
│          • greetd-compat    /run/halmasuit/greetd.sock               │
│          • control IPC      /run/halmasuit/control.sock              │
│          • D-Bus consumer   → logind (PrepareForSleep,               │
│                                       PrepareForShutdown,            │
│                                       Lock signals)                  │
│          • D-Bus server     → org.halmasuit.Compositor1              │
│          • foreground wl_client over time:                            │
│             ├─ PHASE rootfs-splash  halmasuit (wallpaper, internal)    │
│             ├─ PHASE greeter        DankGreeter / regreet / …        │
│             │                       (as 'greeter' user)              │
│             ├─ PHASE session        niri                             │
│             │                       (as authenticated user, launched │
│             │                        by the halmasuit-session broker)│
│             ├─ PHASE locked         ext-session-lock-v1 client       │
│             └─ PHASE shutdown       halmasuit (wallpaper, internal)     │
└──────────────────────────────────────────────────────────────────────┘
```

Halmasuit replaces **both Plymouth and greetd** with one binary that lives
in both the initramfs and the rootfs and is the same process throughout.
DRM master is taken directly (not via logind) in the initramfs, because
logind does not exist yet. After `switch_root`, halmasuit re-execs itself,
drops privileges from root to the `compositor` system user, and continues —
the DRM master held by the file descriptor survives both the exec and the
privilege drop. logind starts up later in the boot but is never asked for
the device; logind's role for halmasuit's seat is reduced to session
management only.

---

## Adapter principle: halmasuit owns the pixels, never the feature

Halmasuit's job is to own the seat's display surface and host one
foreground `wl_client` at a time. It is **not** halmasuit's job to
implement the features that today prompt the user from a TTY or run as
their own separate display owner. Each such feature already has a wire
contract — a Unix socket protocol, a Wayland protocol, or a D-Bus
interface — that some other binary or library on the system already
speaks. Halmasuit **adopts that contract on the appropriate side** and
the feature's UI plugs in as a `wl_client` (or, where the contract is
naturally daemon-side, as an in-process adapter module). Halmasuit
fulfils the wire contract of the program it is adopting; the program's
logic and UI are unchanged or pluggable.

This pattern is what keeps halmasuit thin. If LUKS prompts need to look
nice during boot, the contract is "speak the systemd password-agent
protocol," not "patch halmasuit." If a new lock screen wants to ship,
it's an `ext-session-lock-v1` client, not a halmasuit fork. Halmasuit's
surface stays bounded to the three things only it can do: own the
display, host `wl_clients`, manage phase transitions.

### Wrap points

The table below enumerates every place in the Linux boot/login pipeline
where userspace currently drops to a TTY, runs a competing display owner,
or otherwise interrupts visual continuity — and the wire contract
halmasuit (or a plug-in `wl_client`) adopts to absorb it.

| Wrap point | Existing wire contract | Where halmasuit fits | Plug-in form |
|---|---|---|---|
| **Greeter** | greetd JSON-over-Unix-socket ([spec](https://man.sr.ht/~kennylevinsen/greetd/protocol.md)) | `halmasuit-greetd` crate (in-process server) | Any greetd-compatible greeter binary: DankGreeter, regreet, tuigreet, gtkgreet, agreety |
| **LUKS / dm-verity / password prompts during boot** | systemd password-agent protocol ([spec](https://systemd.io/PASSWORD_AGENTS/)) — `/run/systemd/ask-password/ask.*` inotify + response sockets | `halmasuit-luks` is a `wl_client` running during `INITRAMFS_SPLASH`; registers as a password agent | Replaceable by any other `wl_client` that implements the agent protocol |
| **fsck progress + repair prompts** | systemd-fsckd progress protocol over `/run/systemd/fsck.progress` | `halmasuit-fsck` is a `wl_client` that renders progress overlays + Y/N prompts | Replaceable by any other `wl_client` speaking the same socket |
| **Lock screen** | Wayland `ext-session-lock-v1` (halmasuit exposes), plus `org.freedesktop.login1.Manager.Lock` D-Bus signal (halmasuit subscribes) | halmasuit hosts the lock client and brokers session activation around it | Any existing `ext-session-lock-v1` client: swaylock, hyprlock, gtklock, custom |
| **Emergency / rescue UI** | `emergency.target` invokes a unit; today that unit is `sulogin` on a console | `halmasuit-emergency` is a `wl_client` that does PAM-as-root and execs a graphical terminal | Replaceable by any other `wl_client` registered for emergency.target |
| **Fast user switching** | `org.freedesktop.login1.Manager.ActivateSession` D-Bus + multi-session logind support | halmasuit hosts multiple per-user `wl_client` session subtrees concurrently and swaps which is foreground | Switching UI itself is a session-level concern (lives in niri / DMS, not in halmasuit) |
| **First-boot setup** | `systemd-firstboot` and related | Optional `halmasuit-firstboot` `wl_client`; low priority | Plug-in |
| **Polkit authentication prompts mid-session** | `org.freedesktop.PolicyKit1.AuthenticationAgent` D-Bus | **Not a wrap point.** Polkit auth agents already work as ordinary `wl_client`s of niri during `SESSION` phase. Halmasuit needs no special integration. | — |

The greeter wrap point is the only one where the adapter lives **inside**
halmasuit's process — because the greetd state machine needs to drive
halmasuit's foreground-client lifecycle (terminate greeter, exec niri)
and call PAM in halmasuit's address space. Every other adapter lives in
its own `wl_client` process.

**What halmasuit does NOT reimplement:**
- LUKS keyslot verification (`cryptsetup` / kernel dm-crypt do it)
- fsck repair decisions (`e2fsck` / `xfs_repair` / etc. do them)
- Polkit authorization logic (`polkitd` does it)
- logind's session management (logind does it)
- PAM authentication (PAM does it; for the greeter wrap, libpam runs in the privileged out-of-process `halmasuit-session` broker — never in the compositor's address space — and the compositor only relays the conversation; emergency and lock-screen adapters call PAM in their own process)

Halmasuit absorbs only the *frontend* of each — the surface through which
the user interacts. The backend stays where it lives.

### The greeter as the canonical example

The greeter is the most important wrap point and the one v2 implements
first. Detailed flow:

Wire types and the JSON codec are owned locally in `halmasuit-greetd`.
The upstream [`greetd_ipc`](https://crates.io/crates/greetd_ipc) crate
is GPL-3.0-only; linking it would force halmasuit's binary to GPL,
conflicting with the workspace's dual MIT-OR-Apache posture (which
matches the Rust-Wayland infrastructure tier: smithay, wlroots,
Weston). The types in `halmasuit-greetd` are a clean-room
reimplementation from the public protocol spec at
<https://man.sr.ht/~kennylevinsen/greetd/protocol.md>. Drift mitigation
is a suite of canonical-payload roundtrip tests that pin the JSON
shape against payloads from that spec — if upstream changes the
format, the tests break and we notice. What halmasuit-greetd owns
beyond the wire types is the daemon-side logic: the greetd state
machine and the integration points that swap halmasuit's foreground
`wl_client` when auth succeeds. It holds no `pam_handle_t` and links
no libpam — it relays the conversation to the privileged
`halmasuit-session` broker. Reusing greetd-the-daemon itself as a
library is also not feasible: its privilege model (run as root,
`execve` the user session from the handle-owning process) is
incompatible with halmasuit's split — the compositor is unprivileged
and holds no handle, and the privileged broker that does own the
handle never `execve`s from it (it forks once and the non-setuid
child drops privileges; PAM treats an exec from the session-opening
process as logout).

Flow:

1. halmasuit launches the configured greeter binary with environment:
   - `WAYLAND_DISPLAY=halmasuit-0` (points at halmasuit's Wayland socket)
   - `GREETD_SOCK=/run/halmasuit/greetd.sock`
   - `XDG_RUNTIME_DIR=/run/user/<greeter-uid>`
2. Greeter binary renders its UI as a Wayland client of halmasuit.
3. User submits credentials. Greeter sends `create_session { username }`
   over the greetd socket.
4. Halmasuit relays the conversation to the privileged
   `halmasuit-session` broker, which runs libpam in an ephemeral,
   SIGKILL-able, `setrlimit`-bounded privileged fork (the compositor
   holds no `pam_handle_t` and links no libpam). PAM challenges flow
   back to the greeter via `auth_message`, the responses return
   through `post_auth_message_response`, and the compositor passes
   only the serialized conversation frames in each direction.
5. On PAM success, the greeter sends `start_session { cmd: ["niri"] }`,
   which the compositor forwards to the broker.
6. Halmasuit terminates the greeter. The broker — already root,
   holding the *same* `pam_handle_t` through
   `pam_setcred`/`pam_open_session` — `fork`s once; the non-setuid
   child drops privileges in-process (setresgid →
   `setgroups(getgrouplist(PAM-resolved user, primary gid))` — the
   identity-derived set ONLY, never the broker's own `getgroups()`;
   Amendment A9 → setresuid → re-verify → `PR_SET_NO_NEW_PRIVS` →
   `execve` with the `pam_getenvlist`-MERGED environment) and execs
   niri as the authenticated user. The broker
   parent keeps the handle open, `waitpid`s the leader, and runs
   `pam_close_session` → `pam_setcred(PAM_DELETE_CRED)` → `pam_end`
   at logout.
7. Halmasuit's Wayland server now hosts niri as its sole foreground
   client.

> **Status.** The privilege-separated `halmasuit-session` broker
> described in steps 4–6 is the **live** auth/session path: the
> unprivileged compositor relays the greetd conversation to it over a
> `SOCK_SEQPACKET` channel (sans-IO; Amendments A6/A7/A8). The former
> in-compositor `crates/halmasuit-pam` and the setuid `halmasuit-spawn`
> helper are **deleted** — single libpam surface (`halmasuit-session`),
> no setuid inode in the closure (landed atomically with the broker
> going live, Amendment A4). Proven end-to-end with real `pam_unix` +
> `pam_mount` and no mocks, including `login-flash` (PID + frame
> continuity through the broker-launched session). See "Authentication
> and session lifecycle" below and
> [`PLAN.md`'s "Privilege-separation decision record"](PLAN.md#privilege-separation-decision-record).

DMS patch needed: roughly twenty lines in the `dms-greeter` launcher
script to skip its nested-niri spawn when `WAYLAND_DISPLAY` is already
set by the parent. DankGreeter's QML and Quickshell content run
unmodified.

---

## Visual identity

The wallpaper plane is **not** "a logo on a background." It is composited
internally by halmasuit (no separate client) and persists across every
phase where halmasuit hosts no other foreground client; the Phase-B
vision evolves it into a full GPU-accelerated shader-driven render path,
starting in the initramfs. It is the visible signature of the system
from kernel handoff to user login, and from user logout back to
power-off. This is the layer that makes a Linux desktop feel like
intentional industrial design instead of an apology for booting.

### Phase-B foundation (the in-repo instrument)

The in-repo G-layer mechanism is built as the structural foundation
the Phase-B initramfs prepend extends *backward* — through
`switch_root` and DRM-master-fd survival — without rework. Three
pieces compose tier-agnostically:

- **The wallpaper plane is a halmasuit-internal full-output element
  composited from frame 0** (no external client, no pre-client solid
  phase — amendment G1). The Phase-B initramfs path inherits the same
  internal plane; `switch_root`/re-exec changes *when* halmasuit
  starts, not *what* it composites.
- **`assert_no_flash_stream` is anchored at frame 0**, not at a
  later client-first-frame: the wallpaper `ClientFirstFrame{Wallpaper}`
  must precede every `frame_rendered`, and every frame is
  wallpaper-covered. The assertion makes no rootfs-only assumption, so
  Phase-B's `full-boot-flash` gate prepends the initramfs→rootfs
  frames onto the same stream and the same predicate holds across the
  `switch_root` seam without weakening it. The contract is pinned by
  the no-VM `just vis-selftest` synthetic proof.
- **The offscreen GLES + `ExportMem` readback** is headless and
  Mesa-llvmpipe-deterministic with no GPU/GBM dependency, so the
  pixel-exact wallpaper assertion runs identically wherever halmasuit
  runs — initramfs or rootfs, CI or hardware.

Phase-B is therefore an *extension* of this instrument, not a
reimplementation: the wallpaper, the frame-0 invariant, and the readback
already exist and are gated; Phase-B adds the earlier tier in front of
them.

### Capability and cost

The GPU is fully capable from the moment its KMS driver loads. amdgpu,
i915, nouveau, and the rest expose Vulkan and OpenGL ES through Mesa
at the kernel level. To use those capabilities inside the initramfs we
ship:

- Mesa, trimmed to the driver(s) for target hardware — ~80 MB compressed.
- `vulkan-loader` + the Vulkan ICD for the target GPU — ~10 MB.
- `wgpu` (or direct `ash`) statically linked into `halmasuit` (the
  Phase-B GPU wallpaper path) — ~20 MB.

~100 MB initramfs addition, decompressed once per boot. Trivially
affordable on any 2026 system; Steam Deck's initramfs is larger and
decompresses in ~100 ms on NVMe. The payoff is uniform, high-quality
visual presentation across the *entire* boot/login/logout pipeline, not
a tiny logo on a tiny window.

### The killer move: one continuous animation across phases

Because halmasuit is one long-lived process and the splash is one
long-lived `wl_client`, the animation it renders can be *continuous*
across every phase transition:

```
INITRAMFS_SPLASH → ROOTFS_SPLASH → GREETER → LOCKED ↔ SESSION → SHUTDOWN
       │                │            │         │                  │
       └────────────────┴────────────┴─────────┴──────────────────┘
                same splash animation runs through all of this
                (overlaid by greeter UI / niri / lock-screen
                 client / shutdown text as appropriate)
```

The greeter doesn't appear *after* the splash; the greeter appears *in
front of* the splash, which continues running underneath. The lock
screen is the same — the splash backdrop is still alive behind the
lock client. niri is the exception (full-screen opaque), but on
logout the splash is already running and just becomes visible again.
macOS cannot do this because their compositor doesn't live across the
greeter→session boundary. ours does. it would be malpractice not to
use it.

### Aesthetic rules

These constrain the form, not the ambition.

- **Intentional, not frantic.** No fast cuts, no aggressive motion. The
  user is waiting for the system; the splash conveys "the system is
  composed and proceeding," not "look at me, I'm a screensaver."
- **Calm enough to be a backdrop, rich enough to be the foreground.**
  When the splash is alone (initramfs phase), it carries the entire
  visual moment. When the greeter is on top, it has to recede without
  going inert. Both modes have to work.
- **Distinctly Linux.** Not a stolen Apple aesthetic. The opportunity is
  to express what is actually distinct about Linux — programmability,
  configurability, demoscene roots, the freedom to do something nobody
  else's OS will. macOS won't ship a procedurally generated identity
  the user can theme because Apple controls the aesthetic. We can.
- **Theme-driven.** The user's nixos-config can parameterize the splash:
  colors, intensity, motif, even custom shader injection at the
  expert/advanced level. The system identity is *theirs*, not the
  project's.
- **No text during boot.** Whatever the splash renders, it does not
  render words. Text is what fbcon does and what we are taking from
  fbcon. The splash is purely visual.

### Reference points

- **Steam Deck boot animation** — animated, branded, fast, dignified.
- **PS5 / Switch boot** — dynamic but never tiring across repeated viewings.
- **The demoscene tradition** — Farbrausch's *fr-08: .the .product*,
  Conspiracy's *Chaos Theory*, and the broader scene's proof that
  extraordinary visuals fit in tiny binaries.
- **Anti-reference:** Apple's iOS boot. Restrained, polished, deliberately
  under-ambitious next to what halmasuit could be.

### Implementation sketch (deferred to v2 implementation)

What the wallpaper plane actually *renders* is a design question separate
from the architecture and is deferred to the Phase-B visual design notes
(TBD). halmasuit composites it internally; the architecture's only
constraint is that the Phase-B GPU path render at the display's native
refresh rate with no special privileges. Anything that fits inside those
constraints is in scope.

---

## Process model and UID handoff

- **halmasuit** runs as `compositor` (system user, no shell, no login)
  post-`switch_root`; as root in initramfs (before the userdb exists).
  Owns DRM master directly — `DRM_IOCTL_SET_MASTER` against
  `/dev/dri/card0`, not via logind brokerage. The probe in
  [`RESEARCH.md`](RESEARCH.md) confirms the kernel allows continuous
  master holding without logind involvement, and logind raises no
  contention when it boots up later. Holds the Wayland/greetd/IPC sockets.
- **Greeter process** runs as `greeter` (system user, no shell, no login,
  no FS access beyond its own cache dir). Same posture as greetd's
  greeter user. Sees keystrokes (Wayland input) including typed
  passwords; relays auth via greetd protocol to halmasuit.
- **`halmasuit-session`** is the minimal privileged broker. It runs in
  the host mount namespace, owns the single `pam_handle_t` for the
  whole lifecycle, ends as root for `pam_open_session`, and is a
  single socket-activated systemd unit (no standing root daemon — it
  idle-exits and PID 1 re-activates it on demand). It is the **only**
  privileged code path for greeter auth/session. There is no setuid
  binary in this path: the broker is already root, so it `fork`s once
  and the **non-setuid child** drops privileges in-process and execs
  the session leader; the broker parent keeps the handle for
  `pam_close_session`. The relocated privilege-drop discipline is a
  single fuzzable function (no `unsafe` outside the quarantined
  `pam_ffi`/`worker` modules; credential buffers `Zeroize`d the moment
  PAM completes).
- **niri** runs as the authenticated user. Inherits the Wayland socket
  via `WAYLAND_DISPLAY` env. Renders its scene to a single Wayland
  surface that halmasuit composites onto the display.

The privilege-drop sequence in the broker's non-setuid session-leader
child (straight-line, every syscall return-checked; the child
`_exit`s — never returns/`?` — on any failure so it cannot re-enter
the root parent):

```
assert EUID == 0 (broker is root; child has not dropped yet)
assert target_uid >= UID_MIN (typically 1000); reject (uid_t)-1 / overflow
assert target_gid >= UID_MIN
pwent (uid, gid, user) cross-check
setresgid(target_gid, target_gid, target_gid)
setgroups( getgrouplist(PAM-resolved user, target_gid) — identity-derived ONLY, never the broker's getgroups() )
setresuid(target_uid, target_uid, target_uid)
re-verify getresuid/getresgid all equal the target
prctl(PR_SET_NO_NEW_PRIVS)
reset the signal mask
execve(cmd, sanitized_argv, pam_getenvlist-MERGED env)   # absolute path, no PATH search
```

No intervening syscalls touch user-controlled state between the first
`setres*` and `execve`. The supplementary groups are
`getgrouplist(PAM-resolved user, primary gid)` **ONLY** — the
OpenSSH/login/GDM identity-derived shape (Amendment A9; see
[`PLAN.md`](PLAN.md#privilege-separation-decision-record)) —
**never** the privileged broker's own `getgroups()`. Under R1
`pam_setcred` runs in the broker, which carries its own groups (e.g.
`shadow` for pam_unix's in-process getspnam); sourcing the leader's
set from that process is the CVE-2021-41617 / sddm#1159
privilege-escalation class. `pam_group`/`group.conf` conditional
grants are out of scope under the one-handle-in-parent model. The
environment is `pam_getenvlist()` **MERGED** with a fixed allowlist,
never the raw greeter-supplied env (a clobber hazard for
`pam_env`-class modules).

The UID-floor refusal is what makes the privilege split *not* security
theater. A compromised compositor can ask the broker to start a
session — that is in the threat model and not preventable. What the
floor prevents is using that to escalate to root or any other system
user: the broker independently re-derives identity from PAM
(`pam_get_user` → pwent) and enforces uid **and** gid ≥ `UID_MIN`,
rejecting `(uid_t)-1`/negative/overflow (the CVE-2019-14287 class),
never trusting a compositor-asserted identity (`SO_PEERCRED`
authenticates the peer, it never authorizes the action). With the
floor in place, the worst-case outcome of full code execution in
halmasuit is "drive a session for a legitimate non-system user," not
"system compromise" — the persistent damage is in `$HOME`, not in
`/`. Removing the floor turns the split into theater.

---

## Authentication and session lifecycle

PAM runs **out of process**, in the privileged `halmasuit-session`
broker — never in the compositor's address space (co-tenanting
credentials and `dlopen`'d PAM modules with the Mesa/smithay RCE
surface is the exact privsep violation OpenSSH/GDM exist to avoid).
The NixOS module registers the `halmasuit` PAM service file and the
broker's socket-activated unit; the compositor links no libpam and
holds no `pam_handle_t`.

The architecture is the OpenSSH/GDM privilege-separation topology,
specialized to a process-continuous system compositor. Three tiers:

- **(C) Compositor — unprivileged relay.** Holds no handle, no
  credential material, no host-ns privilege; `dlopen`s no PAM module.
  Between the greeter and the broker it passes only serialized,
  length-bounded conversation frames (the greetd state machine in
  `halmasuit-greetd` is the wire/lifecycle layer; it links no libpam).
- **(B) `halmasuit-session` — privileged host-ns broker.** Owns one
  `pam_handle_t` for the **entire** lifecycle: `pam_start` →
  `pam_authenticate` → `pam_acct_mgmt` → `pam_setcred(ESTABLISH)` →
  `pam_open_session` → … → `pam_close_session` →
  `pam_setcred(DELETE)` → `pam_end`. The handle is never split across
  processes and the owner never `execve`s between auth and session
  (PAM treats that as logout). The blocking `pam_authenticate` runs in
  an **ephemeral, SIGKILL-able, `setrlimit`-bounded privileged fork**
  driven over a `SOCK_SEQPACKET` socketpair (SIGKILL directly, no
  SIGTERM grace — it is blocked in libpam with no cancellation point).
  The broker is one calloop **event-loop** process multiplexing the
  listener fd, the in-flight worker, and signals — so a reconnect can
  **evict** the in-flight attempt — and **idle-exits** so no standing
  root process exists when nothing is in flight (PID 1's retained
  socket re-activates it losslessly). A single global slot
  (system-wide one-seat) with evict-old gated to the `SO_PEERCRED`
  greeter peer plus a churn throttle.
- **(A) Session leader — fork-then-drop in a non-setuid child.** After
  `pam_open_session` the root broker `fork`s once; the child becomes
  the leader and drops privileges in-process (the sequence in "Process
  model and UID handoff"); the broker parent stays root, holds the
  handle, `waitpid`s the child, then `pam_close_session` →
  `pam_setcred(DELETE_CRED)` → `pam_end`.

**Session-spec sequencing (greetd model).** After
`pam_setcred(ESTABLISH)` and the auth-success report, and **before**
`pam_open_session`, the handle owner does one blocking IPC read for
the `start_session` spec (`cmd` + `env`), `pam_putenv`s that env into
the handle, *then* opens the session — so `pam_systemd`/logind and
`pam_mount` register the session against the correct environment. The
leader's `execve` environment is `pam_getenvlist()` MERGED with the
fixed allowlist (its supplementary groups, by contrast, are
`getgrouplist(PAM-resolved user)`-only — Amendment A9). This
intervening non-PAM I/O between `setcred`
and `open_session` is PAM-legal and is greetd's exact ordering.

State machine (the wire/relay layer, in `halmasuit-greetd`):

```
IDLE
  ↓ create_session { username }
SESSION_CREATED(id)
  ↓ broker: pam_authenticate() in the killable fork — challenges relayed to greeter
  ↓ post_auth_message_response → … → PAM result
AUTH_SUCCESS(id, uid, gid)    AUTH_FAILED(id) → IDLE
  ↓ start_session { cmd }     (broker: putenv → open_session → fork-then-drop child)
SESSION_RUNNING
  ↓ session exits             (broker parent: close_session → setcred(DELETE) → pam_end)
IDLE
```

Explicitly state-machine-checked: `start_session` is only valid in
`AUTH_SUCCESS`, never in `IDLE` or `SESSION_CREATED`; there is no path
to a running session that bypasses a real
`pam_authenticate`+`pam_acct_mgmt` success. Property tests exercise
the transitions; cargo-fuzz fuzzes the wire decoder.

**Teardown / reaping.** The broker's slot owns the worker's
`WorkerHandle{pid,pidfd}`; kill is via `pidfd_send_signal` (`ESRCH`
treated as benign), reap is a synchronous `waitpid` at every
connection-terminal point — success, auth-fail, greeter-cancel,
out-of-phase, and the transport-error path (worker died mid-relay) —
so no transient zombie lingers. The pure `#![forbid(unsafe_code)]`
protocol crate stays pid-unaware; there is no `Drop`-based reaping and
no second `waitid(P_PIDFD)` reaper. The earlier design's
"extend-the-compositor's-SIGCHLD-reaper" idea is **superseded** by
this out-of-process slot-owned reaping and no longer applies — it went
away with the deleted in-compositor PAM path (R9, Amendment A4). The
compositor reaps only its own greeter child; the session leader is the
broker's child, broker-reaped.

Credential material (challenge strings, password responses) is
`Zeroize`d the moment PAM completes; nothing sensitive outlives the
PAM transaction. Identity is PAM-derived and independently
re-verified by the broker (`pam_get_user` → pwent, UID/GID floor,
pwent cross-check) — never the pre-auth client-supplied string.

> **Status.** This broker is built, deployed (socket-activated
> host-ns unit), and VM-proven end-to-end with real PAM and no mocks:
> `run-pam-auth` (auth in the killable fork), `session-r5r6` (evict-old
> reachable from the event loop + no-standing-root idle-exit + lossless
> re-activation), and the flagship `session-onehandle` (real
> `pam_mount` decrypts+mounts a LUKS `$HOME` at `pam_open_session`
> using the auth-phase password recovered from the **same**
> `pam_handle_t` — a split handle would silently fail this). The
> unprivileged compositor relays to this broker over a sans-IO
> `SOCK_SEQPACKET` channel (Amendments A6/A7/A8); the broker's
> SO_PEERCRED gate authorizes its trusted **relay peer** (the
> compositor in the live topology — the greeter is gated at the
> compositor's own greetd socket; identity is independently
> PAM-derived, R8). The former in-process `crates/halmasuit-pam` +
> setuid `halmasuit-spawn` are **deleted** atomically with the broker
> becoming live (Amendment A4).
> [`PLAN.md`'s "Privilege-separation decision record"](PLAN.md#privilege-separation-decision-record)
> is the canonical decision record (Amendments A1–A9, with primary-
> source derivations and DO-NOT-REVISIT conditions).

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
| 5 | TOCTOU during the broker's privilege drop | Identity is PAM-derived (`pam_get_user` → pwent), re-verified by the broker. The drop runs in the broker's already-single-threaded ephemeral worker child, straight-line with no intervening user-controlled-state syscalls between the first `setres*` and `execve`, and `getresuid`/`getresgid` re-verify the drop before exec. Standard sudo/OpenSSH hardening. |
| 6 | Bug in the privileged broker → privilege escalation | The privilege-drop+exec is a single fuzzable function (cargo-fuzz targets the function); `unsafe` is confined to the quarantined `pam_ffi`/`worker` modules, everything else is `#![forbid(unsafe_code)]`. The broker is a single socket-activated unit with no standing root when idle, audited on every change (a CLAUDE.md security-review event). |
| 7 | Credential material lingers in halmasuit memory after auth | `zeroize` crate clears PAM challenge/response buffers immediately after auth completes. Only authenticated UID is retained. |
| 8 | DRM master takeover by another process | The kernel enforces single-master-per-DRM-node: once halmasuit calls `DRM_IOCTL_SET_MASTER`, any other process's `SET_MASTER` returns `EACCES` until halmasuit closes the fd or calls `DROP_MASTER`. logind, if it tries to broker the device for another caller post-boot, will be denied the same way. Halmasuit takes master directly (no logind brokerage) — confirmed by [`RESEARCH.md`](RESEARCH.md). |
| 9 | D-Bus method abuse — random user calls `RestartInnerWM` | polkit rules shipped with the NixOS module. Privileged methods require `wheel` group or interactive authentication. |
| 10 | Lock-screen bypass | Lock screen is an `ext-session-lock-v1` client. Halmasuit refuses to release the lock until the client demonstrates a successful PAM round-trip. Same flow as greeter, in-session. |
| 11 | Compromised halmasuit asks the broker to start a session as uid 0 / a system uid / `(uid_t)-1` | **UID floor**: the broker independently re-derives identity from PAM (`pam_get_user` → pwent) and refuses any `uid`/`gid < UID_MIN` (typically 1000), rejecting `(uid_t)-1`/negative/overflow (CVE-2019-14287 class) and a pwent (uid,gid,user) mismatch — *before* the group/uid drop. It never trusts a compositor-asserted "PAM succeeded for uid N" (`SO_PEERCRED` authenticates the peer, never authorizes the action). Worst case bounded to a session as a legitimate non-system user. **This is the load-bearing security property of the privilege split** — without it, the split is theater. |
| 12 | Compromised halmasuit uses retained `CAP_KILL` to denial-of-service system services | halmasuit retains `CAP_KILL` in its effective set so it can SIGKILL the greeter on session start (the greeter runs under a different uid, and `kill(2)` requires either EUID match or `CAP_KILL`). `CAP_KILL` bypasses the EUID match check and lets the holder signal any process system-wide. A compromised compositor could SIGKILL `init`, `sshd`, or another user's session — a DoS primitive, not full RCE. Mitigations: `CAP_KILL` is the *only* capability halmasuit retains post-drop (`CapPrm=CapEff={CAP_KILL}`, everything else cleared); the compositor cannot ptrace, write `/proc/*/mem`, load kernel modules, or open `/dev/mem`. The compositor's **bounding set is empty** (`CapBnd=0`): it execs no setuid helper, so nothing it or any child execs can ever gain a capability (Epic R15 least-authority). `CAP_KILL` is retained via the post-drop `capset` (effective+permitted), not via bounding. The privilege split's primary defense is that halmasuit-the-process is unprivileged enough to bound a compromise to "DoS the system" rather than "own the system." |

### Initramfs phase: temporary root

In `INITRAMFS_SPLASH`, halmasuit necessarily runs as root — the user
database (`/etc/passwd`, NSS) does not exist yet in initramfs, so there
is no `compositor` user to drop to. This is the same posture Plymouth
has today during the same boot phase, and the same code is exposed to
the same attack surface (an attacker capable of compromising halmasuit
during the seconds-long initramfs window could escalate from root).

What v2 does to keep this short:

- The privilege drop to `compositor` happens **immediately** at the
  re-exec across `switch_root`, not in some later "post-graphical-ready"
  step. The non-privileged image of halmasuit is the long-lived one;
  the privileged one lives only as long as the initramfs phase does
  (typically 1–3 seconds on modern hardware).
- The initramfs binary is the same crate as the rootfs binary, just
  built with a `--features initramfs` flag that gates the small amount
  of phase-specific code (direct DRM open, password-agent registration,
  etc.). The vast majority of halmasuit's code is reachable in both
  configurations; reducing the initramfs feature set does not
  meaningfully shrink the attack surface there.
- The systemd password-agent protocol (`halmasuit-luks`) is the highest-
  risk surface during initramfs because it sees passphrases. It runs as
  a separate `wl_client` process (not in halmasuit's address space) but
  also as root in initramfs. After re-exec to rootfs the equivalent
  prompts (lock-screen re-auth, polkit) are no longer privileged.

### Posture vs current greetd

Halmasuit's posture is **strictly better** than greetd's. greetd-the-daemon
runs as **root** — full compromise of greetd is full compromise of the
system, with no privilege boundary to fall back on. halmasuit factors
that privilege into the minimal `halmasuit-session` broker — a single
socket-activated unit with **no standing root process when idle**, a
single fuzzable privilege-drop function, libpam confined to its own
address space behind a length-bounded frame relay — and runs the
compositor itself unprivileged with exactly one retained capability:
`CAP_KILL` in the effective set (for the greeter-kill on session
start; see threat model row 12 for the bounded blast radius). No
DAC bypass, no `ptrace` of arbitrary processes, no `/dev/mem`, no
`init_module`. Concretely:

- **Bug-class delta.** Most exploitable bugs (info leaks, OOB reads,
  partial heap overwrites, Wayland state-machine errors, smithay surface
  bugs) become non-fatal when the process has no privileges to escalate.
  In a root greetd or root halmasuit, the same bug class is a
  kernel-attack primitive — root can `ptrace` arbitrary processes, write
  `/proc/*/mem`, open `/dev/mem`, `init_module`, etc.
- **Full-RCE bound.** Even on full code-execution in halmasuit, the
  broker's independently re-derived UID floor (row 11 above) caps the
  blast radius at a legitimate non-system user. greetd has no
  equivalent cap because it *is* root.
- **Audit ratio.** The privileged surface goes from "all of greetd
  plus its deps, as root, forever" to one socket-activated broker that
  is not even running when idle, whose privilege-drop is a single
  fuzzable function and whose `unsafe` is confined to the quarantined
  `pam_ffi`/`worker` modules.

The pattern is standard split-privilege design, the same one OpenSSH
uses (privileged `sshd` + unprivileged per-connection child) and the
same one Windows uses (privileged `winlogon`/`lsass` + DWM running as a
virtual service account, not SYSTEM). The privilege isn't hidden — it's
**factored**.

The broker's privilege-drop function, the Wayland/broker socket
peer-credential checks, and the greetd state machine are the things
that must have fuzz tests and property tests.

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
├── LICENSE-APACHE              # Apache-2.0 (one half of dual license)
├── LICENSE-MIT                 # MIT (other half; user picks at consumption)
├── .github/
│   └── workflows/
│       └── ci.yml              # nix flake check on ubuntu-24.04 + cachix
├── crates/
│   ├── halmasuit/              # compositor binary (v2) — links every lib below
│   ├── halmasuit-kms/          # DRM/KMS direct-scanout core, modeset, primary plane (v2)
│   ├── halmasuit-protocols/    # Wayland XML + wayland-rs codegen (v2)
│   ├── halmasuit-greetd/       # greetd wire-protocol server + state machine; relays to the broker, links no libpam (v2)
│   ├── halmasuit-session/      # LIVE privileged host-ns PAM/session broker — one pam_handle_t whole lifecycle, killable auth fork, fork-then-drop non-setuid session leader, relay-peer SO_PEERCRED gate (epic §0)
│   ├── halmasuit-session-ipc/  # frozen SOCK_SEQPACKET compositor↔broker wire contract (pure, no_std-friendly)
│   ├── halmasuit-luks/         # systemd password-agent wl_client adapter (v2)
│   ├── halmasuit-fsck/         # systemd-fsckd progress wl_client adapter (v2)
│   ├── halmasuit-emergency/    # emergency-shell wl_client adapter (v2)
│   ├── halmasuit-ipc/          # JSON-RPC control plane types (v2)
│   ├── halmasuit-cli/          # halmasuit msg CLI (v2)
│   └── halmasuit-test/         # NixOS test harness helpers (v1: live)
# (halmasuit-pam + setuid halmasuit-spawn DELETED — Epic R10/R14/R15;
#  PAM lives only in halmasuit-session, privilege-drop+exec only in
#  its non-setuid session-leader child.)
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

### v2 — One process from initramfs to session

v2 fuses what earlier drafts of this document split into v2 (greetd
replacement) and v4 (initramfs integration). They are one milestone
because splitting them is what causes the boot flash — shipping a v2
that exists only after `graphical.target` would be a worse Plymouth and
solve nothing of the project's stated mission.

Implementation is staged into **Phase A** (rootfs spine) and **Phase
B** (initramfs survival). The split is execution sequencing only;
the architectural commitment is unchanged. Phase A is complete and its
auth/session model has since been rebuilt by the privilege-separation
epic (the `halmasuit-session` broker; see "Authentication and session
lifecycle" above and
[`PLAN.md`'s "Privilege-separation decision record"](PLAN.md#privilege-separation-decision-record)).
See [`PLAN.md`](PLAN.md) for the in-scope status table.

Scope:

- `halmasuit-kms`: DRM/KMS direct-scanout core. Open device, become
  master, atomic modeset, manage primary plane. Handle the simpledrm →
  real-KMS driver migration (snapshot framebuffer pixels, modeset on new
  device, paint snapshot, release old). Reference: Plymouth's
  `ply-renderer-drm.c`.
- `halmasuit` binary that runs in **both** initramfs and rootfs. Comes
  up early in initramfs, takes DRM master, brings up Wayland server,
  composites its internal wallpaper plane as the foreground. Re-execs
  itself across `switch_root` from the rootfs binary path, preserving
  DRM master fd and Wayland-socket fd across the exec, and drops
  privileges from root to `compositor` system user post-exec.
- The wallpaper plane: a logo + background composited internally by
  halmasuit (no separate client). Present in the `INITRAMFS_SPLASH`,
  `ROOTFS_SPLASH`, and `SHUTDOWN_SPLASH` phases.
- `halmasuit-luks`: systemd password-agent adapter `wl_client`.
  Required for any encrypted-rootfs system to boot through halmasuit
  without dropping to a TTY prompt.
- `halmasuit-greetd`: greetd wire-protocol server + state machine
  (links no libpam; relays to the broker).
- `halmasuit-session`: the privileged host-ns PAM/session broker —
  one `pam_handle_t` for the whole lifecycle, ephemeral killable auth
  fork, non-setuid fork-then-drop session leader, socket-activated
  with idle-exit, relay-peer SO_PEERCRED gate (epic §0). It is the
  **live** auth/session path: the unprivileged compositor relays to it
  sans-IO (Amendments A6/A7/A8). `halmasuit-pam` + setuid
  `halmasuit-spawn` are **deleted** (Epic R10/R14/R15, Amendment A4).
- D-Bus integration (logind subscriptions + `org.halmasuit.Compositor1`
  server). After re-exec to rootfs only; not in initramfs.
- NixOS module that wires halmasuit into both initramfs
  (`boot.initrd.systemd.services.halmasuit`) and rootfs
  (`systemd.services.halmasuit`); replaces both Plymouth and greetd;
  installs the socket-activated `halmasuit-session` broker unit and
  the PAM service file `halmasuit`; configures `compositor` and
  `greeter` system users. No `security.wrappers` setuid entry exists
  (the setuid `halmasuit-spawn` is deleted — R15); the module sets the
  broker's relay-peer uid to the compositor in the live topology.
- DankGreeter launcher patch (~20 lines).
- New VM test: `tests/full-boot-flash.nix` — frame-capture from kernel
  handoff through to `SESSION` phase, asserts no all-black frame and no
  DSSIM jump above threshold across any transition.

Scope refinements live in [`PLAN.md`](PLAN.md). All adapters
(`halmasuit-luks`, `halmasuit-fsck`, `halmasuit-emergency`) are in scope
for v2 at happy-path quality; edge-case UX (LUKS retry, fsck repair
prompts, emergency recovery menu) is Phase B polish.

Done condition:

- `tests/login-flash.nix` passes (greeter→session boundary).
- `tests/full-boot-flash.nix` passes (whole-boot continuity).
- gnomon boots with halmasuit and Plymouth + greetd both removed from
  the system entirely.

### v3 — Direct-scanout optimization

- Design and publish `ext-halmasuit-host-v1` Wayland protocol for
  inner-compositor direct-scanout handshake.
- niri-side client implementation (likely as a fork patch initially,
  upstream attempt later).
- Atomic-modeset code in halmasuit to put niri's dmabuf directly on a
  CRTC plane when conditions allow.
- Falls back to normal nested composition when an overlay is up
  (recovery menu, lock screen, fsck progress).

Removes the GPU tax that v2 pays for nested rendering. Frame latency
matches running niri natively.

### v4 and beyond

- **Graceful crash recovery.** When niri (or whichever inner WM is
  configured) crashes, halmasuit survives — its sole `wl_client` just
  disconnected. Halmasuit swaps the foreground back to its internal
  wallpaper plane rendering a "session ended" scene, then transitions to `GREETER`
  phase for re-login. The apps that were running under niri are gone
  (same as today), but the user experience is a clean recovery UI
  rather than a black screen with leaked kernel text. Costs almost
  nothing beyond what v2 builds.
- **Fast user switching.** Two user sessions live concurrently;
  halmasuit hosts each user's niri as a separate `wl_client` subtree,
  swaps which is foreground. The switching UI itself lives in niri/DMS,
  not in halmasuit.
- **HDR + VRR pass-through across nesting.** Requires plumbing color
  management and presentation timing through the direct-scanout path
  (v3 prerequisite).
- **Multi-seat.** A single halmasuit process serving multiple physical
  seats — each seat its own DRM device, own foreground `wl_client`
  pipeline, shared codebase.
- **Graphical recovery mode.** When the user session won't start at
  all (broken niri config, missing binaries, broker session-launch
  failure),
  halmasuit paints a recovery menu — re-login as different user, drop
  to `halmasuit-emergency` shell adapter, reboot. Halmasuit is already
  running; recovery is just another phase.
- **Screen casting / remote display.** Owned by halmasuit since it's
  the long-lived display owner; stable across session changes.

**Explicitly not pursuing:** *true* crash isolation with client
preservation across niri restart. This would require apps to connect to
halmasuit's Wayland socket directly (not niri's) and would reduce niri
to a non-compositor policy daemon driven by a custom halmasuit protocol.
That is a fork of niri maintained against upstream forever — same
problem for hyprland, cosmic-comp, or any other inner WM. The cost is
not justified by the gain, and the gain (apps surviving a niri restart)
is small compared to the v2 win (no flashes anywhere visible during the
normal boot/login/logout pipeline). Graceful crash *recovery* above gives
us a clean failure UX without the protocol fork.

---

## Out of scope (explicit non-goals for v1 and v2)

These exist so the scope cannot creep without an explicit decision.

- **NOT in v1:** any halmasuit compositor code. The compositor crates are
  empty placeholders. v1 is purely test infrastructure.
- **NOT in v2:** direct-scanout / single-composition optimization. v2
  pays the double-composition GPU cost (halmasuit composites niri's
  surface, niri composites its apps). Acceptable trade-off for
  scope/risk; v3 fixes it via the `ext-halmasuit-host-v1` protocol.
- **NOT in v2:** client preservation across niri restart. If niri
  crashes, its apps die — same as a niri crash today on any system.
  v4's "graceful crash recovery" gives a clean halmasuit-painted
  recovery overlay, but the apps themselves are not preserved.
  *True* client preservation is explicitly not on the roadmap (it would
  require forking niri into a non-compositor policy daemon).
- **NOT in v2:** multi-seat, HDR, VRR pass-through.
- **v2 ships the wallpaper engine; shader and video backends are
  typed scaffolding.** The bottom-most plane is owned by
  `WallpaperEngine` with three pluggable backends (image / shader /
  video) behind one trait. The image backend is wired and renders
  the configured PNG/JPEG/WebP. The shader (GLSL ES 100 + declared
  uniforms, Shadertoy-compat preamble) and video (`ffmpeg-the-third`
  + minimal libavcodec h264 + dav1d) backends ship as typed stubs
  that fail closed at construction; the wallpaper-engine epic's
  follow-up tasks wire them. The Vulkan/wgpu Phase-B initramfs path
  consumes the same engine surface from the initramfs onward — no
  re-architecture, just extending backward through the kernel
  handoff.
- **NOT in v2 (happy path only):** advanced UX in the adapter crates —
  LUKS retry / advanced options, fsck repair-decision Y/N flow,
  emergency recovery menu. The adapters themselves (`halmasuit-luks`,
  `halmasuit-fsck`, `halmasuit-emergency`) ARE in scope at happy-path
  quality per [`PLAN.md`](PLAN.md).
- **NOT in v2:** OpenTelemetry export. Adding `tracing-opentelemetry`
  later is a one-line subscriber change; not needed until we have spans
  worth exporting.

---

## Anti-patterns (forbidden)

Things we will not do regardless of pressure:

- **NO running halmasuit (the compositor) as root.** It runs as the
  `compositor` user and holds no `pam_handle_t`. The only privileged
  greeter-auth code path is the `halmasuit-session` broker — a single
  socket-activated unit with no standing root when idle, audited on
  every change (a CLAUDE.md security-review event). No PAM in the
  compositor's address space (co-tenanting credentials + `dlopen`'d
  modules with the RCE surface is the privsep violation this deletes).
- **NO setuid binary in the broker's session-launch path.** The
  broker is already root and forks-then-drops in a **non-setuid**
  child (greetd/OpenSSH/GDM/login/su all do this); a world-exec
  setuid-root inode is the PwnKit / CVE-2019-14287 / CVE-2021-3156 /
  CVE-2023-22809 attack class. The setuid `halmasuit-spawn` helper is
  **deleted** — there is no setuid inode in the closure at all
  (Epic R15); no setuid spawn path may be re-introduced.
- **NO sourcing the session-leader child's supplementary groups from
  the privileged broker's own `getgroups()`** (Amendment A9; see
  [`PLAN.md`](PLAN.md#privilege-separation-decision-record)). They
  are `getgrouplist(PAM-resolved user, primary gid)`
  ONLY — the OpenSSH/login/GDM identity-derived shape. The broker
  carries its own privileged groups (e.g. `shadow`); grafting them
  onto the dropped session is the CVE-2021-41617 / sddm#1159
  escalation class. `pam_group`/`group.conf` conditional grants are
  out of scope under the one-handle-in-parent model. **NO inheriting
  environment in the child** either: env is `pam_getenvlist()` MERGED
  with a fixed allowlist, never a blind replace (it clobbers
  `pam_env`/`pam_systemd`/`pam_mount` state).
- **NO splitting `pam_handle_t` across processes / two-handle
  design.** `pam_set_data`/`PAM_AUTHTOK` are process-local heap;
  pam_mount/gnome-keyring/krb5 silently break across a split (locked
  `$HOME`, no error). One handle, one address space, whole lifecycle.
- **NO `unsafe` outside the broker's quarantined `pam_ffi`/`worker`
  modules.** Everything else is `#![forbid(unsafe_code)]`.
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

1. ~~**PAM bindings** (v2).~~ **RESOLVED: use `pam-sys` 1.0.0-alpha5
   directly, no wrapper crate.** Dual MIT-OR-Apache (matches workspace
   posture); the `1.0.0-alpha` label is misleading — it has shipped
   alphas for 4 years and the FFI surface is ~70 lines of Rust over
   a stable C ABI. Higher-level wrappers (`pam-client`, `pam`) flatten
   the conversation-message style enum and hide the `pam_conv` pointer
   we need to wire to a channel; `pam-client` is also MPL-2.0 which is
   the same license-posture problem we just paid to avoid. greetd
   itself uses `pam-sys` directly, validating the path. The libpam FFI
   lives in **exactly one** crate, `halmasuit-session` (the broker);
   its `unsafe` is quarantined to the `pam_ffi`/`worker` modules. The
   implementation is the OpenSSH/GDM shape, NOT a per-PAM-session
   thread inside the compositor: the broker holds one `pam_handle_t`
   for the whole lifecycle and runs the blocking `pam_authenticate` in
   an ephemeral, SIGKILL-able, `setrlimit`-bounded privileged **fork**
   driven over a `SOCK_SEQPACKET` socketpair; the `extern "C"` conv
   trampoline marshals challenges to and responses from that channel.
   Pitfalls (carried into the broker FFI, with origin attribution to
   the earlier in-process implementation): `catch_unwind` inside the
   `extern "C"` conv (panic-across-FFI is UB), `zeroize` response
   buffers immediately after `strdup`, set the PAM items before
   `authenticate`, handle `PAM_NEW_AUTHTOK_REQD` distinctly.
2. ~~**smithay revision** (v2).~~ **RESOLVED: pinned to niri's current
   git revision** (`ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`) in
   workspace `Cargo.toml`. Re-evaluate the pin opportunistically when
   touching smithay-adjacent code; no time-driven update cadence.
3. ~~**`switch_root` re-exec mechanism** (v2).~~ **RESOLVED via
   empirical validation in [`RESEARCH.md`](RESEARCH.md) Phases 1, 2,
   and 3:** production halmasuit uses `SurviveFinalKillSignal=yes`
   (systemd v255+, Phase 2) — the upstream-supported general-purpose
   replacement for the storage-only `@argv[0]` convention. Same PID
   and DRM master fd preserved across the boundary; no FD-passing
   needed. Phase 3 validated that an additional `execve` layer for
   clean rootfs-systemd handoff is feasible if v3 wants it. v2 Phase A
   ships with just `SurviveFinalKillSignal=yes`; the orphan-unit
   `SIGTERM` from rootfs systemd ~1s post-pivot is handled via a
   graceful SIGTERM handler. `@argv[0]` (Phase 1) remains a documented
   fallback for systemd-v254-and-older deployments.
4. **Exact Wayland protocol surface** for v2 (vs additions in v3+). The
   v2 list above is the minimum; we may add `wp_drm_lease_v1` or
   `linux-explicit-synchronization-v1` if useful.
5. **OCR in test pipeline.** May defer text-leak detection to v1.5
   if tesseract bindings prove fiddly; current tests ship with
   black-frame and DSSIM-jump detection only.
6. **D-Bus surface details.** The method list above is a starting set;
   final surface depends on what desktop-environment integration
   requires in practice.
7. **`halmasuit-luks` UI form.** During `INITRAMFS_SPLASH` the screen
   shows the splash; when cryptsetup needs a passphrase, do we
   (a) replace the splash with `halmasuit-luks` as foreground, or
   (b) overlay `halmasuit-luks`'s prompt on top of the still-rendered
   splash via subsurface composition? (b) is more visually continuous
   but more compositor complexity. Decision when the adapter is
   implemented.
