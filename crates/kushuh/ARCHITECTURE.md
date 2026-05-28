# kushuh — ARCHITECTURE

A Wayland compositor designed around **role-based layout**: workspaces
are not bags of windows but sets of named slots ("roles") that apps
bind to. Operations target roles by name, not positions by direction.
The metaphor is Eclipse perspectives for the whole desktop — and the
navigator is a literal **star map** where each perspective is a star,
each window is a planet in orbit, and switching perspectives is a
fly-through transition.

This document captures the complete design space discussed during
brainstorming. It is not a roadmap of dated milestones; it is the
architecture, prior art, and open questions for the project.

---

## Naming convention

- **Lowercase** in code, config, CLI, crate names, and project prose:
  `kushuh`, `halmasuit`. Matches halmasuit's existing crate-naming
  pattern and Unix tradition.
- **Capitalized** in prose only when explicitly referring to the
  deity: "named after the Hurrian moon god Kushuh."

So `kushuh` is the binary and the project; Kushuh is the deity it's
named after. Same for `halmasuit` / Ḫalmašuit.

### Etymology

**halmasuit** (HAHL-mah-shoo-it; from Hittite Ḫalmašuit / Hattic
Ḫanwašuit, "the throne") — the deified throne goddess of the Hittite-
Hattic pantheon. Bronze Age Anatolian, attested as early as the
~17th century BCE. Personification of royal authority and protector
of the king. Closely associated with the war-god Wurunkatte. The
word literally means "she on which one sits": `ḫa-` (locative) +
`-waš-` (root "to sit") + `-it` (feminine suffix).

**kushuh** (KOO-shoo; from Hurrian Kušuḫ) — the Hurrian moon god,
adopted into the Hittite pantheon and ranked above the sun god
Šimegi in the Hurrian hierarchy. Depicted as a winged figure with a
crescent helmet, standing on a lion. "Lord of the Oath." In the
*Song of Silver* myth, hurled from heaven by the demon Ušḫuni — an
explicit astral-mechanics myth (a celestial body thrown across the
cosmos). The final ḫ is a velar fricative; English speakers
typically drop it.

The pairing — Hittite throne goddess + Hurrian moon god — is
intentionally syncretic. The Hittite pantheon was famously syncretic,
absorbing deities from Hattic, Hurrian, and Luwian traditions. Naming
this project pair the same way is honest to the source material.
Both names describe their layer's function: `halmasuit` is the
throne (where the system sits, the framework), `kushuh` is the
celestial body (the visible compositor, the star map that runs on
the throne).

---

## Why this exists

The status quo of Linux compositors offers three layout models:

- **Tiling** (Sway, i3, Hyprland-dwindle, AeroSpace): apps fill the
  screen via spatial-primitive operations — split here, move left,
  swap tiles. Position primary, app identity secondary.
- **Floating** (Mutter, KWin, macOS, Windows): apps live on a 2D
  plane wherever they're placed. User-placement primary.
- **Scrolling-tiling** (niri, PaperWM): apps form a horizontal scroll
  of columns. Time-axis primary.

Every existing compositor treats *position* as primary and *app
identity* as secondary. Window-rules and assignment systems (i3
`assign`, niri `open-on-workspace`, AeroSpace `workspace-to-monitor-
force-assignment`) let users layer app-identity routing on top, but
the underlying engine still thinks in tiles/columns.

`kushuh` inverts this. **App identity is the primary abstraction.**
A role names a position-on-screen and (optionally) binds an app to
it. Operations name the role. The layout is a declarative property
of the role-set, not an emergent artifact of window-add history.

