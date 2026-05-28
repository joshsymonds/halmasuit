// halmasuit — Linux system compositor.
//
// Long-lived display-server process owning the GPU from
// `graphical.target` to shutdown. Hosts the greeter and the user
// session as nested wayland clients of itself, replacing greetd
// entirely. The convergence epic (#12) made halmasuit's protocol
// surface conformant for arbitrary Qt 6 + GTK 4 toolkits:
//
//   wayland.xml core (6):   wl_compositor, wl_subcompositor, wl_shm,
//                           wl_output, wl_seat (kbd/ptr/touch),
//                           wl_data_device_manager
//   stable (5):             xdg_wm_base, wp_viewporter, wp_presentation,
//                           zwp_linux_dmabuf_v1, zwp_tablet_manager_v2
//   unstable (4):           zxdg_output_manager_v1, zxdg_exporter_v2,
//                           zwp_pointer_gestures_v1,
//                           zwp_primary_selection_device_manager_v1
//   staging (5):            xdg_activation_v1, wp_fractional_scale_v1,
//                           wp_cursor_shape_manager_v1, xdg_wm_dialog_v1,
//                           xdg_toplevel_icon_manager_v1
//   Qt parity (2):          zxdg_decoration_manager_v1,
//                           zwp_text_input_manager_v3
//   GTK parity (3):         zwp_idle_inhibit_manager_v1,
//                           zwp_keyboard_shortcuts_inhibit_manager_v1,
//                           wp_single_pixel_buffer_manager_v1
//   wlr family (1):         zwlr_layer_shell_v1
//
// Render path: GLES + DrmCompositor via smithay. Output mode is the
// real DRM mode from the kernel; on the SKIP/no-DRM test bypass a
// synthesized 1280×800@60 Hz placeholder stands in. See
// ARCHITECTURE.md for the full design and roadmap.

mod broker_relay;
mod broker_session;
mod context;
mod cursor;
#[cfg(feature = "frame_audit")]
mod dbus;
mod drm;
#[cfg(feature = "frame_audit")]
mod frame_audit;
#[cfg(feature = "frame_audit")]
mod offscreen;
mod swap_gate;
mod wallpaper;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
// calloop's `Mode` and smithay's `output::Mode` collide; rename calloop's
// to keep the smithay one as `Mode` (used more often).
use calloop::{
    EventLoop, Interest, LoopHandle, Mode as CalloopMode, PostAction, RegistrationToken,
};
use std::os::fd::{AsRawFd, OwnedFd};

use halmasuit_greetd::server::{SpawnRequest, bind_socket, peer_credentials};

use crate::broker_session::{BrokerEpisode, connect_broker};
use halmasuit_introspect::{Event, Phase, ShutdownReason, emit};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState,
    Event as InputEventTrait, InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    PointerMotionEvent,
};
use smithay::backend::libinput::LibinputInputBackend;
use smithay::desktop::layer_map_for_output;
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface, WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use tracing_subscriber::EnvFilter;

/// Maximum concurrent greetd connections halmasuit will accept. The
/// SO_PEERCRED check authorises only the configured greeter uid, so
/// the cap protects against a buggy or compromised greeter looping
/// on connect(). A real greeter opens one connection at a time;
/// reaching this cap is a runaway signal.
const MAX_GREETD_CONNECTIONS: usize = 4;

/// Compositor state passed to calloop callbacks. Holds the smithay
/// per-protocol state structs and the greetd-connection map; each new
/// protocol adds its `*State` here as it lands.
#[allow(
    clippy::struct_excessive_bools,
    reason = "frame_pending / shutdown_armed / shutting_down / \
              session_first_frame_emitted are semantically distinct \
              lifecycle bits (per-tick render coalesce / shutdown-signal \
              arming / post-shutdown render gating / swap-gate key \
              signaling). Folding them into a bitflag enum would obscure \
              call-site intent without removing the per-flag cost."
)]
struct HalmasuitState {
    display_handle: DisplayHandle,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    seat_state: SeatState<Self>,
    /// The single `wl_seat` (keyboard + pointer). libinput events are
    /// routed here and forwarded to the keyboard-focused client.
    seat: Seat<Self>,
    // OutputManagerState exists to keep the xdg_output_manager global
    // alive; nothing reads the field directly (delegate_output! handles
    // dispatch via the type), hence the leading underscore.
    _output_manager_state: OutputManagerState,
    /// The smithay `Output` representing halmasuit's single display.
    /// Constructed from the real DRM mode in the production path
    /// (synthesized 1920×1080 only on the SKIP bypass). Read by the
    /// `WlrLayerShellHandler` to route new layer surfaces to the
    /// correct `LayerMap`, and by the commit handler to build render
    /// elements from those layers.
    output: Output,
    shm_state: ShmState,
    /// wlr-layer-shell global state. New layer surfaces (BACKGROUND /
    /// BOTTOM / TOP / OVERLAY) land in `new_layer_surface` and get
    /// mapped into the per-output `LayerMap` (accessed via
    /// `layer_map_for_output`). Composited in z-order during
    /// `render_with_elements` from the commit-driven render path.
    layer_shell_state: WlrLayerShellState,
    /// `zwp_linux_dmabuf_v1` state. The global is created at startup
    /// from the renderer's supported dmabuf formats; the
    /// `dmabuf_state` field stores the smithay-side bookkeeping and
    /// is referenced via `delegate_dmabuf!`. On the SKIP (no-DRM)
    /// path there is no renderer, so no global is advertised —
    /// clients fall back to wl_shm cleanly. R10 (convergence).
    dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    /// The dmabuf global handle. `None` on the SKIP path (no
    /// renderer → no formats to advertise).
    _dmabuf_global: Option<smithay::wayland::dmabuf::DmabufGlobal>,
    /// `wp_presentation` global state. Created at startup with
    /// `CLOCK_MONOTONIC`. Clients use this to receive
    /// `wp_presentation_feedback.presented` for surfaces that asked
    /// for it; halmasuit emits the events from the VBlank handler.
    /// R9 (convergence).
    _presentation_state: smithay::wayland::presentation::PresentationState,
    /// Monotonic counter for the presentation `sequence` field —
    /// increments once per VBlank. R9.
    presentation_seq: u64,
    /// Cursor image state — the latest `wl_pointer.set_cursor` from
    /// the focused client. R8b tracks this; R8b-render will
    /// composite the `Surface` variant as an overlay above the
    /// foreground tree at `PointerHandle::current_location()` minus
    /// the surface's hotspot. Today the field is set but not
    /// rendered — clients' `set_cursor` succeeds (no protocol error)
    /// but no visible cursor appears.
    cursor_status: CursorImageStatus,
    /// P1 (review-round-3): coalesce per-commit repaints.
    /// `commit()` is called once per `wl_surface.commit` — a single
    /// logical frame from a Qt/GTK client commonly fires 10+ commits
    /// (root + N subsurfaces + cursor + popups). Pre-fix the commit
    /// handler synchronously called `repaint()` each time, burning
    /// the GPU on a tree that wouldn't update visibly until the next
    /// VBlank anyway. Now the commit handler just sets this flag;
    /// the main loop drains it once between dispatch and
    /// flush_clients, doing one repaint per dispatch cycle regardless
    /// of how many commits arrived. Swap / layer-destroy /
    /// toplevel-destroy still render synchronously because those are
    /// rare visible-state transitions that benefit from immediate
    /// feedback.
    frame_pending: bool,
    /// Cached output mode (width, height in logical pixels).
    /// `Output::current_mode()` is an Arc-locked lookup; pointer +
    /// touch input handlers call it at libinput event rate (up to
    /// 1000/s for gaming mice) for clamping / absolute-coord
    /// transform. v1's output mode is fixed at startup (no hot-plug),
    /// so caching here removes the lock from every input event. The
    /// `(1280, 800)` default mirrors the SKIP/no-DRM bypass mode.
    output_size: (i32, i32),
    /// Cached VBlank refresh period derived once from the output's
    /// `current_mode()`. Recomputing `1_000_000_000_000 / refresh_mhz`
    /// every VBlank closure runs is wasteful and pointless — the mode
    /// is stable across the compositor's lifetime in v1. Used by the
    /// presentation-feedback `Refresh::fixed(...)` payload.
    refresh_period: std::time::Duration,
    /// `wp_viewporter` state (Phase B). Smithay handles all the
    /// crop+scale logic via the protocol's own state; this field
    /// just owns the global. Both Qt 6 and GTK 4 bind viewporter
    /// for HiDPI fractional-scale composition / subsurface scaling.
    _viewporter_state: smithay::wayland::viewporter::ViewporterState,
    /// `wp_fractional_scale_manager_v1` state (Phase B). HiDPI
    /// fractional-scale negotiation; smithay manages the per-surface
    /// scale advertisement.
    _fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState,
    /// `wp_single_pixel_buffer_manager_v1` state (Phase B). GTK 4
    /// uses this for solid-color backgrounds without allocating a
    /// shm/dmabuf.
    _single_pixel_buffer_state: smithay::wayland::single_pixel_buffer::SinglePixelBufferState,
    /// `zwp_pointer_gestures_v1` state (Phase B). GTK 4 touchpad
    /// gesture protocol — swipe / pinch / hold passthrough.
    _pointer_gestures_state: smithay::wayland::pointer_gestures::PointerGesturesState,
    /// `zwp_tablet_manager_v2` state (Phase B). Tablet input
    /// (stylus, eraser, tablet pad). Both toolkits expose tablet
    /// API; protocol is the wire layer.
    _tablet_manager_state: smithay::wayland::tablet_manager::TabletManagerState,
    /// `zxdg_decoration_manager_v1` state (Phase B). Qt 6 binds
    /// this to ask whether to draw its own titlebar; halmasuit
    /// hosts one fullscreen toplevel and draws no decorations, so
    /// we always answer `ServerSide` (= "no decoration is the
    /// server's contribution"; client also draws none).
    _xdg_decoration_state: smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    /// `xdg_activation_v1` state (Phase B). Qt 6 / GTK 4 use this
    /// to ask for window activation. halmasuit's foreground is
    /// driven by the greeter→session lifecycle, not by client
    /// activation requests — we accept tokens (default) and
    /// log+ignore the activation requests themselves. Field is
    /// `pub`(crate) because the handler returns a `&mut` to it.
    xdg_activation_state: smithay::wayland::xdg_activation::XdgActivationState,
    /// `zwp_idle_inhibit_manager_v1` state (Phase B). GTK 4 binds.
    /// halmasuit has no idle behavior in v1; inhibit/uninhibit are
    /// no-ops protocol-side, smithay tracks the inhibitor set.
    _idle_inhibit_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    /// `zwp_keyboard_shortcuts_inhibit_manager_v1` state (Phase B).
    /// halmasuit has no global keyboard shortcuts to intercept (all
    /// keyboard input goes straight to the focused client), so the
    /// protocol records inhibitor requests but they're effectively
    /// no-ops. `pub`(crate) because the handler returns a `&mut`.
    keyboard_shortcuts_inhibit_state:
        smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
    /// `xdg_foreign_v2` state (Phase B). KDE / GNOME settings panels
    /// and dialog-spawning toolkit code use this to export/import
    /// toplevel handles across processes. halmasuit hosts a single
    /// fullscreen toplevel per phase so cross-client embedding is
    /// inert in v1; smithay tracks handles via the handler's
    /// `xdg_foreign_state()` accessor.
    xdg_foreign_state: smithay::wayland::xdg_foreign::XdgForeignState,
    /// `xdg_wm_dialog_v1` state (Phase B). GTK 4 / Qt 6 use this to
    /// mark a toplevel as a modal/non-modal dialog. halmasuit takes
    /// no action on the hint — smithay's default no-op
    /// `dialog_hint_changed` is fine.
    _xdg_dialog_state: smithay::wayland::shell::xdg::dialog::XdgDialogState,
    /// `xdg_toplevel_icon_manager_v1` state (Phase B). Modern Qt 6 /
    /// GTK 4 set toplevel icons via this protocol. halmasuit shows
    /// no titlebars or task list in v1 so the icon is unused; smithay
    /// caches the request, default no-op `set_icon` handler is fine.
    _xdg_toplevel_icon_manager: smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager,
    /// `wl_data_device_manager` state (Phase B — first focus-bearing
    /// semantic protocol). The `wayland.xml` core selection/DnD
    /// global. halmasuit hosts one fullscreen toplevel per phase so
    /// cross-client DnD is non-existent in v1; selection (clipboard)
    /// routing between clients is handled internally by smithay. The
    /// handler returns `&mut` from `data_device_state()`, so the
    /// field is `pub`(crate)-visible without the leading underscore.
    data_device_state: smithay::wayland::selection::data_device::DataDeviceState,
    /// `zwp_primary_selection_device_manager_v1` state (Phase B —
    /// second focus-bearing protocol). X11-style middle-click primary
    /// buffer. Shares the single `SelectionHandler` impl with
    /// `data_device_state` (smithay routes both via
    /// `SelectionTarget::{Primary,Clipboard}`). Field is `pub`(crate)
    /// because `PrimarySelectionHandler::primary_selection_state`
    /// returns `&mut`.
    primary_selection_state: smithay::wayland::selection::primary_selection::PrimarySelectionState,
    /// `zwp_text_input_manager_v3` state (Phase B — third and final
    /// focus-bearing protocol). Qt-mandatory IME protocol. halmasuit
    /// hosts no input-method server (input-method-v2 is out of the
    /// 25-protocol scope), so clients that bind text-input-v3 won't
    /// get IM service — but they bind successfully, can call
    /// `enable`/`disable`, and the compositor tracks focus through
    /// `TextInputSeat::text_input().set_focus(...)`. Stored as a
    /// field (anvil discards) so the global's lifetime is explicit.
    _text_input_manager_state: smithay::wayland::text_input::TextInputManagerState,
    /// `wp_cursor_shape_manager_v1` state (Phase B — staging tier).
    /// Clients call `set_shape(serial, shape)` to request a named
    /// cursor (Default, Pointer, Text, etc.). Smithay routes the
    /// request directly through `SeatHandler::cursor_image` as
    /// `CursorImageStatus::Named(CursorIcon)`, which halmasuit's
    /// R8b-state implementation already stores. Visible cursor
    /// compositing is the orthogonal R8b-render follow-up.
    _cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState,
    /// Layer roles for which `Event::ClientFirstFrame` has already
    /// been emitted (emit-once-per-role). The visual-backdrop
    /// continuity assertion keys off the first
    /// `ClientFirstFrame { role: Background }`.
    seen_layer_roles: std::collections::HashSet<halmasuit_introspect::LayerRole>,
    /// The single fullscreen xdg_toplevel composited above the splash
    /// (greeter/session). v1 is one output, no window management — at
    /// most one toplevel is the foreground.
    foreground_toplevel: Option<ToplevelSurface>,
    /// Smithay popup tracker. `track_popup` on `new_popup`,
    /// `commit` per surface commit, `cleanup` once per VBlank.
    /// Popup grabs (`grab` handler) still log-and-ignore until R8
    /// wires `wl_seat` for pointer routing — at that point the grab
    /// will go through `PopupManager::grab_popup` and a `PopupGrab`
    /// installed on the seat's pointer.
    popups: PopupManager,
    /// Which client is the foreground, driven by the greetd lifecycle
    /// (req 17): `Greeter` until `start_session` succeeds, then
    /// `Session`. Gates keyboard focus (greeter layer vs session
    /// toplevel) so focus follows the lifecycle, not connection order.
    foreground: halmasuit_introspect::Foreground,
    /// Registration token for the libinput calloop source. halmasuit
    /// opens `/dev/input/event*` directly (no libseat brokerage; R2.3)
    /// while still root via `setup_libinput_direct`; libinput holds
    /// the OwnedFds across the subsequent privilege drop. `None` on
    /// the SKIP (no-DRM/dev) path and on the initramfs-pre-pivot path
    /// (the post-pivot `setup_post_pivot_input` populates it once
    /// `/dev/input/event*` is available on the rootfs). The token is
    /// removed by `graceful_shutdown` so the calloop loop stops
    /// dispatching against the input subsystem during shutdown.
    libinput_token: Option<RegistrationToken>,

    // ── greetd I/O ──────────────────────────────────────────────────────
    /// LoopHandle for inserting per-connection sources from inside
    /// callbacks (specifically the listener's accept handler).
    loop_handle: LoopHandle<'static, Self>,
    /// Per-connection state, keyed by an opaque monotonic id captured
    /// in each connection's calloop callback closure.
    connections: HashMap<usize, ConnState>,
    /// Monotonic counter for fresh connection ids.
    next_conn_id: usize,
    /// PAM service (`/etc/pam.d/<service>`) — the `pam_start` hint sent
    /// to the broker in `BeginAuth`. Authoritative identity is always
    /// the broker's PAM-resolved `Success` (Epic R8), never this.
    pam_service: String,
    /// Path of the privileged `halmasuit-session` broker socket
    /// (`SOCK_SEQPACKET`). Each greeter episode `connect`s a fresh
    /// channel here and owns it for the whole episode (Amendment A6).
    broker_socket: PathBuf,
    /// Authorised greeter UID; connections from any other uid are
    /// dropped by `handle_listener_ready`.
    greeter_uid: u32,
    /// Epic #47 R2.1: gates the SIGTERM handler's response.
    ///
    /// In the fromInitrd deployment, SIGTERM is sent twice in a
    /// single boot: once during the boot pivot's
    /// `systemd-shutdown --switch-root` kill spree (which we ignore
    /// — survival is the whole point of the fromInitrd shape), and
    /// again during the rootfs→shutdownRamfs pivot at actual system
    /// shutdown (which SHOULD drive graceful tear-down).
    /// Distinguishing them needs state: this flag flips true when
    /// `Phase::RootfsReady` is emitted, which only happens AFTER the
    /// boot pivot is complete.
    /// A SIGTERM with this flag false in fromInitrd mode is the boot
    /// pivot's kill spree (ignored, deferred to the
    /// SurviveFinalKillSignal directive and the broker root-fd
    /// handoff); with this flag true, it is the real shutdown signal.
    ///
    /// In rootfs-only mode (`services.halmasuit.enable=true` with no
    /// fromInitrd), the flag flips true at startup completion — there's
    /// no boot pivot to ignore.
    shutdown_armed: bool,
    /// Epic #47 R2.2: set by `graceful_shutdown` after the wallpaper-
    /// only recomposite. The render path observes this to keep the
    /// wallpaper plane composited (no greeter, no session toplevel)
    /// from now until the kernel halts the process. halmasuit's main
    /// loop has no clean-exit termination — it keeps painting through
    /// the rootfs→shutdownRamfs pivot via SurviveFinalKillSignal +
    /// the shutdownRamfs storePaths wiring (probe-validated by
    /// halmasuit-shutdown-probe-phase{0,1,2}).
    shutting_down: bool,
    /// Held so the registered listener token survives for the lifetime
    /// of the compositor; calloop drops the source on shutdown.
    greetd_listener_token: Option<RegistrationToken>,

    /// The GLES + GBM + DrmCompositor stack. Constructed in `main()`
    /// while still root via [`drm::setup_drm_direct`]; the underlying
    /// DRM fd's master designation survives the subsequent `setresuid`
    /// to the compositor user (drm-master-probe Phase 1).
    ///
    /// Mutated from the calloop callback for `DrmEvent::VBlank` —
    /// each vblank acks the previous frame and (currently) re-renders
    /// the same brand clear color. Subsequent epic subtasks (B.3+)
    /// will populate the element list with wl_clients.
    ///
    /// `None` only when `HALMASUIT_SKIP_DRM_MASTER` was set (dev/test
    /// bypass); production deployments never see this.
    drm_backend: Option<drm::DrmBackend>,

    /// calloop registration of the DRM event source. Holding this
    /// token keeps the page-flip event stream wired into the event
    /// loop; dropping it would silently unregister vblank handling.
    _drm_token: Option<RegistrationToken>,

    /// Greeter we spawned at startup, held as a pid + pidfd pair.
    /// The pidfd is the kernel-anchored signal target (race-free
    /// across pid recycling — `pidfd_send_signal(2)`); the pid is
    /// retained for the introspection event field. SIGKILLed by the
    /// Amendment-A5 swap gate (`apply_swap_action`) when BOTH keys are
    /// in (`SessionOpened` + the session client's first non-empty
    /// frame) — the greeter stays visible underneath until then, then
    /// releases halmasuit's foreground slot as the session takes it
    /// (no flash). The SIGCHLD reaper picks up the zombie. `None` when
    /// `HALMASUIT_GREETER_COMMAND` was unset OR the slot was already
    /// consumed by the swap kill.
    greeter: Option<GreeterHandle>,

    /// uid of the authenticated user session, recorded by
    /// `record_session_started` once the broker has accepted the
    /// relayed `StartSession` (the broker-launched session). The
    /// Wayland accept path authorises a connecting peer when its
    /// SO_PEERCRED uid is `greeter_uid` (pre-auth) or this value
    /// (post-auth); the session connects under its own uid, distinct
    /// from the greeter's. `None` until a session spawn succeeds.
    session_uid: Option<u32>,

    /// Amendment A5 two-key flash-free swap gate. The VISIBLE
    /// greeter→session swap (SIGKILL greeter + `foreground = Session`)
    /// fires only on AND(`SessionOpened`, the session client's first
    /// non-empty frame). `session_uid` above is set EARLIER (on
    /// `spawned`, so the session client can authorise+connect+paint);
    /// this gate governs only WHEN that becomes visible, and the
    /// revert (back to splash) on `SessionEnded`/disconnect.
    swap: swap_gate::SwapGate,
    /// Emit-once latch for `Event::SessionClientFirstFrame` (key 2):
    /// the session client commits many frames; the introspect marker
    /// and the gate input fire on the first non-empty one only.
    session_first_frame_emitted: bool,
    /// Compositor-monotonic baseline for `wl_callback.done(uint time)`.
    /// `start_time.elapsed()` is `CLOCK_MONOTONIC`-by-construction, so
    /// the timestamps we hand to `wl_surface.frame` callbacks satisfy
    /// the spec's monotonic-non-decreasing requirement (Wayland
    /// Appendix A). Truncated to u32 ms at the call site (~49.7-day
    /// wrap, per the protocol wire format).
    start_time: std::time::Instant,
}

/// Greeter identity post-spawn. The pidfd is the load-bearing
/// authoritative reference to the greeter process — sending
/// signals through it is immune to the pid-reuse race that
/// raw `kill(pid, …)` exhibits after SIGCHLD reaps the entry.
struct GreeterHandle {
    pid: u32,
    pidfd: std::os::fd::OwnedFd,
}

