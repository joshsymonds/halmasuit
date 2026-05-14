// halmasuit/src/drm.rs — DRM backend (B.1 slice: dumb-buffer scanout).
//
// First production wiring of halmasuit's display ownership. Opens
// /dev/dri/card0 (or HALMASUIT_DRM_DEVICE), becomes DRM master via
// `acquire_master_lock`, picks the first connected connector + its
// preferred mode, allocates an XRGB8888 dumb buffer at that
// resolution, fills it with a caller-supplied clear color, wraps it
// as a DRM framebuffer, and drives the CRTC via SETCRTC. Returns an
// `ActiveScanout` whose drop releases all of it cleanly.
//
// This is the simplest path that proves halmasuit can drive a real
// display end-to-end. The GLES + GBM + DrmCompositor stack
// (epic Approach B.2) layers on top of this — same Card, same
// CRTC/connector pair, different buffer allocator and renderer.
//
// Pattern mirrors `drm-master-probe` and `halmasuit-visual-test-standin`:
// a `Card` newtype around `std::fs::File` with `AsFd` + the two
// `drm-rs` marker traits, then a small `ActiveScanout` value that
// pins every handle for the process lifetime so the kernel keeps
// scanning out our pixels.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;

use drm::Device;
use drm::buffer::DrmFourcc;
use drm::control::{
    Device as ControlDevice, Mode, connector, crtc, dumbbuffer::DumbBuffer, framebuffer,
};

/// DRM device handle. Newtype around `std::fs::File`; implements the
/// two `drm-rs` marker traits via `AsFd` so the device API is reachable
/// directly on `card` values.
pub struct Card(pub std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

/// Bundle of state pinned for the process lifetime once halmasuit
/// owns the scanout. Dropping this value:
///
/// 1. Drops `_dumb`, which destroys the kernel-side dumb buffer.
/// 2. Drops `card`, which closes the DRM fd; the kernel releases
///    master designation, the CRTC reverts to whatever the firmware
///    handoff left, and the framebuffer handle is reaped automatically.
///
/// We don't explicitly destroy the framebuffer / clear the CRTC: fd
/// close + dumb-buffer destroy is sufficient cleanup and matches the
/// pattern `drm-master-probe` uses successfully across SIGTERM.
pub struct ActiveScanout {
    /// DRM device file (master designation lives on this fd). Held
    /// for the process lifetime; the GLES + DrmCompositor subtask
    /// (B.2) will take a `&Card` from here to bind its GBM allocator
    /// against the same device.
    pub _card: Card,
    /// Dumb buffer with the rendered pixels. RAII-destroys on drop.
    pub _dumb: DumbBuffer,
    /// Framebuffer handle wrapping `_dumb`.
    pub _fb: framebuffer::Handle,
    /// CRTC currently driven by `_fb`.
    pub _crtc: crtc::Handle,
    /// Connector receiving scan-out from `_crtc`.
    pub _connector: connector::Handle,
    /// Mode in effect at SETCRTC time. The B.2 GLES subtask will read
    /// this to size the GBM-backed framebuffer to the same dimensions.
    pub _mode: Mode,
}

/// Open `path` for DRM access and acquire master via the drm-rs
/// typed wrapper (`acquire_master_lock`, which is the same
/// `DRM_IOCTL_SET_MASTER` ioctl as the previous raw-nix
/// implementation, but with a typed Error).
pub fn open_and_set_master(path: &Path) -> io::Result<Card> {
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| io::Error::other(format!("open({}): {e}", path.display())))?;
    let card = Card(dev);
    card.acquire_master_lock()
        .map_err(|e| io::Error::other(format!("DRM SET_MASTER on {}: {e}", path.display())))?;
    Ok(card)
}

/// Mode-set the first connected connector to scan out a solid-color
/// dumb buffer. `color` is XRGB8888 little-endian (`[B, G, R, X]`).
///
/// Returns the `ActiveScanout` that must be retained for the process
/// lifetime; dropping it tears down the scanout cleanly.
///
/// # Errors
///
/// Bubbles any DRM ioctl failure, plus explicit `io::Error::other`
/// bails for the structurally-invalid environments (no connected
/// connector, connector has no modes, no CRTCs available).
pub fn scan_out_clear_color(card: Card, color: [u8; 4]) -> io::Result<ActiveScanout> {
    let res = card
        .resource_handles()
        .map_err(|e| io::Error::other(format!("resource_handles: {e}")))?;

    let connector_info = res
        .connectors()
        .iter()
        .filter_map(|&h| card.get_connector(h, true).ok())
        .find(|info| info.state() == connector::State::Connected)
        .ok_or_else(|| io::Error::other("no connected DRM connector"))?;

    let mode = *connector_info
        .modes()
        .first()
        .ok_or_else(|| io::Error::other("connected DRM connector has no modes"))?;
    let (w, h) = mode.size();

    let crtc_handle = *res
        .crtcs()
        .first()
        .ok_or_else(|| io::Error::other("no DRM CRTCs available"))?;

    let mut dumb = card
        .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)
        .map_err(|e| io::Error::other(format!("create_dumb_buffer: {e}")))?;

    {
        let mut map = card
            .map_dumb_buffer(&mut dumb)
            .map_err(|e| io::Error::other(format!("map_dumb_buffer: {e}")))?;
        for pixel in map.as_mut().chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
    }

    let fb = card
        .add_framebuffer(&dumb, 24, 32)
        .map_err(|e| io::Error::other(format!("add_framebuffer: {e}")))?;

    let connector = connector_info.handle();
    card.set_crtc(crtc_handle, Some(fb), (0, 0), &[connector], Some(mode))
        .map_err(|e| io::Error::other(format!("set_crtc: {e}")))?;

    Ok(ActiveScanout {
        _card: card,
        _dumb: dumb,
        _fb: fb,
        _crtc: crtc_handle,
        _connector: connector,
        _mode: mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the `ActiveScanout` and `Card` types are `Sized`
    /// and their public surfaces compile. Hardware-touching paths
    /// require a real DRM device and are exercised by
    /// `tests/visual-halmasuit-clear.nix`. This test exists so a
    /// future refactor that breaks the public surface fails at the
    /// unit level before VM tests are exercised.
    #[test]
    fn types_have_known_sizes() {
        fn assert_sized<T: Sized>() {}
        assert_sized::<ActiveScanout>();
        assert_sized::<Card>();
    }
}
