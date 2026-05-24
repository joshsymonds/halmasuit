// R8b-render — Xcursor theme loader + named-cursor pixmap cache.
//
// Halmasuit owns the cursor render for any pointer position over its
// own surface tree (background + the foreground client when that
// client isn't a nested compositor drawing its own cursor). The
// Wayland model is server-draws: a client tells the server "use this
// surface as the cursor" or "use this named shape" via
// `wl_pointer.set_cursor` / `wp_cursor_shape_device.set_shape`; the
// server composites the result at `pointer_location - hotspot`.
//
// A nested compositor (niri-as-session, Quickshell-as-greeter) draws
// its OWN cursor into its OWN output buffer and calls
// `wl_pointer.set_cursor(NULL)` on halmasuit's pointer to hide
// halmasuit's. That path keeps working with this module live: when
// the focused client sets `CursorImageStatus::Hidden`,
// `render_elements` returns an empty Vec and nothing composites.
//
// Theme convention: read `$XCURSOR_THEME` (default `default`) and
// `$XCURSOR_SIZE` (default 24) at startup; the NixOS module sets
// these in halmasuit.service's env and the broker propagates them
// into the session leader's exec env (Amendment A9 allowlist) so
// halmasuit and its child compositors render the same cursor theme.
//
// Fallback: when no theme is reachable (e.g. `XCURSOR_THEME=missing`
// or the headless test VM has no cursor themes installed), render a
// minimal procedural arrow into an RGBA buffer — invariant: a
// non-empty cursor image always exists, so the render path doesn't
// have to handle "theme missing" as a separate case.

use std::sync::Arc;
use std::time::Duration;

use xcursor::CursorTheme;
use xcursor::parser::{Image, parse_xcursor};

/// One loaded Xcursor name (e.g. `default`, `pointer`, `text`) — a
/// set of `Image` frames at varying sizes / animation positions.
/// `xcursor::parser::Image::pixels_rgba` is the byte layout the
/// `MemoryRenderBuffer` consumer wants (RGBA8, row-major,
/// `width*height*4` bytes).
#[derive(Clone)]
pub struct CursorIcons {
    icons: Arc<Vec<Image>>,
    nominal_size: u32,
}

impl CursorIcons {
    fn from_theme(theme: &CursorTheme, name: &str, size: u32) -> Option<Self> {
        let path = theme.load_icon(name)?;
        let bytes = std::fs::read(&path).ok()?;
        let icons = parse_xcursor(&bytes)?;
        if icons.is_empty() {
            return None;
        }
        Some(Self {
            icons: Arc::new(icons),
            nominal_size: size,
        })
    }

    /// Animation-aware frame selection. Picks the icon matching the
    /// nominal size, walks the animation timeline modulo total
    /// duration, returns the active frame's pixels + hotspot.
    pub fn current_frame(&self, time: Duration) -> &Image {
        // Choose the size variant nearest to `nominal_size`.
        let nearest = self
            .icons
            .iter()
            .min_by_key(|img| (i64::from(img.size) - i64::from(self.nominal_size)).abs())
            .expect("constructor guarantees non-empty icons");
        let variants: Vec<&Image> = self
            .icons
            .iter()
            .filter(|img| img.width == nearest.width && img.height == nearest.height)
            .collect();
        let total: u32 = variants.iter().map(|img| img.delay).sum();
        if total == 0 {
            return variants[0];
        }
        // `time.as_millis()` saturates at u128; clamp to u32 for the
        // modulo. Any frame past u32::MAX ms (~49 days) wraps — that
        // is the wl_pointer.frame time convention.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "u128 → u32 truncation is the Wayland animation-cursor convention; \
                      the wraparound is the modulo we want"
        )]
        let mut millis = time.as_millis() as u32 % total;
        for img in variants {
            if millis < img.delay {
                return img;
            }
            millis -= img.delay;
        }
        unreachable!("modulo-total above guarantees a frame is selected")
    }
}

/// Per-compositor cursor theme. Carries the loaded `default` icon
/// (the most common request — when no client has called `set_cursor`
/// halmasuit shows this) and lazily loads other named icons on
/// demand.
pub struct CursorTheming {
    theme: Option<CursorTheme>,
    size: u32,
    default: CursorIcons,
}