/// Per-client metadata. smithay's `CompositorHandler` requires us to
/// store a `CompositorClientState` per client; we tuck it inside our
/// `ClientData` impl so it's automatically cleaned up on disconnect.
struct ClientState {
    compositor_state: CompositorClientState,
    /// SO_PEERCRED uid of the connecting peer, captured at accept.
    /// Lets the commit handler tell the session client (uid ==
    /// `session_uid`) from the greeter (uid == `greeter_uid`) so the
    /// Amendment-A5 key 2 (session client's first non-empty frame)
    /// fires for the right client — connection identity is NOT
    /// authority (R8), but it IS how the compositor knows whose pixels
    /// just arrived.
    uid: u32,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl HalmasuitState {
    /// Route one libinput event to the seat. Keyboard keys go to the
    /// keyboard-focused client; pointer events (motion, button, axis)
    /// go to the keyboard-focused client as well — halmasuit hosts
    /// one fullscreen surface at a time, so the surface under the
    /// pointer trivially equals the keyboard focus. Per-protocol
    /// `frame()` batching is emitted after each event group.
    fn dispatch_libinput(&mut self, event: InputEvent<LibinputInputBackend>) {
        match event {
            InputEvent::Keyboard { event } => {
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let time = event.time_msec();
                let code = event.key_code();
                let key_state = event.state();
                keyboard.input::<(), _>(self, code, key_state, serial, time, |_, _, _| {
                    FilterResult::Forward
                });
            }
            InputEvent::PointerMotion { event } => self.on_pointer_relative_motion(&event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.on_pointer_absolute_motion(&event);
            }
            InputEvent::PointerButton { event } => self.on_pointer_button(&event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis(&event),
            InputEvent::TouchDown { event } => self.on_touch_down(&event),
            InputEvent::TouchUp { event } => self.on_touch_up(&event),
            InputEvent::TouchMotion { event } => self.on_touch_motion(&event),
            InputEvent::TouchFrame { event: _ } => self.on_touch_frame(),
            InputEvent::TouchCancel { event: _ } => self.on_touch_cancel(),
            _ => {
                // Tablet, switch — not in v1 scope. Future epics.
            }
        }
    }

    /// Output-local pointer focus + coordinates for the foreground
    /// surface. Returns `None` if no input-accepting surface exists.
    /// halmasuit hosts one fullscreen surface, so the location is
    /// always the output origin (0, 0).
    fn pointer_focus(
        &self,
    ) -> Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )> {
        let keyboard = self.seat.get_keyboard()?;
        keyboard
            .current_focus()
            .map(|s| (s, smithay::utils::Point::from((0.0_f64, 0.0_f64))))
    }

    /// Clamp absolute pointer coords to the output extents.
    fn clamp_pointer(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> smithay::utils::Point<f64, smithay::utils::Logical> {
        let (w_i, h_i) = self.output_size;
        let (w, h) = (f64::from(w_i), f64::from(h_i));
        smithay::utils::Point::from((pos.x.clamp(0.0, w), pos.y.clamp(0.0, h)))
    }

    fn on_pointer_relative_motion<E: PointerMotionEvent<LibinputInputBackend>>(&mut self, evt: &E) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let delta = evt.delta();
        let mut location = pointer.current_location() + delta;
        location = self.clamp_pointer(location);
        let focus = self.pointer_focus();
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location,
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
        // R8b-render: feed the cursor render position.
        if let Some(backend) = self.drm_backend.as_mut() {
            backend.set_pointer_location(location);
        }
    }

    fn on_pointer_absolute_motion<E: AbsolutePositionEvent<LibinputInputBackend>>(
        &mut self,
        evt: &E,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let (w, h) = self.output_size;
        let location = evt.position_transformed((w, h).into());
        let focus = self.pointer_focus();
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
        // R8b-render: feed the cursor render position.
        if let Some(backend) = self.drm_backend.as_mut() {
            backend.set_pointer_location(location);
        }
    }

    fn on_pointer_button<E: PointerButtonEvent<LibinputInputBackend>>(&mut self, evt: &E) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let button = evt.button_code();
        let state: ButtonState = evt.state();
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn on_pointer_axis<E: PointerAxisEvent<LibinputInputBackend>>(&mut self, evt: &E) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let h = evt
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| evt.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
        let v = evt
            .amount(Axis::Vertical)
            .unwrap_or_else(|| evt.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);
        let mut frame = AxisFrame::new(evt.time_msec()).source(evt.source());
        if h != 0.0 {
            frame = frame
                .relative_direction(Axis::Horizontal, evt.relative_direction(Axis::Horizontal))
                .value(Axis::Horizontal, h);
            if let Some(d) = evt.amount_v120(Axis::Horizontal) {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "wl_pointer.axis_v120 is i32 per protocol; smithay's libinput \
                              backend returns f64 but the source is an integer scancode \
                              from the kernel evdev REL_WHEEL_HI_RES axis, always small \
                              and well within i32"
                )]
                let discrete = d as i32;
                frame = frame.v120(Axis::Horizontal, discrete);
            }
        }
        if v != 0.0 {
            frame = frame
                .relative_direction(Axis::Vertical, evt.relative_direction(Axis::Vertical))
                .value(Axis::Vertical, v);
            if let Some(d) = evt.amount_v120(Axis::Vertical) {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "wl_pointer.axis_v120 is i32 per protocol; smithay's libinput \
                              backend returns f64 but the source is an integer scancode \
                              from the kernel evdev REL_WHEEL_HI_RES axis, always small \
                              and well within i32"
                )]
                let discrete = d as i32;
                frame = frame.v120(Axis::Vertical, discrete);
            }
        }
        if evt.source() == AxisSource::Finger {
            if evt.amount(Axis::Horizontal) == Some(0.0) {
                frame = frame.stop(Axis::Horizontal);
            }
            if evt.amount(Axis::Vertical) == Some(0.0) {
                frame = frame.stop(Axis::Vertical);
            }
        }
        // Suppress unused-import warning when no axis source is Finger:
        let _ = AxisRelativeDirection::Identical;
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    // Touch handlers. halmasuit hosts one fullscreen toplevel/layer
    // surface per phase, so the touch target trivially equals the
    // keyboard focus (same as the pointer model). Touch absolute
    // coords come from the libinput event transformed against the
    // current output extents.

    fn on_touch_down<E: smithay::backend::input::TouchDownEvent<LibinputInputBackend>>(
        &mut self,
        evt: &E,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let (w, h) = self.output_size;
        let location = evt.position_transformed((w, h).into());
        let focus = self.pointer_focus();
        touch.down(
            self,
            focus,
            &smithay::input::touch::DownEvent {
                slot: evt.slot(),
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: evt.time_msec(),
            },
        );
    }

    fn on_touch_up<E: smithay::backend::input::TouchUpEvent<LibinputInputBackend>>(
        &mut self,
        evt: &E,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        touch.up(
            self,
            &smithay::input::touch::UpEvent {
                slot: evt.slot(),
                serial: SERIAL_COUNTER.next_serial(),
                time: evt.time_msec(),
            },
        );
    }

    fn on_touch_motion<E: smithay::backend::input::TouchMotionEvent<LibinputInputBackend>>(
        &mut self,
        evt: &E,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let (w, h) = self.output_size;
        let location = evt.position_transformed((w, h).into());
        let focus = self.pointer_focus();
        touch.motion(
            self,
            focus,
            &smithay::input::touch::MotionEvent {
                slot: evt.slot(),
                location,
                time: evt.time_msec(),
            },
        );
    }

    fn on_touch_frame(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    fn on_touch_cancel(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
    }

    /// Point keyboard focus at `surface` (or clear it). No-op if it is
    /// already the focus, to avoid enter/leave churn.
    fn set_keyboard_focus(&mut self, surface: Option<WlSurface>) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if keyboard.current_focus() == surface {
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, surface, serial);
    }

    /// Apply the Amendment-A5 two-key swap-gate decision to the
    /// visible scene. `Swap` (both keys in): SIGKILL the greeter (its
    /// wl_client must release halmasuit's slot before the session
    /// takes it — req 17), flip `foreground` to `Session`, focus the
    /// session toplevel, re-composite. `Revert` (`SessionEnded` or
    /// session-client disconnect, post-swap): foreground back to
    /// `Greeter`, clear focus, re-composite — on logout the splash is
    /// already running underneath and just becomes visible again
    /// (ARCHITECTURE.md), so this is flash-free in the same way the
    /// forward swap is. `None`: nothing changed on screen.
    fn apply_swap_action(&mut self, action: swap_gate::SwapAction) {
        match action {
            swap_gate::SwapAction::None => return,
            swap_gate::SwapAction::Swap => {
                if let Some(g) = self.greeter.take() {
                    let pid = g.pid;
                    match pidfd_send_signal(&g.pidfd, libc::SIGKILL) {
                        Ok(()) => emit(&Event::GreeterTerminated { pid }),
                        Err(e) => {
                            let error = format!("{e}");
                            tracing::warn!(
                                %error,
                                greeter_pid = pid,
                                "pidfd_send_signal greeter SIGKILL failed; session proceeds"
                            );
                            emit(&Event::GreeterKillFailed { pid, error });
                        }
                    }
                }
                self.foreground = halmasuit_introspect::Foreground::Session;
                emit(&Event::ForegroundChanged {
                    to: halmasuit_introspect::Foreground::Session,
                });
                if let Some(t) = self.foreground_toplevel.as_ref() {
                    let s = t.wl_surface().clone();
                    self.set_keyboard_focus(Some(s));
                }
            }
            swap_gate::SwapAction::Revert => {
                self.foreground = halmasuit_introspect::Foreground::Greeter;
                emit(&Event::ForegroundChanged {
                    to: halmasuit_introspect::Foreground::Greeter,
                });
                self.set_keyboard_focus(None);
                // Epic #47 R1: the previous session's uid lingered in
                // `session_uid` and the swap_gate had transitioned to a
                // terminal Reverted state — both blocked the next login
                // from succeeding. Clear and re-arm.
                self.session_uid = None;
                self.swap = swap_gate::SwapGate::new();
                // Respawn the greeter so the user can log back in. The
                // SIGKILL at swap time left `state.greeter = None`;
                // ask the broker (uid 998 compositor cannot setuid
                // to greeter uid; broker is the policy authority).
                // Degraded state on failure: log + continue, wallpaper
                // stays visible, no greeter — operator can systemctl
                // restart halmasuit to recover.
                match broker_spawn_greeter() {
                    Ok(handle) => {
                        tracing::info!(
                            greeter_pid = handle.pid,
                            "greeter respawned via broker after session end"
                        );
                        emit(&Event::GreeterSpawned { pid: handle.pid });
                        self.greeter = Some(handle);
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "post-logout broker_spawn_greeter failed; halmasuit running \
                             without a greeter until operator intervenes"
                        );
                    }
                }
            }
        }
        // Re-composite now so the swap/revert is visible this frame and
        // never via a stale/black intermediate (the no-flash invariant
        // this project exists for). On revert we composite with no
        // foreground toplevel — the persistent splash/backdrop layers
        // are already running underneath.
        let fg_surface = match action {
            swap_gate::SwapAction::Swap => self
                .foreground_toplevel
                .as_ref()
                .map(|t| t.wl_surface().clone()),
            _ => None,
        };
        if let Some(backend) = self.drm_backend.as_mut()
            && let Err(e) = backend.render_layer_elements(
                &self.output,
                fg_surface.as_ref(),
                HALMASUIT_BRAND_CLEAR,
            )
        {
            tracing::warn!(error = %e, "render_layer_elements on swap/revert failed");
        }
    }
}

impl CompositorHandler for HalmasuitState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client missing ClientState — every client is inserted with one")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Advance smithay's per-surface buffer tracking state — must
        // be called on the ORIGINAL committed surface so smithay's
        // per-surface cache sees the right key. Without it,
        // `render_elements_from_surface_tree` sees no current buffer
        // and the surface paints nothing.
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // R3 (convergence epic): a synchronized subsurface's commit
        // is CACHED at the parent and applied only when the parent
        // commits (wl_subsurface contract). Skip downstream work —
        // smithay holds the state until the parent's commit
        // propagates it (smallvil pattern).
        if smithay::wayland::compositor::is_sync_subsurface(surface) {
            return;
        }
        // A commit on a desync subsurface implicitly commits the root
        // tree, so all remaining work acts on the root.
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        let surface = &root;

        self.send_deferred_xdg_initial_configure(surface);
        self.handle_layer_shell_commit(surface);
        self.maybe_emit_session_first_frame(surface);
        self.maybe_focus_foreground_toplevel(surface);
        // P1: coalesce per-commit repaints. The main loop drains
        // `frame_pending` once per dispatch cycle, doing one repaint
        // per N commits instead of N repaints. See the field doc.
        self.frame_pending = true;
    }
}

impl HalmasuitState {
    /// R4 (convergence epic) + R5 (PopupManager): send the deferred
    /// initial xdg-shell configure on the client's first commit, per
    /// xdg-shell.xml `xdg_surface`: "The client must call
    /// wl_surface.commit ... before it will receive the initial
    /// configure event." Smithay canonical pattern (smallvil
    /// `handlers/xdg_shell.rs:152-189`):
    ///   1. Advance the PopupManager state machine for popup
    ///      commits (positioner caching, unmapped→mapped move).
    ///   2. For toplevels: check `is_initial_configure_sent()` on
    ///      the `ToplevelSurface` inventory and send once.
    ///   3. For popups: `find_popup(surface)` returns the
    ///      `PopupKind`; check `is_initial_configure_sent()` and
    ///      send once. Initial popup configure is always allowed
    ///      (smithay returns `NotReactive` only on RE-configure of
    ///      a non-reactive popup).
    fn send_deferred_xdg_initial_configure(&mut self, surface: &WlSurface) {
        self.popups.commit(surface);

        // P3: O(1) toplevel-role + initial-configure-sent check via
        // the surface's data map, before the O(N)
        // toplevel_surfaces().iter().find — which is the only place
        // we can recover the `ToplevelSurface` to call
        // `send_configure()`. The common case (post-initial-configure
        // commits) short-circuits at the data-map check; the rare
        // case (first commit of a toplevel) does one linear scan
        // (over the in-scope toplevel count, currently 1 in v1).
        let needs_initial_configure =
            smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .is_some_and(|d| !d.lock().unwrap().initial_configure_sent)
            });
        if needs_initial_configure
            && let Some(toplevel) = self
                .xdg_shell_state
                .toplevel_surfaces()
                .iter()
                .find(|t| t.wl_surface() == surface)
                .cloned()
        {
            toplevel.send_configure();
        }
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            let _ = popup.send_configure();
        }
    }

    /// wlr-layer-shell commit work: send the deferred initial
    /// configure (the spec requires it in response to the client's
    /// first commit, which carries anchor/exclusive-zone/size — so
    /// `arrange` sees the committed anchor state), arrange the layer
    /// map, then emit `ClientFirstFrame` and apply
    /// focus-follows-foreground on the first buffered commit per
    /// role.
    fn handle_layer_shell_commit(&mut self, surface: &WlSurface) {
        let mut map = layer_map_for_output(&self.output);
        let Some(layer) = map
            .layer_for_surface(surface, smithay::desktop::WindowSurfaceType::TOPLEVEL)
            .cloned()
        else {
            return;
        };
        let initial_configure_sent = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceData>()
                .is_some_and(|d| d.lock().unwrap().initial_configure_sent)
        });
        map.arrange();
        // Release the LayerMap lock before `send_configure` and any
        // downstream render path — `render_layer_elements` calls
        // `layer_map_for_output` again on the same output and would
        // re-borrow this same map.
        drop(map);
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        }

        let has_buffer =
            smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                s.buffer().is_some()
            })
            .unwrap_or(false);
        if !has_buffer {
            return;
        }

        // Emit `ClientFirstFrame { role }` once per layer role, the
        // first time a surface of that role has a committed buffer
        // halmasuit will composite. Drives the visual-backdrop
        // continuity assertion (Epic #1 req 11). Cheap
        // state-transition marker (not frame_audit-gated).
        let role = match layer.layer() {
            Layer::Background => halmasuit_introspect::LayerRole::Background,
            Layer::Bottom => halmasuit_introspect::LayerRole::Bottom,
            Layer::Top => halmasuit_introspect::LayerRole::Top,
            Layer::Overlay => halmasuit_introspect::LayerRole::Overlay,
        };
        if self.seen_layer_roles.insert(role) {
            emit(&Event::ClientFirstFrame { role });
        }
        // Focus-follows-foreground (req 17): a keyboard-interactive
        // layer client (the greeter) gets keyboard focus only while
        // the foreground is `Greeter`. After `start_session` the
        // foreground is `Session`, so a lingering/teardown greeter
        // layer never steals focus from the session.
        if self.foreground == halmasuit_introspect::Foreground::Greeter
            && layer.cached_state().keyboard_interactivity
                != smithay::wayland::shell::wlr_layer::KeyboardInteractivity::None
        {
            self.set_keyboard_focus(Some(surface.clone()));
        }
    }

    /// Amendment A5 key 2: the session Wayland client's first
    /// committed buffer of non-zero size. The client is identified
    /// by its SO_PEERCRED uid == `session_uid` — connection identity
    /// is NOT authority (that is PAM-derived, R8), but it IS how the
    /// compositor knows WHOSE pixels just arrived. Emit-once; feeds
    /// the two-key `SwapGate` (key 1 is the broker's
    /// `SessionOpened`). Swapping before this point would show the
    /// session "window" before it has painted — the exact flash this
    /// project deletes.
    fn maybe_emit_session_first_frame(&mut self, surface: &WlSurface) {
        if self.session_first_frame_emitted {
            return;
        }
        let Some(suid) = self.session_uid else {
            return;
        };
        if surface_client_uid(surface) != Some(suid) {
            return;
        }
        let non_empty =
            smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                s.buffer_size().is_some_and(|sz| sz.w > 0 && sz.h > 0)
            })
            .unwrap_or(false);
        if !non_empty {
            return;
        }
        self.session_first_frame_emitted = true;
        emit(&Event::SessionClientFirstFrame);
        let a = self.swap.session_first_frame();
        self.apply_swap_action(a);
    }

    /// Focus-follows-foreground (req 17 / R13b): the foreground
    /// xdg_toplevel gets keyboard focus on its first buffered commit.
    /// The function runs every commit but is bound to the
    /// foreground_toplevel identity check + has-buffer guard, so it
    /// becomes effective exactly once per toplevel mapping. Applies
    /// to BOTH the greeter (greeter-niri's toplevel) and the session
    /// (the broker-execed session leader's toplevel) — keystrokes
    /// flow into whatever client owns the foreground surface, which
    /// is the chain visual-dankgreeter exercises end-to-end.
    fn maybe_focus_foreground_toplevel(&mut self, surface: &WlSurface) {
        let fg = self
            .foreground_toplevel
            .as_ref()
            .map(|t| t.wl_surface().clone());
        if fg.as_ref() != Some(surface) {
            return;
        }
        let has_buffer =
            smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                s.buffer().is_some()
            })
            .unwrap_or(false);
        if has_buffer {
            // Emit a transition marker ONCE per focus change. The
            // current_focus check avoids spamming the log on every
            // VBlank-driven commit. VM tests poll this marker to
            // synchronize on "halmasuit has delivered wl_keyboard.
            // enter to the greeter" instead of sleeping a wall-clock
            // window.
            let already_focused = self
                .seat
                .get_keyboard()
                .and_then(|kb| kb.current_focus())
                .as_ref()
                == Some(surface);
            if !already_focused {
                tracing::info!("FOREGROUND_TOPLEVEL_KEYBOARD_FOCUSED");
            }
            self.set_keyboard_focus(Some(surface.clone()));
        }
    }

    /// Re-render the scene with the now-current buffers (synchronous
    /// one-frame-per-commit; fine for these low-frequency scenes).
    /// The foreground toplevel is composited above the layer
    /// background by `render_layer_elements`.
    ///
    /// Keeps rendering when `shutting_down` is true so video and
    /// shader wallpapers continue animating through shutdown. After
    /// graceful_shutdown clears `foreground_toplevel`, the composite
    /// is wallpaper-only — exactly the frame stream the user sees
    /// from PrepareForShutdown to kernel halt.
    fn repaint(&mut self) {
        let fg_surface = if self.shutting_down {
            None
        } else {
            self.foreground_toplevel
                .as_ref()
                .map(|t| t.wl_surface().clone())
        };
        if let Some(backend) = self.drm_backend.as_mut()
            && let Err(e) = backend.render_layer_elements(
                &self.output,
                fg_surface.as_ref(),
                HALMASUIT_BRAND_CLEAR,
            )
        {
            tracing::warn!(error = %e, "render_layer_elements on commit failed");
        }
    }
}

impl WlrLayerShellHandler for HalmasuitState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        tracing::info!(
            namespace = %namespace,
            layer = ?layer,
            "new layer surface"
        );
        // smithay distinguishes the wire-type `wlr_layer::LayerSurface`
        // (raw protocol object) from `desktop::LayerSurface` (the
        // scene-graph helper that `LayerMap` operates on). The
        // desktop wrapper owns the namespace string. Map onto our
        // single output's LayerMap; multi-output routing comes later
        // and would dispatch on the `_output` arg.
        //
        // Do NOT send a configure here. The wlr-layer-shell spec
        // mandates the initial configure be sent in response to the
        // client's *initial commit* — that commit is what carries the
        // client's anchor/size requests. Configuring now (before the
        // client has committed `set_anchor`) makes `LayerMap::arrange`
        // see an unanchored, zero-size surface and fall back to
        // half-output dimensions. The initial configure is sent from
        // the `commit` handler instead (see `ensure_layer_configured`).
        let desktop_surface = smithay::desktop::LayerSurface::new(surface, namespace);
        let mut map = layer_map_for_output(&self.output);
        if let Err(e) = map.map_layer(&desktop_surface) {
            tracing::warn!(error = ?e, "failed to map layer surface");
        }
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        let mut map = layer_map_for_output(&self.output);
        // Find the desktop-wrapped layer with this underlying wire
        // surface and unmap it. LayerMap doesn't expose a lookup-by-
        // wire-surface helper, so we walk the layers and match by
        // `layer_surface()` equality.
        let to_remove: Option<smithay::desktop::LayerSurface> = map
            .layers()
            .find(|l| l.layer_surface() == &surface)
            .cloned();
        if let Some(layer) = to_remove {
            map.unmap_layer(&layer);
        }
        // A client going away changes the scene: the layer beneath
        // (e.g. the splash background) must be re-composited now, not
        // only on the next surviving-client commit — otherwise the
        // last frame (the departed client) stays on screen. This is
        // also the no-flash requirement for the real greeter→session
        // teardown (Epic #1 req 11/17). Drop the LayerMap lock first;
        // render_layer_elements re-locks it for this output.
        drop(map);
        let fg = self
            .foreground_toplevel
            .as_ref()
            .map(|t| t.wl_surface().clone());
        if let Some(backend) = self.drm_backend.as_mut()
            && let Err(e) =
                backend.render_layer_elements(&self.output, fg.as_ref(), HALMASUIT_BRAND_CLEAR)
        {
            tracing::warn!(error = %e, "render_layer_elements on layer_destroyed failed");
        }
    }
}
smithay::delegate_layer_shell!(HalmasuitState);

impl XdgShellHandler for HalmasuitState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // halmasuit hosts exactly one fullscreen toplevel above the
        // splash (the greeter, then the session — F2 makes the swap
        // greetd-driven). Stage the pending state (output mode,
        // fullscreen + activated) — the initial `xdg_surface.configure`
        // is sent from the commit handler on the client's first
        // wl_surface.commit, per xdg-shell spec (smithay smallvil
        // pattern; see commit handler R4 block).
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
        let (w, h): (i32, i32) = self
            .output
            .current_mode()
            .map_or((1280, 800), |m| (m.size.w, m.size.h));
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
            state.states.set(xdg_toplevel::State::Activated);
            state.states.set(xdg_toplevel::State::Fullscreen);
        });
        // R6 (convergence epic): emit `wl_surface.enter` for the
        // toplevel so the client picks the correct buffer scale,
        // transform, and frame timing for this output. Layer-shell
        // surfaces already receive this via `LayerMap::arrange`;
        // xdg-toplevels were missing it. Sub-tree walks (subsurfaces
        // under the toplevel) are out of scope until a client needs
        // them (the root surface is sufficient for HiDPI / scale
        // negotiation in Qt 6 / GTK 4).
        self.output.enter(surface.wl_surface());
        tracing::info!(w, h, "xdg_toplevel mapped as fullscreen foreground");
        self.foreground_toplevel = Some(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // R6 (convergence epic): mirror of the `enter` in
        // `new_toplevel` — pair the wl_surface.enter sent at mapping
        // with a wl_surface.leave on destruction. Smithay's
        // `Output::leave` is idempotent (no-op if not in the set), so
        // this is safe even when the toplevel never reached mapping.
        self.output.leave(surface.wl_surface());

        if self.foreground_toplevel.as_ref() == Some(&surface) {
            // The destroyed toplevel was the foreground. If it
            // belongs to the SESSION client (matched via SO_PEERCRED
            // uid == session_uid), it's the A5.5 revert trigger —
            // the session Wayland client disconnected, and the
            // authoritative signal (broker's `SessionEnded`) will
            // arrive too (whichever first reverts, the gate makes
            // the other inert). If the surface belongs to the
            // greeter (different uid), this is just the greeter
            // exiting at the swap point — already-paired with
            // session_first_frame elsewhere; do NOT call
            // session_client_gone (which would inappropriately
            // disarm the swap gate while session_opened or
            // session_first_frame is still pending).
            let surface_uid = surface_client_uid(surface.wl_surface());
            let was_session = match (self.session_uid, surface_uid) {
                (Some(sid), Some(suid)) => sid == suid,
                _ => false,
            };
            self.foreground_toplevel = None;
            // The foreground is gone — clear keyboard focus and
            // re-composite so the layers beneath (splash) reappear
            // immediately (no stale/black frame; req 11/17).
            self.set_keyboard_focus(None);
            if let Some(backend) = self.drm_backend.as_mut()
                && let Err(e) =
                    backend.render_layer_elements(&self.output, None, HALMASUIT_BRAND_CLEAR)
            {
                tracing::warn!(error = %e, "render on toplevel_destroyed failed");
            }
            if was_session {
                let a = self.swap.session_client_gone();
                self.apply_swap_action(a);
            }
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // R5 (convergence epic): stage the positioner-derived geometry
        // and register the popup with `PopupManager`. The popup's
        // initial `xdg_popup.configure` is sent from the commit
        // handler on the client's first wl_surface.commit, gated on
        // `is_initial_configure_sent()` (xdg-shell deferred-configure
        // contract, R4). Unconstrain-against-the-output is intentionally
        // omitted: halmasuit hosts one fullscreen toplevel whose
        // geometry equals the output, so the positioner's own geometry
        // is already screen-relative-correct; full unconstrain logic
        // (smallvil `unconstrain_popup`) needs the
        // window-geometry-on-output knowledge a `Space` would provide,
        // which v1 doesn't have.
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        match self.popups.track_popup(PopupKind::Xdg(surface)) {
            Ok(()) => {
                // R5 observable: visual-popup.nix grep's for this exact
                // string to assert PopupManager has accepted the popup
                // into its tracking tree. Without `track_popup`, smithay's
                // `PopupSurface::send_configure` still answers from the
                // staged positioner state — so a "configure geometry was
                // non-zero" assertion would pass even with track_popup
                // ripped out. The marker here is the load-bearing R5
                // signal: it is emitted ONLY on successful track_popup,
                // and find_popup / popups.cleanup / popup-grab routing
                // all depend on the surface being in this tree.
                tracing::info!("POPUP_TRACKED: PopupManager::track_popup ok");
            }
            Err(e) => tracing::warn!(error = %e, "PopupManager::track_popup failed"),
        }
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
        // Pointer grabs require a wl_seat (not yet exposed) — log and ignore.
        tracing::debug!("popup grab requested before wl_seat is advertised; ignoring");
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        // Apply the new positioner state (geometry + positioner) and
        // emit a `repositioned` event with the client's token, per
        // xdg-shell. Smallvil pattern: `with_pending_state` + the
        // matching `send_repositioned`.
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}

impl SeatHandler for HalmasuitState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
        // R7 (convergence epic) — COMPLETE. focus_changed updates
        // every focus subsystem atomically with keyboard focus:
        //   - data-device (selection/clipboard owner)
        //   - primary-selection (X11-style middle-click buffer)
        //   - text-input-v3 (IME focus; clients see enter/leave)
        // The data-device and primary-selection setters take a
        // kernel-validated Client (resolved from the focused
        // WlSurface). text-input takes the surface directly via the
        // seat's lazily-initialized TextInputHandle.
        use smithay::wayland::text_input::TextInputSeat;
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        smithay::wayland::selection::data_device::set_data_device_focus(
            &self.display_handle,
            seat,
            client.clone(),
        );
        smithay::wayland::selection::primary_selection::set_primary_focus(
            &self.display_handle,
            seat,
            client,
        );
        seat.text_input().set_focus(focused.cloned());
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // R8b state-tracking (kept) + R8b-render forward (new): mirror
        // the latest CursorImageStatus into the DRM backend's cursor
        // state. The renderer rebakes the named pixmap from the
        // xcursor theme on the next scene_elements call when the icon
        // name changes; Hidden / Surface variants short-circuit
        // there.
        self.cursor_status = image.clone();
        if let Some(backend) = self.drm_backend.as_mut() {
            backend.set_cursor_status(image);
        }
    }
}

