//! Pre-compositor TTY graphics-mode switch — Plymouth's job, done by us.
//!
//! Opens `/dev/tty1`, issues `ioctl(KDSETMODE, KD_GRAPHICS)`, exits.
//! That single ioctl tells the kernel to stop drawing the text
//! console on tty1. Combined with `quiet loglevel=1
//! vt.global_cursor_default=0` on the kernel cmdline, this keeps the
//! physical screen black between BIOS handoff and halmasuit's first
//! frame — the visible space Plymouth occupies on Plymouth-bearing
//! distros.
//!
//! Wired by `nix/module.nix` as an initramfs systemd oneshot with
//! `DefaultDependencies=no`, `Before=systemd-modules-load.service`,
//! `Before=halmasuit.service`, `WantedBy=initrd.target`. Running
//! before `systemd-modules-load` puts the VT in graphics mode BEFORE
//! the kernel modules (notably `nvidia_drm` on gnomon) emit their
//! driver-init prints.
//!
//! Forensics are preserved: KDSETMODE only stops *rendering* the
//! text console; the kernel still writes printk lines to `dmesg` /
//! journal / `console=ttyS0`. `dmesg -b` after boot still shows
//! every message the user would have seen on the framebuffer.
//!
//! References:
//! - `KDSETMODE` ioctl — `<linux/kd.h>` (constant `0x4B3A`,
//!   `KD_GRAPHICS = 0x01`).
//! - Plymouth's equivalent: `src/libply-splash-core/ply-terminal.c`
//!   in the upstream Plymouth tree.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::process::ExitCode;

const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_GRAPHICS: libc::c_int = 0x01;
const TTY_PATH: &str = "/dev/tty1";

fn main() -> ExitCode {
    match set_kd_graphics() {
        Ok(()) => {
            // Write a single marker to stdout that the systemd unit
            // captures into the journal — the VM test greps for it
            // to assert the unit fired before halmasuit's first frame.
            println!("halmasuit-tty-graphics: {TTY_PATH} → KD_GRAPHICS");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("halmasuit-tty-graphics: KDSETMODE on {TTY_PATH} failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn set_kd_graphics() -> io::Result<()> {
    let tty = OpenOptions::new().write(true).open(TTY_PATH)?;
    #[expect(
        unsafe_code,
        reason = "KDSETMODE ioctl on /dev/tty1 — Plymouth-equivalent VT graphics-mode switch; libc has no safe wrapper."
    )]
    // SAFETY: `tty.as_raw_fd()` is a valid open fd into /dev/tty1 for
    // the duration of this block. `KDSETMODE` accepts a single int
    // payload (`KD_GRAPHICS`/`KD_TEXT`) per `<linux/kd.h>`. The ioctl
    // has no userspace memory side effects; failure is reported via
    // -1 + errno, which `Error::last_os_error()` lifts.
    let rc = unsafe { libc::ioctl(tty.as_raw_fd(), KDSETMODE, KD_GRAPHICS) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kdsetmode_constant_matches_uapi() {
        // <linux/kd.h>: `#define KDSETMODE 0x4B3A`.
        assert_eq!(KDSETMODE, 0x4B3A);
        // KD_GRAPHICS = 0x01; KD_TEXT = 0x00. Don't swap.
        assert_eq!(KD_GRAPHICS, 0x01);
    }
}