This matches the pattern every modern IDE has used for 25 years
(Eclipse perspectives, VS Code panels, JetBrains tool windows,
Visual Studio docking) but has never been promoted to first-class
in a Linux compositor. The closest existing implementations are
documented in [Prior art](#prior-art) below; none of them have role-
binding as the central abstraction.

### Where the design pressure came from

The author runs niri with 14 patches against it. Roughly 5-6 of
those patches are "make niri's layout model behave more like
Mutter's floating model for traditional desktop apps." Roughly 3-4
are "fix Zoom's specific misbehavior." The remaining 4-5 are
niri-design-specific polish. The dominant theme is fighting niri's
spatial-primitive engine to get identity-keyed behavior.

The author's existing `focus-or-spawn` script binds apps to letter
keys (`Alt+W` → Firefox wherever it is, `Alt+T` → kitty, etc.). The
script ignores spatial state and operates by app identity. This is
already the operative compositor model in userland — `kushuh`
promotes it to the engine level.

---

## Core abstraction: roles, perspectives, layouts

### Role

A **role** is a named position on screen with optional app binding:

```kdl
role "editor" {
    monitor "DP-3"
    region 0 0 100 100   # x, y, width, height as percentage of monitor
    bind-app "kitty"     # sticky binding
}

role "browser" {
    monitor "DP-2"
    region 0 0 60 100
    bind-app "firefox"
}

role "companion" {
    monitor "DP-2"
    region 60 0 40 100
    bind-app-candidates ["claude-desktop" "spotify"]   # cycle
}

role "scratch" {
    monitor "DP-2"
    region 0 80 100 20
    # no bind-app — flex role, manual placement via slot-place
}
```

Three binding modes:

- **Sticky**: one app, always there. `bind-app "X"`. The role IS
  that app's slot. Closing X leaves the role empty; relaunching
  fills it again.
- **Cycle**: multiple candidates, swap with `Mod+R, →` or per-app
  keys. The role hosts one of N candidate apps; user chooses which.
- **Flex**: no binding. `slot-place` ad hoc; any app can land here.

### Perspective

A **perspective** is a named set of roles:

```kdl
perspective "code" {
    roles "editor" "browser" "companion"
}

perspective "meeting" {
    role "meeting-video" {
        monitor "DP-2"
        region 0 0 100 100
        bind-app-pattern "Zoom" title="^Meeting$"
    }
    role "meeting-chat" {
        monitor "DP-3"
        region 0 0 100 50
        bind-app "slack"
    }
    role "notes" {
        monitor "DP-3"
        region 0 50 100 50
        bind-app "obsidian"
    }
}

perspective "reading" {
    role "primary" {
        monitor "DP-3"
        region 0 0 100 100
        bind-app-pattern "firefox" title="(?i)reader"
    }
}
```

Switching perspectives is the equivalent of switching workspaces in
tiling compositors, but it changes the **whole layout**, not just
which set of windows is visible. The desktop reshapes around the new
role set.

### Ambient role

Every perspective has an implicit **ambient** role for unbound apps:

- Default: full-monitor on the last-focused monitor
- Anything launched without a binding lands here
- `Mod+A` cycles ambient windows
- `Mod+B <role-letter>` promotes ambient → role binding (writes to
  runtime overrides)

The escape hatch. Without it, role-based becomes prescriptive in a
bad way — users get punished for launching novel apps they hadn't
already bound.

### Per-monitor roles

Roles bind to specific monitors via the `monitor` field. Adding a
third monitor doesn't shuffle existing role layouts; users define
new roles on the new output and reference them from perspectives
that should use it. Hotplug-on-the-fly: when a monitor disappears,
roles bound to it become inactive (their apps move to a `hidden`
holding area); when it reappears, roles re-activate where they were.

---

## Navigation

| Bind | Action |
|---|---|
| `Alt+<letter>` | Focus app by identity (focus-or-spawn — bring me to X) |
| `Mod+<letter>` | Focus role by name (jump to "browser" role wherever it lives) |
| `Mod+<digit>` | Switch perspective |
| `Alt+H/J/K/L` | Spatial nudge — focus role to the left/below/above/right |
| `Alt+Shift+H/J/K/L` | Move focused role's contents to adjacent role (swap) |
| `Mod+R <letter>` | Place app at right slot of current focus (slot-place primitive) |
| `Mod+L <letter>` | Place app at left slot |
| `Mod+R →` | Cycle right slot through candidates |
| `Mod+Return` | Spawn new window of focused role's app (tab/stack) |
| `Mod+J/K` | Cycle within current role's stack |
| `Alt+Space` | Launcher (rofi/wofi/DMS) for ambient apps |
| `Mod+B <letter>` | Bind focused window to role X |
| `Mod+Shift+E` | Enter layout-edit mode (drag role boundaries) |
| `Mod+Shift+H/L` | Nudge role boundaries (shrink/grow current role) |
| `Super+S` | Open star-map navigator |
| `Super+M` | Open floorplan view (debug overlay of role structure) |

The user's existing AeroSpace muscle memory transfers cleanly:
`Alt+H/J/K/L` is still spatial nudge. What changes is that
`Mod+1/2/3` switches *perspectives* (whole-desktop layouts), not
1D workspaces. The identity-keyed `Alt+<letter>` binds are
unchanged from the current `focus-or-spawn` workflow.

---

## The star-map navigator

### Concept

A **literal visualization** of the desktop as a star system:

- **Star** = perspective. Current perspective is the close, bright
  sun; others recede into the deep field at configured positions in
  the star field.
- **Orbital ring** = monitor. Inner ring = primary monitor, outer =
  secondary, etc. Radial position encodes which monitor a window is
  on.
- **Planet** = window. Sized by tile area, colored by app identity
  (firefox a cool blue, kitty a warm gray, Discord a pale violet).
- **Angular position** on the ring = position within the monitor.
  12 o'clock for top-bound roles, 3 o'clock for right-bound, etc.
  Polar position mirrors cartesian position on screen.
- **Multi-planet orbits** (tabbed-within-role): planets at the same
  radius, different angles. Like Jupiter and Saturn at different
  positions on the ecliptic.

### Geometry is derived, not authored

The orbital geometry is a projection of the role configuration:

```
For each perspective P:
  star_position = auto-laid-out in the star field (or user-configured)
  For each role R in P:
    orbit_radius = monitor_index(R.monitor) + 1
    For each window W in R:
      planet_angle = angle_of_region_center(R.region)
      planet_size = sqrt(R.region.area / monitor_area)
      planet_color = app_color(W.app_id)
```

You don't draw the star map. You write role config and the star map
renders the inevitable consequence. That's why it's both navigation
tool *and* visualization — the picture is the same shape as the
system.

### Navigation gestures

- `Super+S` enters star-map. Current desktop pulls back, dissolving
  into the chosen perspective's star.
- Pan with `hjkl` / arrows / mouse.
- `Tab` cycles to nearest star. `Enter` warps: the chosen star zooms
  in, its planets resolve into the actual windows, dissolves back to
  desktop. **The animation IS the perspective swap.**
- Within a star, planets are individually focusable.
- `Enter` on a non-current-star planet swaps perspective AND focuses
  that window. Two operations, one gesture.
- `Esc` exits star-map, returns to where you were.
- `Super+S Super+S` — quick toggle (symmetric press).
- Long-press `Super+S` — drift mode (gentle orbital motion, you
  watch your desktop turn while idle).

### Aesthetic primitives

- **Living orbits**: planets slowly rotate (full revolution ~5 min)
  when in star-map mode. Visible only there; ignorable.
- **Ghost planets**: windows that *should* be running per
  `autostart-in-role` but aren't appear as translucent outlines at
  their orbital position. Click to spawn-and-focus. Config intent
  visible alongside reality.
- **Comets**: floating dialogs and ambient windows traverse
  elliptical paths — they enter the system, cross, and leave.
  Visually distinct from settled planets; the aesthetic enforces
  the temporary-by-design semantic.
- **Constellation lines**: optional thin lines connecting planets
  in the same role. Visual grouping when a role has multiple
  stacked windows.
- **Nebula backdrop**: halmasuit's wallpaper shader pipeline (e.g.
  hexrain) is *already* the cosmic background in every mode,
  including star-map. Because halmasuit renders the wallpaper
  below the nested `kushuh` compositor, entering star-map just
  means `kushuh` renders the star-system overlay with maximum
  transparency in the negative space — and the wallpaper that's
  been there all along becomes the deep field. Visual coherence
  for free.