impl OutputHandler for HalmasuitState {}

impl ShmHandler for HalmasuitState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl smithay::wayland::dmabuf::DmabufHandler for HalmasuitState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        // Try importing the dmabuf into the GLES renderer. On
        // success the client may now attach this dmabuf as a buffer
        // to any wl_surface; on failure the client gets a buffer
        // error and may fall back to wl_shm.
        use smithay::backend::renderer::ImportDma;
        let imported = self
            .drm_backend
            .as_mut()
            .is_some_and(|b| b.renderer.import_dmabuf(&dmabuf, None).is_ok());
        if imported {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
    }
}
smithay::delegate_dmabuf!(HalmasuitState);
smithay::delegate_presentation!(HalmasuitState);
smithay::delegate_viewporter!(HalmasuitState);
smithay::delegate_fractional_scale!(HalmasuitState);
smithay::delegate_single_pixel_buffer!(HalmasuitState);
smithay::delegate_pointer_gestures!(HalmasuitState);
smithay::delegate_tablet_manager!(HalmasuitState);

// Phase B handlers: smithay traits with sensible default impls.
// Nothing protocol-visible to customize for v1.
impl smithay::wayland::fractional_scale::FractionalScaleHandler for HalmasuitState {}
impl smithay::wayland::tablet_manager::TabletSeatHandler for HalmasuitState {}

// Phase B: xdg-decoration handler. halmasuit always answers
// `ServerSide` — its single fullscreen toplevel needs no decoration,
// and `ServerSide` means "no client-side draws contribute either"
// (which is what fullscreen-greeter / fullscreen-session want).
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
impl smithay::wayland::shell::xdg::decoration::XdgDecorationHandler for HalmasuitState {
    fn new_decoration(&mut self, toplevel: smithay::wayland::shell::xdg::ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        toplevel.send_pending_configure();
    }
    fn request_mode(
        &mut self,
        toplevel: smithay::wayland::shell::xdg::ToplevelSurface,
        _mode: DecorationMode,
    ) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        toplevel.send_pending_configure();
    }
    fn unset_mode(&mut self, toplevel: smithay::wayland::shell::xdg::ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        toplevel.send_pending_configure();
    }
}
smithay::delegate_xdg_decoration!(HalmasuitState);

impl smithay::wayland::xdg_activation::XdgActivationHandler for HalmasuitState {
    fn activation_state(&mut self) -> &mut smithay::wayland::xdg_activation::XdgActivationState {
        &mut self.xdg_activation_state
    }
    fn request_activation(
        &mut self,
        _token: smithay::wayland::xdg_activation::XdgActivationToken,
        _token_data: smithay::wayland::xdg_activation::XdgActivationTokenData,
        _surface: WlSurface,
    ) {
        // halmasuit's foreground is greeter→session lifecycle driven.
        // Client activation requests are accepted (token tracked) but
        // do not switch focus or surface visibility in v1.
        tracing::debug!("xdg_activation: request_activation (ignored in v1)");
    }
}
smithay::delegate_xdg_activation!(HalmasuitState);

// Phase B: zwp_idle_inhibit — no idle behavior in v1; inhibit/
// uninhibit are no-ops.
impl smithay::wayland::idle_inhibit::IdleInhibitHandler for HalmasuitState {
    fn inhibit(&mut self, _surface: WlSurface) {}
    fn uninhibit(&mut self, _surface: WlSurface) {}
}
smithay::delegate_idle_inhibit!(HalmasuitState);

// Phase B: zwp_keyboard_shortcuts_inhibit — no global shortcuts in
// v1; smithay tracks inhibitor objects, default trait methods are
// fine (new_inhibitor / inhibitor_destroyed are no-ops).
impl smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitHandler
    for HalmasuitState
{
    fn keyboard_shortcuts_inhibit_state(
        &mut self,
    ) -> &mut smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }
}
smithay::delegate_keyboard_shortcuts_inhibit!(HalmasuitState);

// Phase B: xdg_foreign_v2 — cross-client toplevel handle export/import.
// halmasuit hosts one fullscreen toplevel per phase so cross-client
// embedding is inert in v1; smithay tracks handle bookkeeping.
impl smithay::wayland::xdg_foreign::XdgForeignHandler for HalmasuitState {
    fn xdg_foreign_state(&mut self) -> &mut smithay::wayland::xdg_foreign::XdgForeignState {
        &mut self.xdg_foreign_state
    }
}
smithay::delegate_xdg_foreign!(HalmasuitState);

// Phase B: xdg_wm_dialog_v1 — GTK 4 / Qt 6 mark toplevels as
// modal/non-modal dialogs via this. halmasuit takes no action on the
// hint; smithay's default no-op `dialog_hint_changed` is fine.
impl smithay::wayland::shell::xdg::dialog::XdgDialogHandler for HalmasuitState {}
smithay::delegate_xdg_dialog!(HalmasuitState);

// Phase B: xdg_toplevel_icon_manager_v1 — toolkits set toplevel icons
// via this protocol. halmasuit shows no titlebars/task list in v1;
// smithay caches the request, default no-op `set_icon` is fine.
impl smithay::wayland::xdg_toplevel_icon::XdgToplevelIconHandler for HalmasuitState {}
smithay::delegate_xdg_toplevel_icon!(HalmasuitState);

// Phase B: wl_data_device_manager — wayland.xml core selection/DnD.
// Selection (clipboard) routing between clients is internal to
// smithay; the handler is the dispatch entry. halmasuit's
// SelectionUserData is `()` because there is no xwayland-to-Wayland
// selection forwarding (the anvil pattern that uses non-unit user
// data). DnD grab handlers use smithay defaults:
// `WaylandDndGrabHandler::dnd_requested` cancels the source (correct
// for the single-fullscreen-toplevel model), and the DndGrabHandler
// reap path is a no-op.
impl smithay::wayland::selection::data_device::DataDeviceHandler for HalmasuitState {
    fn data_device_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::data_device::DataDeviceState {
        &mut self.data_device_state
    }
}
impl smithay::wayland::selection::data_device::WaylandDndGrabHandler for HalmasuitState {}
impl smithay::input::dnd::DndGrabHandler for HalmasuitState {}
impl smithay::wayland::selection::SelectionHandler for HalmasuitState {
    type SelectionUserData = ();
}
smithay::delegate_data_device!(HalmasuitState);

// Phase B: zwp_primary_selection_device_manager_v1 — X11-style
// middle-click primary buffer. Shares the SelectionHandler with
// data-device; smithay routes via SelectionTarget::{Primary,Clipboard}
// at the single entry point. Only the state accessor is required;
// `new_selection` / `send_selection` defaults from SelectionHandler
// are reused (halmasuit does no xwayland forwarding).
impl smithay::wayland::selection::primary_selection::PrimarySelectionHandler for HalmasuitState {
    fn primary_selection_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &mut self.primary_selection_state
    }
}
smithay::delegate_primary_selection!(HalmasuitState);

// Phase B: zwp_text_input_manager_v3 — Qt-mandatory IME protocol.
// No explicit handler trait; the delegate macro provides all dispatch.
// halmasuit hosts no input-method-v2 server, so text-input clients
// bind successfully and track focus via TextInputHandle in seat
// user_data, but never receive preedit/commit events.
smithay::delegate_text_input_manager!(HalmasuitState);

// Phase B: wp_cursor_shape_manager_v1 — named-cursor protocol.
// Smithay's delegate routes set_shape requests through
// SeatHandler::cursor_image as CursorImageStatus::Named(...). The
// existing R8b-state cursor_image impl stores the latest status,
// so no additional handler is needed; visible cursor compositing
// from that state is the orthogonal R8b-render follow-up.
smithay::delegate_cursor_shape!(HalmasuitState);

impl BufferHandler for HalmasuitState {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
        // No buffer-tracking state yet; the renderer task will hook
        // into this to evict any cached GPU resources for the buffer.
    }
}

// ── greetd I/O integration ──────────────────────────────────────────────

/// Per-greeter-connection EPISODE state held in
/// `HalmasuitState::connections` (Amendments A6/A7/A8).
///
/// The greeter `UnixStream` lives inside its own calloop `Generic`
/// source. The `BrokerEpisode` here is the SOLE owner of the broker
/// `SeqpacketChannel` (`OwnedFd`) for the whole episode (A6); the
/// broker fd is watched by a SECOND calloop `Generic` over a
/// non-owning borrowed-fd newtype (A8) whose `RegistrationToken` is
/// `broker_token`. Teardown order is load-bearing (A8.2): the greeter
/// source's callback removes `broker_token` BEFORE `connections`
/// removes this `ConnState` (which drops the episode and is the one
/// `close(2)` of the broker fd).
struct ConnState {
    episode: BrokerEpisode,
    /// Bytes pending to the greeter. Both the greeter source (from
    /// `feed_greeter`) and the broker source (from `on_broker_readable`)
    /// push here; the greeter source flushes it (A8.4: cross-source
    /// coupling goes through this shared state, never source→source).
    write_buf: Vec<u8>,
    /// calloop token of the broker fd's borrowed-fd `Generic` source.
    /// Removed before this `ConnState` drops (A8.2).
    broker_token: Option<RegistrationToken>,
    /// calloop token of the greeter `UnixStream` source. `None` once
    /// the greeter source has detached (greeter killed at hand-off, or
    /// disconnected) — the episode then runs to `SessionEnded` driven
    /// solely by the broker source.
    greeter_token: Option<RegistrationToken>,
    /// The whole EPISODE is over (auth abort, broker EOF/fail-closed,
    /// `SessionEnded`, fatal) ⇒ full teardown. Distinct from the
    /// greeter connection merely ending at hand-off: the broker
    /// channel must outlive the greeter (A5/A6 — transport lifetime ≥
    /// PAM-handle lifetime ≥ auth-state lifetime).
    terminate: bool,
    /// The broker forked-then-dropped the session leader (greetd
    /// reached `Spawning`); the greeter has been torn down. Past this
    /// the greeter source only detaches itself; the episode lives on
    /// the broker channel until `SessionEnded`.
    spawned: bool,
    /// Amendment A5.6: the SOLE owner of the poll-only leader pidfd the
    /// broker passed via SCM_RIGHTS on `SessionOpened`. Its `close(2)`
    /// is this `OwnedFd`'s drop (at `ConnState` teardown); a SECOND
    /// `Generic` over a non-owning [`BorrowedLeaderPidfd`] watches it
    /// readable (= leader exited). NEVER waitid/reap/`pidfd_send_signal`
    /// — the broker is the sole reaper (R9/A5); this is a latency /
    /// broker-crash-resilience accelerator, not the authoritative
    /// signal (that is `SessionEnded`).
    leader_pidfd: Option<OwnedFd>,
    /// calloop token of the leader-pidfd backstop source. Removed
    /// before this `ConnState`'s `leader_pidfd` `OwnedFd` drops (A8.2):
    /// the one-shot callback `PostAction::Remove`s it (deregister at
    /// end-of-dispatch) while the `OwnedFd` lives on in `ConnState`,
    /// and every full-teardown site removes it (if still armed) before
    /// dropping the `ConnState`.
    leader_pidfd_token: Option<RegistrationToken>,
}

/// Non-owning calloop fd wrapper for the broker `SeqpacketChannel`
/// (Amendment A8.1). calloop's `Generic<F: AsFd>` only ever calls
/// `as_fd()` (register/reregister/unregister) and NEVER `close(2)`s;
/// holding a bare `RawFd` here means the source owns nothing and
/// closes nothing. The sole `OwnedFd` lives in `ConnState::episode`;
/// the single `close(2)` is that `OwnedFd`'s drop. Soundness of the
/// borrow rests on the A8.2 invariant: the source is removed before
/// the owning `ConnState` is dropped.
struct BorrowedBrokerFd(std::os::fd::RawFd);

impl std::os::fd::AsFd for BorrowedBrokerFd {
    #[expect(
        unsafe_code,
        reason = "A8.1: calloop needs an AsFd to arm epoll; we must NOT \
                  give it an owning handle (no dup/Rc/Arc — A6/A8.3). \
                  The RawFd outlives this source because the owning \
                  OwnedFd in ConnState::episode is dropped only AFTER \
                  loop_handle.remove(broker_token) (A8.2)."
    )]
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: `self.0` is a live OS fd for the duration of any
        // call to this method. Soundness:
        //  - The sole `OwnedFd` for this fd lives in
        //    `ConnState::episode`'s broker `SeqpacketChannel` (A6: one
        //    owner per episode).
        //  - calloop only ever invokes `as_fd()` on this borrow during
        //    epoll register/reregister/unregister; it never calls
        //    `close(2)` on a `BorrowedFd`.
        //  - The `RegistrationToken` for this source is removed via
        //    `loop_handle.remove(broker_token)` BEFORE the owning
        //    `OwnedFd` drops at end-of-episode (A8.2 teardown order).
        //  - No `dup`/`Rc`/`Arc` of this fd exists anywhere (A8.3),
        //    so no race where another path closes it early can occur.
        // Consequence: the fd referenced by `self.0` is valid every
        // time `as_fd()` runs, satisfying `BorrowedFd::borrow_raw`'s
        // contract that the caller guarantees fd validity for the
        // returned `BorrowedFd<'_>`'s lifetime.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.0) }
    }
}

/// Non-owning calloop fd wrapper for the Amendment-A5.6 poll-only
/// leader pidfd backstop. Same A8.1 discipline as [`BorrowedBrokerFd`]:
/// calloop only `as_fd()`s (epoll arm/disarm), never `close(2)`s; the
/// sole `OwnedFd` is `ConnState::leader_pidfd`, dropped only AFTER this
/// source is deregistered (A8.2 — the one-shot callback
/// `PostAction::Remove`s it; full teardown removes the token first).
/// The compositor only ever WATCHES this fd (`EPOLLIN` = leader gone);
/// it never `waitid`/`pidfd_send_signal`s it (R9/A5 — broker is the
/// sole reaper).
struct BorrowedLeaderPidfd(std::os::fd::RawFd);

impl std::os::fd::AsFd for BorrowedLeaderPidfd {
    #[expect(
        unsafe_code,
        reason = "A8.1/A5.6: calloop needs an AsFd to arm epoll on the \
                  leader pidfd; it must NOT own/close it (no dup/Rc/Arc). \
                  The RawFd outlives this source — ConnState::leader_pidfd \
                  (the sole OwnedFd) drops only AFTER the token is removed \
                  (A8.2)."
    )]
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: `self.0` is a live OS pidfd for the duration of any
        // call to this method. Soundness:
        //  - The sole `OwnedFd` for this pidfd lives in
        //    `ConnState::leader_pidfd`, received once from the broker
        //    via SCM_RIGHTS on `SessionOpened` (A5.6).
        //  - calloop only ever invokes `as_fd()` on this borrow during
        //    epoll register/reregister/unregister; it never calls
        //    `close(2)` on a `BorrowedFd`.
        //  - The one-shot calloop callback returns `PostAction::Remove`
        //    on EPOLLIN, deregistering the source BEFORE the owning
        //    `OwnedFd` is dropped (A8.2). Full-teardown paths also
        //    remove the token before dropping `ConnState`.
        //  - The compositor only ever EPOLL-WATCHES this pidfd; it
        //    never `waitid`/`pidfd_send_signal`s it (R9/A5 — broker is
        //    the sole reaper). The borrow's read-only nature combined
        //    with A8.2 teardown order keeps the underlying fd valid
        //    for every `as_fd()` call.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.0) }
    }
}

/// calloop callback for the greetd listening socket. Accept loop drives
/// `accept` non-blocking until `WouldBlock`; each accepted stream
/// authorises against `state.greeter_uid` via SO_PEERCRED and then
/// registers as its own `Generic<UnixStream>` calloop source.
///
/// `listener` is `&mut NoIoDrop<UnixListener>`; `UnixListener::accept`
/// takes `&self` so we go through `Deref`. No unsafe needed.
#[allow(
    clippy::unnecessary_wraps,
    reason = "calloop callback signature requires Result<PostAction, io::Error>"
)]
#[allow(
    clippy::needless_pass_by_ref_mut,
    reason = "calloop callback signature requires &mut NoIoDrop<T>"
)]
fn handle_listener_ready(
    _: calloop::Readiness,
    listener: &mut calloop::generic::NoIoDrop<UnixListener>,
    state: &mut HalmasuitState,
) -> Result<PostAction, io::Error> {
    let listener: &UnixListener = listener;
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let creds = match peer_credentials(&stream) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "peer_credentials failed; dropping connection");
                        continue;
                    }
                };
                if creds.uid != state.greeter_uid {
                    tracing::warn!(
                        peer_uid = creds.uid,
                        expected = state.greeter_uid,
                        "rejected greeter connection from unauthorised uid",
                    );
                    drop(stream);
                    continue;
                }
                // Connection cap. SO_PEERCRED already restricts to the
                // configured greeter uid, but a buggy or compromised
                // greeter could still open connections in a loop and
                // exhaust calloop sources / map entries. The cap is
                // generous: a real greeter opens one connection at a
                // time. Reaching the cap signals a runaway.
                if state.connections.len() >= MAX_GREETD_CONNECTIONS {
                    tracing::warn!(
                        cap = MAX_GREETD_CONNECTIONS,
                        active = state.connections.len(),
                        "greetd connection cap reached; dropping new connection",
                    );
                    drop(stream);
                    continue;
                }
                if let Err(e) = stream.set_nonblocking(true) {
                    tracing::warn!(error = %e, "set_nonblocking on accepted stream failed");
                    continue;
                }
                let id = state.next_conn_id;
                state.next_conn_id += 1;

                // A6: connect a fresh broker channel; the episode owns
                // it for the whole episode. A connect failure is
                // recoverable — drop this greeter connection (the
                // greeter retries); never proceed without the broker.
                let episode = match connect_broker(&state.broker_socket) {
                    Ok(chan) => BrokerEpisode::new(chan, state.pam_service.clone()),
                    Err(e) => {
                        tracing::warn!(error = %e, socket = ?state.broker_socket,
                            "connect to halmasuit-session broker failed; dropping greeter connection");
                        drop(stream);
                        continue;
                    }
                };
                // A8.1: register the broker fd as a NON-OWNING source.
                let broker_raw = episode.broker_fd().as_raw_fd();

                let greeter_tok = match state.loop_handle.insert_source(
                    Generic::new(stream, Interest::BOTH, CalloopMode::Level),
                    move |readiness, stream, state| {
                        handle_connection_ready(id, readiness, stream, state)
                    },
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to register greeter source; dropping");
                        // `episode` drops here → the one close(2) of the
                        // broker fd. No source registered yet (A8.2 n/a).
                        continue;
                    }
                };
                let broker_tok = match state.loop_handle.insert_source(
                    Generic::new(
                        BorrowedBrokerFd(broker_raw),
                        Interest::READ,
                        CalloopMode::Level,
                    ),
                    move |readiness, _fd, state| handle_broker_ready(id, readiness, state),
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to register broker source; dropping");
                        // Roll back the greeter source so no orphaned
                        // source survives without a ConnState.
                        state.loop_handle.remove(greeter_tok);
                        // `episode` drops here → the one close(2).
                        continue;
                    }
                };
                state.connections.insert(
                    id,
                    ConnState {
                        episode,
                        write_buf: Vec::new(),
                        broker_token: Some(broker_tok),
                        greeter_token: Some(greeter_tok),
                        terminate: false,
                        spawned: false,
                        leader_pidfd: None,
                        leader_pidfd_token: None,
                    },
                );
                tracing::debug!(id, peer_uid = creds.uid, "accepted greeter connection");
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                break;
            }
        }
    }
    Ok(PostAction::Continue)
}