impl CursorTheming {
    /// Load the theme named by `$XCURSOR_THEME` (default `default`)
    /// at size `$XCURSOR_SIZE` (default 24). Always succeeds — on
    /// any failure, returns a theme whose only icon is the
    /// procedural arrow fallback.
    pub fn load() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);
        let theme = CursorTheme::load(&name);
        let default = CursorIcons::from_theme(&theme, "default", size)
            .or_else(|| CursorIcons::from_theme(&theme, "left_ptr", size))
            .unwrap_or_else(|| Self::procedural_fallback(size));
        Self {
            theme: Some(theme),
            size,
            default,
        }
    }

    /// Load `name` (e.g. `pointer`, `text`, `wait`). Falls back to
    /// the `default` icon if the theme has no entry for this name.
    pub fn load_named(&self, name: &str) -> CursorIcons {
        self.theme
            .as_ref()
            .and_then(|t| CursorIcons::from_theme(t, name, self.size))
            .unwrap_or_else(|| self.default.clone())
    }

    /// Build a procedural fallback when no theme is reachable. A
    /// solid-white northwest-pointing triangle bounded inside a
    /// `size × size` RGBA8 buffer with hotspot at (0, 0). Not
    /// pretty, but visible (clearly not the brand-clear color, so
    /// `assert_no_flash_stream` sees real content; matches no theme
    /// so the user can tell the theme didn't load).
    fn procedural_fallback(size: u32) -> CursorIcons {
        let s = size.max(8) as usize;
        let mut pixels = vec![0u8; s * s * 4];
        for y in 0..s {
            for x in 0..s {
                // Triangle: keep (x, y) where x <= y and y <= s - x.
                // That carves a wedge from the top-left corner down
                // and to the right — a primitive arrow tip.
                if x <= y && (x + y) <= s {
                    let i = (y * s + x) * 4;
                    pixels[i] = 0xff; // R
                    pixels[i + 1] = 0xff; // G
                    pixels[i + 2] = 0xff; // B
                    pixels[i + 3] = 0xff; // A
                }
            }
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "size argument is always ≤ a reasonable cursor dimension; \
                      truncation to u16 fits the hotspot/dimension contract of xcursor::Image"
        )]
        let dim = s as u16;
        CursorIcons {
            icons: Arc::new(vec![Image {
                size,
                width: u32::from(dim),
                height: u32::from(dim),
                xhot: 0,
                yhot: 0,
                delay: 0,
                pixels_rgba: pixels,
                pixels_argb: vec![],
            }]),
            nominal_size: size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Procedural fallback produces an opaque, partially-white
    /// triangle in a `size x size` buffer. Load-bearing invariant:
    /// a non-empty cursor image always exists, so the render path
    /// doesn't need a separate "theme missing" branch.
    #[test]
    fn procedural_fallback_has_visible_pixels() {
        let icons = CursorTheming::procedural_fallback(24);
        let frame = icons.current_frame(Duration::ZERO);
        assert_eq!(frame.width, 24);
        assert_eq!(frame.height, 24);
        let expected = u32::try_from(frame.pixels_rgba.len())
            .expect("pixel buffer length fits u32 for cursor dimensions");
        assert_eq!(
            expected,
            frame.width * frame.height * 4,
            "RGBA buffer must be width*height*4 bytes"
        );
        let opaque_count = frame
            .pixels_rgba
            .chunks_exact(4)
            .filter(|p| p[3] == 0xff)
            .count();
        assert!(
            opaque_count > 0,
            "fallback must have at least one opaque pixel — \
             a fully-transparent cursor is invisible"
        );
        assert!(
            opaque_count < (24 * 24),
            "fallback shouldn't fill the entire bounding box — \
             that's a square, not a cursor"
        );
    }

    /// Procedural fallback honors the requested nominal size.
    #[test]
    fn procedural_fallback_respects_size() {
        let icons = CursorTheming::procedural_fallback(48);
        let frame = icons.current_frame(Duration::ZERO);
        assert_eq!(frame.width, 48);
        assert_eq!(frame.height, 48);
    }

    /// Minimum-size clamp: a sub-8 size still produces a visible,
    /// well-formed image (clamped to ≥ 8 in the procedural fallback
    /// path so we don't degenerate to a 1px nothing).
    #[test]
    fn procedural_fallback_clamps_minimum_size() {
        let icons = CursorTheming::procedural_fallback(3);
        let frame = icons.current_frame(Duration::ZERO);
        assert!(frame.width >= 8, "expected clamp to 8; got {}", frame.width);
        assert_eq!(frame.width, frame.height, "fallback is square");
    }

    /// Hotspot lookup on the fallback is at the upper-left corner —
    /// the cursor's "point" matches the pointer's reported location.
    #[test]
    fn procedural_fallback_hotspot_is_origin() {
        let icons = CursorTheming::procedural_fallback(24);
        let frame = icons.current_frame(Duration::ZERO);
        assert_eq!(frame.xhot, 0);
        assert_eq!(frame.yhot, 0);
    }
}
