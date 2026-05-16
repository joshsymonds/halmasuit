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

/// Brand clear color halmasuit paints before any client commits,
/// as the RGB bytes of an `#0a0014` pixel. A pixel exactly equal to
/// this is "halmasuit's clear", not client content; `backdrop_coverage`
/// is the fraction of pixels that differ from it.
const CLEAR_RGB: [u8; 3] = [0x0a, 0x00, 0x14];

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

/// Result of analyzing one composited frame. Field semantics match
/// the like-named `Event::FrameRendered` fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameStats {
    pub mean_luminance: f64,
    pub backdrop_coverage: f64,
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
// names; pixel and area counts are bounded far below 2^52 so the
// usize->f64 casts are exact, not lossy.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_precision_loss,
    reason = "conventional channel/coord names; counts << 2^52 so f64 is exact"
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

    let mut luma_sum = 0.0_f64;
    let mut non_clear = 0_usize;
    // 8x8 block luma accumulators for the average-hash.
    let mut block_sum = [0.0_f64; 64];
    let mut block_cnt = [0_u32; 64];

    for y in 0..height {
        let by = (y * 8 / height).min(7);
        for x in 0..width {
            let i = (y * width + x) * 4;
            let (r, g, b) = (bytes[i], bytes[i + 1], bytes[i + 2]);
            let lum = luma(r, g, b);
            luma_sum += lum;
            if [r, g, b] != CLEAR_RGB {
                non_clear += 1;
            }
            let bx = (x * 8 / width).min(7);
            block_sum[by * 8 + bx] += lum;
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

    FrameStats {
        mean_luminance: luma_sum / px as f64,
        backdrop_coverage: non_clear as f64 / px as f64,
        phash,
    }
}

#[cfg(test)]
mod tests {
    use super::{CLEAR_RGB, analyze, luma};

    const EPS: f64 = 1e-12;

    fn fill(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            v.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0xFF]);
        }
        v
    }

    #[test]
    fn all_clear_color_is_zero_coverage_and_dim() {
        let s = analyze(&fill(16, 16, CLEAR_RGB), 16, 16);
        assert!(s.backdrop_coverage.abs() < EPS);
        // luma(#0a0014) is tiny but nonzero.
        let expected = luma(0x0a, 0x00, 0x14);
        assert!((s.mean_luminance - expected).abs() < 1e-9);
        assert!(s.mean_luminance < 0.02);
    }

    #[test]
    fn all_white_is_full_coverage_and_bright() {
        let s = analyze(&fill(16, 16, [255, 255, 255]), 16, 16);
        assert!((s.backdrop_coverage - 1.0).abs() < EPS);
        assert!((s.mean_luminance - 1.0).abs() < 1e-9);
    }

    #[test]
    fn all_black_is_full_coverage_and_dark() {
        // Black (0,0,0) is NOT the clear color, so a client painting
        // black still counts as covered — and it must read as a black
        // frame (mean_luminance ~0) so the no-black-frame invariant
        // can fire.
        let s = analyze(&fill(16, 16, [0, 0, 0]), 16, 16);
        assert!((s.backdrop_coverage - 1.0).abs() < EPS);
        assert!(s.mean_luminance.abs() < EPS);
    }

    #[test]
    fn half_covered_is_half_coverage() {
        // Left half clear, right half green.
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
        assert!((s.backdrop_coverage - 0.5).abs() < 1e-9);
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
