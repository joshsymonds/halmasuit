//! Epic #71 R-honest.7 — compositor-owned cooperative VT switching
//! (the **home-VT model**).
//!
//! halmasuit is the singular long-lived display server, so it owns its
//! own ("home") VT as the kernel's `VT_PROCESS` controller — it does
//! NOT grab the target VT on each switch. The home VT fd is opened in
//! halmasuit's root startup window, the same way the DRM master fd is
//! (`drm::setup_drm_direct`); the broker is not involved (VT arbitration
//! is a steady-state compositor concern, not an auth/session one).
//!
//! ## Why own the home VT instead of grabbing the target
//!
//! Grabbing the target VT (`TIOCSCTTY` + `VT_SETMODE` on tty<target>)
//! collides with that VT's getty: switching to a console is exactly the
//! recovery use case, the getty `vhangup()`s the VT, the grabbed fd is
//! revoked, and every subsequent `VT_RELDISP` returns EIO — the switch
//! wedges. Owning the home VT instead makes a switch a plain
//! `VT_ACTIVATE(target)` on our controlling-tty fd; the kernel sends
//! relsig for the home VT, we release it, and the target (a getty, the
//! recovery console) comes up untouched. This is the model every
//! production direct-KMS stack uses.
//!
//! ## Lifecycle
//!
//! 1. Startup (root window): [`setup_home_vt_controller`] on the
//!    home-VT fd — `setsid` + `TIOCSCTTY` + `VT_SETMODE(VT_PROCESS,
//!    relsig, acqsig)`. The controlling-tty designation survives the
//!    privilege drop, so the VT ioctls keep working unprivileged
//!    (Phase 0 verdict — no `CAP_SYS_TTY_CONFIG`).
//! 2. Chord `Ctrl+Alt+F<n>` ([`detect_vt_chord`]) → [`vt_activate`] on
//!    the home fd. Non-blocking; the kernel sends relsig async.
//! 3. relsig (RT signal, [`vt_relsig`]) on a dedicated [signalfd]
//!    ([`create_vt_signalfd`]) → drop DRM master + [`vt_reldisp`]
//!    `RELEASE` → the kernel completes the switch.
//! 4. acqsig ([`vt_acqsig`], delivered when the kernel's own
//!    Ctrl+Alt+F<home> binding switches back) → reacquire master +
//!    `vt_reldisp` `ACKACQ`.
//!
//! [signalfd]: https://man7.org/linux/man-pages/man2/signalfd.2.html

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

/// Map a keysym to a VT switch target VT number, if the keysym is one
/// of the `XF86Switch_VT_N` cooperative-switching keysyms xkb
/// generates for Ctrl+Alt+F<N>. Returns `None` for any other keysym.
///
/// xkb resolves Ctrl+Alt+F<N> to keysym `XF86Switch_VT_<N>` in the
/// standard layouts (per `/usr/share/X11/xkb/symbols/srvr_ctrl`); a
/// bare F<N> press without the modifiers resolves to plain `F<N>` and
/// must NOT trigger a VT switch. This means the chord-modifier check
/// is structurally embedded in the keysym — we don't need to inspect
/// `ModifiersState` separately. (smithay's anvil example uses the
/// same pattern.)
///
/// System chords are hardcoded per Epic #71's anti-patterns — NOT
/// user-configurable. F-keys 1..=12 only; F13+ pass through.
#[must_use]
pub fn detect_vt_chord(keysym_raw: u32) -> Option<u8> {
    // KEY_XF86Switch_VT_1 = 0x1008FE01, ..._12 = 0x1008FE0C — a
    // contiguous range. The constants come from xkbcommon's
    // `keysymdef.h` (re-exported from smithay).
    const XF86_SWITCH_VT_1: u32 = 0x1008_FE01;
    const XF86_SWITCH_VT_12: u32 = 0x1008_FE0C;
    if (XF86_SWITCH_VT_1..=XF86_SWITCH_VT_12).contains(&keysym_raw) {
        u8::try_from(keysym_raw - XF86_SWITCH_VT_1 + 1).ok()
    } else {
        None
    }
}

