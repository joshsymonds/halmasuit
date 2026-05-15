// halmasuit-layer-shell-test-client — test wl_client for B.3.
//
// Connects to halmasuit's `wayland-0` socket, binds `wlr-layer-shell`
// with role `Background`, creates an `wl_shm` buffer filled with a
// known solid color (a recognizable green: `#16C44E`), commits it
// fullscreen, then waits for SIGTERM. Used by
// tests/visual-halmasuit-layer.nix to verify halmasuit composites
// layer-shell clients into its scanout.
//
// Not production code. Patterned on smithay-client-toolkit's own
// `layer.rs` example, simplified to:
//   * single shm buffer (no dynamic resize after configure)
//   * single layer role (Background)
//   * single color (test fixture, not configurable)
//   * exits on SIGTERM, not on close events

#![deny(unsafe_code)]
// reason: sctk's idiomatic State struct uses `registry_state`,
// `output_state`, `compositor_state` field names; renaming them
// would diverge from every sctk downstream example.
#![allow(clippy::struct_field_names)]
// reason: wayland buffer dimensions are u32 in the wire protocol but
// i32 in sctk's create_buffer / damage_buffer APIs. Our test sizes
// are bounded to 1920x1080 (well below i32::MAX); no wrap is
// achievable in practice.
#![allow(clippy::cast_possible_wrap)]

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
};

use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_output;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::Format;
use smithay_client_toolkit::reexports::client::protocol::wl_surface;
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};

/// Solid color the background surface paints. ARGB8888 little-endian
/// (sctk's slot-pool default format), bytes `[B, G, R, A]`.
///
/// `#16C44E` = (R=0x16, G=0xC4, B=0x4E). A medium green, distinct
/// from halmasuit's brand `#0a0014` so the visual golden is
/// unambiguous about which surface painted which region.
const TEST_COLOR_BGRA: [u8; 4] = [0x4E, 0xC4, 0x16, 0xFF];

fn main() -> anyhow::Result<()> {
    let conn =
        Connection::connect_to_env().map_err(|e| anyhow::anyhow!("connect to wayland: {e}"))?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    // Create a 1280x800 surface — matches what halmasuit reports for
    // virtio-gpu-pci by default. The layer-shell configure event will
    // tell us the actual size halmasuit wants; we accept it.
    let surface = compositor.create_surface(&qh);
    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Background,
        Some("halmasuit-layer-shell-test-client"),
        None, // any output
    );
    layer_surface.set_anchor(Anchor::all());
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_exclusive_zone(-1); // ignore exclusive zones
    layer_surface.commit();

    // SHM pool sized for 1920x1080xARGB; resized at configure time.
    let pool = SlotPool::new(1920 * 1080 * 4, &shm)?;

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        _compositor_state: compositor,
        _layer_shell: layer_shell,
        _layer_surface: layer_surface,
        pool,
        configured: false,
        width: 0,
        height: 0,
    };

    eprintln!("layer-shell-test-client: bound, waiting for configure");

    // Roundtrip-then-dispatch loop until configured + painted.
    loop {
        event_queue.blocking_dispatch(&mut state)?;
    }
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    /// Held for the surface's lifetime; the layer surface goes away if
    /// the LayerShell global is dropped.
    _compositor_state: CompositorState,
    _layer_shell: LayerShell,
    /// Held to keep the wayland proxy alive — sctk destroys the
    /// surface when this is dropped. Not read; `configure` receives
    /// its own `&LayerSurface`.
    _layer_surface: LayerSurface,
    pool: SlotPool,
    configured: bool,
    width: u32,
    height: u32,
}

impl LayerShellHandler for State {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        // halmasuit closed our layer surface. Exit so the test driver
        // can see the close (rather than hanging until SIGTERM).
        std::process::exit(0);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        // halmasuit's configure may pass (0, 0) meaning "client picks";
        // pick the connector's preferred mode size in that case. We
        // don't know it client-side, so fall back to 1280x800 (what
        // virtio-gpu-pci reports in our test VMs).
        let (w, h) = if width == 0 || height == 0 {
            (1280, 800)
        } else {
            (width, height)
        };
        self.width = w;
        self.height = h;
        self.configured = true;
        self.paint(layer);
        eprintln!("layer-shell-test-client: painted {w}x{h}");
    }
}

impl State {
    fn paint(&mut self, layer: &LayerSurface) {
        let stride = self.width as i32 * 4;
        let height = self.height as i32;
        let (buffer, canvas) = self
            .pool
            .create_buffer(self.width as i32, height, stride, Format::Argb8888)
            .expect("create_buffer");
        for pixel in canvas.chunks_exact_mut(4) {
            pixel.copy_from_slice(&TEST_COLOR_BGRA);
        }
        layer
            .wl_surface()
            .damage_buffer(0, 0, self.width as i32, height);
        buffer.attach_to(layer.wl_surface()).expect("attach_to");
        layer.commit();
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
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
delegate_layer!(State);
delegate_registry!(State);
