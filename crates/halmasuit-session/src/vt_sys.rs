//! Hand-rolled VT_ACTIVATE ioctl wrapper for Epic #71 R1.
//!
//! The privileged broker fires `VT_ACTIVATE` via this module. nix
//! doesn't expose VT ioctls in its safe layer, so we wrap the
//! `libc::ioctl` call here — quarantined in its own module the
//! same way `pam_sys.rs` quarantines libpam FFI. The rest of the
//! crate (including `broker.rs`) stays `#![forbid(unsafe_code)]`.
//!
//! `VT_ACTIVATE` (0x5606 per `linux/vt.h`) takes a single integer
//! argument: the target VT number. It requires `CAP_SYS_TTY_CONFIG`
//! OR controlling-TTY perm. The broker holds the cap implicitly as
//! root, so the call succeeds regardless of which fd we issue it
//! against.

#![expect(
    unsafe_code,
    reason = "VT_ACTIVATE is not exposed by nix; wrap libc::ioctl here, quarantined in its own module like pam_sys.rs"
)]

use std::ffi::CString;
use std::io;

/// VT_ACTIVATE ioctl number, per `linux/vt.h`.
///
/// The number is a stable kernel uAPI constant (since the VT
/// subsystem's introduction). No version-skew risk.
const VT_ACTIVATE: u64 = 0x5606;

/// Issue `VT_ACTIVATE(target_vt)` on `/dev/tty0`. The kernel reads
/// only the ioctl number + arg from the fd; any open TTY satisfies
/// the perm check, and `/dev/tty0` is the canonical anchor.
///
/// # Errors
///
/// Returns `io::Error::last_os_error()` if `open(2)` or `ioctl(2)`
/// fails. The broker logs + replies with `VtSwitchRejected
/// {BrokerInternal}` on error.
pub fn vt_activate(target_vt: u8) -> io::Result<()> {
    let path = CString::new("/dev/tty0").expect("static C string, no embedded NUL");
    // SAFETY: standard libc::open call with valid CString and known
    // flag constants. fd is checked for error (-1) before use.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: VT_ACTIVATE takes a single integer arg per linux/vt.h.
    // We pass target_vt (u8) widened to c_long, which is the ioctl
    // arg type. fd is a valid open TTY descriptor.
    let rc = unsafe { libc::ioctl(fd, VT_ACTIVATE as _, libc::c_long::from(target_vt)) };
    let err = if rc < 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    // SAFETY: close on a valid fd we opened; ignore the return value
    // because there's nothing the caller can do if close itself fails.
    let _ = unsafe { libc::close(fd) };
    err.map_or(Ok(()), Err)
}
