// halmasuit/src/offscreen.rs — test-only offscreen GLES render target.
//
// Entirely `#[cfg(feature = "frame_audit")]` (the `mod` line in
// main.rs is gated). Compiled into `halmasuit-debug`, never into the
// production `halmasuit` binary — the production `cargo tree` stays
// clean (Epic #1 req 6/7; verified by the no-features cargo-tree gate).
//
// Why this module exists: headless under Mesa llvmpipe there is no GPU
// and no GBM scanout buffer that is portably CPU-mappable — the
// `DrmCompositor` swapchain frame is page-flip-owned. To produce a
// pixel-correct, deterministic readback of the *same* scene the
// production pipeline composites, we render the identical element set a
// second time into our OWN offscreen `GlesTexture` (the canonical
// smithay screenshot path: `Offscreen::create_buffer` → `Bind::bind` →
// `OutputDamageTracker::render_output` → `ExportMem::copy_framebuffer`
// → `ExportMem::map_texture`). llvmpipe renders into that texture even
// with no GPU and no GBM allocator, so the readback is exact and
// reproducible run-to-run.
//
// The smithay API used here (`Offscreen<GlesTexture>`, `Bind`,
// `ExportMem`, `OutputDamageTracker`) is pinned at smithay rev
// `ff5fa7df`; the byte order `ExportMem` yields for `Fourcc::Abgr8888`
// is `[R, G, B, A]` per pixel, tightly packed, row-major, no padding —
// the same layout `frame_audit::analyze` consumes.
//
// This module is `unsafe`-free: it composes smithay's safe Bind /
// ExportMem / OutputDamageTracker surface and never touches a raw GL or
// EGL pointer (the only `unsafe` in the renderer path — `EGLDisplay`/
// `GlesRenderer` construction — lives in `drm.rs`, not here).

use std::io;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Offscreen};
use smithay::output::Output;
use smithay::utils::{Point, Rectangle, Size};

/// Render `elements` over `clear_color` into a fresh offscreen
/// `GlesTexture` sized to `output`'s current mode, read it back, and
/// return `(rgba, width_px, height_px)`.
///
/// `rgba` is tightly-packed `[R, G, B, A]` per pixel (the byte order
/// `ExportMem::map_texture` produces for `Fourcc::Abgr8888`),
/// row-major, `width * height * 4` bytes — exactly the layout
/// [`crate::frame_audit::analyze`] and [`expected_solid_frame`] use.
///
/// This is the headless-llvmpipe screenshot path: it does NOT touch
/// the `DrmCompositor` GBM scanout swapchain (which is page-flip-owned
/// and not portably CPU-mappable). It re-composites the identical
/// element set into our own target, so the result is pixel-identical
/// to what was scanned out and deterministic across runs.
///
/// # Errors
///
/// Bubbles any offscreen-allocation, bind, render, or readback failure
/// from smithay with context.
pub fn read_frame_rgba<E>(
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[E],
    clear_color: Color32F,
) -> io::Result<(Vec<u8>, usize, usize)>
where
    E: RenderElement<GlesRenderer>,
{
    let mode = output
        .current_mode()
        .ok_or_else(|| io::Error::other("offscreen: output has no current mode"))?;
    let (w, h) = (mode.size.w, mode.size.h);
    let wu =
        usize::try_from(w).map_err(|_| io::Error::other("offscreen: negative mode width"))?;
    let hu =
        usize::try_from(h).map_err(|_| io::Error::other("offscreen: negative mode height"))?;
    if wu == 0 || hu == 0 {
        return Err(io::Error::other("offscreen: zero mode size"));
    }

    let mut tex: GlesTexture =
        Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, (w, h).into())
            .map_err(|e| io::Error::other(format!("offscreen create_buffer: {e}")))?;
    let rgba = {
        let mut fb = Bind::bind(renderer, &mut tex)
            .map_err(|e| io::Error::other(format!("offscreen bind: {e}")))?;
        let mut dt = OutputDamageTracker::from_output(output);
        dt.render_output(renderer, &mut fb, 0, elements, clear_color)
            .map_err(|e| io::Error::other(format!("offscreen render_output: {e:?}")))?;
        let region = Rectangle::new(Point::from((0, 0)), Size::from((w, h)));
        let mapping = ExportMem::copy_framebuffer(renderer, &fb, region, Fourcc::Abgr8888)
            .map_err(|e| io::Error::other(format!("offscreen copy_framebuffer: {e}")))?;
        // `fb` (and its tex binding) is unused past the readback; drop
        // it before `map_texture` per significant_drop_tightening.
        drop(fb);
        let bytes = ExportMem::map_texture(renderer, &mapping)
            .map_err(|e| io::Error::other(format!("offscreen map_texture: {e}")))?;
        bytes.to_vec()
    };
    Ok((rgba, wu, hu))
}

