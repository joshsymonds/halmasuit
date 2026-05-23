// halmasuit — Linux system compositor.
//
// v2 Phase A spine. This binary lives from `multi-user.target` to shutdown
// and will host greeter + session as nested wl_clients. Today it brings
// up smithay's Wayland-server event loop, binds a Wayland socket, and
// advertises foundational protocol globals: `wl_compositor`,
// `wl_subcompositor`, `xdg_wm_base`, `wl_seat`, `wl_output`, `wl_shm`.
// Connecting clients can create surfaces, top-levels, software buffers,
// and discover inputs/outputs. Scanout is via a dumb-buffer clear color
// (`#0a0014` brand purple); the GLES + DrmCompositor renderer is a
// subsequent subtask. The advertised wl_output stays at a synthesized
// 1920×1080@60Hz placeholder until smithay's output state is wired to
// real DRM mode info (also a subsequent subtask). Additional globals
// (`linux-dmabuf-v1`, `presentation-time`, `ext-session-lock-v1`, …)
// land later. See ARCHITECTURE.md.

mod broker_relay;
mod broker_session;
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
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
// calloop's `Mode` and smithay's `output::Mode` collide; rename calloop's
// to keep the smithay one as `Mode` (used more often).
use calloop::{
    EventLoop, Interest, LoopHandle, Mode as CalloopMode, PostAction, RegistrationToken,
};
use std::os::fd::{AsRawFd, OwnedFd};

use halmasuit_greetd::server::{SpawnRequest, bind_socket, peer_credentials};

use crate::broker_session::{BrokerEpisode, connect_broker};
use halmasuit_introspect::{Event, Phase, ShutdownReason, emit};
use smithay::backend::input::{Event as InputEventTrait, InputEvent, KeyboardKeyEvent};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::desktop::layer_map_for_output;
use smithay::input::keyboard::FilterResult;
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
struct HalmasuitState {
    running: bool,
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
    /// Layer roles for which `Event::ClientFirstFrame` has already
    /// been emitted (emit-once-per-role). The visual-backdrop
    /// continuity assertion keys off the first
    /// `ClientFirstFrame { role: Background }`.
    seen_layer_roles: std::collections::HashSet<halmasuit_introspect::LayerRole>,
    /// The single fullscreen xdg_toplevel composited above the splash
    /// (greeter/session). v1 is one output, no window management — at
    /// most one toplevel is the foreground.
    foreground_toplevel: Option<ToplevelSurface>,
    /// Which client is the foreground, driven by the greetd lifecycle
    /// (req 17): `Greeter` until `start_session` succeeds, then
    /// `Session`. Gates keyboard focus (greeter layer vs session
    /// toplevel) so focus follows the lifecycle, not connection order.
    foreground: halmasuit_introspect::Foreground,
    /// The libseat session brokering halmasuit's DRM (and, in E2, the
    /// libinput) device fds. Retained for the process lifetime: if it
    /// drops, seatd tears the session down and the brokered fds are
    /// revoked. `None` only on the SKIP (no-DRM/dev) path. Epic
    /// layer E; survival across the privilege drop validated by
    /// drm-master-probe Phase 4.
    _libseat_session: Option<LibSeatSession>,

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
    /// Held so the registered listener token survives for the lifetime
    /// of the compositor; calloop drops the source on shutdown.
    _greetd_listener_token: Option<RegistrationToken>,

