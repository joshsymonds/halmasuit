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

#[cfg(feature = "frame_audit")]
mod dbus;
mod drm;
#[cfg(feature = "frame_audit")]
mod frame_audit;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
// calloop's `Mode` and smithay's `output::Mode` collide; rename calloop's
// to keep the smithay one as `Mode` (used more often).
use calloop::{
    EventLoop, Interest, LoopHandle, Mode as CalloopMode, PostAction, RegistrationToken,
};
use halmasuit_greetd::PamSession;
use halmasuit_greetd::server::{
    Connection, PamSessionFactory, SpawnRequest, bind_socket, peer_credentials,
};
use halmasuit_introspect::{Event, Phase, ShutdownReason, emit};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::desktop::layer_map_for_output;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
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
    // `_seat` is retained so the seat global stays registered for the
    // lifetime of the compositor; capabilities are added when real
    // input devices come online in a future task.
    _seat: Seat<Self>,
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
    /// Factory that builds a `PamThread` for each `CreateSession`.
    pam_factory: Arc<PamThreadFactory>,
    /// Authorised greeter UID; connections from any other uid are
    /// dropped by `handle_listener_ready`.
    greeter_uid: u32,
    /// Path to the `halmasuit-spawn` setuid helper, invoked when a
    /// connection reaches `SpawnRequest`.
    spawn_bin: PathBuf,
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
    /// retained for the introspection event field. Killed in the
    /// `SessionRequested` handler — per Epic #1, the greeter
    /// wl_client must be killed before the user session takes
    /// halmasuit's foreground slot. The SIGCHLD reaper picks up
    /// the zombie. `None` when `HALMASUIT_GREETER_COMMAND` was
    /// unset OR the greeter slot has already been consumed by
    /// the kill-on-session-start path.
    greeter: Option<GreeterHandle>,
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
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
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
                }
            }
        }

        // Re-render the scene with the now-current buffers. For B.3
        // the call is synchronous (one frame per commit) — fine for
        // single-client low-frequency scenes. B.5+ adds frame_audit
        // and a calloop-idle-scheduled damage-driven path.
        if let Some(backend) = self.drm_backend.as_mut()
            && let Err(e) = backend.render_layer_elements(&self.output, HALMASUIT_BRAND_CLEAR)
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
        if let Some(backend) = self.drm_backend.as_mut()
            && let Err(e) = backend.render_layer_elements(&self.output, HALMASUIT_BRAND_CLEAR)
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
        // A client created an xdg_toplevel. Send a configure so the
        // client knows it can proceed; real geometry comes when an
        // output exists. For now: zero-size configure is a valid
        // "compositor-decides" signal.
        tracing::debug!("new xdg_toplevel");
        surface.send_configure();
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

/// Per-greeter-connection state held in `HalmasuitState::connections`.
/// The `UnixStream` itself lives inside its calloop `Generic` source; we
/// only keep the per-fd state-machine driver and the outbound write
/// buffer here.
struct ConnState {
    conn: Connection,
    write_buf: Vec<u8>,
    /// Set when we want the source to be removed after the write buffer
    /// drains. Triggered on `SpawnRequest`, EOF, codec errors, or
    /// `ProcessOutput::close`.
    close_after_drain: bool,
}

/// `PamSessionFactory` implementation that returns a real `PamThread`
/// per `CreateSession`. Stored behind `Arc` and cloned into each
/// `Connection::new` call.
struct PamThreadFactory {
    /// PAM service name — looked up at `/etc/pam.d/<service>`. Defaults
    /// to `"halmasuit"`; overridable via `HALMASUIT_PAM_SERVICE` env.
    service: String,
}

