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

/// Encode `#RRGGBB` as XRGB8888 little-endian for filling a dumb
/// buffer: byte order is `[B, G, R, X]`. The X byte is always zero
/// (alpha is ignored in XRGB).
///
/// `const fn` so it can build the brand-clear constant at compile
/// time. Pinned by `xrgb_le_pins_byte_order` below — any change to
/// the byte layout that breaks the visual goldens trips the unit
/// test first.
#[must_use]
pub const fn xrgb_le(r: u8, g: u8, b: u8) -> [u8; 4] {
    [b, g, r, 0]
}

/// Bundle of state pinned for the process lifetime once halmasuit
/// owns the scanout.
///
/// **Happy-path drop order:**
/// 1. `_dumb` drops first, destroying the kernel-side dumb buffer.
/// 2. `card` drops last, closing the DRM fd; the kernel releases
///    master designation and reaps any still-registered framebuffer
///    handle automatically.
///
/// We don't explicitly destroy the framebuffer / clear the CRTC: fd
/// close + dumb-buffer destroy is sufficient cleanup and matches the
/// pattern `drm-master-probe` uses successfully across SIGTERM.
///
/// **Error-path cleanup** (for callers of `scan_out_clear_color`): if
/// `add_framebuffer` or `set_crtc` returns Err, the `ActiveScanout`
/// is never constructed. Cleanup happens via locals dropping at
/// function return: the `DumbBuffer` RAII-destroys, and the
/// fresh `framebuffer::Handle` (if `add_framebuffer` succeeded but
/// `set_crtc` failed) has no Drop impl but is reaped by the kernel
/// when the `Card`'s fd closes at end of scope. As B.2 adds GBM /
/// EGL allocations into this constructor, the implicit story will
/// need to become an explicit builder or RAII guard.
#[expect(
    dead_code,
    reason = "card + mode are part of the public seam B.2 reads; the underscore-prefixed fields are RAII-only and intentionally unread"
)]
pub struct ActiveScanout {
    /// DRM device file (master designation lives on this fd). Held
    /// for the process lifetime; the GLES + DrmCompositor subtask
    /// (B.2) will take a `&Card` from here to bind its GBM allocator
    /// against the same device.
    pub card: Card,
    /// Dumb buffer with the rendered pixels. RAII-destroys on drop.
    _dumb: DumbBuffer,
    /// Framebuffer handle wrapping `_dumb`. No `Drop`; the kernel
    /// reaps it when the `card` fd closes.
    _fb: framebuffer::Handle,
    /// CRTC currently driven by `_fb`.
    _crtc: crtc::Handle,
    /// Connector receiving scan-out from `_crtc`.
    _connector: connector::Handle,
    /// Mode in effect at SETCRTC time. The B.2 GLES subtask reads
    /// this to size the GBM-backed framebuffer to the same dimensions.
    pub mode: Mode,
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
/// dumb buffer. `color` is XRGB8888 little-endian (`[B, G, R, X]`) —
/// use [`xrgb_le`] to build the byte array from a logical `#RRGGBB`.
///
/// Returns the `ActiveScanout` that must be retained for the process
/// lifetime; dropping it tears down the scanout cleanly.
///
/// # Errors
///
/// Bubbles any DRM ioctl failure, plus explicit `io::Error::other`
/// bails for the structurally-invalid environments (no connected
/// connector, connector has no modes, no CRTCs available). Error
/// returns clean up via the function-scope drop chain documented on
/// [`ActiveScanout`].
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
        card,
        _dumb: dumb,
        _fb: fb,
        _crtc: crtc_handle,
        _connector: connector,
        mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the `xrgb_le` byte ordering. The visual goldens depend on
    /// `[B, G, R, X]` little-endian for XRGB8888; a refactor that
    /// transposes channels would trip this before any VM test runs.
    /// Pure-function unit test; no DRM device required.
    #[test]
    fn xrgb_le_pins_byte_order() {
        // #0a0014 → red=0x0a, green=0x00, blue=0x14
        // little-endian XRGB8888 → [B, G, R, X] = [0x14, 0x00, 0x0a, 0x00]
        assert_eq!(xrgb_le(0x0A, 0x00, 0x14), [0x14, 0x00, 0x0A, 0x00]);

        // Pure red, green, blue, white sanity cases.
        assert_eq!(xrgb_le(0xFF, 0x00, 0x00), [0x00, 0x00, 0xFF, 0x00]);
        assert_eq!(xrgb_le(0x00, 0xFF, 0x00), [0x00, 0xFF, 0x00, 0x00]);
        assert_eq!(xrgb_le(0x00, 0x00, 0xFF), [0xFF, 0x00, 0x00, 0x00]);
        assert_eq!(xrgb_le(0xFF, 0xFF, 0xFF), [0xFF, 0xFF, 0xFF, 0x00]);
    }
}