    /// The GLES + GBM + DrmCompositor stack. Constructed in `main()`
    /// while still root via [`drm::setup_drm_backend`]; the underlying
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
    /// keyboard-focused client (focus is set by
    /// [`Self::set_keyboard_focus`], driven by the layer-shell
    /// keyboard-interactivity policy in the commit handler — layer F
    /// replaces that with the greeter→session machine). Pointer
    /// capability exists; full pointer routing (focus-follows,
    /// cursor) is layer F's concern, so non-keyboard events are
    /// accepted but not yet dispatched.
    fn dispatch_libinput(&mut self, event: InputEvent<LibinputInputBackend>) {
        if let InputEvent::Keyboard { event } = event {
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
        // Advance smithay's per-surface buffer tracking state. This
        // is what makes committed shm buffers visible to the renderer
        // when WaylandSurfaceRenderElement is built. Without this,
        // render_elements_from_surface_tree sees no current buffer
        // and the surface paints nothing.
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // wlr-layer-shell initial configure. The spec requires the
        // initial configure to be sent in response to the client's
        // first commit (which carries its anchor/exclusive-zone/size
        // requests). `arrange` here therefore sees the committed
        // anchor state — for a fully-anchored background that yields
        // the full output size instead of the half-output fallback
        // `arrange` uses for unanchored zero-size surfaces.
        {
            let mut map = layer_map_for_output(&self.output);
            if let Some(layer) = map
                .layer_for_surface(surface, smithay::desktop::WindowSurfaceType::TOPLEVEL)
                .cloned()
            {
                let initial_configure_sent =
                    smithay::wayland::compositor::with_states(surface, |states| {
                        states
                            .data_map
                            .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceData>()
                            .is_some_and(|d| d.lock().unwrap().initial_configure_sent)
                    });
                map.arrange();
                // Release the LayerMap lock before `send_configure` /
                // the render path below: `render_layer_elements` calls
                // `layer_map_for_output` again on the same output and
                // would re-borrow this same map.
                drop(map);
                if !initial_configure_sent {
                    layer.layer_surface().send_configure();
                }

                // Emit `ClientFirstFrame { role }` once per layer
                // role, the first time a surface of that role has a
                // committed buffer halmasuit will composite. Drives
                // the visual-backdrop continuity assertion (Epic #1
                // req 11). Unconditional (not frame_audit-gated) — a
                // cheap state-transition marker.
                let has_buffer =
                    smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                        s.buffer().is_some()
                    })
                    .unwrap_or(false);
                if has_buffer {
                    let role = match layer.layer() {
                        Layer::Background => halmasuit_introspect::LayerRole::Background,
                        Layer::Bottom => halmasuit_introspect::LayerRole::Bottom,
                        Layer::Top => halmasuit_introspect::LayerRole::Top,
                        Layer::Overlay => halmasuit_introspect::LayerRole::Overlay,
                    };
                    if self.seen_layer_roles.insert(role) {
                        emit(&Event::ClientFirstFrame { role });
                    }
                    // Focus-follows-foreground (req 17): a
                    // keyboard-interactive layer client (the greeter)
                    // gets keyboard focus only while the foreground is
                    // `Greeter`. After `start_session` the foreground
                    // is `Session`, so a lingering/teardown greeter
                    // layer never steals focus from the session.
                    if self.foreground == halmasuit_introspect::Foreground::Greeter
                        && layer.cached_state().keyboard_interactivity
                            != smithay::wayland::shell::wlr_layer::KeyboardInteractivity::None
                    {
                        self.set_keyboard_focus(Some(surface.clone()));
                    }
                }
            }
        }

        // Amendment A5 key 2: the session Wayland client's first
        // committed buffer of non-zero size. The client is identified
        // by its SO_PEERCRED uid == `session_uid` — connection
        // identity is NOT authority (that is PAM-derived, R8), but it
        // IS how the compositor knows WHOSE pixels just arrived.
        // Emit-once; feeds the two-key `SwapGate` (key 1 is the
        // broker's `SessionOpened`). Swapping before this point would
        // show the session "window" before it has painted — the exact
        // flash this project deletes.
        if !self.session_first_frame_emitted
            && let Some(suid) = self.session_uid
            && surface_client_uid(surface) == Some(suid)
        {
            let non_empty =
                smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                    s.buffer_size().is_some_and(|sz| sz.w > 0 && sz.h > 0)
                })
                .unwrap_or(false);
            if non_empty {
                self.session_first_frame_emitted = true;
                emit(&Event::SessionClientFirstFrame);
                let a = self.swap.session_first_frame();
                self.apply_swap_action(a);
            }
        }

        // Focus-follows-foreground (req 17): the session's
        // xdg_toplevel gets keyboard focus on its first buffered
        // commit, but only once the greetd lifecycle has put the
        // foreground in `Session` (set by the A5 swap gate) — never on
        // connection identity alone.
        let fg_surface = self
            .foreground_toplevel
            .as_ref()
            .map(|t| t.wl_surface().clone());
        if self.foreground == halmasuit_introspect::Foreground::Session
            && fg_surface.as_ref() == Some(surface)
        {
            let has_buffer =
                smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
                    s.buffer().is_some()
                })
                .unwrap_or(false);
            if has_buffer {
                self.set_keyboard_focus(Some(surface.clone()));
            }
        }

        // Re-render the scene with the now-current buffers (synchronous
        // one-frame-per-commit; fine for these low-frequency scenes).
        // The foreground toplevel is composited above the layer
        // background by `render_layer_elements`.
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
        // greetd-driven). Configure it to the output mode, fullscreen
        // + activated, so the client paints full-screen immediately.
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
        surface.send_configure();
        tracing::info!(w, h, "xdg_toplevel mapped as fullscreen foreground");
        self.foreground_toplevel = Some(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if self.foreground_toplevel.as_ref() == Some(&surface) {
            // Post-`spawned` the foreground toplevel IS the session
            // client (its toplevel replaced the greeter's in
            // `new_toplevel`); its destruction = the session Wayland
            // client disconnected — Amendment A5.5 revert trigger (the
            // authoritative signal is still the broker's `SessionEnded`
            // frame; whichever arrives first reverts, the gate makes
            // the other inert).
            let was_session = self.session_uid.is_some();
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

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        tracing::debug!("new xdg_popup");
        // Popups need a `send_configure` once we have geometry; for
        // now ack the surface so the client doesn't stall.
        let _ = surface.send_configure();
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
        _positioner: PositionerState,
        token: u32,
    ) {
        // Acknowledge the reposition with the token; real placement comes later.
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

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&Self::KeyboardFocus>) {
        // Focus tracking lands when there are multiple foreground
        // clients to switch between. Phase A hosts at most one
        // wl_client at a time.
    }
}