/// The exact RGBA8 readback a `width * height` frame of pure
/// `clear_rgb` (no client content) must produce: every pixel is
/// `[r, g, b, 0xFF]`, tightly packed, row-major.
///
/// This is the deterministic exact-image reference for the clear scene.
/// The visual gate compares the live offscreen readback against the
/// PNG of this buffer via ssimulacra2 (≥ 95.0); the `frame_audit` unit
/// tests compare [`read_frame_rgba`]'s analyzed output against bytes
/// built here. It replaces the old `mean_luminance < 0.02` /
/// `backdrop_coverage` proxy heuristics with an exact-image model.
#[must_use]
pub fn expected_solid_frame(width: usize, height: usize, clear_rgb: [u8; 3]) -> Vec<u8> {
    let [r, g, b] = clear_rgb;
    let mut v = Vec::with_capacity(width * height * 4);
    for _ in 0..width * height {
        v.extend_from_slice(&[r, g, b, 0xFF]);
    }
    v
}

/// Per-pixel exact match of two RGBA8 readbacks of the same geometry.
///
/// `true` iff both buffers are at least `width * height * 4` bytes and
/// every one of those bytes is identical. This is the deterministic
/// exact-image equality used by the `frame_audit` unit tests; the VM
/// gate uses ssimulacra2 (≥ 95.0) instead so minor llvmpipe rounding
/// is tolerated, never bit-exact equality (epic anti-pattern).
#[must_use]
pub fn frames_exactly_equal(a: &[u8], b: &[u8], width: usize, height: usize) -> bool {
    let n = width * height * 4;
    a.len() >= n && b.len() >= n && a[..n] == b[..n]
}

#[cfg(test)]
mod tests {
    use super::{expected_solid_frame, frames_exactly_equal};
    use crate::drm::CLEAR_RGB;

    #[test]
    fn expected_solid_frame_is_tightly_packed_rgba_with_opaque_alpha() {
        let f = expected_solid_frame(4, 3, CLEAR_RGB);
        assert_eq!(f.len(), 4 * 3 * 4);
        for px in f.chunks_exact(4) {
            assert_eq!(px, [CLEAR_RGB[0], CLEAR_RGB[1], CLEAR_RGB[2], 0xFF]);
        }
    }

    #[test]
    fn frames_exactly_equal_is_true_for_identical_clear_frames() {
        let a = expected_solid_frame(8, 8, CLEAR_RGB);
        let b = expected_solid_frame(8, 8, CLEAR_RGB);
        assert!(frames_exactly_equal(&a, &b, 8, 8));
    }

    #[test]
    fn frames_exactly_equal_is_false_for_one_wrong_pixel() {
        let a = expected_solid_frame(8, 8, CLEAR_RGB);
        let mut b = expected_solid_frame(8, 8, CLEAR_RGB);
        b[0] ^= 0xFF; // corrupt a single channel
        assert!(!frames_exactly_equal(&a, &b, 8, 8));
    }

    #[test]
    fn frames_exactly_equal_is_false_for_black_vs_clear() {
        // The negative-proof model: a black frame must NOT match the
        // brand clear frame (this is the bug the project exists to
        // catch — halmasuit producing black instead of #0a0014).
        let clear = expected_solid_frame(16, 16, CLEAR_RGB);
        let black = expected_solid_frame(16, 16, [0, 0, 0]);
        assert!(!frames_exactly_equal(&clear, &black, 16, 16));
    }

    #[test]
    fn frames_exactly_equal_is_false_for_short_buffer() {
        let a = expected_solid_frame(16, 16, CLEAR_RGB);
        assert!(!frames_exactly_equal(&a, &[0u8; 4], 16, 16));
    }
}
