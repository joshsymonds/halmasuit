// halmasuit/src/frame_audit.rs — test-only frame analysis.
//
// Entirely `#[cfg(feature = "frame_audit")]` (the `mod` line in
// main.rs is gated). Compiled into `halmasuit-debug`, never into the
// production `halmasuit` binary. See Epic #1 req 6/7 and the
// `[features]` comment in Cargo.toml.
//
// `analyze()` is pure arithmetic over an RGBA8 readback (the bytes
// `ExportMem::copy_framebuffer` produces for `Fourcc::Abgr8888`,
// which GL maps to `GL_RGBA` — one pixel = `[R, G, B, A]`). No image
// crate, no img_hash: the perceptual hash is an in-house 8x8
// average-hash so production `cargo tree` stays clean.

/// Brand clear color halmasuit paints before any client commits, as
/// the RGB bytes of an `#0a0014` pixel. A pixel byte-equal to this is
/// "halmasuit's clear" (the uncovered sentinel), not client content;
/// `clear_pixel_count` is the EXACT number of such pixels. Re-exported
/// from [`crate::drm::CLEAR_RGB`] — the single source of truth shared
/// with the renderer clear and the `offscreen` exact-image model — so
/// the audit can never disagree with what was actually painted.
use crate::drm::CLEAR_RGB;

/// Rec.709 perceptual luma of one RGB pixel, normalized `0.0..=1.0`.
fn luma(r: u8, g: u8, b: u8) -> f64 {
    // `mul_add` per clippy::suboptimal_flops — one rounding step,
    // marginally more accurate than the naive sum-of-products.
    let acc = 0.0722_f64.mul_add(
        f64::from(b),
        0.2126_f64.mul_add(f64::from(r), 0.7152 * f64::from(g)),
    );
    acc / 255.0
}

/// Result of analyzing one composited frame. EXACT integer/boolean
/// facts (no fuzzy aggregate floats); field semantics match the
/// like-named `Event::FrameRendered` fields. The no-flash stream gate
/// is asserted over these exact values with zero tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStats {
    /// Total pixels (`width * height`).
    pub pixel_count: u64,
    /// EXACT count of pixels byte-equal to [`CLEAR_RGB`] (the
    /// uncovered-clear sentinel).
    pub clear_pixel_count: u64,
    /// EXACT count of pixels byte-equal to pure black `#000000`.
    pub black_pixel_count: u64,
    /// The frame is all-clear, all-black, or empty — the zero-tolerance
    /// flash predicate.
    pub degenerate: bool,
    /// 64-bit average-hash perceptual fingerprint (exact integer).
    pub phash: u64,
}

/// Analyze an RGBA8 frame readback.
///
/// `bytes` is `width * height` pixels, 4 bytes each (`[R, G, B, A]`),
/// row-major, no padding (the `copy_framebuffer` PBO path is tightly
/// packed). `width`/`height` are pixels.
///
/// # Panics
///
/// Panics if `bytes.len() < width * height * 4` or either dimension is
/// zero — both are caller bugs (the readback size is derived from the
/// output mode), not runtime conditions.
// reason: r,g,b,x,y are the conventional pixel-channel / coordinate
// names; the phash block-luma means stay f64 but pixel/area counts are
// bounded far below 2^52 so the usize->f64 casts there are exact.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_precision_loss,
    reason = "conventional channel/coord names; phash block counts << 2^52 so f64 is exact"
)]
#[must_use]
pub fn analyze(bytes: &[u8], width: usize, height: usize) -> FrameStats {
    assert!(width > 0 && height > 0, "frame_audit: zero-sized frame");
    let px = width * height;
    assert!(
        bytes.len() >= px * 4,
        "frame_audit: readback {} bytes < {} pixels * 4",
        bytes.len(),
        px
    );

    // EXACT integer pixel classification — no luminance/coverage
    // float aggregates. `clear`/`black` are byte-equality counts; a
    // flash is "the clear sentinel showed through" (clear > 0 after
    // the backdrop maps) or "all clear / all black" (degenerate),
    // both decidable with zero tolerance.
    let mut clear = 0_u64;
    let mut black = 0_u64;
    // 8x8 block luma accumulators for the average-hash. The phash is
    // itself an exact integer fingerprint; only its internal block
    // means are floats (content-shift detection, not a flash gate).
    let mut block_sum = [0.0_f64; 64];
    let mut block_cnt = [0_u32; 64];

    for y in 0..height {
        let by = (y * 8 / height).min(7);
        for x in 0..width {
            let i = (y * width + x) * 4;
            let (r, g, b) = (bytes[i], bytes[i + 1], bytes[i + 2]);
            if [r, g, b] == CLEAR_RGB {
                clear += 1;
            }
            if [r, g, b] == [0, 0, 0] {
                black += 1;
            }
            let bx = (x * 8 / width).min(7);
            block_sum[by * 8 + bx] += luma(r, g, b);
            block_cnt[by * 8 + bx] += 1;
        }
    }

    let mut blocks = [0.0_f64; 64];
    let mut blocks_mean = 0.0_f64;
    for (slot, (&sum, &cnt)) in blocks
        .iter_mut()
        .zip(block_sum.iter().zip(block_cnt.iter()))
    {
        // Every block gets >=1 pixel for any width,height >= 8; for
        // tiny test frames a block may be empty — treat as 0.0.
        *slot = if cnt > 0 { sum / f64::from(cnt) } else { 0.0 };
        blocks_mean += *slot;
    }
    blocks_mean /= 64.0;

    let mut phash = 0_u64;
    for (i, &bl) in blocks.iter().enumerate() {
        if bl > blocks_mean {
            phash |= 1 << i;
        }
    }

    let pixel_count = px as u64;
    // Degenerate iff every pixel is the clear sentinel, or every pixel
    // is pure black, or there are no pixels. `analyze` asserts px > 0
    // above, so the empty case is structurally handled by the caller;
    // it is folded in here so the predicate is total over the schema.
    let degenerate = pixel_count == 0 || clear == pixel_count || black == pixel_count;

    FrameStats {
        pixel_count,
        clear_pixel_count: clear,
        black_pixel_count: black,
        degenerate,
        phash,
    }
}

