//! Epic #71 R3 — the macOS-Force-Quit analog for halmasuit.
//!
//! Three layers, landing one sub-task at a time:
//!
//! - **R3.1 (this commit)** — chord-interception + a boolean
//!   `diag_overlay_open` flag on `HalmasuitState`. No rendering.
//!   Ctrl+Alt+Shift+Esc toggles the flag (the Linux Secure
//!   Attention Key, aligned with systemd PR #29542's
//!   `SecureAttentionKey` direction). Bare Esc dismisses when open.
//! - **R3.2** — actual overlay rendering layer composited on top
//!   of the wallpaper + foreground surfaces. Reads state (current
//!   SystemPhase, broker connection, journal tail, window list).
//! - **R3.3** — `org.halmasuit.Compositor1` DBus surface with the
//!   strict read/write split documented in Epic #71's requirements.
//! - **R3.4** — `halmasuit` CLI tool consuming the DBus surface.
//!
//! The chord is HARDCODED per Epic anti-patterns — not
//! user-configurable. A reconfigurable chord would itself be an
//! attack surface (a compromised config could shadow it).

/// XK_Escape per xkbcommon's `keysymdef.h`.
const XK_ESCAPE: u32 = 0xFF1B;

/// Returns true iff the keysym + modifiers match
/// `Ctrl+Alt+Shift+Esc` exactly. Used as the chord that opens the
/// diagnostic overlay (the Linux Secure Attention Key).
///
/// All three modifiers MUST be held simultaneously — Ctrl+Esc
/// alone, Alt+Esc alone, Shift+Esc alone, or any pair of two of
/// them must NOT trigger. This is the standard Linux SAK semantic.
///
/// The chord modifier check is intentionally inspected explicitly
/// here (unlike R2.1's VT-switch chord, where xkb resolves the
/// modifier+F-key combination to a unique `XF86Switch_VT_*` keysym):
/// Escape with any modifier combination still resolves to plain
/// `XK_Escape`, so we must check `ModifiersState` ourselves.
#[must_use]
pub const fn detect_overlay_chord(keysym_raw: u32, ctrl: bool, alt: bool, shift: bool) -> bool {
    ctrl && alt && shift && keysym_raw == XK_ESCAPE
}

/// Returns true iff the keysym is bare `Escape` (with or without
/// modifiers — though the overlay-dismiss path only checks this on
/// keys that AREN'T the open-chord, so callers should run
/// `detect_overlay_chord` first and use this as a fallback for
/// dismissal). Used to close the diagnostic overlay.
#[must_use]
pub const fn is_dismiss_key(keysym_raw: u32) -> bool {
    keysym_raw == XK_ESCAPE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chord MUST require all three modifiers + the Escape
    /// keysym. Verified across the 8-way modifier truth table.
    #[test]
    fn detect_overlay_chord_requires_all_three_modifiers() {
        // All 8 modifier combinations. Only (true, true, true) matches.
        for ctrl in [false, true] {
            for alt in [false, true] {
                for shift in [false, true] {
                    let matched = detect_overlay_chord(XK_ESCAPE, ctrl, alt, shift);
                    let expected = ctrl && alt && shift;
                    assert_eq!(
                        matched, expected,
                        "ctrl={ctrl} alt={alt} shift={shift}: expected match={expected}, got {matched}",
                    );
                }
            }
        }
    }

    /// The chord MUST NOT trigger on non-Escape keysyms even when
    /// all three modifiers are held.
    #[test]
    fn detect_overlay_chord_rejects_non_escape_keysyms() {
        // Sample of common non-Escape keysyms: Return, Tab, F1,
        // BackSpace, Space, ASCII 'a', and the XF86Switch_VT_1
        // VT-switch keysym from R2.1.
        for keysym in [
            0xFF0D_u32, // Return
            0x0061,     // a
            0xFF09,     // Tab
            0xFF08,     // BackSpace
            0x0020,     // space
            0xFFBE,     // F1
            0x1008_FE01, // XF86Switch_VT_1 (R2.1 chord — must NOT
                        //                  collide with R3.1)
        ] {
            assert!(
                !detect_overlay_chord(keysym, true, true, true),
                "keysym 0x{keysym:08x} must NOT trigger the overlay chord \
                 even with Ctrl+Alt+Shift",
            );
        }
    }

    /// `is_dismiss_key` matches bare Escape and nothing else.
    #[test]
    fn is_dismiss_key_matches_only_escape() {
        assert!(is_dismiss_key(XK_ESCAPE));
        for keysym in [
            0xFF0D_u32,  // Return
            0x0061,      // a
            0xFF09,      // Tab
            0x1008_FE01, // XF86Switch_VT_1
        ] {
            assert!(!is_dismiss_key(keysym), "keysym 0x{keysym:08x}");
        }
    }
}
