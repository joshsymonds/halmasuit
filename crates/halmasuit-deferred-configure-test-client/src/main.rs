// halmasuit-deferred-configure-test-client — protocol-level observer
// of xdg-shell's initial-configure timing.
//
// xdg-shell contract (XML, `xdg_surface`):
//   "The client must call wl_surface.commit ... before it will receive
//    the initial configure event."
//
// The compositor MUST defer sending the initial `xdg_surface.configure`
// until it has seen the client's first commit on the corresponding
// `wl_surface`. The smithay canonical pattern (smallvil, anvil): do
// nothing protocol-visible in `new_toplevel` / `new_popup`; check
// `XdgToplevelSurfaceData.initial_configure_sent` in the commit
// handler and send configure once on first matching commit.
//
// This client drives a deterministic 2-phase observation:
//
//   PHASE 1 — toplevel created, NO commit yet:
//     create wl_surface, get xdg_surface, get xdg_toplevel.
//     flush; roundtrip (drains any spec-violating eager configure
//     event from the compositor).
//     emit `DEFERRED_CONFIGURE_PHASE1: configure_received=<bool>`.
//     CONTRACT: configure_received MUST be false.
//
//   PHASE 2 — first wl_surface.commit, then dispatch:
//     wl_surface.commit (empty initial commit per spec).
//     flush; roundtrip.
//     emit `DEFERRED_CONFIGURE_PHASE2: configure_received=<bool>`.
//     CONTRACT: configure_received MUST be true (compositor sends
//     the deferred initial configure in response to the first commit).
//
// The test driver greps the stderr output for the two phase lines.
//
// After observation, the client ACKs the configure, attaches a small
// buffer (so halmasuit's render path has something to composite — no
// flash regression vector) and sleeps until killed.
//
// Not production code.

#![deny(unsafe_code)]

use std::os::fd::AsFd;
use std::time::Duration;

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1;
use wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1;
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2;
use wayland_protocols::wp::viewporter::client::wp_viewporter;
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1;
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const SURFACE_W: u32 = 128;
const SURFACE_H: u32 = 128;
const SURFACE_BGRA: [u8; 4] = [0x00, 0xAA, 0xFF, 0xFF];