// ── Home-VT controller setup + VT_ACTIVATE ──────────────────────────
//
// FFI quarantine — same `#[expect(unsafe_code)]` pattern as
// `pidfd_send_signal` in main.rs. The workspace-level
// `unsafe_code = "warn"` lint (denied by -D warnings) is the gate; each
// unsafe block carries an explicit reason.

/// TIOCNOTTY ioctl number, per `asm-generic/ioctls.h`.
const TIOCNOTTY: u64 = 0x5422;
/// TIOCSCTTY ioctl number, per `asm-generic/ioctls.h`.
const TIOCSCTTY: u64 = 0x540E;
/// VT_SETMODE ioctl number, per `linux/vt.h`.
const VT_SETMODE: u64 = 0x5602;
/// VT_ACTIVATE ioctl number, per `linux/vt.h`.
const VT_ACTIVATE: u64 = 0x5606;
/// VT_RELDISP ioctl number, per `linux/vt.h`.
const VT_RELDISP: u64 = 0x5605;
/// VT_SETMODE mode value: cooperative-switching with kernel signals.
const VT_PROCESS: u8 = 0x01;

/// `VT_RELDISP` arg for the RELEASE half (relsig): `1` = "release
/// acknowledged, permit the switch away". Sent after dropping DRM
/// master so the kernel completes the switch to the incoming VT.
pub const VT_RELDISP_RELEASE: libc::c_long = 1;
/// `VT_RELDISP` arg for the ACQUIRE half (acqsig): `VT_ACKACQ` = `2` =
/// "acquire acknowledged". Sent after reacquiring DRM master when our
/// home VT becomes active again.
pub const VT_RELDISP_ACKACQ: libc::c_long = 2;

/// Make `fd` (our home VT, opened in the root startup window) the
/// compositor's `VT_PROCESS`-mode controlling terminal: detach any
/// existing CTTY, `setsid` (best-effort), `TIOCSCTTY`, then
/// `VT_SETMODE(VT_PROCESS, relsig, acqsig)`. After this the kernel
/// delivers relsig/acqsig to us on every switch away from / back to the
/// home VT, and [`vt_activate`] on this fd is permitted (it is our
/// controlling tty — no `CAP_SYS_TTY_CONFIG` needed, per Phase 0).
///
/// # Errors
/// Any errno from `setsid` / `TIOCSCTTY` / `VT_SETMODE`.
pub fn setup_home_vt_controller(fd: BorrowedFd<'_>) -> io::Result<()> {
    detach_existing_ctty();
    // `setsid()` is best-effort. The compositor is typically a session
    // leader already (systemd unit); the EPERM-on-already-leader case
    // is fine — `TIOCSCTTY` below still binds the home VT as our CTTY.
    match nix::unistd::setsid() {
        Ok(_) | Err(nix::errno::Errno::EPERM) => {}
        Err(e) => return Err(io::Error::other(format!("setsid: {e}"))),
    }
    tiocsctty(fd.as_raw_fd())?;
    vt_setmode_process(fd.as_raw_fd(), vt_relsig(), vt_acqsig())?;
    Ok(())
}