/// calloop callback for one greetd connection. Reads pending bytes,
/// drives the state machine, writes pending replies. Returns
/// `PostAction::Remove` when the connection should be closed (the
/// `UnixStream` drops with the source, releasing the fd).
///
/// `stream` is `&mut NoIoDrop<UnixStream>`. `Read` and `Write` are
/// implemented for `&UnixStream` (not just `&mut`), so we can read and
/// write through a shared reference and never need unsafe to escape
/// the `NoIoDrop` wrapper.
#[allow(
    clippy::unnecessary_wraps,
    reason = "calloop callback signature requires Result<PostAction, io::Error>"
)]
#[allow(
    clippy::needless_pass_by_ref_mut,
    reason = "calloop callback signature requires &mut NoIoDrop<T>; we go through Deref"
)]
#[allow(
    clippy::too_many_lines,
    reason = "callback body is one state-machine step; splitting hurts readability more than it helps"
)]
fn handle_connection_ready(
    id: usize,
    readiness: calloop::Readiness,
    stream: &mut calloop::generic::NoIoDrop<UnixStream>,
    state: &mut HalmasuitState,
) -> Result<PostAction, io::Error> {
    let mut stream_ref: &UnixStream = stream;
    let Some(connstate) = state.connections.get_mut(&id) else {
        // ConnState was already removed (e.g. shutdown); ditch the source.
        return Ok(PostAction::Remove);
    };

    if readiness.readable {
        let mut buf = [0u8; 4096];
        loop {
            match stream_ref.read(&mut buf) {
                Ok(0) => {
                    // Greeter closed. Post-hand-off this is expected
                    // (the greeter was SIGKILLed) and ends ONLY the
                    // greeter side — the episode lives on the broker
                    // channel until SessionEnded (A5/A6: transport ≥
                    // PAM-handle ≥ auth-state). Pre-hand-off a greeter
                    // disconnect aborts the episode.
                    if !connstate.spawned {
                        connstate.terminate = true;
                    }
                    break;
                }
                Ok(n) => {
                    let out = connstate.episode.on_greeter_bytes(&buf[..n]);
                    connstate.write_buf.extend(out.greeter_reply);
                    if let Some(spawn) = out.spawned {
                        // The broker forked-then-dropped the leader
                        // (R7); the compositor never execs anything
                        // (R3/R15). Amendment A5: identity bookkeeping
                        // ONLY (authorises the session client to
                        // connect+paint) — the greeter is NOT killed
                        // and the foreground is NOT flipped here. The
                        // VISIBLE swap is two-key-gated (SessionOpened
                        // + the session client's first frame) and
                        // happens on the broker/commit paths.
                        connstate.spawned = true;
                        record_session_started(&spawn, &mut state.session_uid);
                    }
                    if out.terminate {
                        connstate.terminate = true;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(error = %e, id, "read failed on greeter connection");
                    if !connstate.spawned {
                        connstate.terminate = true;
                    }
                    connstate.write_buf.clear();
                    break;
                }
            }
        }
    }

    if readiness.writable && !connstate.write_buf.is_empty() {
        loop {
            if connstate.write_buf.is_empty() {
                break;
            }
            match stream_ref.write(&connstate.write_buf) {
                Ok(0) => {
                    if !connstate.spawned {
                        connstate.terminate = true;
                    }
                    connstate.write_buf.clear();
                    break;
                }
                Ok(n) => {
                    connstate.write_buf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(error = %e, id, "write failed on greeter connection");
                    if !connstate.spawned {
                        connstate.terminate = true;
                    }
                    connstate.write_buf.clear();
                    break;
                }
            }
        }
    }

    // Read teardown decisions off `connstate` before its borrow ends.
    let episode_over = connstate.terminate && connstate.write_buf.is_empty();
    let detach_greeter_only = !connstate.terminate
        && connstate.spawned
        && connstate.greeter_token.is_some()
        && connstate.write_buf.is_empty();
    let broker_tok = connstate.broker_token;
    let leader_pidfd_tok = connstate.leader_pidfd_token;
    // `connstate`'s borrow of `state.connections` ends here.

    if episode_over {
        // Full teardown. A8.2: deregister BOTH borrowed-fd sources
        // (broker channel + the A5.6 leader-pidfd backstop) BEFORE the
        // `ConnState` (the sole `OwnedFd` of each) drops.
        if let Some(t) = broker_tok {
            state.loop_handle.remove(t);
        }
        if let Some(t) = leader_pidfd_tok {
            state.loop_handle.remove(t);
        }
        state.connections.remove(&id);
        return Ok(PostAction::Remove); // greeter source self-removes
    }
    if detach_greeter_only {
        // greetd's connection is terminal after StartSession and the
        // greeter process was SIGKILLed; drop ONLY this (greeter)
        // source. The `ConnState` + broker source live on until
        // `SessionEnded` (A5/A6).
        if let Some(cs) = state.connections.get_mut(&id) {
            cs.greeter_token = None;
        }
        return Ok(PostAction::Remove);
    }
    Ok(PostAction::Continue)
}

/// calloop callback for the per-episode broker `SOCK_SEQPACKET` source
/// (Amendment A7/A8). Readable → ONE non-blocking framed recv fed into
/// the episode → greeter reply buffered (A8.4: flushed by the greeter
/// source, never written here) + lifecycle consumed. Broker EOF /
/// transport error → the episode fails closed (A7.4) as an ordinary
/// source event — the render/calloop thread never blocks.
#[allow(
    clippy::unnecessary_wraps,
    reason = "calloop callback signature requires Result<PostAction, io::Error>"
)]
#[allow(
    clippy::needless_pass_by_ref_mut,
    reason = "calloop callback signature requires &mut NoIoDrop<T>; unused here"
)]
fn handle_broker_ready(
    id: usize,
    readiness: calloop::Readiness,
    state: &mut HalmasuitState,
) -> Result<PostAction, io::Error> {
    let Some(connstate) = state.connections.get_mut(&id) else {
        // ConnState already torn down. The broker source's borrowed-fd
        // `Generic` owns nothing (A8.1) — dropping it closes no fd.
        return Ok(PostAction::Remove);
    };

    let (key1_session_opened, session_ended) = if readiness.readable {
        let out = connstate.episode.on_broker_readable();
        connstate.write_buf.extend(out.greeter_reply);
        if let Some(spawn) = out.spawned {
            connstate.spawned = true;
            record_session_started(&spawn, &mut state.session_uid);
        }
        if out.terminate {
            connstate.terminate = true;
        }
        (out.session_opened, out.session_ended)
    } else {
        (false, None)
    };

    let terminate = connstate.terminate;
    let greeter_attached = connstate.greeter_token.is_some();
    // `connstate`'s borrow ends here.

    // Amendment A5 two-key gate: key 1 (`SessionOpened`) and the
    // revert trigger (`SessionEnded`) arrive on the broker channel.
    // Apply BEFORE any teardown so a `SessionEnded` revert
    // re-composites the splash before the `ConnState` drops (no
    // black/stale intermediate — the no-flash invariant on the revert
    // side too).
    if key1_session_opened {
        emit(&Event::SessionOpened);
        let a = state.swap.session_opened();
        state.apply_swap_action(a);
        // A5.6: arm the poll-only leader-pidfd liveness backstop the
        // broker passed via SCM_RIGHTS on this same frame.
        arm_leader_pidfd_backstop(id, state);
    }
    if let Some(outcome) = session_ended {
        emit(&Event::SessionEnded {
            outcome: session_exit_of(outcome),
        });
        let a = state.swap.session_ended();
        state.apply_swap_action(a);
    }

    if terminate {
        if greeter_attached {
            // The greeter source owns the write vehicle + the A8.2
            // teardown ordering; it will flush any fail-closed reply
            // then full-teardown. Nothing to do here.
            return Ok(PostAction::Continue);
        }
        // Post-hand-off: the broker source is the sole driver. We
        // cannot remove the current source synchronously and
        // `PostAction::Remove` defers deregistration to end-of-dispatch
        // — so DEFER the `ConnState` drop to an idle callback that runs
        // AFTER this source is deregistered, keeping the A8.2 ordering
        // (epoll-deregister before the one `close(2)`). The idle also
        // removes the A5.6 leader-pidfd backstop source (if still
        // armed) BEFORE dropping the `ConnState` — same A8.2 ordering
        // for that second borrowed fd.
        state.loop_handle.insert_idle(move |st| {
            if let Some(t) = st.connections.get(&id).and_then(|cs| cs.leader_pidfd_token) {
                st.loop_handle.remove(t);
            }
            st.connections.remove(&id);
        });
        return Ok(PostAction::Remove);
    }
    Ok(PostAction::Continue)
}

/// Amendment A5.6 — arm the poll-only leader-pidfd liveness backstop.
///
/// Takes the SCM_RIGHTS leader pidfd the broker attached to
/// `SessionOpened` (absent on the fd-less/older path — the backstop is
/// an accelerator, NOT the authoritative signal, which is
/// `SessionEnded`) and registers a SECOND borrowed-fd `Generic` source
/// over it. The sole `OwnedFd` is stored in `ConnState::leader_pidfd`
/// BEFORE the non-owning source is registered (A8.1/A8.2). The
/// compositor only ever WATCHES this fd; it never
/// `waitid`/`pidfd_send_signal`s it (R9/A5 — the broker is the sole
/// reaper).
fn arm_leader_pidfd_backstop(id: usize, state: &mut HalmasuitState) {
    let Some(fd) = state
        .connections
        .get_mut(&id)
        .and_then(|cs| cs.episode.take_leader_pidfd())
    else {
        return;
    };
    let raw = fd.as_raw_fd();
    // Store the SOLE OwnedFd in ConnState BEFORE registering the
    // non-owning source — the borrow is valid only while it lives.
    if let Some(cs) = state.connections.get_mut(&id) {
        cs.leader_pidfd = Some(fd);
    } else {
        return; // ConnState vanished; `fd` drops (closes) here.
    }
    match state.loop_handle.insert_source(
        Generic::new(BorrowedLeaderPidfd(raw), Interest::READ, CalloopMode::Level),
        move |_readiness, _fd, st| handle_leader_pidfd_ready(id, st),
    ) {
        Ok(tok) => {
            if let Some(cs) = state.connections.get_mut(&id) {
                cs.leader_pidfd_token = Some(tok);
                // The privilege-crossing fd made it worker→broker→
                // compositor and is armed poll-only (A5.6).
                emit(&Event::SessionLeaderPidfdArmed);
            } else {
                // ConnState vanished between the two borrows: the just-
                // registered source would fire against a dropped fd —
                // deregister it now (A8.2).
                state.loop_handle.remove(tok);
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "leader-pidfd backstop registration failed; \
                 SessionEnded / client-disconnect still drive the revert"
            );
            // No source watches it — drop the OwnedFd we stored.
            if let Some(cs) = state.connections.get_mut(&id) {
                cs.leader_pidfd = None;
            }
        }
    }
}

/// calloop callback for the A5.6 leader-pidfd backstop. `EPOLLIN` on a
/// pidfd means the target task has EXITED. Poll-only (A5.6): we MUST
/// NOT `waitid`/reap/`pidfd_send_signal` it (not our child — `ECHILD`;
/// the broker is the sole reaper, R9/A5). It is one-shot — the leader
/// exits exactly once — and a latency/robustness ACCELERATOR for the
/// revert, not the authoritative signal. The [`swap_gate::SwapGate`]
/// makes whichever of {this, `SessionEnded`, session-client
/// disconnect} arrives later inert ⇒ exactly one revert.
#[expect(
    clippy::unnecessary_wraps,
    reason = "calloop callback signature requires Result<PostAction, io::Error>"
)]
fn handle_leader_pidfd_ready(
    id: usize,
    state: &mut HalmasuitState,
) -> Result<PostAction, io::Error> {
    if let Some(cs) = state.connections.get_mut(&id) {
        // The source is being removed (PostAction::Remove below); clear
        // the token so the full-teardown sites don't double-remove an
        // already-removed registration.
        cs.leader_pidfd_token = None;
    }
    emit(&Event::SessionLeaderExitedViaPidfd);
    let a = state.swap.session_client_gone();
    state.apply_swap_action(a);
    // Deregister at end-of-dispatch; `ConnState` keeps the `OwnedFd`
    // (closed at its own teardown, AFTER this deregister — A8.2).
    Ok(PostAction::Remove)
}

/// Record that the broker has launched the session leader (it
/// forked-then-dropped in a non-setuid child per R7 — the compositor
/// never execs anything, R3/R15): set the session uid and emit
/// `SessionRequested`. Amendment A5: identity bookkeeping ONLY — this
/// authorises the session client to connect to the Wayland socket
/// (`wayland_peer_authorized`) and paint, but does NOT kill the
/// greeter or flip the foreground. The VISIBLE swap is deferred to the
/// two-key gate (AND of `SessionOpened` and the session client's first
/// non-empty frame); swapping here would reintroduce the flash.
/// Idempotent — a second `spawned` (seen from both the greeter and
/// broker sources) is a no-op.
/// Map the wire [`halmasuit_session_ipc::SessionOutcome`] to the
/// introspect [`halmasuit_introspect::SessionExit`]. The crash-vs-clean
/// distinction is preserved (Amendment A5.2; GDM
/// `SESSION_EXITED`/`SESSION_DIED` — NOT collapsed like greetd).
/// The SO_PEERCRED uid captured for `surface`'s wl_client at accept
/// (`ClientState::uid`). `None` if the surface has no live client or
/// no `ClientState` (every client halmasuit inserts has one — see the
/// Wayland accept handler).
fn surface_client_uid(surface: &WlSurface) -> Option<u32> {
    surface
        .client()
        .and_then(|c| c.get_data::<ClientState>().map(|cs| cs.uid))
}

const fn session_exit_of(
    outcome: halmasuit_session_ipc::SessionOutcome,
) -> halmasuit_introspect::SessionExit {
    match outcome {
        halmasuit_session_ipc::SessionOutcome::Exited { code } => {
            halmasuit_introspect::SessionExit::Exited { code }
        }
        halmasuit_session_ipc::SessionOutcome::Signaled { signal } => {
            halmasuit_introspect::SessionExit::Signaled { signal }
        }
    }
}

fn record_session_started(spawn: &SpawnRequest, session_uid: &mut Option<u32>) {
    if session_uid.is_some() {
        return;
    }
    emit(&Event::SessionRequested {
        uid: spawn.uid,
        gid: spawn.gid,
    });
    *session_uid = Some(spawn.uid);
}

/// Phase B post-pivot setup AFTER the process-root migration has
/// already chrooted halmasuit into rootfs's filesystem view. The
/// migration runs as its own calloop-driven phase (see
/// `PivotPhase::Connecting` / `AwaitingReply` in the pivot-poll timer
/// in `main` below); this function is invoked from the final
/// `PivotPhase::Setup` tick once the chroot is in place.
///
/// Sequence (mirrors the rootfs-only deployment's startup):
///   1. Bind the greetd listener while still root (binds the socket
///      with the configured ownership / permissions).
///   2. Spawn the greeter while still root (child uses `pre_exec` to
///      `setresuid` to the greeter user before `execve`).
///   3. Drop privileges to the compositor uid (in-process
///      `setresgid`/`setresuid`). After this, the rest of halmasuit's
///      lifetime is unprivileged.
///
/// Per-step failures log + continue; halmasuit stays alive in a
/// degraded state (no greeter / still root) so the pivot itself is
/// observably complete via `Phase::RootfsReady`, and the operator
/// can diagnose via journald. A panic here would be much worse —
/// halmasuit would die mid-boot leaving the rootfs without its
/// compositor.
fn run_post_pivot_setup(state: &mut HalmasuitState) -> io::Result<()> {
    // Accumulate per-step errors so the function reports them in
    // aggregate at the end (or as context on a hard failure).
    // Steps 1 + 2 are degraded-state recoverable: a greetd bind
    // failure or greeter spawn failure means "no auth surface
    // right now," but halmasuit can stay alive as the unprivileged
    // compositor and the operator can fix forward without
    // restarting the boot. Step 3 (drop_privileges) is NOT
    // recoverable in place: if drop fails and we're still root,
    // halmasuit must die so the unit's `Restart=on-failure` cycles
    // the boot. The "die rather than continue as root" floor
    // applies symmetrically to the "no HALMASUIT_COMPOSITOR_UID
    // env" path below.
    let mut step_errors: Vec<String> = Vec::new();

    // (1) Bind greetd listener while still root.
    match setup_greetd_listener(&state.loop_handle) {
        Ok((token, greeter_uid, pam_service)) => {
            state.greetd_listener_token = Some(token);
            state.greeter_uid = greeter_uid;
            state.pam_service = pam_service;
        }
        Err(e) => {
            tracing::error!(error = %e, "post-pivot: setup_greetd_listener failed");
            step_errors.push(format!("setup_greetd_listener: {e}"));
        }
    }

    // (2) Spawn greeter via broker. The broker reads
    // HALMASUIT_GREETER_COMMAND/UID/etc. from its own env and forks-
    // then-drops to the greeter uid; the compositor is already
    // uid 998 here (Phase B fromInitrd dropped before this runs in
    // some configurations) so a setuid-from-compositor would EPERM
    // anyway. Broker is the policy authority (Epic #47 R1).
    match broker_spawn_greeter() {
        Ok(handle) => {
            tracing::info!(
                greeter_pid = handle.pid,
                "post-pivot greeter spawned via broker"
            );
            emit(&Event::GreeterSpawned { pid: handle.pid });
            emit(&Event::ForegroundChanged {
                to: halmasuit_introspect::Foreground::Greeter,
            });
            state.greeter = Some(handle);
        }
        Err(e) => {
            tracing::error!(error = %e, "post-pivot broker_spawn_greeter failed");
            step_errors.push(format!("broker_spawn_greeter: {e}"));
        }
    }

    // libinput — post-pivot input wiring. The Phase B initramfs phase
    // deliberately starts with no libinput (no `/dev/input/event*` on
    // devtmpfs is interesting pre-pivot — only halmasuit-luks needs
    // input, and it handles its own raw VT). Post-pivot, the rootfs
    // is mounted and `/dev/input/event*` is populated; we enumerate
    // them directly via `setup_libinput_direct` (R2.3: no libseat / no
    // seatd anywhere in the runtime closure). halmasuit is still root
    // at this point — `drop_privileges` runs further down — so the
    // direct opens succeed.
    //
    // Errors are degraded-state recoverable (same posture as
    // setup_greetd_listener / spawn_greeter above): if /dev/input is
    // missing or libinput rejects every device, log + continue without
    // input. The visible surface still composites; only interactive
    // greeters lose input.
    if state.libinput_token.is_none() {
        step_errors.extend(setup_post_pivot_input(state));
    }

    // frame_audit only: retry the D-Bus `Snapshot()` server connect
    // post-chroot. The pre-pivot connect (in `main`) targets the
    // initramfs system bus, which has no halmasuit ownership policy
    // — it bounces with `AccessDenied`. The post-chroot bus is
    // rootfs's dbus-broker, whose policy DOES grant `org.halmasuit`
    // to halmasuit; a fresh connect here succeeds. In rootfs-only
    // `enable` deployments this branch never runs (`post_pivot_setup`
    // is invoked only from the fromInitrd pivot-poll timer); the
    // single main()-time connect is sufficient there.
    //
    // Sync `serve` is the only correct shape here, even though we're
    // inside the calloop dispatcher and zbus' SASL handshake can burn
    // ~tens of ms. The connect MUST run on the caller thread with
    // root euid: `drop_privileges` below is the next thing this
    // function does, dbus-broker authenticates the connecting peer
    // via SO_PEERCRED at `connect(2)` time, and that reads the
    // *current* effective uid. Any thread-internal connect would
    // race the drop and authenticate as the post-drop compositor uid,
    // refused by the `<allow own user="root"/>` policy on
    // org.halmasuit. The one-time post-pivot jitter is harmless: no
    // frames are racing against it yet (the greeter is still loading
    // QML when this fires).
    #[cfg(feature = "frame_audit")]
    if let Some(backend) = state.drm_backend.as_ref() {
        dbus::serve(backend.snapshot_handle());
    }

    // (3) Drop privileges. Compositor uid is now resolvable against
    // the rootfs /etc/passwd (the pivot completed). If drop fails
    // OR the env is missing while still root, return Err — the
    // unit's Restart=on-failure cycles the boot, which is strictly
    // preferable to continuing as root with no greeter.
    match compositor_uid_from_env()? {
        Some(uid) => {
            if let Err(e) = drop_privileges(uid) {
                return Err(io::Error::other(format!(
                    "drop_privileges({uid}): {e}; \
                     prior step errors: {step_errors:?}"
                )));
            }
            emit(&Event::PhaseEntered {
                phase: Phase::Deprivileged,
            });
        }
        None if nix::unistd::geteuid().is_root() => {
            return Err(io::Error::other(format!(
                "post-pivot: refusing to stay as root without \
                 HALMASUIT_COMPOSITOR_UID; set \
                 services.halmasuit.compositorUid in the fromInitrd \
                 deployment. Prior step errors: {step_errors:?}"
            )));
        }
        None => {
            tracing::warn!("HALMASUIT_COMPOSITOR_UID unset post-pivot; staying as current user");
        }
    }

    if !step_errors.is_empty() {
        // We dropped successfully but with prior step failures.
        // Surface the aggregate so the journal carries a single
        // grep-able "degraded" marker; halmasuit stays alive.
        tracing::error!(
            errors = ?step_errors,
            "post-pivot setup completed in DEGRADED state \
             (privilege drop succeeded; auth-surface setup failed)"
        );
    }

    Ok(())
}

/// Phase B v2 process-root migration helpers — one non-blocking
/// step each, driven from the pivot-poll calloop timer
/// (`PivotPhase::Connecting` / `AwaitingReply` in `main` below).
///
/// The cross-pivot migration: halmasuit's surviving process-root
/// references the initramfs's filesystem, leaving /etc/passwd,
/// /nix/store, /run/systemd unreachable. The broker (rootfs systemd
/// unit, rootfs process-root) sends its `/proc/self/root` fd via
/// SCM_RIGHTS on receipt of `CompositorToBroker::RequestRootFd`;
/// halmasuit `fchdir + chroot`s into it.
///
/// Hard rule (CLAUDE.md): "The compositor never blocks the render/
/// calloop thread on broker IPC. ... No blocking recv/send-then-recv,
/// with or without a timeout." So this migration runs as a calloop
/// state machine across multiple timer ticks: each tick performs at
/// most one non-blocking syscall (connect attempt, recv attempt) and
/// either transitions phase or re-arms the timer.
///
/// `try_connect_and_request_root_fd` — one non-blocking attempt at
/// connecting to the broker socket and sending the `RequestRootFd`
/// frame. Returns the channel on success so the caller can park it
/// in the `AwaitingReply` phase state; returns the connect/send
/// error otherwise so the caller can decide to retry or fail.
fn try_connect_and_request_root_fd(
    broker_socket: &Path,
) -> Result<crate::broker_session::SeqpacketChannel, io::Error> {
    use halmasuit_session_ipc::CompositorToBroker;

    let chan = connect_broker(broker_socket)
        .map_err(|e| io::Error::other(format!("connect_broker: {e:?}")))?;
    chan.send(&CompositorToBroker::RequestRootFd)
        .map_err(|e| io::Error::other(format!("send RequestRootFd: {e:?}")))?;
    Ok(chan)
}

/// `try_recv_root_fd` — one non-blocking attempt at receiving the
/// broker's `RootFd` reply on an established channel. `Ok(Some(fd))`
/// = broker has replied; `Ok(None)` = no datagram pending yet
/// (caller re-arms the timer); `Err(e)` = transport error or the
/// broker refused (e.g., SO_PEERCRED gate rejected the peer).
fn try_recv_root_fd(
    chan: &crate::broker_session::SeqpacketChannel,
) -> io::Result<Option<std::os::fd::OwnedFd>> {
    let Some((_frame, root_fd)) = chan
        .recv_with_fd()
        .map_err(|e| io::Error::other(format!("recv RootFd: {e:?}")))?
    else {
        return Ok(None);
    };
    let root_fd = root_fd
        .ok_or_else(|| io::Error::other("broker sent RootFd without SCM_RIGHTS fd attachment"))?;
    Ok(Some(root_fd))
}

/// `apply_chroot_to_root_fd` — fchdir + chroot to the broker's
/// `/proc/self/root`. Both syscalls land via nix's safe wrappers
/// (no unsafe block). Requires `CAP_SYS_CHROOT`; halmasuit has it
/// as root pre-drop. After this returns Ok, halmasuit's
/// process-root IS rootfs's process-root.
fn apply_chroot_to_root_fd(root_fd: &std::os::fd::OwnedFd) -> io::Result<()> {
    nix::unistd::fchdir(root_fd)
        .map_err(|e| io::Error::other(format!("fchdir(broker root fd): {e}")))?;
    nix::unistd::chroot(".").map_err(|e| io::Error::other(format!("chroot(\".\"): {e}")))?;
    Ok(())
}

/// Post-pivot libinput attach (Phase B fromInitrd path only).
///
/// The Phase B initramfs phase deliberately starts with no libinput
/// — there's no input subsystem available until the rootfs is up
/// (`/dev/input/event*` lives on devtmpfs, which the kernel mounts
/// before the rootfs is selected, but no input is needed pre-pivot
/// because halmasuit-luks is the only thing reading password input
/// and it handles its own raw VT). Post-pivot, we enumerate input
/// devices via the same direct-open path the rootfs deployment uses
/// — see `setup_libinput_direct`. No libseat, no seatd, no brokerage:
/// the running compositor is still root at this point (privilege drop
/// happens later in `run_post_pivot_setup`'s sequence).
///
/// Each step is degraded-state recoverable (same posture as
/// `setup_greetd_listener` / `spawn_greeter` in `run_post_pivot_setup`):
/// errors are returned as strings for the caller's `step_errors`
/// aggregate; the function never panics or returns Err.
fn setup_post_pivot_input(state: &mut HalmasuitState) -> Vec<String> {
    let mut errs: Vec<String> = Vec::new();
    match setup_libinput_direct(&state.loop_handle) {
        Ok(token) => {
            state.libinput_token = Some(token);
            tracing::info!("post-pivot: libinput backend attached (direct, no libseat)");
        }
        Err(e) => {
            tracing::error!(error = %e, "post-pivot: setup_libinput_direct failed");
            errs.push(format!("post-pivot setup_libinput_direct: {e}"));
        }
    }
    errs
}

/// Direct-open `LibinputInterface` impl: no libseat / seatd brokerage.
///
/// `setup_libinput_direct` calls into this while halmasuit is still
/// root, so the O_RDWR opens against `/dev/input/event*` succeed against
/// the kernel's uaccess-tagged ACL the same way they would for any other
/// root opener. libinput stashes the resulting OwnedFds; after
/// `drop_privileges` the compositor runs as the compositor user, but the
/// fds remain valid — Linux checks file permissions at `open(2)`, not
/// at subsequent `read(2)` / `write(2)`.
struct DirectInputInterface;
impl smithay::reexports::input::LibinputInterface for DirectInputInterface {
    fn open_restricted(
        &mut self,
        path: &std::path::Path,
        flags: i32,
    ) -> Result<std::os::fd::OwnedFd, i32> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let accmode = flags & libc::O_ACCMODE;
        std::fs::OpenOptions::new()
            .custom_flags(flags)
            .read(accmode == libc::O_RDONLY || accmode == libc::O_RDWR)
            .write(accmode == libc::O_WRONLY || accmode == libc::O_RDWR)
            .open(path)
            .map(std::os::fd::OwnedFd::from)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
    }
    fn close_restricted(&mut self, fd: std::os::fd::OwnedFd) {
        drop(fd);
    }
}

