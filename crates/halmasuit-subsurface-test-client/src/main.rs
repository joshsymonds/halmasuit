// halmasuit-subsurface-test-client — wl_subsurface sync-semantics
// test client for halmasuit's convergence epic R3.
//
// Drives a deterministic commit sequence that the compositor's
// commit-aggregation contract is tested against:
//
//   t≈0s   connect, bind globals (wl_compositor, wl_subcompositor,
//          xdg_wm_base, wl_shm), create xdg_toplevel + child surface,
//          wl_subcompositor::get_subsurface(child, parent), child
//          enters sync mode (the default).
//   t≈1s   attach + commit parent (becomes mapped foreground), attach
//          + commit child (sync subsurface, state cached at parent).
//   t=2s   PHASE 1 — initial mapping rendered.
//   t=3s   attach a NEW buffer to the child, commit child ONLY (sync
//          subsurface commit; the spec says compositor MUST NOT make
//          this state visible until the parent commits).
//   t=5s   PHASE 2 — sync-subsurface-only commit done.
//   t=12s  commit the parent (no new buffer; commit aggregates the
//          child's pending state). Wide gap so the test driver can
//          record PHASE 2 well past the sync-subsurface commit AND
//          well before the parent commit, eliminating race.
//   t=14s  PHASE 3 — parent commit done.
//   t=16s  exit.
//
// The test driver compares halmasuit's `frame_rendered` counts across
// PHASE 1 → PHASE 2 (must be flat: sync subsurface commit is invisible)
// and PHASE 2 → PHASE 3 (must increase: parent commit applies the
// tree). Not production code.

#![deny(unsafe_code)]
#![allow(
    clippy::struct_field_names,
    reason = "test-client `State` has multiple parent_* / sub_* dimension \
              fields that share the parent/sub prefix; renaming to \
              avoid the prefix clash would hurt readability for a \
              throwaway protocol probe"
)]

use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::Format;
use smithay_client_toolkit::reexports::client::protocol::{
    wl_output, wl_subcompositor, wl_subsurface, wl_surface,
};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
};

/// Parent fill — magenta-ish. Distinct from layer-test green and
/// brand clear so the golden (if added later) is unambiguous.
const PARENT_BGRA: [u8; 4] = [0xAA, 0x22, 0xFF, 0xFF];
/// Initial child fill — yellow.
const CHILD_INITIAL_BGRA: [u8; 4] = [0x00, 0xFF, 0xFF, 0xFF];
/// Re-commit child fill — green. Used in the sync-subsurface-only
/// commit; only becomes visible if/when the parent commits.
const CHILD_RECOMMIT_BGRA: [u8; 4] = [0x00, 0xFF, 0x00, 0xFF];

/// Subsurface size + offset within parent (any non-trivial rect; the
/// exact geometry doesn't matter for the contract test).
const CHILD_W: i32 = 200;
const CHILD_H: i32 = 200;
const CHILD_X: i32 = 100;
const CHILD_Y: i32 = 100;

