//! Epic #71 R-honest.5 — Tier-1 bitmap console font + glyph blitter.
//!
//! The diagnostic overlay (R-honest.6) renders its text — current
//! phase, broker state, window list, journal tail — with this. A
//! crisp 8x8 console bitmap is the *right* aesthetic for a display
//! server's emergency diagnostic surface (it reads as "the machine
//! talking to you", the fbcon / kernel-panic vocabulary), and it
//! avoids the heavy font stack the epic forbids: NO cosmic-text,
//! fontdb, swash, rustybuzz, system-font discovery, or libclang.
//!
//! Glyph data is the public-domain `font8x8` basic-latin table,
//! VENDORED into [`FONT8X8_BASIC`] below (copied verbatim from the
//! `font8x8` crate's `legacy::BASIC_LEGACY`, MIT/Joaquin Rosales,
//! itself the dhepper/font8x8 public-domain font). It is vendored
//! rather than taken as a `crates.io` dependency because the nix
//! build sandbox cannot reach crates.io (403) — same hand-carried-
//! dep posture as the rest of halmasuit. Bit order: each of the 8
//! row-bytes has **bit 0 = leftmost pixel** (LSB-left).

// The blitter + helpers are consumed by the diagnostic-overlay
// renderer in R-honest.6. Landed here standalone + unit-tested
// first. Same staged-surface pattern as vt_switch.rs.
#![allow(
    dead_code,
    reason = "consumed by the diagnostic overlay renderer in R-honest.6"
)]

include!("console_font_data.rs");

/// Glyph cell dimensions (px). font8x8 is 8x8.
pub const GLYPH_W: i32 = 8;
pub const GLYPH_H: i32 = 8;

/// The 8 row-bytes for `ch`. ASCII (0x00–0x7F) indexes the table
/// directly; anything else falls back to `'?'` (0x3F) so arbitrary
/// journal text never panics or renders an out-of-range cell.
#[must_use]
pub const fn glyph(ch: char) -> [u8; 8] {
    let code = ch as u32;
    if code < 128 {
        FONT8X8_BASIC[code as usize]
    } else {
        FONT8X8_BASIC['?' as usize]
    }
}

/// Pixel dimensions `(w, h)` that [`blit_str`] will occupy for
/// `text`: width = longest line × `GLYPH_W`, height = line count ×
/// `GLYPH_H`. Lets the overlay size/position its panel before
/// blitting.
#[must_use]
pub fn text_dims(text: &str) -> (i32, i32) {
    let mut max_cols = 0_i32;
    let mut cols = 0_i32;
    let mut lines = 1_i32;
    for ch in text.chars() {
        if ch == '\n' {
            max_cols = max_cols.max(cols);
            cols = 0;
            lines += 1;
        } else {
            cols += 1;
        }
    }
    max_cols = max_cols.max(cols);
    (max_cols * GLYPH_W, lines * GLYPH_H)
}

