// halmasuit — Linux system compositor.
//
// v2 Phase A spine. This binary lives from `multi-user.target` to shutdown
// and will host greeter + session as nested wl_clients. Today (introspection
// trio) it only emits lifecycle events; DRM, Wayland, greetd, and PAM land
// in subsequent tasks. See ARCHITECTURE.md.

use std::io;

use calloop::EventLoop;
use calloop::signals::{Signal, Signals};
use halmasuit_introspect::{Event, Phase, ShutdownReason, emit};
use tracing_subscriber::EnvFilter;

fn main() -> io::Result<()> {
    // Initialize JSON-to-stderr tracing-subscriber FIRST so even subsequent
    // setup failures surface as structured events instead of bare panics.
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

    let mut event_loop: EventLoop<bool> = EventLoop::try_new().map_err(io::Error::other)?;
    let handle = event_loop.handle();

    // Register the signal source BEFORE emitting PhaseEntered so a SIGTERM
    // that races startup is still observed by the first dispatch.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])?;
    handle
        .insert_source(signals, |event, (), shutting_down: &mut bool| {
            let reason = match event.signal() {
                Signal::SIGTERM => ShutdownReason::SignalTerm,
                Signal::SIGINT => ShutdownReason::SignalInt,
                _ => ShutdownReason::Internal,
            };
            emit(&Event::Shutdown { reason });
            *shutting_down = true;
        })
        .map_err(io::Error::other)?;

    emit(&Event::PhaseEntered { phase: Phase::Init });

    let mut shutting_down = false;
    while !shutting_down {
        event_loop
            .dispatch(None, &mut shutting_down)
            .map_err(io::Error::other)?;
    }

    Ok(())
}