/// Build a libinput backend that opens `/dev/input/event*` devices
/// directly (no libseat / seatd brokerage), attach it to the calloop,
/// and dispatch events to `HalmasuitState::dispatch_libinput`.
///
/// halmasuit is a system compositor and enumerates input devices once
/// at startup — no udev-monitor-driven hotplug. Newly-plugged USB
/// devices won't be picked up without a compositor restart (intentional;
/// matches the "system compositor enumerated at boot" framing). The
/// trade for that loss is removing seatd from the runtime closure
/// entirely, which collapses the rootfs→shutdownRamfs pivot survival
/// surface (R2.3): no standing root daemon to keep alive across the
/// pivot, no smithay `LibSeatSessionNotifier` panic vector when seatd
/// dies mid-shutdown.
///
/// Each `/dev/input/eventN` open happens via `DirectInputInterface`
/// while halmasuit is still root. libinput retains the OwnedFds;
/// subsequent reads work after `drop_privileges` because the kernel
/// checks file permissions at `open()`, not on later I/O.
fn setup_libinput_direct(
    loop_handle: &LoopHandle<'static, HalmasuitState>,
) -> io::Result<RegistrationToken> {
    let mut libinput = smithay::reexports::input::Libinput::new_from_path(DirectInputInterface);
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(it) => it,
        Err(e) => {
            // Test / SKIP-DRM environments may have no /dev/input. Log
            // and proceed with a libinput context that has no devices;
            // the calloop source still drives correctly (it just never
            // produces events).
            tracing::warn!(
                error = %e,
                "/dev/input: read_dir failed; no input devices will be enumerated"
            );
            return loop_handle
                .insert_source(
                    LibinputInputBackend::new(libinput),
                    |event, (), state: &mut HalmasuitState| state.dispatch_libinput(event),
                )
                .map_err(|e| io::Error::other(format!("insert libinput backend: {e}")));
        }
    };
    let mut device_count = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("event") {
            continue;
        }
        let Some(path_str) = path.to_str() else {
            continue;
        };
        // `path_add_device` calls back into `DirectInputInterface::
        // open_restricted`. `None` means libinput rejected the device
        // (e.g., kernel reported no input-class capability). Log +
        // skip; not fatal.
        if libinput.path_add_device(path_str).is_none() {
            tracing::debug!(path = %path.display(), "libinput.path_add_device refused");
            continue;
        }
        device_count += 1;
    }
    tracing::info!(
        device_count,
        "libinput direct: enumerated /dev/input/event* devices"
    );
    loop_handle
        .insert_source(
            LibinputInputBackend::new(libinput),
            |event, (), state: &mut HalmasuitState| state.dispatch_libinput(event),
        )
        .map_err(|e| io::Error::other(format!("insert libinput backend: {e}")))
}

/// Apply the Phase B initramfs group-ownership convention to a freshly
/// bound socket inode.
///
/// The rootfs `enable` deployment lets systemd's
/// `Group = cfg.greeterGroup` directive set the process's egid before
/// each socket bind, so the bound file inherits the right gid. The
/// initramfs Phase B unit can't use `Group=` (no NSS pre-pivot;
/// `Group=halmasuit-greeter` fails 216/GROUP), so we read the
/// operator-supplied `HALMASUIT_WAYLAND_GROUP_GID` and `fchown` the
/// inode to that gid. Without this the bound file ends up `root:root`
/// and any greeter-group member (the spawned greeter, niri-as-session)
/// hits EACCES on `connect(2)`. No-op when the env is unset
/// (rootfs path).
fn apply_wayland_group_gid(path: &Path) -> io::Result<()> {
    let Ok(gid_str) = std::env::var("HALMASUIT_WAYLAND_GROUP_GID") else {
        return Ok(());
    };
    let Ok(gid) = gid_str.parse::<u32>() else {
        return Ok(());
    };
    nix::unistd::chown(path, None, Some(nix::unistd::Gid::from_raw(gid)))
        .map_err(|e| io::Error::other(format!("chgrp {} -> {gid}: {e}", path.display())))
}

/// Bind the greetd socket, register it as a calloop source, and emit
/// `Phase::GreetdReady`. Returns the registration token (held by
/// `HalmasuitState` so the source lives for the compositor's lifetime)
/// plus the resolved greeter UID and PAM service name (cached on the
/// state for use by the listener callback).
fn setup_greetd_listener(
    loop_handle: &LoopHandle<'static, HalmasuitState>,
) -> io::Result<(RegistrationToken, u32, String)> {
    let greetd_path = greetd_socket_path_from_env();
    if let Some(parent) = greetd_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            io::Error::other(format!(
                "create greetd socket parent {}: {e}",
                parent.display()
            ))
        })?;
    }
    if greetd_path.exists() {
        let _ = std::fs::remove_file(&greetd_path);
    }
    let greetd_listener = bind_socket(&greetd_path, 0o660)
        .map_err(|e| io::Error::other(format!("bind greetd socket: {e}")))?;
    greetd_listener.set_nonblocking(true)?;
    apply_wayland_group_gid(&greetd_path)?;
    tracing::info!(socket = ?greetd_path, "greetd socket bound");

    let greeter_uid = greeter_uid_from_env();
    let pam_service = pam_service_from_env();
    tracing::info!(uid = greeter_uid, service = %pam_service, "greeter auth configured");

    let token = loop_handle
        .insert_source(
            Generic::new(greetd_listener, Interest::READ, CalloopMode::Level),
            handle_listener_ready,
        )
        .map_err(io::Error::other)?;

    emit(&Event::PhaseEntered {
        phase: Phase::GreetdReady,
    });

    Ok((token, greeter_uid, pam_service))
}

fn greeter_uid_from_env() -> u32 {
    std::env::var("HALMASUIT_GREETER_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| nix::unistd::getuid().as_raw())
}

fn greetd_socket_path_from_env() -> PathBuf {
    if let Ok(p) = std::env::var("HALMASUIT_GREETD_SOCKET") {
        return PathBuf::from(p);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/halmasuit".into());
    PathBuf::from(runtime).join("greetd.sock")
}

fn pam_service_from_env() -> String {
    std::env::var("HALMASUIT_PAM_SERVICE").unwrap_or_else(|_| "halmasuit".into())
}

/// Path of the privileged `halmasuit-session` broker `SOCK_SEQPACKET`
/// socket. Defaults to `/run/halmasuit-session.sock` — the
/// `ListenSequentialPacket` the socket-activated unit binds
/// (`nix/module.nix`); overridable via `HALMASUIT_BROKER_SOCKET` for
/// dev/test.
fn broker_socket_path_from_env() -> PathBuf {
    std::env::var_os("HALMASUIT_BROKER_SOCKET").map_or_else(
        || PathBuf::from("/run/halmasuit-session.sock"),
        PathBuf::from,
    )
}

/// The transient GL clear color in XRGB8888 little-endian, derived
/// from [`drm::CLEAR_RGB`]. Under epic amendment G1/R6 it is never
/// visible: the wallpaper plane covers the entire output on every
/// frame including frame 0, so there is no observable pre-client
/// solid phase. It remains the uncovered sentinel the no-flash
/// audit keys on — a pixel byte-equal to it means the wallpaper is
/// NOT covering (a flash / broken renderer).
///
/// Built via [`drm::xrgb_le`] from [`drm::CLEAR_RGB`] — the single
/// source of truth — so the byte ordering is unit-tested at build
/// (see `drm::tests::xrgb_le_pins_byte_order`) and the renderer clear
/// can never drift from what `frame_audit` / `offscreen` expect. A
/// wrong byte order, a channel transpose, or `#000000` trips a fast
/// unit test before the visual VM gate.
const HALMASUIT_BRAND_CLEAR: [u8; 4] =
    drm::xrgb_le(drm::CLEAR_RGB[0], drm::CLEAR_RGB[1], drm::CLEAR_RGB[2]);

/// Resolve the DRM device path to use, honoring overrides and the
/// `HALMASUIT_SKIP_DRM_MASTER` dev/test bypass. Returns `Ok(None)` only
/// when the bypass is set under non-root euid (a tracked, warned-about
/// dev launch); returns the path otherwise.
///
/// Fail-closed under euid 0 if the bypass is requested — the SKIP path
/// disarms a core architectural invariant and must not be honored from
/// the production systemd unit.
///
/// `HALMASUIT_DRM_DEVICE` accepts three shapes (see [`drm::DrmDeviceSpec`]):
///   * empty / unset — auto-discover by iterating `/dev/dri/card*` and
///     picking the first card with a connected connector
///   * `/dev/dri/cardN` — use that path directly
///   * `pci:DDDD:BB:DD.F` — resolve the BDF via `/sys/class/drm`
///
/// The resolver retries on `NotFound`-class errors for up to
/// [`DRM_RESOLVE_DEADLINE`] (10 s) to absorb the initramfs-side race
/// where halmasuit's unit may fire before udev finishes creating the
/// device node for a freshly-loaded `nvidia_drm` module.
fn drm_device_path_from_env() -> io::Result<Option<PathBuf>> {
    if std::env::var_os("HALMASUIT_SKIP_DRM_MASTER").is_some() {
        if nix::unistd::geteuid().is_root() {
            return Err(io::Error::other(
                "HALMASUIT_SKIP_DRM_MASTER is the dev/test bypass; refusing \
                 to honor it under euid 0",
            ));
        }
        tracing::warn!(
            "HALMASUIT_SKIP_DRM_MASTER set — not acquiring DRM master. \
             This is the dev/test bypass; production deployments \
             MUST NOT set it."
        );
        return Ok(None);
    }

    let raw = std::env::var_os("HALMASUIT_DRM_DEVICE")
        .and_then(|v| v.into_string().ok())
        .unwrap_or_default();
    // Do NOT include `raw` in the error message — see
    // `drm::DrmDeviceSpecParseError`'s rationale for redacting input
    // out of error displays (env value may someday come from a less-
    // trusted source).
    let spec = drm::DrmDeviceSpec::from_env_value(&raw)
        .map_err(|e| io::Error::other(format!("HALMASUIT_DRM_DEVICE rejected: {e}")))?;
    tracing::info!(?spec, "resolving DRM device");
    let path = drm::resolve_drm_device(&spec, DRM_RESOLVE_DEADLINE)?;
    tracing::info!(?path, "DRM device resolved");
    Ok(Some(path))
}

/// How long [`drm::resolve_drm_device`] retries on `NotFound`-class
/// errors before giving up. 10 s is comfortably above the observed
/// gnomon race (nvidia_drm module insertion → udev node creation
/// completes within a few hundred ms of `systemd-modules-load.service`
/// deactivating); short enough that a missing GPU fails the boot
/// reasonably fast.
const DRM_RESOLVE_DEADLINE: Duration = Duration::from_secs(10);

/// Resolve compositor uid from env. `HALMASUIT_COMPOSITOR_UID` is
/// the operator's contract: when set to a valid `u32`, halmasuit
/// drops privileges to that uid after binding its sockets. `Ok(None)`
/// means the operator deliberately left it unset (dev mode);
/// `Err(_)` means the env var is present but malformed — which the
/// caller treats as fatal rather than silently falling through to
/// the "stay as current user" warning. The privilege drop is
/// load-bearing per the architecture's threat model; an unparsable
/// value must not be confused with "no value at all."
fn compositor_uid_from_env() -> io::Result<Option<u32>> {
    let Some(raw) = std::env::var_os("HALMASUIT_COMPOSITOR_UID") else {
        return Ok(None);
    };
    let s = raw
        .to_str()
        .ok_or_else(|| io::Error::other("HALMASUIT_COMPOSITOR_UID is not valid UTF-8"))?;
    s.parse::<u32>().map(Some).map_err(|e| {
        io::Error::other(format!(
            "HALMASUIT_COMPOSITOR_UID is not a valid u32 ({s:?}): {e}"
        ))
    })
}

/// Resolve the wallpaper config from environment. Returns `None`
/// when neither `HALMASUIT_WALLPAPER_CONFIG` nor `HALMASUIT_WALLPAPER_PATH`
/// is set — non-visual integration tests run without a wallpaper
/// (the legacy clear-only scene); production and visual deployments
/// always set one via `services.halmasuit.wallpaper`.
///
/// Resolution order: `HALMASUIT_WALLPAPER_CONFIG` (JSON file with
/// the full config — needed when a shader declares named uniforms)
/// then `HALMASUIT_WALLPAPER_PATH` (single path with extension
/// inference; shader sources get the default Shadertoy bindings).
///
/// # Errors
///
/// Returns an error when `HALMASUIT_WALLPAPER_CONFIG` is set but
/// the file can't be read or parsed as JSON.
fn wallpaper_config_from_env() -> io::Result<Option<wallpaper::WallpaperConfig>> {
    wallpaper::config::from_env()
}

/// Spawn the greeter binary as a child process running under the
/// greeter system user. Must be called while halmasuit is still
/// root — the child uses `setresuid` between fork and exec.
///
/// The child inherits a minimal env: only what a greetd-protocol
/// greeter actually needs to find halmasuit's sockets and identify
/// itself. The Wayland socket lives at $XDG_RUNTIME_DIR/wayland-0
/// per smithay's convention; `GREETD_SOCK` is the standard env
/// variable greeters look up for the auth socket.
///
/// # Errors
/// Ask the broker to spawn the greeter (Epic #47 R1).
///
/// Replaces the prior in-compositor `spawn_greeter`: the compositor
/// no longer holds CAP_SETUID (it has empty bounding set), so it
/// cannot setresuid to the greeter uid itself. The broker (already
/// root, holds the privileges) does the fork-then-drop; the
/// compositor sends `CompositorToBroker::SpawnGreeter` on a TRANSIENT
/// broker connection and consumes the `BrokerToCompositor::
/// GreeterSpawned { pid }` reply + the pidfd-via-SCM_RIGHTS.
///
/// Sync recv is correct here for the same reason `RequestRootFd`'s
/// initial connect is sync: this only fires at startup + on Revert,
/// not from the live render path; no calloop frame is racing. The
/// broker side also doesn't block — it spawns + replies + closes.
///
/// # Errors
/// Any error from `connect_broker`, `send`, `recv_with_fd`, or a
/// broker reply that doesn't carry the SCM_RIGHTS pidfd attachment.
fn broker_spawn_greeter() -> io::Result<GreeterHandle> {
    use std::os::fd::AsFd;

    use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker};
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    let broker_path = broker_socket_path_from_env();
    let chan = connect_broker(&broker_path)
        .map_err(|e| io::Error::other(format!("connect_broker for SpawnGreeter: {e:?}")))?;
    chan.send(&CompositorToBroker::SpawnGreeter)
        .map_err(|e| io::Error::other(format!("send SpawnGreeter: {e:?}")))?;
    // `recv_with_fd` is MSG_DONTWAIT, so a fresh response isn't ready
    // until the broker has forked + sent. Poll with a generous
    // timeout — broker-side spawn is bounded by `fork() + exec()`
    // plus a few syscalls, well under 5s in practice. Failure to
    // get a reply within this window means the broker died or
    // refused the request (SO_PEERCRED gate, env not configured).
    let mut pollfds = [PollFd::new(chan.as_fd(), PollFlags::POLLIN)];
    let n = poll(
        &mut pollfds,
        PollTimeout::try_from(5_000_i32).unwrap_or(PollTimeout::MAX),
    )
    .map_err(|e| io::Error::other(format!("poll broker reply: {e}")))?;
    if n == 0 {
        return Err(io::Error::other(
            "broker did not reply to SpawnGreeter within 5s",
        ));
    }
    let (frame, pidfd) = chan
        .recv_with_fd()
        .map_err(|e| io::Error::other(format!("recv GreeterSpawned: {e:?}")))?
        .ok_or_else(|| io::Error::other("broker closed without sending GreeterSpawned"))?;
    let pid = match frame {
        BrokerToCompositor::GreeterSpawned { pid } => pid,
        other => {
            return Err(io::Error::other(format!(
                "broker reply was not GreeterSpawned: {other:?}"
            )));
        }
    };
    let pidfd = pidfd
        .ok_or_else(|| io::Error::other("broker GreeterSpawned reply missing SCM_RIGHTS pidfd"))?;
    // i32 → u32: pid is unsigned in this crate's GreeterHandle to
    // match Command::id()'s legacy shape. A negative wire value
    // indicates broker-side narrowing failure (it sends -1 if it
    // couldn't fit the pid in i32). Treat -1 as "broker spawned but
    // could not report pid"; the pidfd is still the authority.
    let pid_u32 = u32::try_from(pid).unwrap_or(0);
    Ok(GreeterHandle {
        pid: pid_u32,
        pidfd,
    })
}

/// Send a signal to a pidfd. Race-free wrt pid reuse — if the
/// task has terminated, returns `ESRCH` regardless of whether
/// the pid has been reused.
///
/// # Errors
/// Any errno from `pidfd_send_signal(2)`.
fn pidfd_send_signal(pidfd: &std::os::fd::OwnedFd, sig: i32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `pidfd_send_signal(2)` reads the fd numerically and
    // an optional `siginfo_t` pointer (we pass NULL — kernel
    // synthesizes the siginfo from the calling context). flags=0.
    #[expect(unsafe_code, reason = "raw pidfd_send_signal syscall via libc")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reap any zombie children with `waitpid(-1, WNOHANG)` in a loop.
/// Called from the SIGCHLD handler: signal delivery is coalesced
/// (multiple children dying between handler runs produce one signal),
/// so a single SIGCHLD may correspond to multiple terminations. Loop
/// until `waitpid` reports no more reapable children. The compositor's
/// only child is its greeter (the session leader is the privileged
/// broker's child — the broker is its sole reaper, Epic R9); without
/// this loop a dead greeter accumulates as a zombie.
/// What a reaped child's pid means relative to the greeter lifecycle.
#[derive(Debug, PartialEq, Eq)]
enum ReapOutcome {
    /// The greeter exited before authentication completed — the wedge
    /// condition (no greeter client, no session, nothing else notices).
    GreeterDiedPreAuth,
    /// The greeter exited after a session start was confirmed
    /// (`session_uid` is set on `spawned`): either the A5 swap gate
    /// SIGKILLed it (both keys in → `GreeterTerminated` emitted) or it
    /// exited on its own in the post-`spawned` window; either way the
    /// expected post-auth zombie, not the pre-auth wedge.
    GreeterDiedExpected,
    /// Not the tracked greeter pid (an already-cleared greeter slot).
    /// The compositor has no other children — the session leader is
    /// the broker's child, never reaped here (Epic R9).
    Other,
}

/// Classify a reaped pid against the tracked greeter pid and the
/// authentication state (`session_uid` is `Some` once
/// `record_session_started` recorded the broker-launched session).
/// Pure so the wedge logic is unit-testable
/// without driving real children through `waitpid`.
fn classify_reaped_child(
    reaped_pid: u32,
    greeter_pid: Option<u32>,
    session_uid: Option<u32>,
) -> ReapOutcome {
    if greeter_pid != Some(reaped_pid) {
        return ReapOutcome::Other;
    }
    if session_uid.is_none() {
        ReapOutcome::GreeterDiedPreAuth
    } else {
        ReapOutcome::GreeterDiedExpected
    }
}

/// Epic #47 R2 graceful shutdown.
///
/// SIGKILL the greeter (if alive), composite wallpaper-only (no
/// greeter, no session toplevel), emit `Event::Shutdown`. The main
/// loop KEEPS RUNNING — halmasuit stays alive painting the wallpaper
/// through systemd-shutdown's kill spree and through the rootfs→
/// shutdownRamfs pivot, until the kernel halts the process. This is
/// the production form of what halmasuit-shutdown-probe-phase{0,1,2}
/// empirically validated; the `SurviveFinalKillSignal=yes` unit
/// directive + the binary's presence in `systemd.shutdownRamfs.
/// storePaths` are what let the same PID + same DRM master survive
/// the pivot.
///
/// The render path observes `state.shutting_down` to suppress any
/// greeter/session input (R2.2); the wallpaper plane is composited
/// from frame 0 by the existing `render_layer_elements` path; setting
/// `state.greeter = None` and `state.foreground_toplevel = None`
/// means the next render has no non-wallpaper elements, so the
/// screen is wallpaper-only from this point until kernel halt.
fn graceful_shutdown(state: &mut HalmasuitState, reason: ShutdownReason) {
    // Idempotent on re-entry. PrepareForShutdown triggers graceful_shutdown
    // first (~0-1 s before the kill spree); the broadcast SIGTERM that
    // follows triggers a second invocation through the signal handler.
    // (`SurviveFinalKillSignal=yes` only exempts the final SIGKILL — the
    // SIGTERM that precedes it is always delivered.) The second call has
    // no work to do: the greeter is already gone, the wallpaper-only
    // recomposite already landed, libinput is already unsubscribed.
    // Bail early.
    if state.shutting_down {
        tracing::info!(
            ?reason,
            "graceful_shutdown re-entry while already shutting down; ignored"
        );
        return;
    }

    if let Some(g) = state.greeter.take() {
        let pid = g.pid;
        match pidfd_send_signal(&g.pidfd, libc::SIGKILL) {
            Ok(()) => emit(&Event::GreeterTerminated { pid }),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    greeter_pid = pid,
                    "graceful_shutdown: pidfd_send_signal greeter SIGKILL failed"
                );
            }
        }
    }
    // Drop the foreground toplevel reference so the wallpaper plane
    // composites alone. The session leader's lifecycle is the broker's
    // problem (it's the sole reaper); we just stop showing the
    // surface.
    state.foreground_toplevel = None;
    state.foreground = halmasuit_introspect::Foreground::Greeter;

    // Recomposite immediately so the wallpaper-only state is visible
    // this frame, not via a stale intermediate. From this point on the
    // device is NOT paused — the existing per-VBlank render loop keeps
    // driving new frames into the wallpaper plane through the entire
    // shutdown sequence. That's load-bearing for video and shader
    // wallpapers: they need continuous CPU/GPU work to animate, and
    // pausing the DrmDevice (the previous R2.4 behavior) froze them on
    // the last-flipped framebuffer. With the device left active, image
    // wallpapers look identical (the same composited frame), shaders
    // keep animating, and videos keep decoding all the way until the
    // kernel halts the process.
    //
    // The trade-off: smithay's render path may surface transient
    // errors during the shutdown handoff (broker dbus disconnect,
    // page-flip ioctls colliding with the systemd-shutdown takeover).
    // Those are logged at WARN and ignored — render_one_frame's
    // existing error path drops the failing frame and continues.
    if let Some(backend) = state.drm_backend.as_mut()
        && let Err(e) = backend.render_layer_elements(&state.output, None, HALMASUIT_BRAND_CLEAR)
    {
        tracing::warn!(error = %e, "graceful_shutdown: render_layer_elements failed");
    }

    // R2.2 / R2.3: remove the libinput source from the calloop loop.
    // halmasuit doesn't need libinput post-shutdown (no input routing
    // to a dying session). With R2.3's direct-open libinput
    // (`setup_libinput_direct`), the device fds are owned by the
    // libinput context — removing the calloop source stops dispatch;
    // dropping the source's owner drops the libinput context, which
    // closes the fds.
    if let Some(tok) = state.libinput_token.take() {
        state.loop_handle.remove(tok);
    }

    emit(&Event::Shutdown { reason });
    state.shutting_down = true;

    // R2.2 liveness signal: write one kmsg line on entry so that
    // tests which deliver SIGTERM directly (visual-shutdown-tear-down
    // sends `kill -TERM`, not the SurviveFinalKillSignal-blocked
    // shutdown spree) can observe the transition immediately. The
    // periodic liveness cadence is driven by the always-on
    // `HALMASUIT_LIVENESS_INTERVAL_MS` timer registered at startup —
    // that one fires regardless of whether the SIGTERM signal handler
    // ran, which is what's needed under SurviveFinalKillSignal=yes
    // where SIGTERM is never delivered.
    let pid = std::process::id();
    let frames = state
        .drm_backend
        .as_ref()
        .map_or(0, drm::DrmBackend::frame_counter);
    write_kmsg(&format!(
        "halmasuit-shutdown-liveness pid={pid} frames={frames} init"
    ));
}

