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
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
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
        state.configure_received
    );

    // PHASE 2: send our first wl_surface.commit (empty per spec),
    // flush, roundtrip. A spec-conformant compositor sends the
    // initial configure in response to THIS commit.
    wl_surface.commit();
    event_queue.flush()?;
    event_queue.roundtrip(&mut state)?;
    eprintln!(
        "DEFERRED_CONFIGURE_PHASE2: configure_received={}",
        state.configure_received
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

    // Sleep — the test driver kills us when its assertions have run.
    // The wait must be long enough to cover the test's full sampling
    // window (~10s) without exiting prematurely.
    loop {
        if event_queue.dispatch_pending(&mut state).is_err() {
            break;
        }
        if state.closed {
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

#[derive(Default)]
struct State {
    configure_received: bool,
    latest_configure_serial: Option<u32>,
    closed: bool,
    live_resources: Vec<LiveResource>,
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
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
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
            state.configure_received = true;
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
            state.closed = true;
        }
    }
}
