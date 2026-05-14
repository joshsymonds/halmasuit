// halmasuit-visual-test-standin — visual-test stand-in scene.
//
// First task of the visual-compositor epic: de-risks the headless-GL
// + golden-comparison capture pipeline before halmasuit's real
// renderer exists.
//
// The standin opens /dev/dri/card0 (override: HALMASUIT_DRM_DEVICE),
// becomes DRM master, finds a connected connector and its preferred
// mode, allocates a dumb buffer at that size, paints a known
// 4-quadrant pattern with a centered black square inset, wraps the
// buffer as a framebuffer, and drives the CRTC. Then it heartbeats
// forever; SIGTERM with the default action kills it and the kernel
// reaps the fd (and thus drops DRM master + destroys the dumb buffer +
// disables the CRTC).
//
// The 4-quadrant + center-square pattern is chosen for deterministic
// visual identification:
//   • TL red, TR green, BL blue, BR white — high-contrast cardinal
//     colors that survive 8-bit quantization, JPEG-style banding, and
//     llvmpipe's color-space quirks intact.
//   • A small black square at the exact center, sized as
//     min(W, H) / 20, gives a sub-pixel-position-stable feature that
//     DSSIM picks up at the lowest gain. Without it, two flat-colored
//     halves of an image score artificially low even when shifted.
//   • The pattern is resolution-agnostic: same logical scene at any
//     mode size; the golden is captured at whatever size the test VM
//     reports and pinned by DSSIM.
//
// Not production code. Lifetime: retired in a later subtask when
// halmasuit's renderer can paint its clear color and the visual VM
// test consumes the real binary instead of this standin.

#![deny(unsafe_code)]

use std::fs::OpenOptions;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use drm::Device;
use drm::buffer::DrmFourcc;
use drm::control::{Device as ControlDevice, connector};

/// DRM device handle. Pattern mirrors `drm-master-probe`'s `Card`:
/// newtype around `std::fs::File`, marker impls for `Device` +
/// `ControlDevice`.
struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

// Colors in XRGB8888 little-endian byte order [B, G, R, X].
const BLACK: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
const RED: [u8; 4] = [0x00, 0x00, 0xFF, 0x00];
const GREEN: [u8; 4] = [0x00, 0xFF, 0x00, 0x00];
const BLUE: [u8; 4] = [0xFF, 0x00, 0x00, 0x00];
const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0x00];

/// Paint the 4-quadrant + center-black-square pattern into a row-major
/// `XRGB8888` (little-endian byte order `[B, G, R, X]`) buffer.
fn paint_quadrants(map: &mut [u8], width: usize, height: usize) {
    let mid_x = width / 2;
    let mid_y = height / 2;
    let cx = width / 2;
    let cy = height / 2;
    let square_half = (width.min(height) / 20) / 2;
    let sq_x0 = cx.saturating_sub(square_half);
    let sq_x1 = (cx + square_half).min(width);
    let sq_y0 = cy.saturating_sub(square_half);
    let sq_y1 = (cy + square_half).min(height);

    for y in 0..height {
        let in_center_y = y >= sq_y0 && y < sq_y1;
        let row_quadrant_color = if y < mid_y {
            (RED, GREEN)
        } else {
            (BLUE, WHITE)
        };
        let row = &mut map[y * width * 4..(y + 1) * width * 4];
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            let color = if in_center_y && x >= sq_x0 && x < sq_x1 {
                &BLACK
            } else if x < mid_x {
                &row_quadrant_color.0
            } else {
                &row_quadrant_color.1
            };
            pixel.copy_from_slice(color);
        }
    }
}

#[expect(
    clippy::infinite_loop,
    reason = "standin holds master + heartbeats until SIGTERM kills it; kernel reaps the fd"
)]
fn main() -> Result<()> {
    let dev_path: PathBuf = std::env::var_os("HALMASUIT_DRM_DEVICE")
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);

    let card = Card(
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev_path)
            .with_context(|| format!("open {}", dev_path.display()))?,
    );

    card.acquire_master_lock().context("SET_MASTER")?;
    eprintln!("standin: SET_MASTER ok");

    let res = card.resource_handles().context("resource_handles")?;

    let Some(con) = res
        .connectors()
        .iter()
        .filter_map(|&h| card.get_connector(h, true).ok())
        .find(|info| info.state() == connector::State::Connected)
    else {
        bail!("no connected connector found");
    };

    let Some(&mode) = con.modes().first() else {
        bail!("connector {:?} has no modes", con.handle());
    };
    let (w, h) = mode.size();
    eprintln!("standin: mode {w}x{h}@{}Hz", mode.vrefresh());

    let &crtc_handle = res.crtcs().first().context("no CRTCs available")?;

    let mut db = card
        .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)
        .context("create_dumb_buffer")?;

    {
        let mut map = card.map_dumb_buffer(&mut db).context("map_dumb_buffer")?;
        paint_quadrants(map.as_mut(), usize::from(w), usize::from(h));
    }

    let fb_handle = card
        .add_framebuffer(&db, 24, 32)
        .context("add_framebuffer")?;

    card.set_crtc(
        crtc_handle,
        Some(fb_handle),
        (0, 0),
        &[con.handle()],
        Some(mode),
    )
    .context("set_crtc")?;
    eprintln!("standin: SETCRTC ok");

    // Heartbeat — hold master forever; SIGTERM is the only exit.
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let t = start.elapsed().as_secs();
        eprintln!("standin: tick t={t}s");
    }
}