#[cfg(test)]
mod tests {
    use super::{CLEAR_RGB, analyze};
    use crate::offscreen::{expected_solid_frame, frames_exactly_equal};

    fn fill(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        expected_solid_frame(w, h, rgb)
    }

    #[test]
    fn all_clear_color_is_exact_clear_and_degenerate() {
        // Exact-integer model (replaces the old `mean_luminance` /
        // `backdrop_coverage` float proxy): a frame of pure clear
        // color is byte-for-byte the canonical clear readback, EVERY
        // pixel counts as clear, and the frame is `degenerate` (the
        // zero-tolerance flash predicate fires — a fully-clear frame
        // after the backdrop maps is exactly the flash). No luminance
        // threshold anywhere.
        let frame = fill(16, 16, CLEAR_RGB);
        assert!(frames_exactly_equal(
            &frame,
            &expected_solid_frame(16, 16, CLEAR_RGB),
            16,
            16
        ));
        let s = analyze(&frame, 16, 16);
        assert_eq!(s.pixel_count, 256);
        assert_eq!(s.clear_pixel_count, 256);
        assert_eq!(s.black_pixel_count, 0);
        assert!(s.degenerate);
    }

    #[test]
    fn black_frame_is_exact_black_and_degenerate() {
        // The bug this project exists to catch: halmasuit painting
        // black instead of #0a0014. Exact-image inequality fires AND
        // the all-black frame reads as degenerate with an exact black
        // pixel count == pixel_count and zero clear pixels.
        let black = fill(16, 16, [0, 0, 0]);
        assert!(!frames_exactly_equal(
            &black,
            &expected_solid_frame(16, 16, CLEAR_RGB),
            16,
            16
        ));
        let s = analyze(&black, 16, 16);
        assert_eq!(s.black_pixel_count, 256);
        assert_eq!(s.clear_pixel_count, 0);
        assert!(s.degenerate);
    }

    #[test]
    fn all_white_is_fully_painted_not_degenerate() {
        // Solid client content (not clear, not black): zero clear
        // pixels, zero black pixels, NOT degenerate — a healthy
        // fully-covered frame.
        let s = analyze(&fill(16, 16, [255, 255, 255]), 16, 16);
        assert_eq!(s.clear_pixel_count, 0);
        assert_eq!(s.black_pixel_count, 0);
        assert!(!s.degenerate);
    }

    #[test]
    fn half_clear_half_content_is_exact_clear_count_not_degenerate() {
        // Left half clear sentinel, right half green. The exact clear
        // count is half the pixels — a partial flash the stream gate
        // catches via `clear_pixel_count != 0` (NOT a >0.95 coverage
        // float). Not degenerate (neither all-clear nor all-black).
        let (w, h) = (16, 16);
        let mut v = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                let p = if x < w / 2 {
                    [CLEAR_RGB[0], CLEAR_RGB[1], CLEAR_RGB[2], 0xFF]
                } else {
                    [0x16, 0xC4, 0x4E, 0xFF]
                };
                v.extend_from_slice(&p);
            }
        }
        let s = analyze(&v, w, h);
        assert_eq!(s.clear_pixel_count, 128);
        assert_eq!(s.pixel_count, 256);
        assert!(!s.degenerate);
    }

    #[test]
    fn single_clear_pixel_is_caught_exactly() {
        // Zero-tolerance: ONE sentinel-clear pixel in an otherwise
        // fully-painted frame is a nonzero exact count. The stream
        // gate asserts == 0, so this single pixel is a detected flash
        // (a fuzzy >0.95-coverage proxy would have rounded it away).
        let (w, h) = (32, 32);
        let mut v = expected_solid_frame(w, h, [0x16, 0xC4, 0x4E]);
        v[0] = CLEAR_RGB[0];
        v[1] = CLEAR_RGB[1];
        v[2] = CLEAR_RGB[2];
        let s = analyze(&v, w, h);
        assert_eq!(s.clear_pixel_count, 1);
        assert!(!s.degenerate);
    }

    #[test]
    fn phash_distinguishes_uniform_from_patterned() {
        let uniform = analyze(&fill(64, 64, [128, 128, 128]), 64, 64);
        // Top half bright, bottom half dark.
        let (w, h) = (64, 64);
        let mut v = Vec::new();
        for y in 0..h {
            for _x in 0..w {
                let c = if y < h / 2 { 220 } else { 10 };
                v.extend_from_slice(&[c, c, c, 0xFF]);
            }
        }
        let patterned = analyze(&v, w, h);
        assert_ne!(uniform.phash, patterned.phash);
        // The patterned phash must have its top-4 rows set and
        // bottom-4 clear (bright > frame-mean on top).
        assert_eq!(patterned.phash & 0x0000_0000_FFFF_FFFF, 0xFFFF_FFFF);
        assert_eq!(patterned.phash & 0xFFFF_FFFF_0000_0000, 0);
    }

    #[test]
    #[should_panic(expected = "zero-sized frame")]
    fn zero_dim_panics() {
        let _ = analyze(&[], 0, 0);
    }

    #[test]
    #[should_panic(expected = "< ")]
    fn short_buffer_panics() {
        let _ = analyze(&[0; 4], 16, 16);
    }
}