/// Blit `text` into the RGBA8 buffer `buf` (row stride `stride_px`
/// pixels = `stride_px * 4` bytes; `height_px` rows) at top-left
/// `(x, y)`, painting each set glyph bit as `rgba` (opaque
/// overwrite). `'\n'` returns the cursor to the starting column and
/// advances one glyph row. Every write is bounds-checked: pixels
/// outside `[0, stride_px) × [0, height_px)` (or past `buf`'s end)
/// are skipped, so blitting partially- or fully-off-buffer is safe.
///
/// Monospace, 8px advance per glyph (font8x8 glyphs carry their own
/// right-side spacing).
pub fn blit_str(
    buf: &mut [u8],
    stride_px: usize,
    height_px: usize,
    x: i32,
    y: i32,
    text: &str,
    rgba: [u8; 4],
) {
    let start_x = x;
    let mut cx = x;
    let mut cy = y;
    for ch in text.chars() {
        if ch == '\n' {
            cx = start_x;
            cy += GLYPH_H;
            continue;
        }
        let rows = glyph(ch);
        for (row, &bits) in rows.iter().enumerate() {
            // `usize::try_from` rejects negative coords (off the top),
            // so no `< 0` check + no sign-losing `as usize` cast.
            let Ok(py) = usize::try_from(cy + i32::try_from(row).expect("row < 8 fits i32")) else {
                continue;
            };
            if py >= height_px {
                continue;
            }
            for col in 0..8_i32 {
                // font8x8: bit 0 = leftmost pixel. Column `col` of the
                // glyph maps to bit `col` of the row byte.
                if bits & (1 << col) == 0 {
                    continue;
                }
                let Ok(px) = usize::try_from(cx + col) else {
                    continue;
                };
                if px >= stride_px {
                    continue;
                }
                let idx = (py * stride_px + px) * 4;
                if let Some(slot) = buf.get_mut(idx..idx + 4) {
                    slot.copy_from_slice(&rgba);
                }
            }
        }
        cx += GLYPH_W;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Space is blank; ASCII indexes the table directly; out-of-range
    /// chars fall back to `'?'` (never an OOB index / panic).
    #[test]
    fn glyph_indexing_and_fallback() {
        assert_eq!(glyph(' '), [0x00; 8], "space is all-zero");
        assert_eq!(glyph('A'), FONT8X8_BASIC['A' as usize]);
        // A non-ASCII char renders the '?' glyph, not garbage / panic.
        assert_eq!(glyph('\u{1F600}'), FONT8X8_BASIC['?' as usize]);
        assert_eq!(glyph('é'), FONT8X8_BASIC['?' as usize]);
    }

    /// `text_dims` matches the line/column geometry `blit_str` walks.
    #[test]
    fn text_dims_counts_lines_and_longest_row() {
        assert_eq!(text_dims(""), (0, GLYPH_H));
        assert_eq!(text_dims("abc"), (3 * GLYPH_W, GLYPH_H));
        assert_eq!(text_dims("ab\ncde"), (3 * GLYPH_W, 2 * GLYPH_H));
        assert_eq!(text_dims("\n"), (0, 2 * GLYPH_H));
    }

    /// Bit order is load-bearing: font8x8 puts bit 0 at the LEFTMOST
    /// pixel. 'L' has a serif top + a full bottom bar (row byte
    /// `0x7F` = bits 0–6 set, bit 7 clear). Under the correct
    /// LSB-left mapping the bottom bar fills columns 0–6 and leaves
    /// column 7 clear; a flipped (MSB-left) blitter would mirror it
    /// (column 0 clear, column 7 set). Assert the correct direction.
    #[test]
    fn blit_str_bit_order_is_lsb_left() {
        // 8x8 black buffer, blit a white 'L' at the origin.
        let stride = 8;
        let height = 8;
        let mut buf = vec![0_u8; stride * height * 4];
        blit_str(&mut buf, stride, height, 0, 0, "L", [255, 255, 255, 255]);

        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * stride + x) * 4;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        let on = [255, 255, 255, 255];
        let off = [0, 0, 0, 0];

        // 'L' = [0x0F,0x06,0x06,0x06,0x46,0x66,0x7F,0x00]. Bottom bar
        // is row 6 (0x7F): cols 0..=6 ON, col 7 OFF (LSB-left).
        for x in 0..=6 {
            assert_eq!(px(x, 6), on, "row6 col{x} must be set (bottom bar)");
        }
        assert_eq!(px(7, 6), off, "row6 col7 must be clear (LSB-left)");
        // Top serif row 0 (0x0F): cols 0..=3 ON, cols 4..=7 OFF.
        for x in 0..=3 {
            assert_eq!(px(x, 0), on, "row0 col{x} must be set (top serif)");
        }
        assert_eq!(px(4, 0), off, "row0 col4 must be clear");
        // Last row (0x00) is entirely blank.
        for x in 0..8 {
            assert_eq!(px(x, 7), off, "row7 must be blank");
        }
    }

    /// Blitting off the buffer edges must clip, never panic or write
    /// out of bounds. Exercise negative coords, past-right, and
    /// past-bottom against a tiny buffer.
    #[test]
    fn blit_str_clips_without_oob() {
        let stride = 4;
        let height = 4;
        // Negative origin (top-left corner of the glyph off-screen).
        let mut buf = vec![0_u8; stride * height * 4];
        blit_str(&mut buf, stride, height, -3, -3, "A", [1, 2, 3, 4]);
        // Far past the right/bottom edge — nothing should be written.
        let mut buf2 = vec![0_u8; stride * height * 4];
        blit_str(&mut buf2, stride, height, 100, 100, "ABC", [1, 2, 3, 4]);
        assert!(
            buf2.iter().all(|&b| b == 0),
            "off-buffer blit wrote nothing"
        );
        // A multi-line string straddling the bottom edge.
        let mut buf3 = vec![0_u8; stride * height * 4];
        blit_str(&mut buf3, stride, height, 0, 0, "AB\nCD\nEF", [9, 9, 9, 9]);
        // (No panic / OOB is the assertion; the harness aborts on either.)
        let _ = (buf, buf3);
    }

    /// Newline returns to the start column and advances one glyph
    /// row: the char after '\n' lands at (start_x, y + GLYPH_H).
    #[test]
    fn blit_str_newline_wraps_to_start_column() {
        // Wide buffer: 2 glyph rows tall, 2 glyphs wide.
        let stride = (2 * GLYPH_W) as usize;
        let height = (2 * GLYPH_H) as usize;
        let mut buf = vec![0_u8; stride * height * 4];
        // "L\nL": second 'L' must start at column 0 of the SECOND row.
        blit_str(&mut buf, stride, height, 0, 0, "L\nL", [255, 0, 0, 255]);
        let px = |x: usize, y: usize| -> u8 { buf[(y * stride + x) * 4] };
        // Bottom bar of the first 'L' is at row 6 (cols 0..6).
        assert_eq!(px(0, 6), 255, "first L bottom bar present");
        // Bottom bar of the second 'L' is at row GLYPH_H + 6 = 14.
        assert_eq!(
            px(0, 14),
            255,
            "second L starts at column 0 of next glyph row"
        );
        // The second 'L' did NOT advance horizontally (no stray pixel
        // at the second glyph column on the first row).
        assert_eq!(
            px(GLYPH_W as usize, 14),
            0,
            "second L is at column 0, not 8"
        );
    }
}