- **Spectral classes**: each perspective's star has a
  color/temperature reflecting its character. `meeting` could be a
  red giant (intense, demanding); `reading` a cool blue main-
  sequence; `code` a yellow main-sequence (familiar, productive).
  Pure flavor.
- **Recency trails**: recently-focused window leaves a brief light
  trail. Visual MRU.

### Modes

- **Invoked** (`Super+S`): full-screen takeover, used for active
  navigation. No constant chrome cost.
- **Minimap** (corner widget, opt-in): tiny live star-map in a
  corner of the wallpaper, always visible. Ambient awareness
  without commitment. Probably 200×200 pixels on the right edge of
  a chosen monitor.

Both modes ship; user enables the minimap if desired.

### Implementation cost

Maybe 3-5K LOC of GL on top of the existing wallpaper shader
pipeline. The render-to-texture pattern for each perspective's mini-
render is straightforward; the rest is camera-pan and animation. The
animation framework (warp-tunnel transition, parallax, planet
motion) is the load-bearing visual work.

---

## Reactive wallpaper / event bus

The wallpaper isn't a static image — it's an **ambient feedback
channel that reflects system state**. The framing: *"the desktop has
weather"*, not "achievement-unlocked gamification." The visual
environment reacts to system state the way weather reflects
atmospheric state — you don't *notice* it constantly, but you
*sense* the mood.

### Event bus

`halmasuit-events` is a new crate exposing a Unix-socket pub/sub for
system events. Lives in halmasuit's workspace; consumed by
halmasuit-wallpaper, kushuh, DMS (eventually), and any external
script that wants to publish.

```rust
pub enum SystemEvent {
    Phase(SystemPhase),                                   // halmasuit
    WindowOpen { app_id: String, role: Option<String> }, // kushuh
    WindowClose { app_id: String },                       // kushuh
    Fullscreen { enabled: bool, app_id: String },        // kushuh
    PerspectiveSwitch { from: String, to: String },      // kushuh
    Focus { app_id: String },                            // kushuh
    CpuLoad(f32),                                         // halmasuit monitors
    MemoryPressure(f32),                                  // halmasuit monitors
    NetworkChange { online: bool },                       // halmasuit
    UserSignal { name: String, payload: Value },         // scripts publish
}

pub enum SystemPhase {
    Boot,            // kernel up, halmasuit starting
    LuksPrompt,      // disk decryption screen
    Greeter,         // signin
    SessionActive,   // user logged in, session running
    Idle,            // long user idle
    Shutdown,        // PrepareForShutdown received
    PostPivot,       // past the rootfs→shutdownRamfs pivot
}
```