fn main() -> anyhow::Result<()> {
    let conn =
        Connection::connect_to_env().map_err(|e| anyhow::anyhow!("connect to wayland: {e}"))?;
    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=2, ())?;
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;
    // R6: bind wl_output so the compositor's `Output::enter` includes
    // an output object in the `wl_surface.enter` event for this
    // client. Smithay's `Output::enter` walks `client_outputs_internal`
    // — a client that hasn't bound wl_output gets nothing.
    let _output: wl_output::WlOutput = globals.bind(&qh, 1..=4, ())?;
    // R10: probe for the zwp_linux_dmabuf_v1 global. Bind succeeds
    // only if halmasuit advertised it (which requires the renderer
    // to have been created — production path with a real DRM device).
    let dmabuf_bound = globals
        .bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(&qh, 3..=4, ())
        .is_ok();
    eprintln!("DMABUF_GLOBAL_BOUND: {dmabuf_bound}");
    // R9: probe for wp_presentation. Bind succeeds if halmasuit
    // advertised the global. Saved so we can request feedback once
    // we have a surface.
    let presentation = globals
        .bind::<wp_presentation::WpPresentation, _, _>(&qh, 1..=2, ())
        .ok();
    eprintln!("PRESENTATION_GLOBAL_BOUND: {}", presentation.is_some());
    probe_phase_b_globals(&globals, &qh);

    let wl_surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&wl_surface, &qh, ());
    let xdg_toplevel = xdg_surface.get_toplevel(&qh, ());
    xdg_toplevel.set_title("halmasuit-deferred-configure-test-client".to_owned());
    xdg_toplevel.set_app_id("halmasuit.test.deferred-configure".to_owned());

    let mut state = State::default();

    // PHASE 1: toplevel created, no commit yet. Flush our requests,
    // then roundtrip so the server has a chance to send any configure
    // event back. A spec-conformant compositor MUST NOT send one yet.
    event_queue.flush()?;
    event_queue.roundtrip(&mut state)?;
    eprintln!(
        "DEFERRED_CONFIGURE_PHASE1: configure_received={}",
        state.has(OBS_CONFIGURE_RECEIVED)
    );

    // PHASE 2: send our first wl_surface.commit (empty per spec),
    // flush, roundtrip. A spec-conformant compositor sends the
    // initial configure in response to THIS commit.
    wl_surface.commit();
    event_queue.flush()?;
    event_queue.roundtrip(&mut state)?;
    eprintln!(
        "DEFERRED_CONFIGURE_PHASE2: configure_received={}",
        state.has(OBS_CONFIGURE_RECEIVED)
    );

    // Post-observation: complete the handshake so halmasuit's render
    // path sees a non-trivial frame (no flash regression vector). ACK
    // every configure we've received (latest serial — xdg-surface
    // serials are cumulative).
    if let Some(serial) = state.latest_configure_serial {
        xdg_surface.ack_configure(serial);
        attach_solid_buffer(&shm, &wl_surface, &qh, &mut state)?;
        wl_surface.commit();
        event_queue.flush()?;
    }

    // R6 (convergence epic): observe `wl_surface.enter`. The
    // compositor MUST send wl_surface.enter for the toplevel after
    // it maps onto the output, so the client can pick buffer scale,
    // transform, and frame timing per-output. Pre-R6: halmasuit
    // never called `Output::enter` for xdg-toplevels (layer-shell
    // got it from `LayerMap::arrange`, but toplevels did not).
    // Roundtrip once more to let the server flush its post-map
    // events, then emit the journal marker.
    event_queue.roundtrip(&mut state)?;
    eprintln!("SURFACE_ENTER_OBSERVED: {}", state.has(OBS_SURFACE_ENTER));

    // R9 (convergence epic): request wp_presentation_feedback for
    // the next commit, then commit (with damage) to trigger a
    // re-render. After the next VBlank, halmasuit MUST emit
    // `presented` (or `discarded`) on the feedback object.
    if let Some(p) = presentation.as_ref() {
        let _feedback = p.feedback(&wl_surface, &qh, ());
        // Touch the surface so the commit actually re-renders and
        // the feedback fires on the next VBlank.
        wl_surface.damage_buffer(
            0,
            0,
            SURFACE_W.try_into().unwrap_or(i32::MAX),
            SURFACE_H.try_into().unwrap_or(i32::MAX),
        );
        wl_surface.commit();
        event_queue.flush()?;
        // Give halmasuit a moment to render + present.
        std::thread::sleep(Duration::from_millis(500));
        event_queue.roundtrip(&mut state)?;
    }
    eprintln!(
        "PRESENTATION_FEEDBACK_OBSERVED: {}",
        state.has(OBS_PRESENTATION_FEEDBACK)
    );

    // Sleep — the test driver kills us when its assertions have run.
    // The wait must be long enough to cover the test's full sampling
    // window (~10s) without exiting prematurely.
    loop {
        if event_queue.dispatch_pending(&mut state).is_err() {
            break;
        }
        if state.has(OBS_CLOSED) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        let _ = event_queue.flush();
        let _ = drain_events(&mut event_queue, &mut state);
    }
    Ok(())
}

fn drain_events(queue: &mut EventQueue<State>, state: &mut State) -> anyhow::Result<()> {
    queue.roundtrip(state)?;
    Ok(())
}

fn attach_solid_buffer(
    shm: &wl_shm::WlShm,
    surface: &wl_surface::WlSurface,
    qh: &QueueHandle<State>,
    state: &mut State,
) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let stride: u32 = SURFACE_W * 4;
    let size: usize = (stride as usize) * (SURFACE_H as usize);
    let mut file = tempfile()?;
    let pixels: Vec<u8> =
        std::iter::repeat_n(SURFACE_BGRA, (SURFACE_W as usize) * (SURFACE_H as usize))
            .flatten()
            .collect();
    file.write_all(&pixels)?;
    file.flush()?;
    file.seek(SeekFrom::Start(0))?;

    // Wire-side wl_shm.create_pool takes i32 size; SURFACE_W * SURFACE_H * 4 = 65536, comfortably in range.
    let pool_size = i32::try_from(size).expect("buffer size fits in i32 by construction");
    let buf_w = i32::try_from(SURFACE_W).expect("SURFACE_W in i32 range");
    let buf_h = i32::try_from(SURFACE_H).expect("SURFACE_H in i32 range");
    let buf_stride = i32::try_from(stride).expect("stride in i32 range");
    let pool = shm.create_pool(file.as_fd(), pool_size, qh, ());
    let buffer = pool.create_buffer(
        0,
        buf_w,
        buf_h,
        buf_stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, buf_w, buf_h);
    // Keep the pool & buffer alive (the wl_surface holds the buffer
    // until the next attach; the pool is referenced by the buffer).
    state.live_resources.push(LiveResource::Pool(pool));
    state.live_resources.push(LiveResource::Buffer(buffer));
    Ok(())
}

