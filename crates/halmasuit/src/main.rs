// halmasuit — Linux system compositor.
//
// v2 Phase A spine. This binary lives from `multi-user.target` to shutdown
// and will host greeter + session as nested wl_clients. Today (smithay
// scaffolding) it brings up smithay's Wayland-server-shaped event loop,
// binds a Wayland socket, and waits for clients. No protocol globals
// are advertised yet — connecting clients see an empty global list.
// `wl_compositor`, `xdg-shell`, etc. land in subsequent tasks.
// See ARCHITECTURE.md.

use std::io;
use std::sync::Arc;

use calloop::EventLoop;
use calloop::signals::{Signal, Signals};
use halmasuit_introspect::{Event, Phase, ShutdownReason, emit};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::wayland::socket::ListeningSocketSource;
use tracing_subscriber::EnvFilter;

/// Compositor state passed to calloop callbacks. Near-empty for now;
/// subsequent tasks add protocol-state fields as their globals land.
struct HalmasuitState {
    running: bool,
    display_handle: DisplayHandle,
}

/// Per-client metadata. Empty until a global needs to associate
/// per-client state. Implementing `ClientData` is the minimum
/// contract for `DisplayHandle::insert_client`.
struct NoopClientData;

impl ClientData for NoopClientData {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

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

    // Initialize the Wayland display + calloop event loop. Display owns
    // protocol state; the loop integrates wayland-server, signal
    // sources, and (eventually) DRM, libinput, etc.
    let display: Display<HalmasuitState> = Display::new().map_err(io::Error::other)?;
    let display_handle = display.handle();

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
    // as a new wl_client. With no globals advertised, the client will
    // see an empty registry and disconnect cleanly.
    loop_handle
        .insert_source(socket, |stream, (), state: &mut HalmasuitState| match state
            .display_handle
            .insert_client(stream, Arc::new(NoopClientData))
        {
            Ok(_client) => tracing::debug!("new wl_client accepted"),
            Err(e) => tracing::error!(error = %e, "insert_client failed"),
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

    let mut state = HalmasuitState {
        running: true,
        display_handle,
    };

    // Main loop: dispatch Wayland client events, flush writes, then
    // block on calloop until something else fires (signal, new client,
    // future DRM/input/IPC sources).
    let mut display = display;
    while state.running {
        display
            .dispatch_clients(&mut state)
            .map_err(io::Error::other)?;
        display.flush_clients().map_err(io::Error::other)?;
        event_loop
            .dispatch(None, &mut state)
            .map_err(io::Error::other)?;
    }

    Ok(())
}
