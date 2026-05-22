// halmasuit-popup-test-client — protocol-level observer of
// xdg_popup initial configure geometry (R5 PopupManager wiring).
//
// xdg-shell contract (`xdg_popup`): the popup's initial configure
// carries the geometry the compositor derived from the positioner
// (x, y, w, h relative to the parent xdg_surface). A compositor
// that forwards an empty / zero-rect geometry has not wired its
// positioner pipeline — a fresh-style client would see an unmapped
// popup or render at (0,0,0,0). Smithay's `PopupManager` +
// `PopupSurface::with_pending_state(geometry = positioner.get_geometry())`
// is the canonical wiring (smallvil pattern at
// `handlers/xdg_shell.rs:41-52`).
//
// This client drives:
//
//   PHASE T — parent toplevel setup:
//     create wl_surface + xdg_surface + xdg_toplevel; first commit
//     triggers the deferred initial configure (R4); ACK; attach
//     buffer; commit.
//
//   PHASE P — popup creation:
//     create wl_surface + xdg_surface + xdg_popup with a deliberate
//     positioner (size 200x100, anchor_rect 10,20,30,40); commit.
//     Then wait for the popup's initial xdg_popup.configure event,
//     which carries the geometry.
//     emit `POPUP_CONFIGURE: x=<x> y=<y> w=<w> h=<h>`.
//     CONTRACT: w > 0 AND h > 0 (i.e., the compositor forwarded a
//     non-empty geometry from the positioner; the precise values
//     depend on the smithay positioner implementation, but a
//     zero-w-or-h configure means the positioner pipeline is
//     broken).
//
// Not production code.

#![deny(unsafe_code)]

use std::os::fd::AsFd;
use std::time::Duration;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};

const SURFACE_W: u32 = 256;
const SURFACE_H: u32 = 256;
const SURFACE_BGRA: [u8; 4] = [0x00, 0xAA, 0xFF, 0xFF];

// Positioner: ask for a 200x100 popup anchored at parent rect (10,20,30,40).
const POSITIONER_W: i32 = 200;
const POSITIONER_H: i32 = 100;
const ANCHOR_X: i32 = 10;
const ANCHOR_Y: i32 = 20;
const ANCHOR_W: i32 = 30;
const ANCHOR_H: i32 = 40;

fn main() -> anyhow::Result<()> {
    let conn =
        Connection::connect_to_env().map_err(|e| anyhow::anyhow!("connect to wayland: {e}"))?;
    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=2, ())?;
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;

    let mut state = State::default();

    // PHASE T: parent toplevel.
    let parent_wl = compositor.create_surface(&qh, SurfaceRole::Parent);
    let parent_xdg = wm_base.get_xdg_surface(&parent_wl, &qh, ());
    let parent_top = parent_xdg.get_toplevel(&qh, ());
    parent_top.set_title("halmasuit-popup-test-client".to_owned());
    parent_top.set_app_id("halmasuit.test.popup".to_owned());
    parent_wl.commit(); // empty initial commit → triggers deferred configure
    event_queue.flush()?;
    event_queue.roundtrip(&mut state)?;
    eprintln!("popup-test-client: parent initial configure received");
    if let Some(s) = state.parent_configure_serial.take() {
        parent_xdg.ack_configure(s);
    }
    let pool = attach_solid_buffer(&shm, &parent_wl, &qh, &mut state)?;
    parent_wl.commit();
    event_queue.flush()?;

    // Let halmasuit process the buffered parent commit before the
    // popup arrives — keeps the journal sequence deterministic.
    event_queue.roundtrip(&mut state)?;

    // PHASE P: popup.
    let positioner = wm_base.create_positioner(&qh, ());
    positioner.set_size(POSITIONER_W, POSITIONER_H);
    positioner.set_anchor_rect(ANCHOR_X, ANCHOR_Y, ANCHOR_W, ANCHOR_H);
    // Defaults for anchor + gravity (None) are fine for this test:
    // smithay's positioner pipeline still emits non-zero geometry.

    let popup_wl = compositor.create_surface(&qh, SurfaceRole::Popup);
    let popup_xdg = wm_base.get_xdg_surface(&popup_wl, &qh, ());
    let _popup = popup_xdg.get_popup(Some(&parent_xdg), &positioner, &qh, ());
    popup_wl.commit(); // empty initial commit → triggers popup configure
    event_queue.flush()?;
    event_queue.roundtrip(&mut state)?;

    if let Some((x, y, w, h)) = state.popup_configure {
        eprintln!("POPUP_CONFIGURE: x={x} y={y} w={w} h={h}");
    } else {
        eprintln!("POPUP_CONFIGURE: missing (no xdg_popup.configure event received)");
    }

    // ACK + attach a small buffer so the popup is "mapped" — the
    // no-flash invariant doesn't depend on this, but keeping it
    // proper avoids any spec error.
    if let Some(s) = state.popup_xdg_configure_serial.take() {
        popup_xdg.ack_configure(s);
    }
    // Attach a small buffer to the popup so the surface is
    // well-formed (halmasuit doesn't yet render popups; this just
    // avoids any spec violation around mapping an unattached popup
    // surface after configure ack).
    let popup_buf = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
    popup_wl.attach(Some(&popup_buf), 0, 0);
    popup_wl.damage_buffer(0, 0, 64, 64);
    popup_wl.commit();
    event_queue.flush()?;
    state.live.push(LiveResource::Buffer(popup_buf));

    // Sleep loop — test driver kills us. Drain events so popups
    // stay alive.
    loop {
        if event_queue.dispatch_pending(&mut state).is_err() {
            break;
        }
        if state.closed {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        let _ = event_queue.flush();
        let _ = event_queue.roundtrip(&mut state);
    }
    Ok(())
}

fn attach_solid_buffer(
    shm: &wl_shm::WlShm,
    surface: &wl_surface::WlSurface,
    qh: &QueueHandle<State>,
    state: &mut State,
) -> anyhow::Result<wl_shm_pool::WlShmPool> {
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
    state.live.push(LiveResource::Buffer(buffer));
    state.live.push(LiveResource::Pool(pool.clone()));
    Ok(pool)
}

fn tempfile() -> anyhow::Result<std::fs::File> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("halmasuit-popup-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

#[derive(Default)]
struct State {
    parent_configure_serial: Option<u32>,
    popup_xdg_configure_serial: Option<u32>,
    popup_configure: Option<(i32, i32, i32, i32)>,
    closed: bool,
    live: Vec<LiveResource>,
}

#[allow(dead_code)]
enum LiveResource {
    Pool(wl_shm_pool::WlShmPool),
    Buffer(wl_buffer::WlBuffer),
}

#[derive(Clone, Copy)]
enum SurfaceRole {
    Parent,
    Popup,
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

impl Dispatch<wl_surface::WlSurface, SurfaceRole> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &SurfaceRole,
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
            // xdg_surface configure is the shared root for both
            // toplevel and popup. We don't know which one this is
            // from the event alone — store on parent slot first,
            // then on popup slot when the second configure arrives.
            if state.parent_configure_serial.is_none() {
                state.parent_configure_serial = Some(serial);
            } else {
                state.popup_xdg_configure_serial = Some(serial);
            }
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
            state.closed = true;
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_popup::Event::Configure {
            x,
            y,
            width,
            height,
        } = event
        {
            state.popup_configure = Some((x, y, width, height));
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_positioner::XdgPositioner,
        _: xdg_positioner::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
