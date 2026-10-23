//! The environment halmasuit adds to every session it relays to the
//! broker: where the session's Wayland display is, and the logind
//! descriptors pam_systemd registers the session with.
//!
//! A greeter launches a session from its `.desktop` entry with little
//! or no env of its own (DMS sends only `XDG_SESSION_TYPE`), and a
//! session such as `niri-session` starts its compositor as a systemd
//! user service. That service finds halmasuit only through the
//! `WAYLAND_DISPLAY` the session leader imports into the user manager,
//! and the user manager only exists once pam_systemd has opened a
//! logind session. halmasuit owns the display and the seat the session
//! runs on, so its entries replace any the greeter sent with the same
//! key.

use std::path::Path;

/// The seat halmasuit drives.
const SEAT: &str = "seat0";

/// Where the kernel reports the foreground VT (`tty1\n`).
const ACTIVE_VT: &str = "/sys/class/tty/tty0/active";

/// `KEY=VALUE` entries for the session: the absolute Wayland socket,
/// a wayland user session, and — when the VT is known — seat0 on that
/// VT, so logind makes the session active on the seat.
pub fn entries(wayland_socket: &Path, vt: Option<u8>) -> Vec<String> {
    let mut env = vec![
        format!("WAYLAND_DISPLAY={}", wayland_socket.display()),
        "XDG_SESSION_TYPE=wayland".to_owned(),
        "XDG_SESSION_CLASS=user".to_owned(),
    ];
    if let Some(vt) = vt {
        env.push(format!("XDG_SEAT={SEAT}"));
        env.push(format!("XDG_VTNR={vt}"));
    }
    env
}

/// The VT number in the kernel's active-VT report (`tty1\n` → 1).
pub fn vt_from_active(active: &str) -> Option<u8> {
    active
        .trim()
        .strip_prefix("tty")?
        .parse::<u8>()
        .ok()
        .filter(|vt| (1..=63).contains(vt))
}

/// The VT the session runs on: halmasuit's home VT when one is
/// configured, else the foreground VT, which is the one halmasuit
/// scans out over.
pub fn session_vt(home_vt: Option<u8>) -> Option<u8> {
    home_vt.or_else(|| {
        std::fs::read_to_string(ACTIVE_VT)
            .ok()
            .as_deref()
            .and_then(vt_from_active)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_name_the_socket_and_a_wayland_user_session() {
        assert_eq!(
            entries(Path::new("/run/halmasuit/wayland-0"), None),
            [
                "WAYLAND_DISPLAY=/run/halmasuit/wayland-0",
                "XDG_SESSION_TYPE=wayland",
                "XDG_SESSION_CLASS=user",
            ]
        );
    }

    #[test]
    fn a_known_vt_puts_the_session_on_seat0() {
        let env = entries(Path::new("/run/halmasuit/wayland-0"), Some(1));
        assert!(env.contains(&"XDG_SEAT=seat0".to_owned()));
        assert!(env.contains(&"XDG_VTNR=1".to_owned()));
    }

    #[test]
    fn active_vt_report_parses_to_its_number() {
        assert_eq!(vt_from_active("tty1\n"), Some(1));
        assert_eq!(vt_from_active("tty12"), Some(12));
    }

    #[test]
    fn active_vt_report_rejects_other_shapes() {
        assert_eq!(vt_from_active(""), None);
        assert_eq!(vt_from_active("tty0"), None);
        assert_eq!(vt_from_active("tty64"), None);
        assert_eq!(vt_from_active("ttyS0"), None);
        assert_eq!(vt_from_active("console"), None);
    }

    #[test]
    fn home_vt_wins_over_the_active_vt() {
        assert_eq!(session_vt(Some(8)), Some(8));
    }
}