impl PamSessionFactory for PamThreadFactory {
    fn build(&self, username: &str) -> Box<dyn PamSession + Send> {
        Box::new(halmasuit_pam::PamThread::new(&self.service, username))
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
                let conn =
                    Connection::new(Arc::clone(&state.pam_factory) as Arc<dyn PamSessionFactory>);
                let insert_result = state.loop_handle.insert_source(
                    Generic::new(stream, Interest::BOTH, CalloopMode::Level),
                    move |readiness, stream, state| {
                        handle_connection_ready(id, readiness, stream, state)
                    },
                );
                match insert_result {
                    Ok(_token) => {
                        state.connections.insert(
                            id,
                            ConnState {
                                conn,
                                write_buf: Vec::new(),
                                close_after_drain: false,
                            },
                        );
                        tracing::debug!(id, peer_uid = creds.uid, "accepted greeter connection");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to register connection with calloop");
                    }
                }
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
                    // EOF — greeter closed cleanly.
                    connstate.close_after_drain = true;
                    break;
                }
                Ok(n) => match connstate.conn.process(&buf[..n]) {
                    Ok(out) => {
                        connstate.write_buf.extend(out.reply);
                        if let Some(spawn) = out.spawn {
                            emit(&Event::SessionRequested {
                                uid: spawn.uid,
                                gid: spawn.gid,
                            });
                            // Kill the greeter before invoking the
                            // session spawn. Per Epic #1: "the greeter
                            // wl_client is killed before niri becomes
                            // foreground." The greeter's Wayland
                            // connection drops, halmasuit notices via
                            // the per-client teardown, niri can take
                            // the foreground slot. The SIGCHLD reaper
                            // picks up the resulting zombie.
                            if let Some(greeter) = state.greeter.take() {
                                let pid = greeter.pid;
                                // SIGKILL via pidfd: race-free wrt pid
                                // reuse. If the greeter already exited
                                // (e.g. crashed earlier and the SIGCHLD
                                // reaper consumed the entry), this
                                // returns ESRCH — we surface that as a
                                // GreeterKillFailed event and proceed.
                                match pidfd_send_signal(&greeter.pidfd, libc::SIGKILL) {
                                    Ok(()) => {
                                        emit(&Event::GreeterTerminated { pid });
                                    }
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
                            match invoke_spawn(&state.spawn_bin, &spawn) {
                                Ok(child) => {
                                    tracing::info!(
                                        spawn_pid = child.id(),
                                        uid = spawn.uid,
                                        "halmasuit-spawn launched"
                                    );
                                }
                                Err(e) => {
                                    // Logged but not fatal — the greeter
                                    // can retry, and halmasuit itself is
                                    // still running.
                                    tracing::warn!(
                                        error = %e,
                                        spawn_bin = ?state.spawn_bin,
                                        "failed to invoke halmasuit-spawn"
                                    );
                                }
                            }
                            connstate.close_after_drain = true;
                        }
                        if out.close {
                            connstate.close_after_drain = true;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, id, "greetd codec error; closing");
                        connstate.close_after_drain = true;
                        break;
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(error = %e, id, "read failed on greetd connection");
                    // Peer is gone; pending writes can never be delivered.
                    // Clear so the close_after_drain predicate below fires
                    // immediately instead of leaving the source registered
                    // for a closed fd, which calloop would then keep firing
                    // (POLLHUP) producing a tight error-log loop.
                    connstate.close_after_drain = true;
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
                    // Peer closed write side with no progress; abandon the
                    // remaining buffer rather than spinning on retries.
                    connstate.close_after_drain = true;
                    connstate.write_buf.clear();
                    break;
                }
                Ok(n) => {
                    connstate.write_buf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(error = %e, id, "write failed on greetd connection");
                    // Peer is gone; the buffered reply can't reach it.
                    // Without clearing write_buf, the close_after_drain
                    // predicate below stays false, the source stays
                    // registered, calloop re-fires on POLLHUP/POLLERR, we
                    // loop. Clear so this single warning fires once and
                    // the connection is reaped on the next predicate check.
                    connstate.close_after_drain = true;
                    connstate.write_buf.clear();
                    break;
                }
            }
        }
    }