/// Best-effort write of a liveness line. The shutdown-liveness timer
/// hits this every 250ms while halmasuit is in the post-SIGTERM
/// wallpaper-only mode. The NixOS module pins halmasuit's stdout to
/// `StandardOutput=journal+kmsg`, so systemd opens fd 1 against
/// /dev/kmsg pre-exec (bypassing the compositor's `ProtectKernelLogs`
/// seccomp filter, which only blocks the compositor from opening it
/// itself). The inherited fd is a character-device handle owned by
/// the kernel, so writes survive the rootfs→shutdownRamfs pivot —
/// post-pivot lines still land on the kernel ring buffer + serial
/// console, where the pivot-survival VM test reads them back.
/// A stderr wrapper that silently swallows write errors. tracing_subscriber's
/// fmt layer ultimately routes through `_eprint`, which **panics** when its
/// write returns `Err` ("failed printing to stderr"). During shutdown,
/// systemd-journald is killed mid-spree and halmasuit's `StandardOutput=kmsg`
/// pipe breaks; the next `tracing::info!`/`warn!` panics, the default panic
/// handler ALSO writes to stderr, that ALSO panics, and Rust's runtime
/// resolves "panic while panicking" by calling `std::process::abort()` —
/// which is exactly the SIGABRT this whole investigation was chasing. The
/// fix is to never let a write error propagate out of the tracing writer.
///
/// `make_shutdown_safe_stderr` is the MakeWriter the fmt layer calls per
/// event; `ShutdownSafeStderr` is the actual `io::Write` it returns. We
/// take a fresh lock per event (cheap, matches what `io::stderr` does).
struct ShutdownSafeStderr(std::io::StderrLock<'static>);

impl io::Write for ShutdownSafeStderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.0.write(buf);
        // Always claim full success so tracing's `_eprint` path never
        // sees an `Err` and never panics. Bytes that don't reach the
        // kernel ring buffer during shutdown are by definition not
        // recoverable anyway.
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let _ = self.0.flush();
        Ok(())
    }
}

fn make_shutdown_safe_stderr() -> ShutdownSafeStderr {
    ShutdownSafeStderr(io::stderr().lock())
}

/// Write a single line to the kernel ring buffer via halmasuit's
/// stdout. The unit's `StandardOutput=file:/dev/kmsg` arranges for
/// systemd (as PID 1, with the necessary privileges) to open
/// `/dev/kmsg` directly and pass that fd as stdout — bypassing
/// journald entirely. The fd survives `systemd-journald` being
/// killed mid-shutdown, so liveness lines keep landing in dmesg
/// through the rootfs→shutdownRamfs pivot until kernel halt.
///
/// Deliberately ignores write/flush errors. The same double-panic
/// hazard documented on `ShutdownSafeStderr` would fire if these
/// were routed through `tracing::warn!` during shutdown — stderr is
/// the same potentially-broken pipe as stdout in dev environments
/// without `file:/dev/kmsg`, and `_eprint`'s `failed printing to
/// stderr` panic would abort the process. Silently dropping bytes
/// is fine: by the time write_kmsg would fail, there's no surviving
/// log sink to report the failure to anyway.
fn write_kmsg(line: &str) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Spawn a background thread that subscribes to
/// `org.freedesktop.login1.Manager.PrepareForShutdown` and posts each
/// `(true,)` signal into a calloop channel back to the main loop, where
/// the receiving callback triggers `graceful_shutdown`.
///
/// Why a thread + channel and not async-zbus inline: halmasuit's
/// calloop is sync, smithay's reexports of calloop don't enable the
/// `executor` feature, and zbus's `blocking::Connection` is a clean
/// fit. The thread owns a Connection + signal-stream iterator; calloop
/// receives only `bool` payloads. The thread exits naturally when the
/// channel's receiver drops (which happens on compositor shutdown — at
/// which point the system bus is also tearing down, so the iterator's
/// next read would error anyway).
///
/// Failure to connect to the system bus or to subscribe is non-fatal:
/// log + continue. halmasuit still works without PrepareForShutdown
/// — it just doesn't get the graceful shutdown cue under
/// `SurviveFinalKillSignal=yes`, and the wallpaper-only mode never
/// engages. SIGTERM-driven paths (visual-shutdown-tear-down's `kill
/// -TERM` test posture, and any operator-issued `systemctl stop`) are
/// unaffected.
fn spawn_prepare_for_shutdown_watcher(loop_handle: &LoopHandle<'static, HalmasuitState>) {
    let (tx, channel) = calloop::channel::channel::<bool>();
    if let Err(e) = loop_handle.insert_source(channel, |event, (), state: &mut HalmasuitState| {
        let calloop::channel::Event::Msg(arming) = event else {
            return;
        };
        if !arming {
            // `PrepareForShutdown(false)` means a previously-scheduled
            // shutdown was cancelled. halmasuit doesn't currently
            // support resuming from the wallpaper-only state (the
            // DRM device is paused; smithay doesn't have a clean
            // "reverse the tear-down" path here). Cancelling
            // shutdown post-tear-down is a vanishingly rare operator
            // pattern; log it and rely on the operator to restart
            // halmasuit if they really meant to keep the session
            // alive.
            tracing::info!("PrepareForShutdown(false): not unwinding wallpaper-only state");
            return;
        }
        if state.shutting_down {
            // Idempotent: a repeated PrepareForShutdown(true) after we
            // already tore down. systemd-logind broadcasts the signal
            // to all subscribers and can theoretically emit it more
            // than once if a `Reboot()` follows a `PowerOff()`.
            return;
        }
        tracing::info!("PrepareForShutdown(true) received; entering graceful_shutdown");
        graceful_shutdown(state, ShutdownReason::PrepareForShutdown);
    }) {
        tracing::error!(error = %e, "insert PrepareForShutdown calloop channel failed");
        return;
    }

    if let Err(e) = std::thread::Builder::new()
        .name("login1-watcher".to_owned())
        .spawn(move || run_prepare_for_shutdown_loop(&tx))
    {
        tracing::error!(error = %e, "spawn login1-watcher thread failed");
    }
}

/// Blocking inner loop for the login1 watcher thread. Connects to the
/// system bus, builds a signal stream on
/// `org.freedesktop.login1.Manager.PrepareForShutdown`, and forwards
/// each `(arming: bool,)` payload through `tx`. Exits if the connection
/// dies, the bus name vanishes, or `tx` is dropped — all of which are
/// terminal for the watcher's purpose.
fn run_prepare_for_shutdown_loop(tx: &calloop::channel::Sender<bool>) {
    let conn = match zbus::blocking::Connection::system() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "login1-watcher: zbus system() failed; no shutdown cue");
            return;
        }
    };

    // Subscribe to Manager.PrepareForShutdown.  zbus's blocking
    // MatchRule/signal-iterator path is the canonical "watch for a
    // specific signal forever" shape.  See zbus 5.x docs.
    let proxy = match zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "login1-watcher: Proxy::new failed");
            return;
        }
    };

    let iter = match proxy.receive_signal("PrepareForShutdown") {
        Ok(it) => it,
        Err(e) => {
            tracing::warn!(error = %e, "login1-watcher: receive_signal failed");
            return;
        }
    };

    tracing::info!("login1-watcher: subscribed to PrepareForShutdown");

    for msg in iter {
        let arming: bool = match msg.body().deserialize() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "login1-watcher: PrepareForShutdown body decode failed");
                continue;
            }
        };
        if tx.send(arming).is_err() {
            // Receiver gone → compositor is shutting down on its own
            // (or never made it past startup). Nothing left to do.
            tracing::info!("login1-watcher: channel closed; exiting");
            return;
        }
    }
    tracing::info!("login1-watcher: signal stream ended");
}

/// Reap zombie children (coalesced SIGCHLD: one signal may cover
/// several deaths — loop until `waitpid` drains). Beyond reaping,
/// attribute each death: a greeter exit *before* authentication (R4)
/// is surfaced as `GreeterDiedPreAuth` and the now-stale greeter
/// handle cleared, instead of vanishing into a discarded status.
fn reap_zombie_children(state: &mut HalmasuitState) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    loop {
        let status = match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => return,
            Ok(status) => status,
        };
        let reaped_pid = match status {
            WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _) => {
                Some(pid.as_raw().cast_unsigned())
            }
            _ => None,
        };
        match reaped_pid.map(|pid| {
            (
                pid,
                classify_reaped_child(
                    pid,
                    state.greeter.as_ref().map(|g| g.pid),
                    state.session_uid,
                ),
            )
        }) {
            Some((pid, ReapOutcome::GreeterDiedPreAuth)) => {
                state.greeter = None;
                emit(&Event::GreeterDiedPreAuth { pid });
                tracing::warn!(greeter_pid = pid, "greeter exited before authentication");
            }
            Some((pid, ReapOutcome::GreeterDiedExpected)) => {
                state.greeter = None;
                tracing::debug!(?status, greeter_pid = pid, "reaped greeter post-auth");
            }
            _ => tracing::debug!(?status, "reaped child"),
        }
    }
}

/// Drop privileges to the configured compositor uid. The compositor
/// execs NO setuid helper and holds NO PAM handle — it relays auth to
/// the privileged `halmasuit-session` broker (Epic R2/R3/R15). So it
/// retains the absolute minimum: only `CAP_KILL`, to signal its
/// greeter child (which runs under a different uid) on session start.
/// The bounding set is emptied COMPLETELY — neither the compositor
/// nor anything it execs can ever gain a capability. Supplementary
/// groups pinned at unit-startup via systemd `SupplementaryGroups=`
/// are intentionally NOT cleared.
///
/// Order is load-bearing:
///   1. Empty the bounding set entirely. `PR_CAPBSET_DROP` requires
///      `CAP_SETPCAP` in the *effective* set, which is full here
///      (still root, no `setresuid` yet); doing it AFTER `setresuid`
///      would `EPERM` (the kernel clears `effective` on the
///      root → non-root transition). The bounding set constrains only
///      caps *gained later* (execve grants / capset→inheritable); it
///      does not retroactively shrink halmasuit's own permitted set,
///      so emptying it does not affect step 5's `CAP_KILL`.
///   2. `prctl(PR_SET_KEEPCAPS, 1)` — without this, `setresuid`
///      below would clear the permitted capability set entirely.
///      KeepCaps preserves permitted; effective is still cleared
///      and must be rebuilt via `capset` (step 5).
///   3. `setresgid(egid, egid, egid)` — pin all three gid components.
///   4. `setresuid(uid, uid, uid)` — drop uid. All three components
///      set to the same value so the process cannot resurrect root.
///      Permitted caps survive due to KeepCaps; effective caps are
///      kernel-cleared.
///   5. Single `capset(2)` writing both permitted and effective to
///      `{CAP_KILL}` in one syscall. The `caps` crate's high-level
///      `set()` does `capget+capset` per CapSet, so calling it
///      twice would be 4 syscalls; the kernel's `cap_user_data_t`
///      carries effective + permitted + inheritable in one payload
///      that `capset(2)` updates atomically. We bypass `caps::set`
///      for that.
///
/// `SECBIT_NOROOT` / `SECBIT_NO_SETUID_FIXUP` are not set here. With
/// the setuid helper deleted there is no setuid-root execve to reason
/// about, but auditing those secbits against the libseat/DRM path is
/// a dedicated hardening pass, out of the privilege-separation epic's
/// scope (tracked as a follow-up, same as the systemd NNP posture —
/// see nix/module.nix).
fn drop_privileges(uid: u32) -> io::Result<()> {
    use caps::CapSet;
    use nix::unistd::{Gid, Uid, getegid, setresgid, setresuid};

    // Step 1: empty the bounding set ENTIRELY while we're still root
    // with CAP_SETPCAP in effective. Nothing is kept: the compositor
    // execs no setuid helper (Epic R15), so no future execve should
    // ever be permitted to gain a capability. The bounding set is
    // preserved across setresuid/fork/execve and constrains only caps
    // gained later — it never retroactively shrinks halmasuit's own
    // permitted/effective sets, so the runtime `CAP_KILL` raised by
    // step 5's `capset` is unaffected. Doing this here (still root,
    // CAP_SETPCAP in effective) avoids the capset choreography
    // otherwise needed to re-raise CAP_SETPCAP after setresuid clears
    // effective.
    for cap in caps::all() {
        caps::drop(None, CapSet::Bounding, cap)
            .map_err(|e| io::Error::other(format!("bounding drop {cap}: {e}")))?;
    }

    caps::securebits::set_keepcaps(true)
        .map_err(|e| io::Error::other(format!("set_keepcaps(true): {e}")))?;

    // Pin the gid explicitly when HALMASUIT_COMPOSITOR_GID is set;
    // fall back to getegid() otherwise. The env-pin is load-bearing
    // for the fromInitrd deployment: initramfs systemd has no NSS
    // (the user/group database auto-creation runs in stage 2
    // activation, long after this unit starts), so `Group=` on the
    // unit fails 216/GROUP for both name and numeric forms. Without
    // this env-driven path the compositor inherits PID1's gid 0
    // and would post-drop run as `compositorUid:0` (root group) —
    // the gid half of the privilege drop would silently regress.
    let target_gid = match std::env::var("HALMASUIT_COMPOSITOR_GID") {
        Ok(s) => match s.parse::<u32>() {
            Ok(0) => {
                // Defense-in-depth gid floor mirroring the existing
                // Nix-side `cfg.compositorUid != 0` assertion. The
                // module sources HALMASUIT_COMPOSITOR_GID from the
                // greeter group's gid (non-zero by default), but an
                // env-override misconfig — or a hostile reuse of the
                // unit's environment — could otherwise land the
                // compositor at gid 0 (root group) after the drop.
                // The CLAUDE.md hard rule about UID_MIN ≥1000 is
                // scoped to the broker's session-leader child; this
                // is the symmetric in-binary floor for the
                // compositor process itself.
                return Err(io::Error::other(
                    "HALMASUIT_COMPOSITOR_GID=0 refused: \
                     the compositor must not run with the root group",
                ));
            }
            Ok(g) => Gid::from_raw(g),
            Err(e) => {
                return Err(io::Error::other(format!(
                    "HALMASUIT_COMPOSITOR_GID={s:?} is not a u32: {e}"
                )));
            }
        },
        Err(_) => getegid(),
    };
    setresgid(target_gid, target_gid, target_gid)
        .map_err(|e| io::Error::other(format!("setresgid({target_gid}): {e}")))?;

    let u = Uid::from_raw(uid);
    setresuid(u, u, u).map_err(|e| io::Error::other(format!("setresuid({u}): {e}")))?;

    capset_permitted_effective_cap_kill()
        .map_err(|e| io::Error::other(format!("capset permitted=effective={{CAP_KILL}}: {e}")))?;

    Ok(())
}

/// Single `capset(2)` writing the calling thread's permitted +
/// effective sets to `{CAP_KILL}` and clearing inheritable.
/// Encapsulates the raw FFI so the caller stays unsafe-free.
///
/// # Errors
/// Any errno from `capset(2)`.
fn capset_permitted_effective_cap_kill() -> io::Result<()> {
    // _LINUX_CAPABILITY_VERSION_3 — the only version still supported
    // by recent kernels. CAP_KILL = 5 per uapi/linux/capability.h.
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const CAP_KILL_BIT: u32 = 1 << 5;

    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0, // 0 ⇒ calling thread
    };
    // _V3 takes a [CapData; 2] (low 32 + high 32 bits of the cap
    // bitmask). CAP_KILL fits in the low half; high half stays zero.
    let data = [
        CapData {
            effective: CAP_KILL_BIT,
            permitted: CAP_KILL_BIT,
            inheritable: 0,
        },
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];

    // SAFETY: `capset(2)` reads `header` (one CapHeader) and `data`
    // (two CapData entries) by pointer. Both are valid stack
    // allocations of the correct repr(C) layout for kernel
    // `__user_cap_header_struct` / `__user_cap_data_struct`.
    // Returns 0 on success, -1 with errno on failure.
    #[expect(unsafe_code, reason = "raw capset syscall via libc")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capset,
            std::ptr::addr_of_mut!(header),
            data.as_ptr(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether a peer that just connected to the Wayland socket is
/// authorised, given its SO_PEERCRED `peer_uid`.
///
/// Epic #1 R1. The socket is chmod 0660 to the `halmasuit-greeter`
/// group so the greeter/session can `connect(2)` — but group
/// membership is not identity. A peer is authorised iff its uid is
/// the greeter uid (pre-auth, the only client) or the authenticated
/// session uid (post-auth, recorded by [`start_session`]). The
/// greeter uid stays valid post-auth because greeter teardown can
/// race the session connect. Mirrors the greetd listener's uid check.
fn wayland_peer_authorized(peer_uid: u32, greeter_uid: u32, session_uid: Option<u32>) -> bool {
    peer_uid == greeter_uid || session_uid == Some(peer_uid)
}

// ── Wayland event-loop integration ──────────────────────────────────────

/// calloop callback for the wayland Display source. The Display is
/// owned by calloop's `Generic` wrapper (`NoIoDrop`); accessing the
/// inner value requires an unsafe call to `get_mut`.
#[expect(
    unsafe_code,
    reason = "calloop's NoIoDrop<Display>::get_mut is unsafe to prevent accidentally dropping the wrapped fd; the callback never drops display"
)]
fn dispatch_display(
    _: calloop::Readiness,
    display: &mut calloop::generic::NoIoDrop<Display<HalmasuitState>>,
    state: &mut HalmasuitState,
) -> Result<PostAction, io::Error> {
    // SAFETY: we never drop the display from this callback; calloop
    // owns the fd for the lifetime of the source.
    unsafe {
        display
            .get_mut()
            .dispatch_clients(state)
            .map_err(io::Error::other)?;
    }
    Ok(PostAction::Continue)
}

smithay::delegate_compositor!(HalmasuitState);
smithay::delegate_xdg_shell!(HalmasuitState);
smithay::delegate_seat!(HalmasuitState);
smithay::delegate_output!(HalmasuitState);
smithay::delegate_shm!(HalmasuitState);

/// Run one main-loop iteration behind a fault boundary (Epic #1 R5).
///
/// `body` is the dispatch+flush step. A `?`-propagated `Err` or a
/// panic in any calloop callback would otherwise unwind out of
/// `main()` → non-zero exit → systemd `Restart=on-failure` → DRM
/// master re-acquired → the visible flash this project exists to
/// delete. So neither escapes: an `Err` is logged and the loop
/// continues; a panic is caught and the loop continues.
///
/// `AssertUnwindSafe` is deliberate. The body borrows `&mut state`,
/// which is not `UnwindSafe`, so a caught panic may leave compositor
/// state inconsistent. Per R5 the no-flash invariant outranks a
/// possibly-degraded session: staying alive degraded beats a
/// guaranteed flash, and clean re-exec recovery is Phase B's job, not
/// a crash-loop's. There is intentionally no retry cap that
/// eventually exits — that would reintroduce the flash. An unbroken
/// error loop is still not a flash.
///
/// The degraded-iteration log is rate-limited. calloop's dispatch
/// timeout is an upper bound, not a floor: a persistently-ready
/// faulted fd (e.g. a Wayland client fd stuck at HUP) returns from
/// `dispatch` immediately every iteration, so the loop spins at CPU
/// speed. Logging once per iteration would reproduce the prior
/// broken-pipe pathology (~237k lines in ~535s). `consecutive_errors`
/// (owned by the caller, reset on the first clean iteration) gates
/// the log to the first few failures plus power-of-two boundaries —
/// O(log n) lines for n failures — and emits a one-line recovery
/// summary when the fault clears.
fn run_loop_iteration(consecutive_errors: &mut u32, body: impl FnOnce() -> io::Result<()>) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(())) => {
            if *consecutive_errors != 0 {
                tracing::error!(
                    suppressed = *consecutive_errors,
                    "event-loop recovered after degraded iterations"
                );
                *consecutive_errors = 0;
            }
        }
        Ok(Err(e)) => {
            *consecutive_errors += 1;
            if should_log_degraded(*consecutive_errors) {
                tracing::error!(
                    error = %e,
                    consecutive = *consecutive_errors,
                    "event-loop dispatch error; degrading in place — \
                     process exit would re-acquire DRM master and flash"
                );
            }
        }
        Err(_panic) => {
            *consecutive_errors += 1;
            if should_log_degraded(*consecutive_errors) {
                tracing::error!(
                    consecutive = *consecutive_errors,
                    "event-loop iteration panicked; degrading in place — \
                     process exit would re-acquire DRM master and flash"
                );
            }
        }
    }
}

/// Whether the `n`-th consecutive degraded iteration should be logged.
/// First five, then power-of-two boundaries: O(log n) lines for a
/// persistent fault instead of one-per-iteration, while still
/// surfacing the fault promptly and marking escalation.
const fn should_log_degraded(n: u32) -> bool {
    n <= 5 || n.is_power_of_two()
}

