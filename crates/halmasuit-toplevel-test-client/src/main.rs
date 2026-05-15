// halmasuit-toplevel-test-client — test wl_client for F1.
//
// Maps a single fullscreen-ish solid-colour `xdg_toplevel` via
// `wl_shm` and holds. Used by tests/visual-halmasuit-toplevel.nix to
// verify halmasuit composites a real xdg toplevel fullscreen over the
// splash BACKGROUND (z-order + xdg-shell mapping). Not production
// code; patterned on sctk's xdg `window` example, trimmed to one
// solid colour and no resize logic.

#![deny(unsafe_code)]
// reason: sctk's idiomatic State struct uses the *_state field names;
// renaming would diverge from every sctk downstream example.
#![allow(clippy::struct_field_names)]
// reason: shm buffer dims are u32 on the wire but i32 in sctk's
// create_buffer API; our sizes are bounded well below i32::MAX.
#![allow(clippy::cast_possible_wrap)]

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::Format;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
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

/// Default solid colour `#FF22AA` (ARGB8888-LE `[B,G,R,A]`) — a
/// magenta distinct from the splash fixture quadrants, the layer
/// client's green, and the brand clear, so the golden is unambiguous.
const DEFAULT_COLOR_BGRA: [u8; 4] = [0xAA, 0x22, 0xFF, 0xFF];

fn parse_color(spec: &str) -> anyhow::Result<[u8; 4]> {
    let hex = spec.strip_prefix('#').unwrap_or(spec);
    anyhow::ensure!(hex.len() == 6, "color must be 6 hex digits, got {spec:?}");
    let red = u8::from_str_radix(&hex[0..2], 16)?;
    let green = u8::from_str_radix(&hex[2..4], 16)?;
    let blue = u8::from_str_radix(&hex[4..6], 16)?;
    Ok([blue, green, red, 0xFF])
}

fn main() -> anyhow::Result<()> {
    let color = match std::env::var("HALMASUIT_TESTCLIENT_COLOR") {
        Ok(s) => parse_color(&s)?,
        Err(_) => DEFAULT_COLOR_BGRA,
    };

    let conn =
        Connection::connect_to_env().map_err(|e| anyhow::anyhow!("connect to wayland: {e}"))?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("halmasuit-toplevel-test-client");
    window.set_app_id("halmasuit.test.toplevel");
    window.commit();

    let pool = SlotPool::new(1920 * 1080 * 4, &shm)?;

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        _compositor_state: compositor,
        _xdg_shell: xdg_shell,
        window,
        pool,
        color,
        width: 0,
        height: 0,
    };

    eprintln!("toplevel-test-client: bound, waiting for configure");
    loop {
        event_queue.blocking_dispatch(&mut state)?;
    }
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    _compositor_state: CompositorState,
    _xdg_shell: XdgShell,
    window: Window,
    pool: SlotPool,
    color: [u8; 4],
    width: u32,
    height: u32,
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
        // halmasuit sends the output mode size (it configures the
        // toplevel fullscreen). Fall back to the virtio-gpu-pci
        // default if it ever defers.
        let w = configure.new_size.0.map_or(1280, std::num::NonZeroU32::get);
        let h = configure.new_size.1.map_or(800, std::num::NonZeroU32::get);
        self.width = w;
        self.height = h;
        self.paint();
        eprintln!("toplevel-test-client: painted {w}x{h}");
    }
}

impl State {
    fn paint(&mut self) {
        let (w, h) = (self.width as i32, self.height as i32);
        let stride = w * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w, h, stride, Format::Argb8888)
            .expect("create_buffer");
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&self.color);
        }
        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        buffer.attach_to(surface).expect("attach_to");
        self.window.commit();
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

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_registry!(State);