/// Issue `VT_ACTIVATE(target_vt)` on our home-VT controlling-tty fd to
/// switch the active console to `target_vt`. The kernel, seeing our home
/// VT is `VT_PROCESS`, sends relsig to us (handled via the signalfd) and
/// completes the switch once we `VT_RELDISP(release)`.
///
/// # Errors
/// `io::Error::last_os_error()` if the ioctl fails (e.g. invalid VT).
pub fn vt_activate(fd: BorrowedFd<'_>, target_vt: u8) -> io::Result<()> {
    // SAFETY: `ioctl(fd, VT_ACTIVATE, n)` reads only the request number
    // and an integer arg; no memory dereference. fd is a valid borrowed
    // controlling-tty descriptor. Errno is checked.
    #[expect(unsafe_code, reason = "raw VT_ACTIVATE ioctl via libc")]
    let rc = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            VT_ACTIVATE as _,
            libc::c_int::from(target_vt),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Issue `VT_RELDISP(fd, arg)` — the compositor's half of the
/// cooperative handshake. On relsig the compositor calls this with
/// [`VT_RELDISP_RELEASE`] to let the kernel finish switching away; on
/// acqsig with [`VT_RELDISP_ACKACQ`] to confirm it has taken its VT
/// back. Per Phase 0's verdict this needs no `CAP_SYS_TTY_CONFIG` — the
/// fd is the compositor's controlling TTY.
///
/// # Errors
/// `io::Error::last_os_error()` if the ioctl fails. Caller logs; never
/// panics.
pub fn vt_reldisp(fd: BorrowedFd<'_>, arg: libc::c_long) -> io::Result<()> {
    // SAFETY: `ioctl(fd, VT_RELDISP, arg)` reads only the request
    // number + an integer arg; no memory dereference. fd is a valid
    // borrowed descriptor. Errno is checked.
    #[expect(unsafe_code, reason = "raw VT_RELDISP ioctl via libc")]
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), VT_RELDISP as _, arg) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Best-effort detach from current controlling TTY (if any). Opens
/// `/dev/tty` (the magic CTTY device) and issues `TIOCNOTTY`. If
/// there's no current CTTY the open returns ENXIO and we skip cleanly.
/// Errors are swallowed: the subsequent `TIOCSCTTY` will fail with a
/// clearer error if detachment was actually needed.
fn detach_existing_ctty() {
    let Ok(tty_file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return; // No current CTTY → nothing to detach.
    };
    // SAFETY: TIOCNOTTY takes no memory args; errno-only. fd is valid
    // (just opened). Return value ignored — best-effort.
    #[expect(unsafe_code, reason = "raw TIOCNOTTY ioctl via libc")]
    let _ = unsafe { libc::ioctl(tty_file.as_raw_fd(), TIOCNOTTY as _) };
}