impl OutputHandler for HalmasuitState {}

impl ShmHandler for HalmasuitState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

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
        // SAFETY: see the #[expect] reason — the fd is live for as long
        // as this source is registered (A8.2 teardown ordering).
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
        // SAFETY: see the #[expect] reason — the fd is live as long as
        // this source is registered (A8.2 teardown ordering).
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
    Ok(Some(std::env::var_os("HALMASUIT_DRM_DEVICE").map_or_else(
        || PathBuf::from("/dev/dri/card0"),
        PathBuf::from,
    )))
}

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

/// Path of the greeter binary to exec at startup. Returns `None` when
/// `HALMASUIT_GREETER_COMMAND` is unset (dev mode — halmasuit runs
/// without a greeter). Production deployments always set it via
/// `services.halmasuit.greeterCommand`.
fn greeter_command_from_env() -> Option<PathBuf> {
    std::env::var_os("HALMASUIT_GREETER_COMMAND").map(PathBuf::from)
}

/// Resolve the wallpaper config from environment. Returns `None`
/// when `HALMASUIT_WALLPAPER_PATH` is unset — non-visual integration
/// tests run without a wallpaper (the legacy clear-only scene);
/// production and visual deployments always set it via
/// `services.halmasuit.wallpaper`. Phase-A: the env var carries a
/// single file path and the backend variant is inferred from the
/// extension (see `wallpaper::config::infer_from_path`); the
/// shader-uniforms task will add a `HALMASUIT_WALLPAPER_CONFIG`
/// TOML companion when richer config (named uniforms, per-monitor)
/// is needed.
fn wallpaper_config_from_env() -> Option<wallpaper::WallpaperConfig> {
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
/// Bubbles passwd-lookup failure, fork failure, or exec failure
/// with context.
fn spawn_greeter(greeter_uid: u32, command: &Path) -> io::Result<GreeterHandle> {
    use nix::unistd::{Gid, Uid, User};
    use std::os::unix::process::CommandExt;

    let user = User::from_uid(Uid::from_raw(greeter_uid))
        .map_err(|e| io::Error::other(format!("getpwuid({greeter_uid}): {e}")))?
        .ok_or_else(|| io::Error::other(format!("no passwd entry for uid {greeter_uid}")))?;

    let gid_raw = user.gid.as_raw();
    let greeter_name = user.name.clone();
    let greeter_home = user.dir;

    let mut cmd = Command::new(command);
    cmd.env_clear()
        .env("USER", &greeter_name)
        .env("LOGNAME", &greeter_name)
        .env("HOME", greeter_home.as_os_str())
        .env("XDG_RUNTIME_DIR", "/run/halmasuit")
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env("GREETD_SOCK", "/run/halmasuit/greetd.sock")
        // PATH so the greeter can exec children (session command,
        // PAM helpers). Match systemd's default unit PATH.
        .env(
            "PATH",
            "/run/wrappers/bin:/run/current-system/sw/bin:/usr/bin:/bin",
        );

    // SAFETY: `pre_exec` runs in the forked child between `fork(2)`
    // and `execve(2)`. The closure must only call async-signal-safe
    // syscalls (man signal-safety(7)). `setgroups`, `setresgid`,
    // and `setresuid` are all on the AS-safe list. We do NOT
    // allocate, log, or take any Rust mutex here.
    let target_gid = Gid::from_raw(gid_raw);
    let target_uid = Uid::from_raw(greeter_uid);
    #[expect(
        unsafe_code,
        reason = "pre_exec runs between fork and exec; closure body is async-signal-safe"
    )]
    unsafe {
        cmd.pre_exec(move || {
            // Restore the default signal mask. The parent (halmasuit)
            // blocks SIGTERM/SIGINT to drive them via calloop's
            // signalfd source; that mask propagates through fork+
            // execve and would leave the greeter unable to receive
            // either signal — systemd's SIGTERM on unit stop is
            // ignored, the cgroup never empties, and the unit ends
            // up in 'failed' state after the final-sigterm timeout.
            let empty = nix::sys::signal::SigSet::empty();
            nix::sys::signal::sigprocmask(
                nix::sys::signal::SigmaskHow::SIG_SETMASK,
                Some(&empty),
                None,
            )?;

            nix::unistd::setgroups(&[target_gid])?;
            nix::unistd::setresgid(target_gid, target_gid, target_gid)?;
            nix::unistd::setresuid(target_uid, target_uid, target_uid)?;
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("spawn greeter {}: {e}", command.display())))?;
    let pid = child.id();
    // Acquire a pidfd for the freshly-spawned child. The pidfd is the
    // race-free signal target — `kill(pid, …)` after SIGCHLD reaps
    // the entry can hit a recycled pid (and with our retained
    // `CAP_KILL`, would signal whichever unrelated process now holds
    // it). pidfd_send_signal targets the original task by fd and
    // returns `ESRCH` once that task has terminated, regardless of
    // pid reuse. There is a tiny window between Command::spawn
    // returning and our pidfd_open here in which the greeter could
    // exit and a new process inherit the pid — for which we
    // tolerate the same risk one round (the next kill returns
    // ESRCH if reused, plus the freshly-opened pidfd is still
    // bound to whatever was at the pid at fd-open time).
    let pidfd =
        pidfd_open_for(pid).map_err(|e| io::Error::other(format!("pidfd_open({pid}): {e}")))?;
    // Child handle is dropped here. On Unix, Child::drop is a no-op
    // (doesn't kill, doesn't reap); we no longer need the type-level
    // identity once the pidfd is in hand. SIGCHLD reaper handles
    // termination accounting via waitpid.
    drop(child);
    Ok(GreeterHandle { pid, pidfd })
}