/// DRM vblank / error handler invoked by the calloop source registered
/// inside `drm::setup_drm_backend` and `drm::setup_drm_direct`. Lifted
/// to a free function so both DRM-bootstrap paths share the exact same
/// per-frame logic (R5 popup cleanup, R2 frame callbacks, R9
/// `wp_presentation_feedback`) — the libseat and direct-open paths
/// differ only in fd acquisition.
fn handle_drm_event(
    event: smithay::backend::drm::DrmEvent,
    _meta: &mut Option<smithay::backend::drm::DrmEventMetadata>,
    state: &mut HalmasuitState,
) {
    // Epic #47 R2.2: once `graceful_shutdown` has run, halmasuit is in
    // the wallpaper-only paint-until-halt state. The last frame is
    // already scanned out and the kernel keeps refreshing it from the
    // bound framebuffer (probe phase 2 validated this). Skipping all
    // per-VBlank ioctls + per-surface presentation-feedback + layer-
    // map walks avoids interacting with any of the subsystems
    // systemd-shutdown is concurrently dismantling (libseat, dbus,
    // greeter/session client trees), which is where the post-SIGTERM
    // unwrap panics that broke an earlier iteration of the
    // pivot-survival test originated.
    if state.shutting_down {
        return;
    }
    match event {
        smithay::backend::drm::DrmEvent::VBlank(_crtc) => {
            // R5 (convergence epic): release dead popups before any
            // per-frame work — once per VBlank is the smithay-
            // recommended cadence and keeps the popup tree from
            // accumulating zombies between commits.
            state.popups.cleanup();
            if let Some(backend) = state.drm_backend.as_mut()
                && let Err(e) = backend.frame_submitted()
            {
                tracing::warn!(error = %e, "DRM frame_submitted failed");
            }
            // R2 (convergence epic): post-present frame-callback
            // emission. Wayland spec Appendix A `wl_surface::frame`
            // requires the server to notify clients when it's a good
            // time to draw the next frame; Mesa's
            // `dri2_wl_surface_throttle` (in `libEGL_mesa`) blocks
            // `eglSwapBuffers` until that callback arrives, so a
            // server that never fires wedges every EGL client on its
            // second swap. Walk the surfaces visible on the just-
            // presented output and send via smithay's canonical
            // helpers; explicit clones drop the `LayerMap` guard
            // before send_frame iterates.
            let time = state.start_time.elapsed();
            // P2 (review-round-3): the per-VBlank allocations below
            // are bounded by design:
            //  - `Vec<LayerSurface>::collect()` size ≤ 4 (Layer enum
            //    cardinality: Background / Bottom / Top / Overlay).
            //    One heap alloc / 16 ms.
            //  - `output.clone()` is an Arc bump (~50 ns). Per-surface
            //    invocations inside the closures below are bounded by
            //    the foreground tree size; smithay's `Output` is
            //    internally Arc-counted.
            // Explicit collect-then-iter drops the LayerMap guard
            // before `send_frame` runs (re-entering the LayerMap
            // inside send_frame would deadlock).
            let output = state.output.clone();
            let layers: Vec<smithay::desktop::LayerSurface> =
                smithay::desktop::layer_map_for_output(&output)
                    .layers()
                    .cloned()
                    .collect();
            for layer in &layers {
                layer.send_frame(&output, time, None, |_, _| Some(output.clone()));
            }
            if let Some(toplevel) = state.foreground_toplevel.as_ref() {
                smithay::desktop::utils::send_frames_surface_tree(
                    toplevel.wl_surface(),
                    &output,
                    time,
                    None,
                    |_, _| Some(output.clone()),
                );
            }

            // R9 (convergence): emit wp_presentation_feedback.presented
            // for any surface that requested it on its last commit.
            // smithay's `take_presentation_feedback_surface_tree` walks
            // the cached feedback per surface; we drive it for the
            // foreground tree + each layer. The
            // primary-scanout-output closure unconditionally returns
            // the single output (halmasuit hosts one output and every
            // visible surface is on it).
            let kind = smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync;
            let mut feedback = smithay::desktop::utils::OutputPresentationFeedback::new(&output);
            if let Some(toplevel) = state.foreground_toplevel.as_ref() {
                smithay::desktop::utils::take_presentation_feedback_surface_tree(
                    toplevel.wl_surface(),
                    &mut feedback,
                    |_, _| Some(output.clone()),
                    |_, _| kind,
                );
            }
            for layer in &layers {
                smithay::desktop::utils::take_presentation_feedback_surface_tree(
                    layer.wl_surface(),
                    &mut feedback,
                    |_, _| Some(output.clone()),
                    |_, _| kind,
                );
            }
            let refresh = smithay::wayland::presentation::Refresh::fixed(state.refresh_period);
            state.presentation_seq = state.presentation_seq.wrapping_add(1);
            feedback.presented::<_, smithay::utils::Monotonic>(
                time,
                refresh,
                state.presentation_seq,
                smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
            );
        }
        smithay::backend::drm::DrmEvent::Error(e) => {
            tracing::warn!(error = %e, "DRM device error");
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "main() is the wiring point — splitting smithay+greetd+calloop \
              setup into helpers obscures the sequential boot story without \
              meaningfully helping reviewers"
)]
fn main() -> io::Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_writer(make_shutdown_safe_stderr)
        .with_env_filter(env_filter)
        .init();

    emit(&Event::Started {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
    });

    // Phase B (boot-from-initrd) runtime detection. `/etc/initrd-release`
    // is the systemd-canonical signal that we're in the initramfs
    // (INITRD_INTERFACE spec); the rootfs filesystem doesn't ship the
    // file. `initrd-switch-root.service` removes it during the pivot.
    //
    // When this is true, halmasuit:
    //   - sets `argv[0][0] = '@'` (defense-in-depth alongside the
    //     unit's `SurviveFinalKillSignal=yes`, per RESEARCH.md
    //     Phase 2);
    //   - opens DRM directly and issues SET_MASTER (no libseat — there
    //     is no seatd in the initramfs);
    //   - skips libinput, greetd, and the greeter (no users yet);
    //   - stays root for its initramfs lifetime; the privilege drop
    //     lands in a follow-up Phase B task that wires the post-pivot
    //     transition. `HALMASUIT_COMPOSITOR_UID` is intentionally not
    //     consulted on this path.
    //   - inserts a calloop poll for `/etc/initrd-release` disappearance
    //     and emits `Phase::RootfsReady` once observed.
    //
    // When this is false, the existing rootfs-only deployment path
    // is unchanged: libseat-brokered DRM, greetd listener, greeter,
    // privilege drop.
    let in_initramfs = context::is_initramfs();
    if in_initramfs {
        context::set_argv0_marker();
        emit(&Event::PhaseEntered {
            phase: Phase::InitramfsInit,
        });
    }

    // Resolve the DRM device path before anything else (the path may
    // be `None` for the dev/test SKIP path). The actual DRM/GBM/EGL/GLES
    // setup happens further down, after the calloop event loop is
    // built — `setup_drm_backend` needs `loop_handle` to wire the
    // page-flip event source.
    let drm_device_path = drm_device_path_from_env()?;
    // The wallpaper plane (epic G1/R3/R6). Constructed once inside
    // `setup_drm_backend`; `is_some()` here also gates the frame-0
    // wallpaper anchor emitted below.
    let wallpaper_config = wallpaper_config_from_env()?;
    let wallpaper_configured = wallpaper_config.is_some();
    // Whether the configured wallpaper needs the periodic wallpaper-engine
    // tick — true for any animated backend (shader, video). Image is
    // static so the tick would be pure waste. This mirrors the runtime
    // `WallpaperBackend::wants_continuous_render` decision, but at the
    // config layer because the timer is registered BEFORE the backend
    // is instantiated. The agreement test in
    // `wallpaper::config::tests::needs_tick_agrees_with_backend_runtime`
    // pins these two views to stay in sync across future variant adds.
    let wallpaper_needs_tick = wallpaper_config
        .as_ref()
        .is_some_and(crate::wallpaper::WallpaperConfig::needs_tick);

    // Initialize the Wayland display + protocol state.
    let display: Display<HalmasuitState> = Display::new().map_err(io::Error::other)?;
    let display_handle = display.handle();
    let compositor_state = CompositorState::new::<HalmasuitState>(&display_handle);
    let xdg_shell_state = XdgShellState::new::<HalmasuitState>(&display_handle);

    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&display_handle, "seat0".to_owned());
    // Keyboard + pointer capabilities. Real events arrive via the
    // libinput backend (inserted on the DRM path below) and are
    // routed to the focused client. XkbConfig::default() is the
    // system default layout; 200ms delay / 25Hz repeat is the
    // conventional wlroots default.
    seat.add_keyboard(smithay::input::keyboard::XkbConfig::default(), 200, 25)
        .map_err(|e| io::Error::other(format!("seat.add_keyboard: {e}")))?;
    seat.add_pointer();
    // wl_touch capability — locked in the 25-protocol scope. libinput
    // touch events route through this handle in `dispatch_libinput`;
    // even on systems without a touch device the capability is
    // advertised, matching wl_seat's contract.
    seat.add_touch();

    let output_manager_state =
        OutputManagerState::new_with_xdg_output::<HalmasuitState>(&display_handle);

    // wl_shm. Empty formats iter requests just ARGB8888 + XRGB8888,
    // which the spec mandates always be advertised. Additional formats
    // come with the renderer task.
    let shm_state = ShmState::new::<HalmasuitState>(&display_handle, std::iter::empty());
    let layer_shell_state = WlrLayerShellState::new::<HalmasuitState>(&display_handle);

    let mut event_loop: EventLoop<HalmasuitState> =
        EventLoop::try_new().map_err(io::Error::other)?;
    let loop_handle = event_loop.handle();

    // Build the DRM backend if a device path was resolved. Two paths
    // (R2.3 collapsed the prior three into two by removing the libseat-
    // brokered branch):
    //
    // 1. `Some(path)`: open the path resolved from `drm_device_path_from_env`
    //    (see `drm::DrmDeviceSpec` for the env contract — Auto-discover
    //    / Path / Pci) directly and issue `SET_MASTER` ourselves (no
    //    libseat, no seatd). Same for both rootfs and initramfs
    //    deployments — halmasuit is a system compositor and holds
    //    master for its entire lifetime. Probes
    //    drm-master-probe-phase{1,2,4} validated that the master
    //    designation survives both `setresuid` and `switch_root`. In
    //    rootfs mode, libinput is enumerated here too via the direct-
    //    open path; in initramfs mode, libinput attaches post-pivot
    //    once `/dev/input/event*` is available on the rootfs.
    // 2. `None`: the dev/test SKIP path — synthesized 1920x1080
    //    placeholder so wl_clients can still discover an output global.
    let (drm_backend, drm_token, output, libinput_token) = if let Some(path) = &drm_device_path {
        let (backend, token, real_output) =
            drm::setup_drm_direct(path, &loop_handle, handle_drm_event, wallpaper_config)?;
        real_output.create_global::<HalmasuitState>(&display_handle);
        emit(&Event::PhaseEntered {
            phase: Phase::DrmMasterAcquired,
        });
        // Rootfs deployment: enumerate input here. Initramfs deployment:
        // `setup_post_pivot_input` runs the same `setup_libinput_direct`
        // call post-pivot. Either way, no libseat: each `/dev/input/
        // event*` is opened directly while halmasuit is still root.
        let libinput_tok = if in_initramfs {
            None
        } else {
            Some(setup_libinput_direct(&loop_handle)?)
        };
        (Some(backend), Some(token), real_output, libinput_tok)
    } else {
        // SKIP path: synthesized placeholder. Geometry is invented;
        // the advertisement exists so clients can discover an output
        // and proceed past their wl_registry phase.
        let output_mode = Mode {
            size: (1920, 1080).into(),
            refresh: 60_000, // 60 Hz, in mHz per the wl_output spec
        };
        let synth = Output::new(
            "output-0".to_owned(),
            PhysicalProperties {
                size: (480, 270).into(), // mm; ~96 DPI assumption
                subpixel: Subpixel::Unknown,
                make: "halmasuit".to_owned(),
                model: "synthesized-1080p".to_owned(),
                serial_number: String::new(),
            },
        );
        synth.create_global::<HalmasuitState>(&display_handle);
        synth.change_current_state(Some(output_mode), None, None, Some((0, 0).into()));
        synth.set_preferred(output_mode);
        (None, None, synth, None)
    };

    // R10 (convergence): zwp_linux_dmabuf_v1 global. Mesa-EGL
    // clients prefer dmabuf over wl_shm — without this global they
    // operate at significantly degraded performance and extra wedge
    // surface area. Format list comes from the renderer's
    // `dmabuf_formats()` when the DRM backend is present; on the
    // SKIP path (no renderer) we don't advertise the global at all
    // (advertising a global we can't serve is the
    // `linux-dmabuf-v1`-lie anti-pattern the epic codifies).
    // Phase B: wp_viewporter — crop+scale-to-output (subsurface
    // scaling, HiDPI fractional-scale composition). Both Qt 6 and
    // GTK 4 bind this; smithay handles all the per-surface logic.
    let viewporter_state =
        smithay::wayland::viewporter::ViewporterState::new::<HalmasuitState>(&display_handle);
    // Phase B: wp_fractional_scale_manager_v1 — HiDPI fractional
    // scale negotiation. Smithay manages per-surface state.
    let fractional_scale_state =
        smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<HalmasuitState>(
            &display_handle,
        );
    // Phase B: wp_single_pixel_buffer_manager_v1 — solid-color
    // buffers without an allocator. GTK 4 uses for backgrounds.
    let single_pixel_buffer_state =
        smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<HalmasuitState>(
            &display_handle,
        );
    // Phase B: zwp_pointer_gestures_v1 — GTK 4 touchpad gestures
    // (swipe / pinch / hold). Smithay manages the per-gesture
    // dispatch from libinput passthrough.
    let pointer_gestures_state = smithay::wayland::pointer_gestures::PointerGesturesState::new::<
        HalmasuitState,
    >(&display_handle);
    // Phase B: zwp_tablet_manager_v2 — stylus / eraser / tablet pad.
    // Smithay manages the per-tool state on the seat.
    let tablet_manager_state = smithay::wayland::tablet_manager::TabletManagerState::new::<
        HalmasuitState,
    >(&display_handle);
    // Phase B: zxdg_decoration_manager_v1 — always answer ServerSide
    // (= "no decoration is server's contribution") for halmasuit's
    // fullscreen toplevel model. Qt avoids double titlebars.
    let xdg_decoration_state = smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<
        HalmasuitState,
    >(&display_handle);
    // Phase B: xdg_activation_v1 — Qt 6 / GTK 4 use this for
    // window activation requests. halmasuit's foreground is
    // greeter→session lifecycle driven; we accept tokens (smithay
    // default) and log+ignore activations.
    let xdg_activation_state = smithay::wayland::xdg_activation::XdgActivationState::new::<
        HalmasuitState,
    >(&display_handle);
    // Phase B: zwp_idle_inhibit_manager_v1 — GTK 4 binds. No idle
    // behavior in v1; inhibit/uninhibit handlers are no-ops.
    let idle_inhibit_state = smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<
        HalmasuitState,
    >(&display_handle);
    // Phase B: zwp_keyboard_shortcuts_inhibit_manager_v1 — halmasuit
    // has no global keyboard shortcuts; inhibitor requests are
    // tracked but inert.
    let keyboard_shortcuts_inhibit_state =
        smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState::new::<
            HalmasuitState,
        >(&display_handle);
    // Phase B: xdg_foreign_v2 — cross-client toplevel handle
    // export/import. v1 hosts one fullscreen toplevel per phase so
    // embedding is inert; smithay tracks handle bookkeeping.
    let xdg_foreign_state =
        smithay::wayland::xdg_foreign::XdgForeignState::new::<HalmasuitState>(&display_handle);
    // Phase B: xdg_wm_dialog_v1 — GTK 4 / Qt 6 mark dialog toplevels
    // via this. halmasuit takes no action on the hint.
    let xdg_dialog_state = smithay::wayland::shell::xdg::dialog::XdgDialogState::new::<
        HalmasuitState,
    >(&display_handle);
    // Phase B: xdg_toplevel_icon_manager_v1 — toolkits attach icons
    // to toplevels via this protocol. halmasuit shows no titlebar/
    // task list in v1; icon is cached but unused.
    let xdg_toplevel_icon_manager =
        smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager::new::<HalmasuitState>(
            &display_handle,
        );
    // Phase B: wl_data_device_manager — wayland.xml core selection/
    // DnD global. Selection (clipboard) routing between clients is
    // handled internally by smithay. Cross-client DnD is non-
    // existent in halmasuit's single-fullscreen-toplevel-per-phase
    // model; smithay's default `dnd_requested` gracefully cancels
    // the source.
    let data_device_state = smithay::wayland::selection::data_device::DataDeviceState::new::<
        HalmasuitState,
    >(&display_handle);
    // Phase B: zwp_primary_selection_device_manager_v1 — X11-style
    // middle-click primary buffer. Shares the SelectionHandler with
    // data-device (smithay routes via SelectionTarget::Primary).
    let primary_selection_state =
        smithay::wayland::selection::primary_selection::PrimarySelectionState::new::<HalmasuitState>(
            &display_handle,
        );
    // Phase B: zwp_text_input_manager_v3 — Qt-mandatory IME protocol.
    // No input-method-v2 paired (out of scope), so clients bind and
    // track focus but receive no preedit/commit_string events. Per-
    // seat TextInputHandle is lazily inserted into Seat user_data on
    // first GetTextInput request.
    let text_input_manager_state =
        smithay::wayland::text_input::TextInputManagerState::new::<HalmasuitState>(&display_handle);
    // Phase B: wp_cursor_shape_manager_v1 — clients request named
    // cursors instead of attaching their own buffers. Smithay routes
    // set_shape through SeatHandler::cursor_image as
    // CursorImageStatus::Named(CursorIcon); R8b-state already stores
    // it, so this is a pure advertise-and-delegate.
    let cursor_shape_state = smithay::wayland::cursor_shape::CursorShapeManagerState::new::<
        HalmasuitState,
    >(&display_handle);

    // R9 (convergence): wp_presentation global. CLOCK_MONOTONIC is
    // what halmasuit's start_time / VBlank timestamps use, so the
    // client's `presented` event timestamps are directly comparable
    // to its other monotonic samples.
    let presentation_state = smithay::wayland::presentation::PresentationState::new::<HalmasuitState>(
        &display_handle,
        u32::try_from(libc::CLOCK_MONOTONIC)
            .expect("CLOCK_MONOTONIC is c_int=1 on Linux; fits u32"),
    );

    let mut dmabuf_state = smithay::wayland::dmabuf::DmabufState::new();
    let dmabuf_global = drm_backend.as_ref().map(|b| {
        use smithay::backend::renderer::ImportDma;
        let formats: Vec<_> = b.renderer.dmabuf_formats().into_iter().collect();
        tracing::info!(
            count = formats.len(),
            "advertising zwp_linux_dmabuf_v1 with renderer-derived format tranche"
        );
        dmabuf_state.create_global::<HalmasuitState>(&display_handle, formats)
    });

    // Bind the Wayland listening socket. smithay's ListeningSocketSource
    // places the socket at $XDG_RUNTIME_DIR/<name>; production halmasuit's
    // systemd unit sets XDG_RUNTIME_DIR=/run/halmasuit via the NixOS
    // module's RuntimeDirectory + Environment directives.
    let socket = ListeningSocketSource::with_name("wayland-0")
        .map_err(|e| io::Error::other(format!("bind wayland socket: {e}")))?;
    let socket_path = socket.socket_name().to_owned();
    tracing::info!(socket = ?socket_path, "wayland socket bound");

    // Widen the socket perms from smithay's default 0700 to 0660 so
    // the greeter (running as the configured greeter system user, in
    // group `halmasuit-greeter` per the NixOS module) can `connect(2)`.
    // The systemd unit's `Group=halmasuit-greeter` directive gives the
    // socket file group ownership; this chmod opens it to the group.
    // Same shape as the greetd socket bind in `halmasuit-greetd`'s
    // `bind_socket` helper.
    //
    // `socket.socket_name()` returns just the basename (e.g.
    // "wayland-0"); the actual file lives at $XDG_RUNTIME_DIR/<name>.
    //
    // Group ownership: the rootfs `enable` deployment lets systemd's
    // `Group = cfg.greeterGroup` directive set the process's egid
    // before this bind, so the file inherits the right gid. The
    // initramfs Phase B unit can't use `Group=` (no NSS in initramfs;
    // `Group=halmasuit-greeter` fails 216/GROUP). Honour an explicit
    // `HALMASUIT_WAYLAND_GROUP_GID` so the same 0660 protections work
    // cross-deployment: the gid is baked into the module from the
    // greeter group's resolved gid in rootfs config.
    {
        use std::os::unix::fs::PermissionsExt;
        let xdg_runtime_dir =
            std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/run/halmasuit".into());
        let abs_socket_path = PathBuf::from(xdg_runtime_dir).join(&socket_path);
        let perms = std::fs::Permissions::from_mode(0o660);
        std::fs::set_permissions(&abs_socket_path, perms).map_err(|e| {
            io::Error::other(format!("chmod 0660 {}: {e}", abs_socket_path.display()))
        })?;
        apply_wayland_group_gid(&abs_socket_path)?;
    }

    // New-client handler: SO_PEERCRED-gate every accepted stream
    // before handing it to the Display. The 0660 chmod above only
    // restricts to the halmasuit-greeter group; this is the peer
    // *identity* gate (greeter pre-auth / session post-auth) that
    // stops a hostile in-group uid from connecting to wayland-0 to
    // screenshot / inject. Mirrors the greetd listener's uid gate.
    loop_handle
        .insert_source(socket, |stream, (), state: &mut HalmasuitState| {
            let creds = match peer_credentials(&stream) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "peer_credentials failed; dropping wayland connection");
                    drop(stream);
                    return;
                }
            };
            if !wayland_peer_authorized(creds.uid, state.greeter_uid, state.session_uid) {
                tracing::warn!(
                    peer_uid = creds.uid,
                    greeter_uid = state.greeter_uid,
                    session_uid = ?state.session_uid,
                    "rejected wayland connection from unauthorised uid",
                );
                drop(stream);
                return;
            }
            let client_data = Arc::new(ClientState {
                compositor_state: CompositorClientState::default(),
                uid: creds.uid,
            });
            match state.display_handle.insert_client(stream, client_data) {
                Ok(_client) => tracing::debug!("new wl_client accepted"),
                Err(e) => tracing::error!(error = %e, "insert_client failed"),
            }
        })
        .map_err(io::Error::other)?;

    // Signal source. Register BEFORE the first dispatch so a SIGTERM
    // racing startup is still caught.
    //
    // SIGTERM has two distinct meanings in the fromInitrd deployment:
    //   1. During the boot pivot, `initrd-cleanup.service` sends
    //      SIGTERM to every initramfs unit as systemd-shutdown tears
    //      down the initramfs and switches root. halmasuit MUST
    //      survive this (the whole point of the fromInitrd shape) —
    //      its `SurviveFinalKillSignal=yes` directive prevents the
    //      kill spree's SIGKILL from landing, and we ignore the
    //      preceding SIGTERM here.
    //   2. At actual system shutdown, systemd-shutdown sends SIGTERM
    //      to halmasuit again BEFORE pivoting to /run/initramfs/
    //      shutdown. This one drives graceful tear-down: SIGKILL the
    //      greeter (if alive), composite wallpaper-only, exit cleanly.
    //
    // `state.shutdown_armed` distinguishes them: false during the
    // boot pivot window (until `Phase::RootfsReady` fires the flip),
    // true thereafter. In rootfs-only (`services.halmasuit.enable=
    // true`) mode the flag starts true at construction — no boot
    // pivot to survive.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGCHLD])?;
    loop_handle
        .insert_source(
            signals,
            move |event, (), state: &mut HalmasuitState| match event.signal() {
                Signal::SIGCHLD => reap_zombie_children(state),
                Signal::SIGTERM if !state.shutdown_armed => {
                    tracing::info!("SIGTERM ignored (shutdown_armed=false, boot pivot in flight)");
                }
                sig => {
                    let reason = match sig {
                        Signal::SIGTERM => ShutdownReason::SignalTerm,
                        Signal::SIGINT => ShutdownReason::SignalInt,
                        _ => ShutdownReason::Internal,
                    };
                    graceful_shutdown(state, reason);
                }
            },
        )
        .map_err(io::Error::other)?;

    emit(&Event::PhaseEntered { phase: Phase::Init });
    emit(&Event::PhaseEntered {
        phase: Phase::WaylandReady,
    });

    // Wrap the Display as a calloop Generic source so client fd
    // activity (new requests on connected clients) wakes the event
    // loop. Without this, dispatch_clients only runs when something
    // else fires the loop, and connected clients hang. (smithay's
    // anvil example uses the same pattern.)
    loop_handle
        .insert_source(
            Generic::new(display, Interest::READ, CalloopMode::Level),
            dispatch_display,
        )
        .map_err(io::Error::other)?;

    // R5 fallback: PopupManager cleanup runs from the DRM VBlank
    // handler at ~60Hz on the production path. The SKIP/no-DRM test
    // bypass has no VBlank source, so without this timer popups
    // would accumulate across the compositor's lifetime if a client
    // creates+disconnects without destroying them. The timer is
    // always-armed (production AND SKIP); on production the VBlank
    // path still does the dominant per-frame cleanup, this timer is
    // a coarse safety net. 1 s cadence is well below any visible-
    // popup lifetime; cleanup() over an empty tree is O(1).
    loop_handle
        .insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            |_deadline, (), state: &mut HalmasuitState| {
                state.popups.cleanup();
                TimeoutAction::ToDuration(Duration::from_secs(1))
            },
        )
        .map_err(|e| io::Error::other(format!("insert popup-cleanup timer: {e}")))?;

    // Epic #47 R2.2 liveness timer.
    //
    // systemd-shutdown's `SurviveFinalKillSignal=yes` semantics
    // SUPPRESS the SIGTERM that would otherwise trigger
    // `graceful_shutdown` — protected units never see it. Without
    // that signal halmasuit can't infer "shutdown is happening" and
    // can't switch into a faster liveness cadence, so we keep one
    // always-running calloop timer that writes
    // `halmasuit-shutdown-liveness pid=N` to stdout (routed to
    // /dev/kmsg by `StandardOutput=kmsg`). The kmsg writes survive
    // the rootfs→shutdownRamfs pivot because /dev/kmsg is a kernel-
    // owned character device that doesn't go away with the rootfs
    // unmount.
    //
    // The cadence is gated on the `HALMASUIT_LIVENESS_INTERVAL_MS`
    // environment variable: unset (the production default), the
    // timer is not registered at all and there is no kmsg noise.
    // The VM pivot-survival test sets it to a small value (25 ms,
    // well below the ~50 ms window between post-pivot pivot-marker
    // and kernel power-off) so it can reliably observe at least one
    // liveness line post-pivot.
    if let Ok(interval_ms_str) = std::env::var("HALMASUIT_LIVENESS_INTERVAL_MS")
        && let Ok(interval_ms) = interval_ms_str.parse::<u64>()
        && interval_ms > 0
    {
        let interval = Duration::from_millis(interval_ms);
        let pid = std::process::id();
        loop_handle
            .insert_source(
                Timer::from_duration(interval),
                move |_deadline, (), state: &mut HalmasuitState| {
                    // `frames=N` is the always-on render counter from
                    // DrmBackend. Asserting this is INCREASING across
                    // post-pivot liveness lines proves the render loop
                    // is actually advancing — not just that the calloop
                    // liveness timer is firing. (See visual-shutdown-
                    // pivot-survival.nix's frame-progression assertion.)
                    let frames = state
                        .drm_backend
                        .as_ref()
                        .map_or(0, drm::DrmBackend::frame_counter);
                    write_kmsg(&format!(
                        "halmasuit-shutdown-liveness pid={pid} frames={frames}"
                    ));
                    TimeoutAction::ToDuration(interval)
                },
            )
            .map_err(|e| io::Error::other(format!("insert liveness timer: {e}")))?;
    }

    // Epic #47 R2.4: org.freedesktop.login1.PrepareForShutdown(true)
    // subscription. systemd-logind broadcasts this signal at the
    // very start of every shutdown sequence (poweroff, reboot,
    // suspend, hibernate, kexec) — BEFORE the unit-stop kill spree,
    // before `SurviveFinalKillSignal=yes` could possibly become
    // relevant. It's the canonical "shutdown is imminent" cue for
    // long-lived system processes that need to clean up gracefully.
    // halmasuit subscribes to it (via a dedicated zbus-blocking
    // thread that posts into a calloop channel back to the main
    // loop) and treats it as the trigger for `graceful_shutdown` —
    // greeter killed, wallpaper-only recomposite, `DrmDevice::pause`,
    // and the rest of the wallpaper-only paint-until-halt path.
    spawn_prepare_for_shutdown_watcher(&loop_handle);

    // greetd socket setup. Production: NixOS module sets
    // HALMASUIT_GREETD_SOCKET to `/run/halmasuit/greetd.sock` and
    // HALMASUIT_GREETER_UID to the greeter system user's uid.
    // Defaults are test-friendly: socket lives in XDG_RUNTIME_DIR,
    // greeter is the running user.
    //
    // Skipped in the initramfs path — greetd has no role pre-rootfs
    // (no `/etc/passwd`, no PAM stack, no broker yet). The values
    // returned by `setup_greetd_listener` (the listener token,
    // greeter uid, PAM service name) feed `HalmasuitState` fields
    // that are reachable only from greetd-connection callbacks, so
    // populating sentinel placeholders is sound when the listener
    // isn't registered.
    let (greetd_listener_token, greeter_uid, pam_service) = if in_initramfs {
        (None, 0_u32, String::new())
    } else {
        let (token, uid, svc) = setup_greetd_listener(&loop_handle)?;
        (Some(token), uid, svc)
    };

    // Ask the broker to spawn the configured greeter (Epic #47 R1).
    // The broker is the policy authority — it reads
    // HALMASUIT_GREETER_COMMAND/UID/etc. from its own env, forks-
    // then-drops a child to the greeter uid, returns the pidfd via
    // SCM_RIGHTS. Greeter failure logs but doesn't abort halmasuit:
    // operators may run halmasuit without a greeter during dev.
    //
    // Skipped in the initramfs path: the greeter is the rootfs
    // login UI; an analogous "halmasuit-luks" prompt for the
    // initramfs path lands as its own task. (The post-pivot
    // `run_post_pivot_setup` does the broker-spawn for fromInitrd.)
    let greeter = if in_initramfs {
        None
    } else {
        match broker_spawn_greeter() {
            Ok(handle) => {
                tracing::info!(greeter_pid = handle.pid, "greeter spawned via broker");
                emit(&Event::GreeterSpawned { pid: handle.pid });
                emit(&Event::ForegroundChanged {
                    to: halmasuit_introspect::Foreground::Greeter,
                });
                Some(handle)
            }
            Err(e) => {
                tracing::error!(error = %e, "broker_spawn_greeter failed");
                None
            }
        }
    };

    // Cache output-mode-derived constants once; the mode is stable
    // across the compositor's lifetime in v1 (no hot-plug), so input
    // and VBlank hot paths read these cached values rather than
    // re-locking `output.current_mode()` per event/frame.
    let output_size = output
        .current_mode()
        .map_or((1280_i32, 800_i32), |m| (m.size.w, m.size.h));
    let refresh_period = std::time::Duration::from_nanos(
        1_000_000_000_000_u64
            / u64::try_from(output.current_mode().map_or(60_000, |m| m.refresh).max(1))
                .expect("refresh clamped to >=1"),
    );

    let mut state = HalmasuitState {
        display_handle,
        compositor_state,
        xdg_shell_state,
        seat_state,
        seat,
        _output_manager_state: output_manager_state,
        output,
        shm_state,
        layer_shell_state,
        dmabuf_state,
        _dmabuf_global: dmabuf_global,
        _presentation_state: presentation_state,
        presentation_seq: 0,
        cursor_status: CursorImageStatus::default_named(),
        frame_pending: false,
        output_size,
        refresh_period,
        _viewporter_state: viewporter_state,
        _fractional_scale_state: fractional_scale_state,
        _single_pixel_buffer_state: single_pixel_buffer_state,
        _pointer_gestures_state: pointer_gestures_state,
        _tablet_manager_state: tablet_manager_state,
        _xdg_decoration_state: xdg_decoration_state,
        xdg_activation_state,
        _idle_inhibit_state: idle_inhibit_state,
        keyboard_shortcuts_inhibit_state,
        xdg_foreign_state,
        _xdg_dialog_state: xdg_dialog_state,
        _xdg_toplevel_icon_manager: xdg_toplevel_icon_manager,
        data_device_state,
        primary_selection_state,
        _text_input_manager_state: text_input_manager_state,
        _cursor_shape_state: cursor_shape_state,
        seen_layer_roles: std::collections::HashSet::new(),
        foreground_toplevel: None,
        popups: PopupManager::default(),
        foreground: halmasuit_introspect::Foreground::Greeter,
        libinput_token,
        loop_handle: loop_handle.clone(),
        connections: HashMap::new(),
        next_conn_id: 0,
        pam_service,
        broker_socket: broker_socket_path_from_env(),
        greeter_uid,
        // Epic #47 R2.1: in rootfs-only mode no boot-pivot kill spree
        // exists, so SIGTERM should drive graceful shutdown from the
        // start. In fromInitrd mode the pivot-poll calloop source flips
        // this true on `Phase::RootfsReady` — before that, SIGTERM is
        // the boot kill spree and stays ignored.
        shutdown_armed: !in_initramfs,
        shutting_down: false,
        greetd_listener_token,
        drm_backend,
        _drm_token: drm_token,
        greeter,
        session_uid: None,
        swap: swap_gate::SwapGate::new(),
        session_first_frame_emitted: false,
        start_time: std::time::Instant::now(),
    };

    // The wallpaper plane is composited from frame 0 (epic G1/R3/R6):
    // emit the `Wallpaper` first-frame anchor BEFORE the initial
    // render so `assert_no_flash_stream` counts frame 0's audit as
    // post-wallpaper (it must already be wallpaper-covered — there is
    // no pre-client solid phase). The `seen_layer_roles` guard makes
    // this the single `ClientFirstFrame{Wallpaper}` for the episode;
    // a later real wallpaper-layer client cannot emit a second one
    // (the no-flash invariant requires exactly one). Skipped when no
    // wallpaper is configured (non-visual integration tests).
    if wallpaper_configured
        && state
            .seen_layer_roles
            .insert(halmasuit_introspect::LayerRole::Wallpaper)
    {
        emit(&Event::ClientFirstFrame {
            role: halmasuit_introspect::LayerRole::Wallpaper,
        });
    }

    // Kick off the render loop with one initial frame. The page-flip
    // for this frame triggers the next vblank, which our DRM event
    // handler observes (`frame_submitted`) — that's the keepalive for
    // the render loop. Subsequent damage events (wl_client commits)
    // queue additional frames over the wallpaper plane.
    //
    // `Phase::ScanoutActive` fires here, on the first successful
    // `queue_frame` — "first pixel via GLES" per the epic's IMMUTABLE
    // Requirement #5 semantics. The SKIP-path state (no `drm_backend`)
    // emits neither event.
    if let Some(backend) = state.drm_backend.as_mut() {
        let queued = backend.render_one_frame(&state.output, HALMASUIT_BRAND_CLEAR)?;
        if queued {
            emit(&Event::PhaseEntered {
                phase: Phase::ScanoutActive,
            });
        } else {
            tracing::warn!(
                "initial render_frame produced no damage; ScanoutActive deferred until next vblank"
            );
        }
    }

    // frame_audit only: start the D-Bus `Snapshot()` server, handing
    // it a clone of the render loop's snapshot slot. Started before
    // the privilege drop below, so the background thread's bus
    // connection authenticates as the current euid (root in
    // production deploys); the connection persists across the
    // subsequent setresuid. Best-effort — `serve` logs and the
    // thread exits if the bus is unreachable. Absent entirely from
    // the production binary.
    #[cfg(feature = "frame_audit")]
    if let Some(backend) = state.drm_backend.as_ref() {
        dbus::serve(backend.snapshot_handle());
    }

    // Wallpaper-engine background tick: a calloop timer at 100 ms
    // drives [`DrmBackend::tick_wallpaper`] AND
    // [`DrmBackend::wallpaper_wants_continuous`]-gated renders for
    // animated wallpaper backends (shader, video). Three
    // responsibilities:
    // (1) call the active backend's
    //     [`WallpaperBackend::poll_pending`] — `VideoBackend` drains
    //     the decoder's IPC socket independently of the render path;
    //     other backends no-op.
    // (2) check whether the active backend has requested a
    //     fallback swap (e.g. relay-dead after the restart budget
    //     exhausted) and execute it — load-bearing for VM-test
    //     Gate 6 / Epic #12 Req 10's "fallback after N forced
    //     crashes" criterion.
    // (3) for `wants_continuous_render` backends, drive
    //     `render_one_frame` every tick so animation advances even
    //     when no Wayland client commits are firing repaints. THIS
    //     IS LOAD-BEARING through shutdown — once niri exits during
    //     `graceful_shutdown`, there are no client commits, and the
    //     tick is the only path that keeps shader/video frames
    //     advancing all the way to kernel halt (Epic #61 R3.1).
    //
    // For image/no-wallpaper configurations, registering the timer
    // would wake the compositor 10× per second forever for a no-op
    // — preventing deep-idle CPU states on battery-backed hardware.
    // Gate the registration on `wallpaper_needs_tick`, which mirrors
    // `WallpaperBackend::wants_continuous_render` at config-time.
    //
    // 100 ms is a deliberate compromise: low enough to bound
    // crash-recovery latency below human-perceptible levels and to
    // animate a shader at 10 fps (visible motion), high enough that
    // an idle compositor stays mostly asleep when the wallpaper is
    // static. Frame-delivery latency for active playback is
    // unaffected because `render_element` ALSO polls when the render
    // path fires; the timer is the keepalive for periods when the
    // render loop has stopped (wallpaper content stabilized → no new
    // vblanks).
    //
    // Wraps around forever via `TimeoutAction::ToDuration(period)`.
    if wallpaper_needs_tick {
        let wallpaper_tick = calloop::timer::Timer::immediate();
        loop_handle
            .insert_source(
                wallpaper_tick,
                |_deadline, &mut (), state: &mut HalmasuitState| {
                    // `DrmBackend::tick_wallpaper` returns a
                    // [`WallpaperTickAction`] encoding both the fallback-
                    // swap state-machine step AND whether the backend
                    // wants a per-tick render. Match on it and render if
                    // the action is non-Idle (Render{Continuous,AndSwapped}).
                    // Static backends (image) return `Idle` — the kernel
                    // keeps scanning out the last-flipped framebuffer
                    // and we don't waste GLES draw calls. Animated
                    // backends (shader, video) return `RenderContinuous`
                    // every tick — shader's `iTime` advances + video's
                    // decoder frames reach the screen until kernel halt.
                    if let Some(backend) = state.drm_backend.as_mut() {
                        let action = backend.tick_wallpaper();
                        if action.wants_render()
                            && let Err(e) =
                                backend.render_one_frame(&state.output, HALMASUIT_BRAND_CLEAR)
                        {
                            tracing::warn!(
                                error = %e,
                                ?action,
                                "wallpaper-tick: render failed",
                            );
                        }
                    }
                    calloop::timer::TimeoutAction::ToDuration(Duration::from_millis(100))
                },
            )
            .map_err(|e| io::Error::other(format!("insert wallpaper tick timer: {e}")))?;
    }

    // Privilege drop. The DRM master FD and both Unix sockets are
    // acquired above while we still have euid==0; everything from
    // here onwards runs as the configured compositor system user with
    // no capability the bounding set could grant. The compositor execs
    // no setuid helper after this drop — it relays auth to the
    // privileged halmasuit-session broker (Epic R3/R15).
    //
    // Fail-closed when running as root with no compositor uid
    // configured: a deploy that forgot to set HALMASUIT_COMPOSITOR_UID
    // would otherwise run the entire compositor as root for its
    // lifetime, silently inverting the architecture's whole point.
    // Ad-hoc dev launches (non-root euid) keep the warn-and-continue
    // shape so they can run without the env at all.
    //
    // In the initramfs path the drop is deferred: halmasuit-luks (a
    // follow-up Phase B task) writes to `/run/systemd/ask-password/`,
    // which is root-only; the compositor user doesn't yet exist in
    // `/etc/passwd` (we're pre-pivot — there is no rootfs `/etc`).
    // The post-pivot drop wiring is the task that lands alongside
    // halmasuit-luks.
    if in_initramfs {
        tracing::info!(
            "in_initramfs: skipping privilege drop. Post-pivot drop is wired by a follow-up Phase B task."
        );
    } else {
        match compositor_uid_from_env()? {
            Some(uid) => {
                drop_privileges(uid)?;
                emit(&Event::PhaseEntered {
                    phase: Phase::Deprivileged,
                });
            }
            None if nix::unistd::geteuid().is_root() => {
                return Err(io::Error::other(
                    "refusing to run as root without HALMASUIT_COMPOSITOR_UID; \
                     set services.halmasuit.compositorUid in production",
                ));
            }
            None => {
                tracing::warn!(
                    "HALMASUIT_COMPOSITOR_UID unset; staying as current user. \
                     This is expected in dev launches; production deployments \
                     set it via services.halmasuit.compositorUid."
                );
            }
        }
    }

    // Phase B (boot-from-initrd) pivot-poll. Calloop timer that
    // checks `/etc/initrd-release` every second; when it disappears,
    // `initrd-switch-root.service` has completed the pivot.
    // halmasuit emits `Phase::RootfsReady`, then a follow-up tick
    // (PivotPhase::AwaitingPostPivot) performs the post-pivot
    // sequence: bind the greetd listener, spawn the greeter, drop
    // privileges to the compositor uid. The ordering is identical to
    // the rootfs-only deployment: bind+spawn while root, then drop.
    //
    // `PostPivotInProgress` is the "next tick will run the post-pivot
    // setup" marker; carries an extra 1s delay after `RootfsReady`
    // for halmasuit-luks to wrap up any in-flight prompt and exit.
    //
    // Identical underlying mechanism to drm-master-probe Phase 1,
    // which validated that the same process can span the
    // initramfs→rootfs boundary unmodified. Same PID, same DRM
    // master fd, same Wayland socket — only the filesystem view
    // flipped underneath us.
    //
    // Inserted only when we started in initramfs; the rootfs-only
    // deployment never registers it.
    if in_initramfs {
        // Phase B state machine. Each tick advances at most ONE
        // non-blocking step, then re-arms the timer at an
        // appropriate cadence. Honors the CLAUDE.md hard rule "the
        // compositor never blocks the render/calloop thread on
        // broker IPC" — every prior synchronous send/recv is now
        // driven from the timer callback. The cumulative window
        // (10s connect + 5s recv) is unchanged; the difference is
        // that DRM page-flip events, wallpaper-tick callbacks, and
        // any other calloop source service NORMALLY during the
        // migration. The longest single tick is one connect attempt
        // + one send (both non-blocking on Unix-domain abstract
        // sockets) — fits comfortably under the 16ms dispatch tick.
        enum PivotPhase {
            /// Polling `/etc/initrd-release` for the pivot itself.
            /// Cadence: 1s. Transitions to `Connecting` on disappear.
            Awaiting,
            /// Pivot happened; attempting to reach the broker.
            /// Cadence: exponential backoff 20ms → 200ms cap, 10s
            /// total deadline.
            Connecting {
                deadline: Instant,
                next_delay: Duration,
            },
            /// Broker reached; `RequestRootFd` sent; waiting for the
            /// SCM_RIGHTS reply. Cadence: 50ms, 5s deadline.
            AwaitingReply {
                chan: crate::broker_session::SeqpacketChannel,
                deadline: Instant,
            },
            /// Chroot complete; ready to bind greetd, spawn greeter,
            /// drop. One-shot then Drop.
            Setup,
        }
        let mut phase = PivotPhase::Awaiting;
        loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_secs(1)),
                move |_deadline, (), state: &mut HalmasuitState| match phase {
                    PivotPhase::Awaiting => {
                        if context::is_initramfs() {
                            TimeoutAction::ToDuration(Duration::from_secs(1))
                        } else {
                            emit(&Event::PhaseEntered {
                                phase: Phase::RootfsReady,
                            });
                            // Epic #47 R2.1: the boot pivot is complete.
                            // A SIGTERM from this point is the real
                            // shutdown signal, not the boot kill spree.
                            state.shutdown_armed = true;
                            phase = PivotPhase::Connecting {
                                deadline: Instant::now() + Duration::from_secs(10),
                                next_delay: Duration::from_millis(20),
                            };
                            // Re-arm immediately for the first
                            // connect attempt.
                            TimeoutAction::ToDuration(Duration::from_millis(20))
                        }
                    }
                    PivotPhase::Connecting {
                        deadline,
                        next_delay,
                    } => match try_connect_and_request_root_fd(&state.broker_socket) {
                        Ok(chan) => {
                            phase = PivotPhase::AwaitingReply {
                                chan,
                                deadline: Instant::now() + Duration::from_secs(5),
                            };
                            // Poll for the reply at a constant 50ms.
                            TimeoutAction::ToDuration(Duration::from_millis(50))
                        }
                        Err(e) => {
                            if Instant::now() >= deadline {
                                tracing::error!(
                                    error = %e,
                                    socket = ?state.broker_socket,
                                    "post-pivot: broker connect timed out (10s); skipping migration"
                                );
                                phase = PivotPhase::Setup;
                                // ToDuration(0): calloop schedules the
                                // timer for the NEXT dispatch tick — not
                                // a busy spin. Used at phase-transition
                                // boundaries so the next phase runs
                                // promptly without sleeping. There is no
                                // `TimeoutAction::Immediate` variant in
                                // calloop 0.14; this is the canonical
                                // "fire-next-tick" idiom for this loop.
                                TimeoutAction::ToDuration(Duration::from_millis(0))
                            } else {
                                // Exponential backoff capped at 200ms.
                                let new_delay =
                                    std::cmp::min(next_delay * 2, Duration::from_millis(200));
                                phase = PivotPhase::Connecting {
                                    deadline,
                                    next_delay: new_delay,
                                };
                                TimeoutAction::ToDuration(next_delay)
                            }
                        }
                    },
                    PivotPhase::AwaitingReply { ref chan, deadline } => {
                        match try_recv_root_fd(chan) {
                            Ok(Some(root_fd)) => {
                                if let Err(e) = apply_chroot_to_root_fd(&root_fd) {
                                    tracing::error!(
                                        error = %e,
                                        "post-pivot: chroot failed; skipping migration"
                                    );
                                } else {
                                    tracing::info!(
                                        "post-pivot: chrooted into broker's root (rootfs view)"
                                    );
                                }
                                phase = PivotPhase::Setup;
                                TimeoutAction::ToDuration(Duration::from_millis(0))
                            }
                            Ok(None) => {
                                if Instant::now() >= deadline {
                                    tracing::error!(
                                        "post-pivot: broker root-fd reply timed out (5s); \
                                         skipping migration"
                                    );
                                    phase = PivotPhase::Setup;
                                    TimeoutAction::ToDuration(Duration::from_millis(0))
                                } else {
                                    TimeoutAction::ToDuration(Duration::from_millis(50))
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "post-pivot: broker root-fd migration failed"
                                );
                                phase = PivotPhase::Setup;
                                TimeoutAction::ToDuration(Duration::from_millis(0))
                            }
                        }
                    }
                    PivotPhase::Setup => {
                        if let Err(e) = run_post_pivot_setup(state) {
                            tracing::error!(
                                error = %e,
                                "post-pivot setup failed; halmasuit will stay in pre-greeter state"
                            );
                        }
                        TimeoutAction::Drop
                    }
                },
            )
            .map_err(|e| io::Error::other(format!("insert pivot-poll timer: {e}")))?;
    }

    // Main loop: wait briefly for any source to fire, then flush any
    // pending outgoing events to clients. flush_clients lives on the
    // DisplayHandle (cloned earlier) since Display itself is now owned
    // by the calloop source above.
    // Consecutive failed-iteration counter for the R5 degrade-in-place
    // log rate-limiter; reset on the first clean iteration.
    //
    // Epic #47 R2.2: there is no clean-exit termination. halmasuit
    // paints from initramfs handoff through kernel halt; the SIGTERM/
    // SIGINT path runs `graceful_shutdown` (greeter killed, wallpaper-
    // only recomposite, Event::Shutdown emitted) but the loop keeps
    // dispatching so the wallpaper plane survives systemd-shutdown's
    // rootfs→shutdownRamfs pivot. External SIGKILL or kernel halt is
    // what stops the process.
    let mut consecutive_errors = 0u32;
    loop {
        run_loop_iteration(&mut consecutive_errors, || {
            event_loop
                .dispatch(Some(Duration::from_millis(16)), &mut state)
                .map_err(io::Error::other)?;
            // P1: drain coalesced commit-driven repaints. Many
            // wl_surface.commits in one dispatch cycle (root +
            // subsurfaces + popups) become a single repaint here.
            if state.frame_pending {
                state.frame_pending = false;
                state.repaint();
            }
            state
                .display_handle
                .flush_clients()
                .map_err(io::Error::other)?;
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // R4 port: the SIGCHLD reaper must distinguish which child died.
    // Greeter exit pre-auth (session_uid still None) is the wedge
    // condition that previously vanished into a discarded waitpid
    // status.
    #[test]
    fn classify_greeter_death_pre_auth() {
        assert_eq!(
            classify_reaped_child(4242, Some(4242), None),
            ReapOutcome::GreeterDiedPreAuth
        );
    }

    #[test]
    fn classify_greeter_death_expected_post_auth() {
        assert_eq!(
            classify_reaped_child(4242, Some(4242), Some(1000)),
            ReapOutcome::GreeterDiedExpected
        );
    }

    #[test]
    fn classify_non_greeter_child_is_other() {
        assert_eq!(
            classify_reaped_child(99, Some(4242), None),
            ReapOutcome::Other
        );
        assert_eq!(
            classify_reaped_child(99, Some(4242), Some(1000)),
            ReapOutcome::Other
        );
    }

    #[test]
    fn classify_with_no_greeter_tracked_is_other() {
        assert_eq!(classify_reaped_child(4242, None, None), ReapOutcome::Other);
    }

    // R5 port: neither a dispatch Err nor a callback panic may escape
    // the loop iteration — escaping = process exit = systemd restart =
    // DRM re-acquire = the flash this project exists to delete.
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn run_loop_iteration_ok_runs_body_and_returns() {
        let ran = AtomicUsize::new(0);
        let mut streak = 0u32;
        run_loop_iteration(&mut streak, || {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(streak, 0, "a clean iteration keeps the error streak at 0");
    }

    #[test]
    fn run_loop_iteration_err_does_not_escape() {
        let ran = AtomicUsize::new(0);
        let mut streak = 0u32;
        run_loop_iteration(&mut streak, || {
            ran.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("dispatch blew up"))
        });
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(streak, 1, "an Err iteration increments the error streak");
    }

    #[test]
    fn run_loop_iteration_panic_does_not_escape() {
        let ran = AtomicUsize::new(0);
        let mut streak = 0u32;
        run_loop_iteration(&mut streak, || {
            ran.fetch_add(1, Ordering::SeqCst);
            panic!("calloop callback panicked");
        });
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(
            streak, 1,
            "a panicking iteration increments the error streak"
        );
    }

    #[test]
    fn run_loop_iteration_ok_resets_streak() {
        // A recovered iteration zeroes the streak so the next fault
        // burst logs from the start again (and emits a recovery line).
        let mut streak = 7u32;
        run_loop_iteration(&mut streak, || Ok(()));
        assert_eq!(streak, 0, "recovery resets the consecutive-error streak");
    }

    #[test]
    fn degraded_log_is_rate_limited_under_persistent_fault() {
        // The R5 degrade-in-place path must NOT log once per iteration
        // (a persistently-ready faulted fd spins with no per-iteration
        // delay — that produced the prior ~237k-lines/535s pattern).
        // Log the first few, then only on power-of-two boundaries, so
        // N failures cost O(log N) lines, not O(N).
        assert!(should_log_degraded(1));
        assert!(should_log_degraded(5));
        assert!(
            !should_log_degraded(6),
            "6 is suppressed (not <=5, not pow2)"
        );
        assert!(!should_log_degraded(7));
        assert!(should_log_degraded(8), "powers of two still log (boundary)");
        assert!(!should_log_degraded(1000));
        assert!(should_log_degraded(1024));
        // O(log N) bound: across 1..=1_000_000 failures, far fewer than
        // the prior incident's hundreds-of-lines-per-second.
        let logged = (1u32..=1_000_000)
            .filter(|&n| should_log_degraded(n))
            .count();
        assert!(
            logged < 30,
            "expected O(log N) logged lines over 1e6 failures, got {logged}"
        );
    }

    // R1 port: the Wayland accept path authorises a peer iff its
    // SO_PEERCRED uid is the greeter uid (pre-auth) or the recorded
    // session uid (post-auth). FVC only chmods the socket 0660 — group
    // restriction, not peer identity; this closes that gap.
    #[test]
    fn wayland_peer_greeter_accepted_pre_auth() {
        assert!(wayland_peer_authorized(1000, 1000, None));
    }

    #[test]
    fn wayland_peer_session_accepted_post_auth() {
        assert!(wayland_peer_authorized(1001, 1000, Some(1001)));
    }

    #[test]
    fn wayland_peer_greeter_still_accepted_post_auth() {
        assert!(wayland_peer_authorized(1000, 1000, Some(1001)));
    }

    #[test]
    fn wayland_peer_unrelated_uid_rejected_pre_and_post_auth() {
        assert!(!wayland_peer_authorized(1234, 1000, None));
        assert!(!wayland_peer_authorized(1234, 1000, Some(1001)));
    }

    #[test]
    fn wayland_peer_root_rejected() {
        assert!(!wayland_peer_authorized(0, 1000, Some(1001)));
        assert!(!wayland_peer_authorized(0, 1000, None));
    }
}
