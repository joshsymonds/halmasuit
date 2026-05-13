// halmasuit — Linux system compositor.
//
// v2 Phase A spine. This binary lives from `multi-user.target` to shutdown
// and will host greeter + session as nested wl_clients. Today it brings
// up smithay's Wayland-server event loop, binds a Wayland socket, and
// advertises foundational protocol globals: `wl_compositor`,
// `wl_subcompositor`, `xdg_wm_base`, `wl_seat`, `wl_output`. Connecting
// clients can create surfaces, top-levels, and discover inputs/outputs.
// Nothing renders yet (no scanout backend); the advertised output is
// a synthesized 1920×1080@60Hz placeholder until DRM lands. Additional
// globals (`wl_shm`, `linux-dmabuf-v1`, …) land in subsequent tasks.
// See ARCHITECTURE.md.

use std::io;
use std::sync::Arc;

use std::time::Duration;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
// calloop's `Mode` and smithay's `output::Mode` collide; rename calloop's
// to keep the smithay one as `Mode` (used more often).
use calloop::{EventLoop, Interest, Mode as CalloopMode, PostAction};
use halmasuit_introspect::{Event, Phase, ShutdownReason, emit};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::socket::ListeningSocketSource;
use tracing_subscriber::EnvFilter;

/// Compositor state passed to calloop callbacks. Holds the smithay
/// per-protocol state structs; each new protocol adds its `*State` here
/// as it lands.
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
    // `_output` keeps the synthesized output alive; real outputs come
    // with the DRM backend.
    _output: Output,
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

    fn commit(&mut self, _surface: &WlSurface) {
        // No-op until there's an output to composite to. Subsequent
        // tasks (wl_output + DRM scanout) will route committed buffers
        // through the scene graph here.
    }
}

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

    // Initialize the Wayland display + protocol state.
    let display: Display<HalmasuitState> = Display::new().map_err(io::Error::other)?;
    let display_handle = display.handle();
    let compositor_state = CompositorState::new::<HalmasuitState>(&display_handle);
    let xdg_shell_state = XdgShellState::new::<HalmasuitState>(&display_handle);

    let mut seat_state = SeatState::new();
    let seat = seat_state.new_wl_seat(&display_handle, "seat0".to_owned());

    let output_manager_state =
        OutputManagerState::new_with_xdg_output::<HalmasuitState>(&display_handle);
    // Synthesized placeholder output. Geometry is invented; the
    // advertisement exists so clients can discover an output and
    // proceed past their wl_registry phase. Real geometry lands when
    // the DRM backend wires actual modes (subsequent task).
    let output_mode = Mode {
        size: (1920, 1080).into(),
        refresh: 60_000, // 60 Hz, in mHz per the wl_output spec
    };
    let output = Output::new(
        "output-0".to_owned(),
        PhysicalProperties {
            size: (480, 270).into(), // mm; ~96 DPI assumption
            subpixel: Subpixel::Unknown,
            make: "halmasuit".to_owned(),
            model: "synthesized-1080p".to_owned(),
            serial_number: String::new(),
        },
    );
    output.create_global::<HalmasuitState>(&display_handle);
    output.change_current_state(Some(output_mode), None, None, Some((0, 0).into()));
    output.set_preferred(output_mode);

    let mut event_loop: EventLoop<HalmasuitState> =
        EventLoop::try_new().map_err(io::Error::other)?;
    let loop_handle = event_loop.handle();

    // Bind the Wayland listening socket. smithay's ListeningSocketSource
    // places the socket at $XDG_RUNTIME_DIR/<name>; production halmasuit's
    // systemd unit sets XDG_RUNTIME_DIR=/run/halmasuit via the NixOS
    // module's RuntimeDirectory + Environment directives.
    let socket = ListeningSocketSource::with_name("wayland-0")
        .map_err(|e| io::Error::other(format!("bind wayland socket: {e}")))?;
    let socket_path = socket.socket_name().to_owned();
    tracing::info!(socket = ?socket_path, "wayland socket bound");

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
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])?;
    loop_handle
        .insert_source(signals, |event, (), state: &mut HalmasuitState| {
            let reason = match event.signal() {
                Signal::SIGTERM => ShutdownReason::SignalTerm,
                Signal::SIGINT => ShutdownReason::SignalInt,
                _ => ShutdownReason::Internal,
            };
            emit(&Event::Shutdown { reason });
            state.running = false;
        })
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

    let mut state = HalmasuitState {
        running: true,
        display_handle,
        compositor_state,
        xdg_shell_state,
        seat_state,
        _seat: seat,
        _output_manager_state: output_manager_state,
        _output: output,
    };

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