/// Open a pidfd for an existing pid.
///
/// Wraps `pidfd_open(2)` (Linux ≥ 5.3). Returns an `OwnedFd` that
/// can be passed to `pidfd_send_signal_owned` for race-free signal
/// delivery, then closed on drop.
///
/// # Errors
/// Any errno from `pidfd_open` (notably `ESRCH` if the pid has
/// already exited between fork and this call).
fn pidfd_open_for(pid: u32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    let pid_signed = i32::try_from(pid)
        .map_err(|_| io::Error::other(format!("pid {pid} does not fit in i32")))?;
    // SAFETY: `pidfd_open(2)` is a numeric syscall with no pointer
    // arguments. Returns a non-negative fd on success or -1 with
    // errno set on failure. We construct the OwnedFd only on success.
    #[expect(unsafe_code, reason = "raw pidfd_open syscall via libc")]
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::pid_t::from(pid_signed), 0_u32) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // syscall returns `c_long` (i64 on x86_64); a valid fd from
    // `pidfd_open(2)` fits in i32 by construction (kernel-side
    // fd allocator caps well below INT_MAX). Use try_from for the
    // narrowing to surface any future drift.
    let raw_fd: i32 = i32::try_from(raw)
        .map_err(|_| io::Error::other(format!("pidfd_open returned out-of-range fd {raw}")))?;
    // SAFETY: `raw_fd` is a fresh kernel fd from the successful
    // syscall above; nothing else holds it.
    #[expect(unsafe_code, reason = "wrap fresh fd into OwnedFd")]
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
    Ok(fd)
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
    use nix::unistd::{Uid, getegid, setresgid, setresuid};

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

    let egid = getegid();
    setresgid(egid, egid, egid).map_err(|e| io::Error::other(format!("setresgid({egid}): {e}")))?;

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
        .with_writer(io::stderr)
        .with_env_filter(env_filter)
        .init();

    emit(&Event::Started {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
    });

    // Resolve the DRM device path before anything else (the path may
    // be `None` for the dev/test SKIP path). The actual DRM/GBM/EGL/GLES
    // setup happens further down, after the calloop event loop is
    // built — `setup_drm_backend` needs `loop_handle` to wire the
    // page-flip event source.
    let drm_device_path = drm_device_path_from_env()?;
    // The wallpaper plane (epic G1/R3/R6). Constructed once inside
    // `setup_drm_backend`; `is_some()` here also gates the frame-0
    // wallpaper anchor emitted below.
    let wallpaper_config = wallpaper_config_from_env();
    let wallpaper_configured = wallpaper_config.is_some();

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

    // Build the DRM backend if a device path was resolved. The real
    // case (production) goes through `drm::setup_drm_backend` and
    // returns the smithay `Output` backed by the actual connector
    // mode. The SKIP case (dev/test, non-root) synthesizes a 1920x1080
    // placeholder so wl_clients can still discover an output global.
    let (drm_backend, drm_token, output, libseat_session) = if let Some(path) = &drm_device_path {
        // Open the libseat session (seatd backend) while still root,
        // BEFORE the privilege drop below. seatd brokers the DRM +
        // input fds and owns DRM master; halmasuit never SET_MASTERs.
        // drm-master-probe Phase 4 validated this session survives
        // the subsequent setresuid.
        let (mut session, libseat_notifier) = LibSeatSession::new().map_err(|e| {
            io::Error::other(format!("LibSeatSession::new (is seatd reachable?): {e}"))
        })?;
        // Service libseat session (activate/pause) events. v1 in-VM:
        // no VT switching (epic out-of-scope) — log only, but the
        // source MUST be registered so libseat's event fd is drained.
        loop_handle
            .insert_source(
                libseat_notifier,
                |event, (), _state: &mut HalmasuitState| {
                    tracing::info!(?event, "libseat session event");
                },
            )
            .map_err(|e| io::Error::other(format!("insert libseat notifier: {e}")))?;
        let (backend, token, real_output) = drm::setup_drm_backend(
            &mut session,
            path,
            &loop_handle,
            |event, _meta, state: &mut HalmasuitState| match event {
                smithay::backend::drm::DrmEvent::VBlank(_crtc) => {
                    if let Some(backend) = state.drm_backend.as_mut()
                        && let Err(e) = backend.frame_submitted()
                    {
                        tracing::warn!(error = %e, "DRM frame_submitted failed");
                    }
                }
                smithay::backend::drm::DrmEvent::Error(e) => {
                    tracing::warn!(error = %e, "DRM device error");
                }
            },
            wallpaper_config,
        )?;

        // libinput, fed device fds through the SAME seatd session
        // (validated surviving setresuid by drm-master-probe Phase 4).
        // Events are routed to the keyboard-focused client.
        let mut libinput = smithay::reexports::input::Libinput::new_with_udev(
            LibinputSessionInterface::from(session.clone()),
        );
        libinput.udev_assign_seat(&session.seat()).map_err(|()| {
            io::Error::other(format!("libinput udev_assign_seat({})", session.seat()))
        })?;
        loop_handle
            .insert_source(
                LibinputInputBackend::new(libinput),
                |event, (), state: &mut HalmasuitState| state.dispatch_libinput(event),
            )
            .map_err(|e| io::Error::other(format!("insert libinput backend: {e}")))?;

        real_output.create_global::<HalmasuitState>(&display_handle);
        emit(&Event::PhaseEntered {
            phase: Phase::DrmMasterAcquired,
        });
        (Some(backend), Some(token), real_output, Some(session))
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
    {
        use std::os::unix::fs::PermissionsExt;
        let xdg_runtime_dir =
            std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/run/halmasuit".into());
        let abs_socket_path = PathBuf::from(xdg_runtime_dir).join(&socket_path);
        let perms = std::fs::Permissions::from_mode(0o660);
        std::fs::set_permissions(&abs_socket_path, perms).map_err(|e| {
            io::Error::other(format!("chmod 0660 {}: {e}", abs_socket_path.display()))
        })?;
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
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGCHLD])?;
    loop_handle
        .insert_source(
            signals,
            |event, (), state: &mut HalmasuitState| match event.signal() {
                Signal::SIGCHLD => reap_zombie_children(state),
                sig => {
                    let reason = match sig {
                        Signal::SIGTERM => ShutdownReason::SignalTerm,
                        Signal::SIGINT => ShutdownReason::SignalInt,
                        _ => ShutdownReason::Internal,
                    };
                    emit(&Event::Shutdown { reason });
                    state.running = false;
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

    // greetd socket setup. Production: NixOS module sets
    // HALMASUIT_GREETD_SOCKET to `/run/halmasuit/greetd.sock` and
    // HALMASUIT_GREETER_UID to the greeter system user's uid.
    // Defaults are test-friendly: socket lives in XDG_RUNTIME_DIR,
    // greeter is the running user.
    let (greetd_listener_token, greeter_uid, pam_service) = setup_greetd_listener(&loop_handle)?;

    // Spawn the configured greeter while halmasuit is still root.
    // The child uses `pre_exec` to setresuid into the greeter user
    // before execve, so the greeter never sees root. After the
    // fork, the parent (halmasuit) proceeds into its own privilege
    // drop below. Greeter failure logs but doesn't abort halmasuit:
    // operators may run halmasuit without a greeter during dev.
    #[expect(
        clippy::option_if_let_else,
        reason = "if/else is easier to read than nested map_or_else closures here"
    )]
    let greeter = if let Some(cmd) = greeter_command_from_env() {
        match spawn_greeter(greeter_uid, &cmd) {
            Ok(handle) => {
                tracing::info!(greeter_pid = handle.pid, greeter_cmd = %cmd.display(), "greeter spawned");
                emit(&Event::GreeterSpawned { pid: handle.pid });
                emit(&Event::ForegroundChanged {
                    to: halmasuit_introspect::Foreground::Greeter,
                });
                Some(handle)
            }
            Err(e) => {
                tracing::error!(error = %e, greeter_cmd = %cmd.display(), "greeter spawn failed");
                None
            }
        }
    } else {
        tracing::warn!(
            "HALMASUIT_GREETER_COMMAND unset; halmasuit running without a greeter. \
             Production deployments set this via services.halmasuit.greeterCommand."
        );
        None
    };

    let mut state = HalmasuitState {
        running: true,
        display_handle,
        compositor_state,
        xdg_shell_state,
        seat_state,
        seat,
        _output_manager_state: output_manager_state,
        output,
        shm_state,
        layer_shell_state,
        seen_layer_roles: std::collections::HashSet::new(),
        foreground_toplevel: None,
        foreground: halmasuit_introspect::Foreground::Greeter,
        _libseat_session: libseat_session,
        loop_handle: loop_handle.clone(),
        connections: HashMap::new(),
        next_conn_id: 0,
        pam_service,
        broker_socket: broker_socket_path_from_env(),
        greeter_uid,
        _greetd_listener_token: Some(greetd_listener_token),
        drm_backend,
        _drm_token: drm_token,
        greeter,
        session_uid: None,
        swap: swap_gate::SwapGate::new(),
        session_first_frame_emitted: false,
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

    // Main loop: wait briefly for any source to fire, then flush any
    // pending outgoing events to clients. flush_clients lives on the
    // DisplayHandle (cloned earlier) since Display itself is now owned
    // by the calloop source above.
    // Consecutive failed-iteration counter for the R5 degrade-in-place
    // log rate-limiter; reset on the first clean iteration.
    let mut consecutive_errors = 0u32;
    while state.running {
        run_loop_iteration(&mut consecutive_errors, || {
            event_loop
                .dispatch(Some(Duration::from_millis(16)), &mut state)
                .map_err(io::Error::other)?;
            state
                .display_handle
                .flush_clients()
                .map_err(io::Error::other)?;
            Ok(())
        });
    }

    // Reached only via the deliberate Shutdown path (`state.running`
    // cleared in the SIGTERM/SIGINT closure): a clean exit 0, which
    // systemd's `Restart=on-failure` correctly does NOT restart.
    Ok(())
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