    if connstate.close_after_drain && connstate.write_buf.is_empty() {
        state.connections.remove(&id);
        return Ok(PostAction::Remove);
    }
    Ok(PostAction::Continue)
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

/// Path to the `halmasuit-spawn` setuid helper. Configurable via env
/// (the NixOS module sets it to `/run/wrappers/bin/halmasuit-spawn`).
/// Fallback resolves via `$PATH` — useful in dev / test where the
/// build artifact is on the path.
fn spawn_bin_from_env() -> PathBuf {
    std::env::var_os("HALMASUIT_SPAWN_BIN")
        .map_or_else(|| PathBuf::from("halmasuit-spawn"), PathBuf::from)
}

/// Brand clear color rendered before any wl_client connects: `#0a0014`
/// in XRGB8888 little-endian. Per the visual-compositor epic's
/// IMMUTABLE Requirement #5, this distinguishes "halmasuit alive, no
/// client yet" from "halmasuit broken / producing black" — every
/// frame painted before halmasuit-splash connects is this exact color.
///
/// Built via [`drm::xrgb_le`] so the byte ordering is unit-tested at
/// build (see `drm::tests::xrgb_le_pins_byte_order`) — silent reverts
/// to the wrong byte order, channel transpose, or `#000000` trip a
/// fast unit test before the visual VM gate.
const HALMASUIT_BRAND_CLEAR: [u8; 4] = drm::xrgb_le(0x0A, 0x00, 0x14);

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
/// until `waitpid` reports no more reapable children. Without this,
/// dead halmasuit-spawn / greeter / session children accumulate as
/// zombies and eventually exhaust the pid namespace.
fn reap_zombie_children() {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => return,
            Ok(status) => tracing::debug!(?status, "reaped child"),
        }
    }
}