/// Phase B advertise-and-delegate global probes — one journal
/// marker per global so the VM test can grep for each. Extracted
/// from `main` to keep it under the 100-line clippy ceiling.
fn probe_phase_b_globals(globals: &wayland_client::globals::GlobalList, qh: &QueueHandle<State>) {
    eprintln!(
        "VIEWPORTER_GLOBAL_BOUND: {}",
        globals
            .bind::<wp_viewporter::WpViewporter, _, _>(qh, 1..=1, ())
            .is_ok()
    );
    eprintln!(
        "FRACTIONAL_SCALE_GLOBAL_BOUND: {}",
        globals
            .bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, _, _>(qh, 1..=1, ())
            .is_ok()
    );
    eprintln!(
        "SINGLE_PIXEL_BUFFER_GLOBAL_BOUND: {}",
        globals
            .bind::<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1, _, _>(
                qh,
                1..=1,
                ()
            )
            .is_ok()
    );
    eprintln!(
        "POINTER_GESTURES_GLOBAL_BOUND: {}",
        globals
            .bind::<zwp_pointer_gestures_v1::ZwpPointerGesturesV1, _, _>(qh, 1..=3, ())
            .is_ok()
    );
    eprintln!(
        "TABLET_MANAGER_GLOBAL_BOUND: {}",
        globals
            .bind::<zwp_tablet_manager_v2::ZwpTabletManagerV2, _, _>(qh, 1..=1, ())
            .is_ok()
    );
    eprintln!(
        "XDG_DECORATION_GLOBAL_BOUND: {}",
        globals
            .bind::<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _, _>(qh, 1..=1, ())
            .is_ok()
    );
    eprintln!(
        "XDG_ACTIVATION_GLOBAL_BOUND: {}",
        globals
            .bind::<xdg_activation_v1::XdgActivationV1, _, _>(qh, 1..=1, ())
            .is_ok()
    );
}

fn tempfile() -> anyhow::Result<std::fs::File> {
    // Anonymous-file via O_TMPFILE on /tmp. POSIX shm_open also works
    // but requires linking librt; this is a test client.
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "halmasuit-deferred-configure-{}",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    // Unlink immediately; the fd keeps it alive.
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

// Observation flags packed into a bitfield. Each phase the test
// client observes flips one bit; the test driver greps for the
// corresponding `<NAME>: <bool>` journal marker.
const OBS_CONFIGURE_RECEIVED: u8 = 1 << 0;
const OBS_SURFACE_ENTER: u8 = 1 << 1;
const OBS_PRESENTATION_FEEDBACK: u8 = 1 << 2;
const OBS_CLOSED: u8 = 1 << 3;

#[derive(Default)]
struct State {
    /// Bitset of `OBS_*` flags — set as each protocol observation
    /// fires.
    observations: u8,
    latest_configure_serial: Option<u32>,
    live_resources: Vec<LiveResource>,
}

impl State {
    const fn has(&self, bit: u8) -> bool {
        self.observations & bit != 0
    }
    const fn set(&mut self, bit: u8) {
        self.observations |= bit;
    }
}

#[allow(dead_code)]
enum LiveResource {
    Pool(wl_shm_pool::WlShmPool),
    Buffer(wl_buffer::WlBuffer),
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_surface::Event::Enter { .. }) {
            state.set(OBS_SURFACE_ENTER);
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The compositor pings to check liveness. Respond promptly.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.set(OBS_CONFIGURE_RECEIVED);
            state.latest_configure_serial = Some(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, xdg_toplevel::Event::Close) {
            state.set(OBS_CLOSED);
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        _: zwp_linux_dmabuf_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_presentation::WpPresentation,
        _: wp_presentation::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(
            event,
            wp_presentation_feedback::Event::Presented { .. }
                | wp_presentation_feedback::Event::Discarded
        ) {
            state.set(OBS_PRESENTATION_FEEDBACK);
        }
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_viewporter::WpViewporter,
        _: wp_viewporter::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        _: wp_fractional_scale_manager_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
        _: wp_single_pixel_buffer_manager_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_pointer_gestures_v1::ZwpPointerGesturesV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
        _: zwp_pointer_gestures_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_tablet_manager_v2::ZwpTabletManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_manager_v2::ZwpTabletManagerV2,
        _: zwp_tablet_manager_v2::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        _: zxdg_decoration_manager_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_activation_v1::XdgActivationV1,
        _: xdg_activation_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