fn main() -> anyhow::Result<()> {
    let conn =
        Connection::connect_to_env().map_err(|e| anyhow::anyhow!("connect to wayland: {e}"))?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    // Raw wl_subcompositor bind — sctk doesn't wrap subsurface
    // sequencing at the level this test needs.
    let subcompositor: wl_subcompositor::WlSubcompositor = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| anyhow::anyhow!("bind wl_subcompositor: {e}"))?;

    // Parent: a normal xdg_toplevel via sctk (handles ping/pong + the
    // initial configure roundtrip).
    let parent_surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(
        parent_surface.clone(),
        WindowDecorations::RequestServer,
        &qh,
    );
    window.set_title("halmasuit-subsurface-test-client");
    window.set_app_id("halmasuit.test.subsurface");
    window.commit();

    // Child wl_surface, wrapped as a wl_subsurface of the parent.
    let child_surface = compositor.create_surface(&qh);
    let subsurface = subcompositor.get_subsurface(&child_surface, &parent_surface, &qh, ());
    subsurface.set_sync();
    subsurface.set_position(CHILD_X, CHILD_Y);
    // Parent must commit for the subsurface's set_sync / set_position
    // to actually apply (they're double-buffered state per the spec).
    parent_surface.commit();

    let pool = SlotPool::new(1920 * 1080 * 4, &shm)?;

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        _compositor_state: compositor,
        _xdg_shell: xdg_shell,
        _subcompositor: subcompositor,
        window,
        child_surface,
        _subsurface: subsurface,
        pool,
        parent_width: 0,
        parent_height: 0,
        configured: false,
    };

    eprintln!("subsurface-test-client: bound, waiting for configure");
    // Phase A: configure roundtrip + initial mapping. Dispatch the
    // configure event, then paint+commit parent+child once each.
    while !state.configured {
        event_queue.blocking_dispatch(&mut state)?;
    }
    state.paint_parent(PARENT_BGRA);
    state.paint_child(CHILD_INITIAL_BGRA);
    eprintln!("subsurface-test-client: initial mapping committed");

    // Drive the rest of the deterministic timeline. Mix in dispatch
    // pumps so any compositor events (frame callbacks, etc.) don't
    // pile up.
    let t0 = Instant::now();
    let pump = |conn: &Connection,
                eq: &mut wayland_client::EventQueue<State>,
                s: &mut State|
     -> anyhow::Result<()> {
        conn.flush()?;
        eq.dispatch_pending(s)?;
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    };

    while t0.elapsed() < Duration::from_secs(3) {
        pump(&conn, &mut event_queue, &mut state)?;
    }
    eprintln!("subsurface-test-client: PHASE 2 — sync subsurface commit");
    state.paint_child(CHILD_RECOMMIT_BGRA);

    while t0.elapsed() < Duration::from_secs(12) {
        pump(&conn, &mut event_queue, &mut state)?;
    }
    eprintln!("subsurface-test-client: PHASE 3 — parent commit");
    // No new parent buffer; just commit the parent surface, which
    // applies the child's pending sync state per wl_subsurface spec.
    state.window.wl_surface().commit();

    // Settle window after the last commit.
    while t0.elapsed() < Duration::from_secs(16) {
        pump(&conn, &mut event_queue, &mut state)?;
    }
    eprintln!("subsurface-test-client: PHASE 4 — exit");
    Ok(())
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    _compositor_state: CompositorState,
    _xdg_shell: XdgShell,
    _subcompositor: wl_subcompositor::WlSubcompositor,
    window: Window,
    child_surface: wl_surface::WlSurface,
    // Kept alive to hold the wl_subsurface object reference; the
    // sequencing actions go directly through `child_surface.commit()`.
    _subsurface: wl_subsurface::WlSubsurface,
    pool: SlotPool,
    parent_width: u32,
    parent_height: u32,
    configured: bool,
}

impl State {
    fn paint_parent(&mut self, color: [u8; 4]) {
        let w = i32::try_from(self.parent_width).expect("test-driver-set width fits i32");
        let h = i32::try_from(self.parent_height).expect("test-driver-set height fits i32");
        let stride = w * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w, h, stride, Format::Argb8888)
            .expect("create parent buffer");
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        buffer.attach_to(surface).expect("attach parent buffer");
        self.window.commit();
    }

    fn paint_child(&mut self, color: [u8; 4]) {
        let stride = CHILD_W * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(CHILD_W, CHILD_H, stride, Format::Argb8888)
            .expect("create child buffer");
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        self.child_surface.damage_buffer(0, 0, CHILD_W, CHILD_H);
        buffer
            .attach_to(&self.child_surface)
            .expect("attach child buffer");
        // Commit the CHILD surface ONLY. Per wl_subsurface spec,
        // because the subsurface is in sync mode, this state is
        // cached at the parent and applied only when the parent
        // commits. This is the load-bearing operation under test.
        self.child_surface.commit();
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        std::process::exit(0);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let w = configure.new_size.0.map_or(1280, std::num::NonZeroU32::get);
        let h = configure.new_size.1.map_or(800, std::num::NonZeroU32::get);
        self.parent_width = w;
        self.parent_height = h;
        self.configured = true;
        eprintln!("subsurface-test-client: configure {w}x{h}");
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

// wl_subcompositor / wl_subsurface have no events for the client to
// handle (they're purely client-driven), so the Dispatch impls are
// trivial.
impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_subcompositor::WlSubcompositor,
        _event: <wl_subcompositor::WlSubcompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wl_subsurface::WlSubsurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_subsurface::WlSubsurface,
        _event: <wl_subsurface::WlSubsurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);