External scripts publish too:

```bash
halmasuit-event publish user.event name="focus-mode"
halmasuit-event publish compositor.fullscreen enabled=true app_id=steam
```

### Wallpaper state machine

The wallpaper engine subscribes to events and runs a KDL-defined
reaction config. The primary mechanism is **uniform morphing** —
interpolating shader uniforms over a duration — not shader-swapping.
Visual continuity across events.

For the **hexrain** shader specifically (the initial theme,
implemented in the author's DankMaterialShell fork), reactions push
palette transitions via the shader's existing `flipOriginX/Y` +
`flipStartTime` + `flipPropDelay` + `flipDuration` machinery. The
propagating-wave palette transition is already built into the
shader. The reactive system just chooses when to fire it and with
what destination palette.

The shader exposes:

- `colorPrimary` / `colorSecondary` / `colorTertiary` /
  `colorPrimaryContainer` — current palette
- `colorPrimaryNext` etc. — destination palette during a transition
- `flipOriginX/Y` — wave origin in virtual-canvas space
- `flipStartTime` — when the wave fires (1e9 = no wave pending)
- `flipPropDelay` — propagation speed (in hex-pitch units)
- `flipDuration` — per-cell transition duration
- Plus the full layered light system: back/front positive/negative
  suns, fast back-sun streaks, neighbour-height leak model, wind
  modulation, bar-zone elevation.

### Boot progression example

```kdl
wallpaper "hexrain" {
    on system.phase=boot {
        # Start black, no suns, no rain
        colorPrimary "#000000"
        colorSecondary "#000000"
        colorTertiary "#000000"
        colorPrimaryContainer "#000000"
        backSunCount 0
        frontSunCount 0
    }
    on system.phase=phase-1 {
        push-next-palette {
            primary "#1a2a4a"
            secondary "#2a3a5a"
            tertiary "#3a4a6a"
            container "#0a1a2a"
        }
        flipOriginX 0.5 * canvas_width
        flipOriginY 0.5 * canvas_height
        flipDuration 1200ms
        # then commit: current ← next, flipStartTime → 1e9
    }
    on system.phase=phase-2 {
        push-next-palette { /* richer blues */ }
        backSunCount 0 -> 2 over 800ms
    }
    on system.phase=luks-prompt {
        push-next-palette { /* amber-tinted */ }
        sunDriftSpeed -> 0.5 over 300ms
    }
    on system.phase=greeter {
        push-next-palette { /* morning-bright */ }
        frontSunCount 0 -> 1 over 400ms
    }
    on system.phase=session-active {
        defer-to-perspective
    }
    on compositor.perspective.switch {
        flipOriginX <perspective-direction-origin>
        flipDuration medium
        push-next-palette <new-perspective-palette>
    }
    on compositor.fullscreen enabled=true {
        intensity 1.0 -> 0.0 over 200ms
    }
    on compositor.fullscreen enabled=false {
        intensity 0.0 -> 1.0 over 400ms
    }
    on system.phase=shutdown {
        push-next-palette { /* all near-black */ }
        flipDuration 2000ms
    }
}
```

### Transition duration defaults

```kdl
transition-duration {
    light 250ms     # window-open, focus-change, small things
    medium 400ms    # perspective-switch, fullscreen, mode changes
    heavy 1500ms    # boot, shutdown, ceremonial transitions
}
```

User retunes the project's tempo by changing three numbers. Default
profile is 250 / 400 / 1500 — snappy for everyday operations,
substantial for ceremonies.

### Themes are pluggable

`hexrain` is the initial theme. Other themes ship with their own
shader + uniform schema + event reactions. The wallpaper engine
doesn't know which theme is loaded — it just knows how to morph
uniforms named in the active theme's schema. Themes ship as data
(shader + KDL reaction config), not code.

---

## Architecture

### Position in the workspace

`kushuh` lives as a workspace crate inside halmasuit:

```
halmasuit/
  crates/
    halmasuit/             # daemon: lifecycle, greeter relay, broker host
    halmasuit-session/     # privileged broker (PAM, session leader)
    halmasuit-decoder/     # video decoder relay (sandboxed)
    halmasuit-events/      # event bus (NEW)
    halmasuit-wallpaper/   # wallpaper engine; eventually extracted from
                           # halmasuit-the-daemon for code organization,
                           # but `kushuh` does not depend on it (the
                           # nested-compositor model means halmasuit
                           # renders the wallpaper, not kushuh).
    kushuh/                # this — compositor binary
    ...
```

Shared workspace = shared git history, shared CI, atomic feature
commits, shared VM test infrastructure. Two coordinated sibling
projects living in one repo because the author is solo.

### Opt-in posture

`kushuh` is a **binary**, not a library. halmasuit doesn't link it.
Users opt in via session config:

```nix
services.halmasuit = {
  enable = true;
  session.command = "${pkgs.kushuh}/bin/kushuh";   # opt-in
};
```

halmasuit can equally well spawn niri or sway or any other
compositor as the session command. It doesn't care which compositor
runs; the integration with `kushuh` is purely via well-defined APIs
(broker socket, event bus, wallpaper protocol). halmasuit's identity
as "lifecycle framework that hosts a compositor" is preserved.

### License posture

Dual MIT-OR-Apache, matching halmasuit and the broader Rust-Wayland
infrastructure tier (smithay, wlroots, Weston). No GPL dependencies
in production code paths. If a protocol implementation requires
reading GPL source, do a clean-room reimplementation following the
`halmasuit-greetd` pattern.

### Dependency posture

Built on **smithay**, pinned to the same revision halmasuit uses
(currently tracking niri's pin). Wayland-protocol XML via smithay's
generator. Calloop for the event loop. Tracing for instrumentation,
matching halmasuit's pattern.

No glib. No bindgen at build time for production deps. No second
libpam consumer (halmasuit-session owns that surface; `kushuh` never
links libpam).

---

## Integration with halmasuit

### The nested-compositor model

halmasuit is the **outer compositor** — owns the GPU, owns the
framebuffer, renders the wallpaper as its own surface directly.
`kushuh` (and niri today) runs **nested inside halmasuit** as a
Wayland client of halmasuit's. The nested compositor renders the
session's content — windows, layout chrome, status bars — with
transparent backgrounds where there isn't opaque window content.
halmasuit's wallpaper shows through everywhere the nested
compositor's render is transparent: gaps between windows, through
transparent windows (kitty with `background_opacity<1.0`), through
the nested compositor's transparent chrome.

So `kushuh` doesn't render the wallpaper. halmasuit does. Always.
This holds whether the session compositor is niri, sway, or
`kushuh`. The wallpaper is a property of the throne, not of the
compositor that happens to sit on it. (Which is, incidentally, the
cleanest possible fit for "halmasuit means throne" — the wallpaper
IS where the throne is rendered, persistently, across session-
compositor restarts and crashes.)

### What `kushuh` consumes from halmasuit

- **Broker socket** (halmasuit-session): if running as session
  compositor, for greeter spawn / session relay.
- **Event bus** (halmasuit-events): publishes compositor events
  (window-open, perspective-switch, fullscreen, focus, role
  transitions) to halmasuit, which uses them to drive the reactive
  wallpaper. Also subscribes to system events (phase, cpu-load,
  idle) if `kushuh` wants to react to system state for its own
  visual chrome (e.g., dimming the layout-edit mode during idle).
- **frame_audit events**: for VM-test introspection (matches Phase
  B / Epic #61 testing patterns).

`kushuh` does NOT consume `halmasuit-wallpaper`. The wallpaper
engine lives in halmasuit and renders below the nested compositor
without any nested-compositor involvement.

### What `kushuh` exposes

- **niri-compatible IPC** (initial): Unix socket responding to
  `niri msg`-shaped JSON. Lets DMS's existing niri backend work out
  of the box. Map "workspace" to "perspective" in the protocol
  surface; emit niri-shaped workspace events when perspectives
  change.
- **kushuh-native IPC** (later): perspective/role-aware vocabulary.
  New DMS module consumes this for perspective-aware widgets.
- **Mutter DBus surface** (`org.gnome.Mutter.ScreenCast`,
  `org.gnome.Mutter.RemoteDesktop`): for Zoom and other apps that
  gate their Wayland code path on `XDG_CURRENT_DESKTOP=GNOME`. Port
  from niri's implementation.
- **PipeWire dual-format screencast**: DMA-BUF + SHM offering,
  matching mutter's `meta-screen-cast-stream-src.c`. The SHM-
  fallback is what wrvsrx's niri PR #1791 adds; port that logic.

---

## DMS integration

Division of labor between DMS (Dank Material Shell) and `kushuh`:

| Concern | Owner |
|---|---|
| Status bar (clock, system tray, MPRIS) | DMS |
| Notifications | DMS |
| App launcher | DMS |
| Audio / brightness OSDs | DMS |
| Lock screen | DMS |
| Workspace indicator (now: perspective indicator) | DMS, with kushuh IPC |
| Star-map navigator | kushuh native |
| Floorplan view | kushuh native |
| Role-edit mode | kushuh native |
| Perspective switcher | kushuh native |
| Wallpaper render | halmasuit (rendered below the nested compositor; visible through kushuh's transparent backgrounds) |

DMS keeps doing what DMS does well. `kushuh` handles the parts
intrinsically about its role/perspective model. The boundary is
clean: DMS is a layer-shell client, `kushuh` is the compositor.

---

## Implementation phases

Five phases, each roughly one halmasuit-style epic. At the author's
demonstrated velocity (~2K LOC/day including tests), each phase is
roughly a week of focused work.

### Phase 1: Types + config schema

- `kushuh` crate scaffolded
- `Role`, `Perspective`, `Region`, `Layout` types
- KDL config parser for role/perspective definitions
- Stub Wayland delegate impls (no rendering)
- Unit tests for the data model

Goal: prove the model is coherent at the type level before writing
a single line of rendering code.

### Phase 2: Smithay scaffolding

- Wayland socket, basic xdg-shell, layer-shell, popups, subsurfaces
- Input handling (libinput via udev, matching halmasuit's direct-
  DRM pattern from R2.3)
- DRM output management, mode setting, hot-plug
- Renders a single client into the focused role
- VM test: kushuh launches, accepts a test client, terminates
  cleanly

Behind a feature flag in halmasuit's session config. niri still
default at this point.

### Phase 3: Role-based layout engine

- Roles + perspectives implemented
- All keybinds wired (focus-by-app, focus-by-role, perspective-
  switch, slot-place, ambient cycle)
- Ambient role + window-rule overrides
- Per-monitor role binding with hotplug handling
- DMS works against niri-compat IPC
- Real-app testing: Firefox, kitty, Slack, Zoom

### Phase 4: Star-map navigator + polish protocols

- Star-map navigator (Super+S) with all aesthetic primitives
- Floorplan view (Super+M)
- Minimap mode (opt-in corner widget)
- Reactive wallpaper integration (consumes halmasuit-events)
- Mutter DBus surface for Zoom compat
- PipeWire dual-format screencast with SHM fallback
- xdg-decoration, drm-syncobj, drm-lease, color-management (app
  compat protocols)

### Phase 5: Flip the default

- `kushuh` becomes halmasuit's default session compositor
- niri retired from the live system (no longer needed as a
  Wayland session compositor; halmasuit hosts everything natively)
- Documentation, public README, project pitch, sharing space

---

## Prior art

### Conceptually closest matches

| Project | Domain | Closeness | Notes |
|---|---|---|---|
| Eclipse perspectives | IDE | 9/10 | The original "named view-slots, apps bind to slots, switch perspective = full layout swap" |
| Microsoft FancyZones (PowerToys) | Windows desktop tiling | 8/10 | Define zones, bind apps to zones (`app-zone-history.json`), snap on creation. Windows-only, not a compositor. |
| Qt Application Manager, CrossControl WM | Embedded/automotive HMI | 8/10 in domain | Role-based JSON region trees with apps bound to regions. Already shipping in cars and industrial HMIs; never came back up to laptops. |
| JetBrains tool windows, VS Code panels, VS docking | IDE | 7/10 each | Same idea, less explicit than Eclipse. |
| StumpWM `define-frame-preference` | Linux WM | 6/10 | 2D named frames + class-based routing. Frames are numbered (not semantically named), and routing is window→frame (not frame→app). Closest Linux ancestor. |
| Material Shell (archived) | GNOME extension | 5/10 | Tried "enforce a layout" but never reached named roles before going unmaintained. |
| Sawfish | Linux WM | 5/10 | Lisp-scriptable matching can express the user's concept entirely in user Lisp, but no built-in named-slot abstraction. |
| Emacs `display-buffer-alist` + window parameters | Editor | 6/10 | Dedicated windows with `slot` parameters; buffers route to slot-bound windows. |
| niri named workspaces | Wayland | 4/10 | 1.5D version (app→named-workspace + monitor pinning). No within-workspace position binding. |
| i3 / Sway `assign`, xmonad `manageHook`, awesome `awful.rules` | Tiling WMs | 2/10 | 1D version (app→workspace). No within-workspace position binding. |
| i3 `mark` / `con_mark` | Tiling WM | 3/10 | Per-window bookmarking; closer to vim marks than to durable role binding. |
| AeroSpace `workspace-to-monitor-force-assignment` | macOS tiling | 4/10 | 1D role-binding (workspace→monitor). Tree tiling within workspaces is spatial-primitive. |
| Komorebi / GlazeWM | Windows tiling | 3-5/10 | i3-style tiling with declarative config; no named slots. |
| yabai | macOS tiling | 2/10 | BSP tiler with rules; no slot primitives. |

### What's genuinely novel

**No Linux compositor ships role-based layout as the *central*
abstraction.** Every Linux candidate ships a spatial-primitive
engine first and overlays a routing-rule layer; FancyZones comes
closest on Windows but isn't a compositor; embedded HMI compositors
ship the model but never crossed back to general-purpose desktops.

The defensible pitch:

> *"kushuh brings the perspective/view-slot model that has defined
> IDE window management for 25 years (Eclipse, JetBrains, VS Code)
> and the zone-and-bind model that FancyZones brought to Windows
> desktop tiling, and promotes it from a routing-rule layer to the
> compositor's primary abstraction. Workspaces define named slots;
> apps bind to slots; operations target roles, not coordinates.
> Layout is a declarative property of the role-set, not an artifact
> of window-add history. niri's named-workspaces is the closest
> existing Linux primitive; StumpWM's `define-frame-preference` is
> the closest historical Linux ancestor; FancyZones is the closest
> live implementation in any desktop platform."*

That paragraph survives review. It cites prior art accurately,
claims a specific gap, and explains the contribution.

---

## Open design questions

These are decisions to make explicitly, not punt on.

### Per-perspective vs global roles

Eclipse: views are shared across perspectives (one Console exists;
multiple perspectives that include it show the same one). Workspace-
style isolation: each perspective gets its own browser instance.
Probable default for `kushuh`: **shared apps, per-perspective role
bindings** — Firefox is one process; multiple perspectives can route
it to different roles. To be confirmed.

### Multi-app cycle vs tabbed within role

When a role has `bind-app-candidates [A, B]`, is the second app
*hidden* (cycle: only one visible at a time) or *tabbed* (both
present, tab between them)? Probable default: **cycle** for
explicitly listed candidates; **tabbed** when multiple instances of
the same app (Firefox + second Firefox window) land in one role.

### Floating dialogs

Apps spawn modals, transient toplevels, splash screens. `kushuh`
needs floating-with-smart-placement (mutter-style) for unhinted
dialogs. The `floating` window-rule attribute exists; smart
placement algorithm is TBD. Probable approach: anchor floating
windows to their parent's role center; fall back to working-area
smart-place when no parent.

### Star-map minimap default

Minimap-corner-widget on by default, or opt-in? Probable default:
**opt-in**, discoverable via star-map mode itself (a "minimap
toggle" within `Super+S`).

### Empty perspective rendering

A perspective with 0 windows running. White dwarf? Neutron star?
Empty space around the star? Probable default: a star with no
planets and an outline ring per monitor — empty configuration is
visible structure.

### Planets-as-thumbnails vs colored-spheres

Live thumbnails (mini-render of each window) on planets looks
incredible, GPU-cost real. Colored spheres + app icons clean,
scales further. Probable default: **colored spheres for distant
stars; thumbnails for focused star** (LOD-based).

### IPC versioning

niri-compat IPC is a moving target. Pin to a specific niri version?
Track latest? Probable: track latest, document the pinned version
per `kushuh` release.

### Wallpaper theme distribution

Themes are pluggable (shader + KDL reaction config). Where do they
live? Probable: ship one or two with `kushuh` (`hexrain` is the
initial); community themes via a separate repo, user-installable to
`~/.config/kushuh/themes/`.

---

## Anti-patterns (forbidden)

These are halmasuit's hard rules, applied to `kushuh`.

- **No `_v2` suffixes, no parallel implementations, no backwards-
  compat shims.** Standard halmasuit rule. Refactor, don't
  accumulate.
- **No spatial primitives as the central abstraction.** "Split here,
  move left" is a niri/Sway/AeroSpace thing. `kushuh`'s central
  operation is "name a role." Spatial nudges exist as a navigation
  convenience layer on top, not as the engine.
- **No fork of niri to add roles.** Forking would put us inside
  YaLTeR's spatial architecture. Build fresh on smithay; the
  Smithay scaffolding cost is real but bounded (~3-5K LOC), and the
  architectural cleanness is worth it.
- **No corporate compositor model.** `kushuh` is one-author. "We'll
  hire a team" or "this is for a Linux distro vendor" doesn't apply.
- **No monetization gating.** If the project grows a community, it
  stays free. GitHub Sponsors / grant funding is optional support,
  not gated access. The desktop-rice community is monetization-
  allergic; don't fight that.
- **No GPL bleed.** Dual MIT-OR-Apache. No linking against GPL-only
  libraries.
- **No in-process PAM.** halmasuit-session owns libpam. `kushuh`
  never links it.
- **No setuid binaries in the closure.** `kushuh` runs as the
  compositor user (typically `halmasuit-compositor`); privilege
  ops route through halmasuit-session.

---

## Funding & sustainability posture

Per public data on every comparable project: **no single-author
Wayland compositor has been turned into meaningful primary income
via donations alone.** Survey of the class:

- **niri (YaLTeR)**: GitHub Sponsors with 181 supporters, amount
  undisclosed. PhD student. Side project.
- **Hyprland (Vaxry)**: launched €5/month "Premium" tier in 2026;
  DHH publicly committed €1000/month. Even so, Vaxry's own
  statement: *"I need something to eat too. Once I end university,
  if I can't make this my full-time job, I will have to severely
  decrease my contributions."*
- **river (ifreund)**: Liberapay + Sponsors + Ko-fi. README states
  development pace is "not sustainable without more financial
  support."
- **sway / wlroots (Drew DeVault)**: makes his living from
  **SourceHut** (the forge), not sway. Handed sway/wlroots to
  Simon Ser in 2020.
- **labwc**: pure volunteer.
- **cosmic-comp**: salaried System76 employees.
- **mutter / KWin**: corporate-employed (Red Hat / SUSE / KDE e.V.).

The two paths to "compositor work pays the bills" are (1) a separate
business that runs alongside (DeVault/SourceHut) or (2) corporate
employment. Donations alone? Nobody has cracked it.

### Realistic outcomes for `kushuh`

1. **Hobby project with optional GitHub Sponsors tip jar** (modal)
2. **Portfolio / reputation building** — opens doors to
   compositor/Linux-infrastructure consulting or employment
3. **Grant funding for specific milestones** (NLnet, Sovereign Tech
   Agency) — burst income, $5-30K per project, not regular
4. **An off-ramp to consulting / corporate compositor work** if
   desired

### Sharing space posture

A community space where people show their `kushuh` configurations
(role definitions, wallpaper themes, custom shaders) across the
boot stages and perspectives. Should be **operationally cheap**, not
monetized:

- Static site (GitHub Pages, Cloudflare Pages, Vercel free tier)
- Submissions via Git (users PR their config + screenshots to a
  public repo; auto-render an index)
- Image/video hosting: link out to YouTube for boot-animation
  clips; Cloudflare R2 for screenshots if needed
- Total burn rate: <$10/month at decent scale

The project does not need to make money. It needs to not cost
money. The desktop-rice community is famously monetization-
allergic; do not try to charge them. The community IS the value.

---

## Conventions

Matching halmasuit's existing CLAUDE.md / project conventions:

- **Just** as entry point: `just check` (lint + nextest), `just
  test-vm` (full VM-test sweep)
- **Edition 2024, Rust 1.95** (matches halmasuit's pin)
- **Workspace lints** in `[workspace.lints]`; per-crate `#![allow]`
  only with `// reason:` comments
- **NixOS VM tests** are the integration-test layer; `tests/
  visual-*.nix` pattern. Phase-B-style golden matrices for visual
  regression; `phash_progression` for animated-wallpaper
  assertions.
- **TDD loop for compositor changes**: targeted VM test as RED;
  full sweep at task boundaries only.
- **frame_audit feature flag** for introspection events
- **Lowercase** in code and project name; **capitalized** in prose
  for the etymological reference

---

## Why this is worth building

The honest case for `kushuh`:

1. **The author's revealed workflow is identity-keyed, not spatial.**
   14 niri patches mostly fight niri's layout model to add identity-
   routing. `focus-or-spawn` is already a userland identity
   primitive. `kushuh` makes identity primary at the engine level —
   eliminating most of the patch surface.

2. **The model has 25 years of UX validation in IDEs.** Eclipse
   perspectives, JetBrains tool windows, VS Code panels. These
   designs work; the question has only been whether someone applies
   them to the Linux desktop. Nobody has.

3. **The star-map navigator is the screenshot.** It's the image
   that explains the project in one frame. *"halmasuit is the
   throne, kushuh is the moon god, the desktop is a star system."*
   Self-explaining identity that converts an abstract claim
   ("first-class role-based layout") into something concrete and
   emotional.

4. **Cost is bounded.** ~3-4 weeks at the author's demonstrated
   velocity (15K-20K LOC + tests). Smaller than halmasuit itself.
   Not a multi-year commitment.

5. **The project's center of gravity is already shifting.**
   halmasuit was framed as power-behind-the-throne, but the
   etymology reveals it always literally meant "throne." Adding
   `kushuh` makes the project's name accurate — halmasuit hosts the
   compositor that lives on the throne.

6. **It's worth building for the author first.** Nobody else needs
   to use it. If it ends up being a personal Linux desktop
   optimized for one person's workflow, that's a successful
   outcome. The desktop-rice community might pick it up; might not.
   The project's primary user is the author.

### The case against

It's still 3-4 weeks of work, and niri-with-patches is currently
working. The decision isn't "should I build this" — it's "do I want
to express this design as a real thing, or is the niri-with-scripts
approximation sufficient?"

If the niri-side friction keeps growing (more app-misbehavior
patches, more layout fights, more cross-monitor work as
multi-monitor scales), `kushuh` becomes the durable answer. If niri
remains tolerable, the work is optional.

The remaining decision is in the author's hands, not in the
architecture.