/// Drop privileges to the configured compositor uid, preserving
/// `CAP_KILL` so halmasuit retains signal authority over its greeter
/// child (which runs under a different uid) on session start.
/// Supplementary groups were pinned at unit-startup via systemd
/// `SupplementaryGroups=` (`shadow` in production, so halmasuit-pam
/// can `getspnam` directly without forking `unix_chkpwd`); they are
/// intentionally NOT cleared.
///
/// Order is load-bearing:
///   1. Drop bounding-set bits except those `halmasuit-spawn` needs
///      after its setuid-root execve. Per capabilities(7) for
///      set-user-ID-root binaries with no file caps:
///      `P'(permitted) = P(inheritable) | P(bounding)`. Keep
///      `{CAP_SETUID, CAP_SETGID}` — exactly what halmasuit-spawn
///      needs to `setresuid`/`setresgid` into the target user.
///      `CAP_KILL` is NOT in bounding: halmasuit retains it via
///      step 5's `capset`, and bounding only restricts caps
///      *gained* via future execve/capset — not the runtime
///      permitted set.
///      `PR_CAPBSET_DROP` requires `CAP_SETPCAP` in the *effective*
///      set, which is full at this point (we're still root, no
///      `setresuid` yet). Doing this drop AFTER `setresuid` would
///      fail with `EPERM` because the kernel clears `effective` on
///      the root → non-root transition.
///   2. `prctl(PR_SET_KEEPCAPS, 1)` — without this, `setresuid`
///      below would clear the permitted capability set entirely.
///      KeepCaps preserves permitted; effective is still cleared
///      and must be rebuilt via `capset` (step 5).
///   3. `setresgid(egid, egid, egid)` — pin all three gid components.
///      Belt-and-suspenders; the `setresuid` below is what actually
///      removes the ability to change gid (CAP_SETGID drops with
///      the uid transition).
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
/// We deliberately do NOT set `SECBIT_NOROOT` (it would prevent
/// `halmasuit-spawn`'s setuid-root execve from gaining the caps
/// listed in step 1) or `SECBIT_NO_SETUID_FIXUP` (it would
/// suppress all cap changes on uid transitions, sidestepping the
/// `KEEP_CAPS` + `capset` dance — but at the cost of leaving
/// the permitted set at "everything halmasuit had as root,"
/// which is the opposite of what we want).
fn drop_privileges(uid: u32) -> io::Result<()> {
    use caps::{CapSet, Capability};
    use nix::unistd::{Uid, getegid, setresgid, setresuid};

    // Step 1: shrink bounding set while we're still root with
    // CAP_SETPCAP in effective. The bounding set is preserved
    // across setresuid, fork, and execve — dropping bits here has
    // the same effect on children as doing it post-drop, and is
    // syscall-cheaper because PR_CAPBSET_DROP doesn't need the
    // capset choreography otherwise required to re-raise
    // CAP_SETPCAP after setresuid clears effective.
    //
    // We keep ONLY `{CAP_SETUID, CAP_SETGID}` in bounding — exactly
    // what `halmasuit-spawn`'s setuid-root execve needs to inherit
    // via `P'(permitted) = P(inheritable) | P(bounding)`. Note that
    // CAP_KILL is NOT kept here: bounding only constrains caps
    // gained later (capset additions to inheritable, or grants
    // during execve), it does NOT retroactively shrink halmasuit's
    // own permitted/effective sets. halmasuit's runtime `CAP_KILL`
    // survives via `KEEP_CAPS + capset` below regardless of bounding.
    let keep_in_bounding = [Capability::CAP_SETUID, Capability::CAP_SETGID];
    for cap in caps::all() {
        if keep_in_bounding.contains(&cap) {
            continue;
        }
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

/// Construct the `Command` that invokes `halmasuit-spawn` with the
/// resolved spawn parameters. Argv shape (from halmasuit-spawn's
/// `parse_argv` docstring):
///
/// ```text
/// halmasuit-spawn <uid> <gid> <user> -- <cmd> [args...]
/// ```
///
/// The child's environment is cleared and re-populated from
/// `request.env` (each entry is a `KEY=VALUE` string per the greetd
/// protocol). `halmasuit-spawn`'s allowlist filters the env before
/// execve.
fn build_spawn_command(spawn_bin: &Path, request: &SpawnRequest) -> Command {
    let mut cmd = Command::new(spawn_bin);
    cmd.arg(request.uid.to_string());
    cmd.arg(request.gid.to_string());
    cmd.arg(&request.username);
    cmd.arg("--");
    for c in &request.cmd {
        cmd.arg(c);
    }
    cmd.env_clear();
    for env in &request.env {
        if let Some((k, v)) = env.split_once('=') {
            cmd.env(k, v);
        }
    }
    cmd
}

/// Fire-and-forget spawn of `halmasuit-spawn`. Returns the spawned
/// `Child` (kept by the caller only for the PID; we never `wait`).
/// The session running under the new user is the kernel's
/// responsibility after `execve`.
fn invoke_spawn(spawn_bin: &Path, request: &SpawnRequest) -> io::Result<Child> {
    build_spawn_command(spawn_bin, request).spawn()
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

    // Initialize the Wayland display + protocol state.
    let display: Display<HalmasuitState> = Display::new().map_err(io::Error::other)?;
    let display_handle = display.handle();
    let compositor_state = CompositorState::new::<HalmasuitState>(&display_handle);
    let xdg_shell_state = XdgShellState::new::<HalmasuitState>(&display_handle);

    let mut seat_state = SeatState::new();
    let seat = seat_state.new_wl_seat(&display_handle, "seat0".to_owned());

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
        )?;
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

    // New-client handler: hand each accepted UnixStream to the Display
    // with a fresh per-client state.
    loop_handle
        .insert_source(socket, |stream, (), state: &mut HalmasuitState| {
            let client_data = Arc::new(ClientState {
                compositor_state: CompositorClientState::default(),
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
                Signal::SIGCHLD => reap_zombie_children(),
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
        _seat: seat,
        _output_manager_state: output_manager_state,
        output,
        shm_state,
        layer_shell_state,
        seen_layer_roles: std::collections::HashSet::new(),
        _libseat_session: libseat_session,
        loop_handle: loop_handle.clone(),
        connections: HashMap::new(),
        next_conn_id: 0,
        pam_factory: Arc::new(PamThreadFactory {
            service: pam_service,
        }),
        greeter_uid,
        spawn_bin: spawn_bin_from_env(),
        _greetd_listener_token: Some(greetd_listener_token),
        drm_backend,
        _drm_token: drm_token,
        greeter,
    };

    // Kick off the render loop with one initial frame. The page-flip
    // for this frame triggers the next vblank, which our DRM event
    // handler observes (`frame_submitted`) — that's the keepalive for
    // the render loop. Future damage events (B.3+ wl_client commits)
    // will queue additional frames. For now the scene is just the
    // brand clear color.
    //
    // `Phase::ScanoutActive` fires here, on the first successful
    // `queue_frame` — moved from B.1's "post-SETCRTC" timing to
    // "first pixel via GLES" per the epic's IMMUTABLE Requirement #5
    // semantics. The SKIP-path state (no `drm_backend`) emits neither
    // event.
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
    // here onwards runs as the configured compositor system user.
    // The setuid wrapper for halmasuit-spawn (set up by the NixOS
    // module's `security.wrappers`) is what allows us to still
    // execve halmasuit-spawn after this drop.
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
    while state.running {
        event_loop
            .dispatch(Some(Duration::from_millis(16)), &mut state)
            .map_err(io::Error::other)?;
        state
            .display_handle
            .flush_clients()
            .map_err(io::Error::other)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn sample_request() -> SpawnRequest {
        SpawnRequest {
            username: "alice".into(),
            uid: 1000,
            gid: 1000,
            cmd: vec!["niri".into(), "--session".into()],
            env: vec![
                "XDG_SESSION_TYPE=wayland".into(),
                "WAYLAND_DISPLAY=wayland-0".into(),
            ],
        }
    }

    #[test]
    fn build_spawn_command_constructs_correct_argv() {
        let cmd = build_spawn_command(Path::new("/usr/bin/halmasuit-spawn"), &sample_request());
        assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/halmasuit-spawn"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(args.len(), 6);
        assert_eq!(args[0], OsStr::new("1000"));
        assert_eq!(args[1], OsStr::new("1000"));
        assert_eq!(args[2], OsStr::new("alice"));
        assert_eq!(args[3], OsStr::new("--"));
        assert_eq!(args[4], OsStr::new("niri"));
        assert_eq!(args[5], OsStr::new("--session"));
    }

    #[test]
    fn build_spawn_command_populates_env_from_greetd_request() {
        let cmd = build_spawn_command(Path::new("/bin/true"), &sample_request());
        // env_clear() then individual env() calls. get_envs() yields
        // only what we explicitly set; verify both pairs are present
        // and that we did NOT inherit PATH/HOME from the test process.
        let map: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            map.get(OsStr::new("XDG_SESSION_TYPE")),
            Some(&Some(OsStr::new("wayland")))
        );
        assert_eq!(
            map.get(OsStr::new("WAYLAND_DISPLAY")),
            Some(&Some(OsStr::new("wayland-0")))
        );
        assert!(!map.contains_key(OsStr::new("PATH")));
        assert!(!map.contains_key(OsStr::new("HOME")));
    }

    #[test]
    fn build_spawn_command_ignores_env_entries_without_equals() {
        // Defensive: a malformed greetd peer might send a `MALFORMED`
        // string. We split at the first `=`; entries without one are
        // silently dropped.
        let mut req = sample_request();
        req.env.push("MALFORMED_NO_EQUALS".into());
        let cmd = build_spawn_command(Path::new("/bin/true"), &req);
        let map: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert!(!map.contains_key(OsStr::new("MALFORMED_NO_EQUALS")));
    }

    #[test]
    fn spawn_bin_from_env_falls_back_to_path_lookup() {
        // With no env override set (this test relies on nextest's
        // per-test isolation; we don't mutate env in this module),
        // the resolved path is the bare "halmasuit-spawn" — a
        // relative PathBuf the OS resolves via $PATH at spawn time.
        if std::env::var_os("HALMASUIT_SPAWN_BIN").is_none() {
            assert_eq!(spawn_bin_from_env(), PathBuf::from("halmasuit-spawn"));
        }
    }
}
