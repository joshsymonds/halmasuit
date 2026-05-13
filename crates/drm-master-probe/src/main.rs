// drm-master-probe — Phase 0 research probe for halmasuit v2.
//
// Asks one question: can a userspace process take DRM master directly
// (no logind brokerage), do a minimal modeset, and hold the master
// indefinitely from boot through multi-user.target — without anything
// on the system contesting it?
//
// The probe paints a solid red frame so the test substrate is also
// visually verifiable in interactive mode. The headless test asserts
// mastery via debugfs and journal logs, not screenshots.
//
// This is research scaffolding. Production DRM ownership lives in
// halmasuit-kms (v2). See ARCHITECTURE.md "The architectural commitment".

#![forbid(unsafe_code)]

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use anyhow::{Context, Result};
use drm::Device;
use drm::buffer::DrmFourcc;
use drm::control::{Device as ControlDevice, connector};

/// Minimal `Device` newtype wrapping a DRM device file.
///
/// Matches the pattern from the upstream `drm-rs` `legacy_modeset` example:
/// implement `AsFd`, then the marker traits `Device` + `ControlDevice`.
struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

#[expect(
    clippy::infinite_loop,
    reason = "the probe is intended to hold DRM master forever and only exit via SIGTERM"
)]
fn main() -> Result<()> {
    // step 1: open /dev/dri/card0
    let card = Card(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/card0")
            .context("step 1: open /dev/dri/card0")?,
    );
    let fd = card.0.as_raw_fd();

    // step 2: become DRM master
    card.acquire_master_lock()
        .context("step 2: SET_MASTER (acquire_master_lock)")?;
    let pid = std::process::id();
    eprintln!("drm-master-probe: SET_MASTER ok pid={pid} fd={fd}");

    // step 3: enumerate DRM resources
    let res = card
        .resource_handles()
        .context("step 3: resource_handles")?;

    // step 4: find a connected connector
    let Some(con) = res
        .connectors()
        .iter()
        .filter_map(|&h| card.get_connector(h, true).ok())
        .find(|info| info.state() == connector::State::Connected)
    else {
        eprintln!("drm-master-probe: error: no connected connector");
        std::process::exit(2);
    };

    // step 5: pick the connector's preferred mode (DRM convention: first)
    let Some(&mode) = con.modes().first() else {
        eprintln!("drm-master-probe: error: connector has no modes");
        std::process::exit(2);
    };
    let (w, h) = mode.size();
    let refresh = mode.vrefresh();
    eprintln!("drm-master-probe: mode {w}x{h}@{refresh}");

    // step 6: pick a CRTC (first available; single-monitor VM substrate)
    let &crtc_handle = res.crtcs().first().context("step 6: no CRTCs available")?;
    eprintln!("drm-master-probe: crtc {crtc_handle:?}");

    // step 7: create dumb buffer at the connector's preferred resolution
    let mut db = card
        .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)
        .context("step 7: create_dumb_buffer")?;

    // step 8: map + fill with solid red
    //
    // XRGB8888 little-endian byte order is [B, G, R, X] per pixel.
    // `chunks_exact_mut(4)` guarantees each chunk is exactly 4 bytes, so
    // `copy_from_slice` of a 4-byte literal is bounds-safe and cleaner than
    // indexed assignment.
    {
        let mut map = card
            .map_dumb_buffer(&mut db)
            .context("step 8: map_dumb_buffer")?;
        for pixel in map.as_mut().chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0x00, 0x00, 0xFF, 0x00]);
        }
    }

    // step 9: wrap dumb buffer as a framebuffer
    let fb = card
        .add_framebuffer(&db, 24, 32)
        .context("step 9: add_framebuffer")?;

    // step 10: drive the connector with our framebuffer at the chosen mode
    card.set_crtc(crtc_handle, Some(fb), (0, 0), &[con.handle()], Some(mode))
        .context("step 10: set_crtc")?;
    eprintln!("drm-master-probe: SETCRTC ok");

    // step 11: heartbeat — hold master forever; SIGTERM is the exit path
    let start = std::time::Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let t = start.elapsed().as_secs();
        eprintln!("drm-master-probe: tick t={t}s");
    }
}