/// Issue `TIOCSCTTY(fd, 0)` to make the inherited fd the calling
/// process's controlling TTY. Caller must be a session leader with no
/// existing controlling TTY (a `setsid()` immediately before is the
/// canonical precondition).
fn tiocsctty(fd: RawFd) -> io::Result<()> {
    // SAFETY: `ioctl(fd, TIOCSCTTY, 0)` reads only the request number
    // and an integer arg; no memory dereference. fd is a borrowed valid
    // descriptor from the caller. Errno is checked.
    #[expect(unsafe_code, reason = "raw TIOCSCTTY ioctl via libc")]
    let rc = unsafe { libc::ioctl(fd, TIOCSCTTY as _, 0_i32) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `struct vt_mode` from `linux/vt.h` (repr(C)).
#[repr(C)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: u16,
    acqsig: u16,
    frsig: u16,
}

/// Issue `VT_SETMODE(fd, &VtMode{PROCESS, relsig, acqsig})` to switch
/// the kernel into cooperative-switching mode and register the signal
/// numbers the kernel will use for release / acquire events.
fn vt_setmode_process(fd: RawFd, relsig: libc::c_int, acqsig: libc::c_int) -> io::Result<()> {
    let mode = VtMode {
        mode: VT_PROCESS,
        waitv: 0,
        relsig: u16::try_from(relsig)
            .map_err(|_| io::Error::other(format!("relsig {relsig} does not fit in u16")))?,
        acqsig: u16::try_from(acqsig)
            .map_err(|_| io::Error::other(format!("acqsig {acqsig} does not fit in u16")))?,
        frsig: 0,
    };
    // SAFETY: `ioctl(fd, VT_SETMODE, &mode)` reads the vt_mode struct by
    // pointer. The struct is a valid stack allocation of the correct
    // repr(C) layout for `struct vt_mode` from `linux/vt.h`. Errno is
    // checked.
    #[expect(unsafe_code, reason = "raw VT_SETMODE ioctl via libc")]
    let rc = unsafe {
        libc::ioctl(
            fd,
            VT_SETMODE as _,
            std::ptr::addr_of!(mode).cast::<libc::c_void>(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ── Cooperative VT-switch realtime signals ──────────────────────────
//
// The kernel's relsig/acqsig (sent on every console switch once we're
// the VT_PROCESS controller) are PROCESS-directed signals: `kill_pid`
// → `group_send_sig_info`, which the kernel delivers to ANY thread in
// our group that hasn't blocked them (signal(7)). SIGUSR1/SIGUSR2 are
// in the namespace libraries grab — Mesa/EGL worker threads, glibc
// helpers, and SIGUSR1 is an X server's parent-readiness signal
// (freedesktop #87322) — so a worker thread silently consumed them and
// the switch handshake never ran. The fix is a REALTIME signal: a
// private number no library touches, blocked process-wide before any
// thread spawns, so it stays pending for a dedicated signalfd on the
// calloop thread. (SDDM uses SIGRTMAX/SIGRTMAX-1 for exactly this
// reason; we use SIGRTMIN-relative numbers past the low RT signals
// glibc reserves for NPTL.) calloop's `Signals` source and nix's
// `SigSet` are standard-signal-only, hence the raw libc here.

/// The realtime signal the kernel uses for VT *release* (relsig) —
/// "someone is switching away, release your VT". `SIGRTMIN+4` clears
/// the low RT signals glibc reserves for NPTL.
#[must_use]
pub fn vt_relsig() -> libc::c_int {
    libc::SIGRTMIN() + 4
}

/// The realtime signal the kernel uses for VT *acquire* (acqsig) —
/// "your VT is active again". See [`vt_relsig`].
#[must_use]
pub fn vt_acqsig() -> libc::c_int {
    libc::SIGRTMIN() + 5
}

/// A decoded cooperative VT-switch signal read from the signalfd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtSignal {
    /// relsig: the kernel is switching away from our VT — drop DRM
    /// master, then `VT_RELDISP(release)`.
    Release,
    /// acqsig: our VT is active again — reacquire DRM master, then
    /// `VT_RELDISP(ackacq)`.
    Acquire,
}

/// Classify a raw signal number into the VT half it represents, or
/// `None` if it is neither relsig nor acqsig.
#[must_use]
pub fn classify_vt_signal(signo: libc::c_int) -> Option<VtSignal> {
    if signo == vt_relsig() {
        Some(VtSignal::Release)
    } else if signo == vt_acqsig() {
        Some(VtSignal::Acquire)
    } else {
        None
    }
}

/// Build a `sigset_t` containing exactly the two VT realtime signals.
fn vt_sigset() -> libc::sigset_t {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` initializes the set in full; `sigaddset`
    // adds the two in-range SIGRTMIN-relative signal numbers. Both
    // write only through the provided pointer; `assume_init` is sound
    // because `sigemptyset` fully initialized the object.
    #[expect(
        unsafe_code,
        reason = "raw sigset_t construction for RT signals (nix SigSet is standard-signal-only)"
    )]
    let set = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), vt_relsig());
        libc::sigaddset(set.as_mut_ptr(), vt_acqsig());
        set.assume_init()
    };
    set
}

/// Block the VT relsig/acqsig realtime signals in the calling thread.
/// MUST run on the main thread BEFORE any other thread is spawned
/// (Mesa/EGL/DBus/etc.), so every thread inherits the block — otherwise
/// the kernel may hand the process-directed VT signal to an unblocked
/// worker thread, which runs the default disposition (terminate) and
/// the signalfd never sees it (the #87322 failure mode).
///
/// # Errors
/// The `pthread_sigmask` errno if blocking fails.
pub fn block_vt_signals() -> io::Result<()> {
    let set = vt_sigset();
    // SAFETY: `pthread_sigmask` reads `set` (valid, just built) and
    // writes nothing (oldset is null). It returns an errno directly
    // (0 = success) rather than setting `errno`.
    #[expect(unsafe_code, reason = "raw pthread_sigmask for RT signals")]
    let rc = unsafe {
        libc::pthread_sigmask(
            libc::SIG_BLOCK,
            std::ptr::addr_of!(set),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

/// Create a `signalfd` delivering the VT relsig/acqsig realtime
/// signals. [`block_vt_signals`] MUST have run first (signalfd only
/// dequeues blocked signals). The fd is NONBLOCK + CLOEXEC, ready to
/// register as a calloop source.
///
/// # Errors
/// `io::Error::last_os_error()` if `signalfd(2)` fails.
pub fn create_vt_signalfd() -> io::Result<OwnedFd> {
    let set = vt_sigset();
    // SAFETY: `signalfd(-1, &set, flags)` reads `set` and returns a new
    // owned fd (or -1); no caller memory is written.
    #[expect(unsafe_code, reason = "raw signalfd for RT signals")]
    let raw = unsafe {
        libc::signalfd(
            -1,
            std::ptr::addr_of!(set),
            libc::SFD_NONBLOCK | libc::SFD_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a freshly-created, valid fd we exclusively own.
    #[expect(unsafe_code, reason = "adopt the freshly-created signalfd")]
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(fd)
}

/// Drain all pending VT signals from the signalfd, decoding each into
/// [`VtSignal`] in arrival order. Returns `Ok(vec)` (possibly empty on
/// EAGAIN). Signal numbers other than relsig/acqsig (which the fd never
/// carries) are skipped.
///
/// # Errors
/// `io::Error::last_os_error()` on a read error other than `EAGAIN`.
pub fn read_vt_signals(fd: BorrowedFd<'_>) -> io::Result<Vec<VtSignal>> {
    let mut out = Vec::new();
    loop {
        let mut si = std::mem::MaybeUninit::<libc::signalfd_siginfo>::uninit();
        // SAFETY: `read` writes up to `size_of::<signalfd_siginfo>()`
        // bytes into `si` (correctly sized) and returns the byte count.
        // `fd` is a valid borrowed signalfd.
        #[expect(unsafe_code, reason = "raw read of signalfd_siginfo")]
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                si.as_mut_ptr().cast::<libc::c_void>(),
                std::mem::size_of::<libc::signalfd_siginfo>(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(out);
            }
            return Err(err);
        }
        if n == 0 {
            return Ok(out);
        }
        // SAFETY: a successful (non-negative) read on a signalfd always
        // returns whole `signalfd_siginfo` structs, so `si` is fully
        // initialized.
        #[expect(unsafe_code, reason = "siginfo initialized by a successful read")]
        let si = unsafe { si.assume_init() };
        if let Ok(signo) = libc::c_int::try_from(si.ssi_signo)
            && let Some(sig) = classify_vt_signal(signo)
        {
            out.push(sig);
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd as _;

    use super::*;

    /// R2.1 chord detection: `XF86Switch_VT_1..=12` map to target VT
    /// 1..=12. The keysym range is contiguous + load-bearing — if
    /// xkbcommon ever renumbers them, the mapping breaks silently.
    #[test]
    fn detect_vt_chord_maps_xf86_switch_vt_to_target() {
        for vt in 1_u8..=12 {
            let keysym = 0x1008_FE00 + u32::from(vt);
            assert_eq!(
                detect_vt_chord(keysym),
                Some(vt),
                "expected VT{vt} for keysym 0x{keysym:08x}",
            );
        }
    }

    /// Bare F-keys (no Ctrl+Alt) MUST NOT trigger a VT switch — xkb
    /// resolves them to keysyms outside the XF86Switch_VT_* range.
    #[test]
    fn detect_vt_chord_ignores_bare_function_keys() {
        for keysym in 0xFFBE_u32..=0xFFC9 {
            assert_eq!(
                detect_vt_chord(keysym),
                None,
                "bare F-key keysym 0x{keysym:08x} must NOT trigger VT switch",
            );
        }
        for keysym in [0x0061_u32, 0x0041, 0xFF0D /* Return */] {
            assert!(detect_vt_chord(keysym).is_none());
        }
    }

    /// XF86Switch_VT_13+ doesn't exist in xkbcommon, but defensively:
    /// keysyms one past the end of the recognized range MUST NOT
    /// resolve. Guards against off-by-one in the range bound.
    #[test]
    fn detect_vt_chord_rejects_above_vt12() {
        assert_eq!(detect_vt_chord(0x1008_FE0D), None);
        assert_eq!(detect_vt_chord(0x1008_FE00), None); // one BEFORE start
    }

    /// `detach_existing_ctty()` is a best-effort no-op when there's no
    /// current CTTY (most test processes). Verifies it doesn't panic
    /// and is idempotent under that common precondition.
    #[test]
    fn detach_existing_ctty_is_safe_when_no_ctty() {
        detach_existing_ctty();
        detach_existing_ctty();
    }

    /// The `VT_RELDISP` arg values are a frozen kernel ABI contract
    /// (`linux/vt.h`): 1 = release-acknowledged (relsig path), 2 =
    /// `VT_ACKACQ` (acqsig path). Swapping them would make the
    /// compositor ack the wrong half of the handshake — pin them.
    #[test]
    fn vt_reldisp_arg_constants_match_kernel_abi() {
        assert_eq!(VT_RELDISP_RELEASE, 1, "VT_RELDISP release arg is 1");
        assert_eq!(VT_RELDISP_ACKACQ, 2, "VT_ACKACQ is 2");
    }

    /// R-honest.7 root-cause fix: relsig/acqsig MUST be realtime signals
    /// (not SIGUSR1/2 — those get stolen by Mesa/EGL/Xwayland threads,
    /// freedesktop #87322). Pin that they are distinct, in the realtime
    /// range, and clear of SIGUSR1/2.
    #[test]
    fn vt_signals_are_distinct_realtime_signals() {
        let rel = vt_relsig();
        let acq = vt_acqsig();
        assert_ne!(rel, acq, "relsig and acqsig must be distinct");
        assert!(
            rel >= libc::SIGRTMIN() && rel <= libc::SIGRTMAX(),
            "relsig {rel} must be in the realtime range [{}, {}]",
            libc::SIGRTMIN(),
            libc::SIGRTMAX(),
        );
        assert!(
            acq >= libc::SIGRTMIN() && acq <= libc::SIGRTMAX(),
            "acqsig {acq} must be in the realtime range [{}, {}]",
            libc::SIGRTMIN(),
            libc::SIGRTMAX(),
        );
        assert_ne!(rel, libc::SIGUSR1, "relsig must not be SIGUSR1");
        assert_ne!(rel, libc::SIGUSR2, "relsig must not be SIGUSR2");
        assert_ne!(acq, libc::SIGUSR1, "acqsig must not be SIGUSR1");
        assert_ne!(acq, libc::SIGUSR2, "acqsig must not be SIGUSR2");
    }

    /// `classify_vt_signal` maps the two VT realtime signals to their
    /// halves and rejects everything else.
    #[test]
    fn classify_vt_signal_maps_only_the_two_rt_signals() {
        assert_eq!(classify_vt_signal(vt_relsig()), Some(VtSignal::Release));
        assert_eq!(classify_vt_signal(vt_acqsig()), Some(VtSignal::Acquire));
        assert_eq!(classify_vt_signal(libc::SIGUSR1), None);
        assert_eq!(classify_vt_signal(libc::SIGUSR2), None);
        assert_eq!(classify_vt_signal(libc::SIGTERM), None);
    }

    /// `block_vt_signals` + `create_vt_signalfd` round-trip: after
    /// blocking and raising relsig at the process, the signalfd
    /// dequeues exactly one `Release`. Proves the real delivery path
    /// (block → pending → signalfd read → classify) end-to-end without
    /// a VT.
    #[test]
    fn vt_signalfd_round_trips_a_raised_relsig() {
        block_vt_signals().expect("block VT signals");
        let fd = create_vt_signalfd().expect("create signalfd");

        // SAFETY: raise() sends the signal to the calling thread; it is
        // blocked, so it becomes pending for the signalfd rather than
        // running a disposition. errno-checked.
        #[expect(
            unsafe_code,
            reason = "raise a blocked RT signal at ourselves for the round-trip"
        )]
        let rc = unsafe { libc::raise(vt_relsig()) };
        assert_eq!(rc, 0, "raise(relsig) failed");

        let sigs = read_vt_signals(fd.as_fd()).expect("read signalfd");
        assert_eq!(
            sigs,
            vec![VtSignal::Release],
            "signalfd must dequeue exactly the raised relsig as Release",
        );
    }
}
