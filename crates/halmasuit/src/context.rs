//! Boot-context detection for the Phase B initramfs deployment.
//!
//! Two helpers used by `main()` to branch between halmasuit's
//! initramfs and rootfs code paths:
//!
//! - [`is_initramfs`] — checks `/etc/initrd-release`, systemd's
//!   canonical "this is initramfs" signal (see
//!   <https://systemd.io/INITRD_INTERFACE/>). The file is shipped
//!   inside the initramfs image and removed by
//!   `initrd-switch-root.service` during the pivot, so its presence
//!   distinguishes the two phases unambiguously.
//! - [`set_argv0_marker`] — writes `'@'` into `argv[0][0]`, the
//!   storage-daemon survival convention from
//!   <https://systemd.io/ROOT_STORAGE_DAEMONS/>. Plymouth and other
//!   processes that must outlive `switch_root` use this exact
//!   technique. Defense-in-depth alongside `SurviveFinalKillSignal=yes`
//!   (the Phase 2 mechanism validated by `drm-master-probe`).
//!
//! Both functions are call-once-at-startup helpers. The argv mutation
//! is `unsafe` (raw pointer write into the argv strings region) but
//! safe in practice: glibc's `__progname_full` is a stable public
//! data symbol pointing at argv[0], and the memory is writable for
//! the life of the process.

use std::path::Path;

/// Returns `true` if the process is running inside the initramfs.
///
/// Mechanism: `/etc/initrd-release` exists in the initramfs image and
/// is deleted by `initrd-switch-root.service` as part of the pivot to
/// the real root. systemd documents this contract in the
/// [INITRD_INTERFACE spec](https://systemd.io/INITRD_INTERFACE/).
///
/// Fail-loud on misconfiguration: a rootfs that still has
/// `/etc/initrd-release` will cause halmasuit to attempt the direct
/// DRM-master path, which fails with `EBUSY` if seatd or another
/// master holds it — surfacing the misconfiguration immediately.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "wired into main.rs in task #3 (DRM direct-open path + initramfs branching)"
    )
)]
pub fn is_initramfs() -> bool {
    is_initramfs_at(Path::new("/etc/initrd-release"))
}

/// [`is_initramfs`] with the marker path injected — separates the
/// detection logic from the canonical filesystem path so unit tests
/// can exercise both presence and absence without root or a real
/// initramfs.
pub fn is_initramfs_at(marker: &Path) -> bool {
    marker.exists()
}

/// Set `argv[0][0] = '@'` so systemd's `switch_root` excludes us from
/// the killing spree at the initramfs→rootfs boundary. See
/// <https://systemd.io/ROOT_STORAGE_DAEMONS/>.
///
/// Plymouth and other storage daemons use this exact technique in
/// production. The mechanism is byte-level mutation of the argv
/// strings region in the process's main-thread stack memory.
///
/// Defense-in-depth: halmasuit also sets
/// `unitConfig.SurviveFinalKillSignal = "yes"` on its initramfs unit
/// (RESEARCH.md Phase 2). Either alone is sufficient on current
/// systemd; using both hedges against regressions in either mechanism
/// (systemd #37700/#40933 churned the argv path recently).
///
/// Idempotent: callable multiple times safely. After the first call
/// `argv[0]` already starts with `'@'`; subsequent calls are no-ops
/// in effect.
#[expect(
    unsafe_code,
    reason = "argv[0] mutation is the systemd @-survival convention per ROOT_STORAGE_DAEMONS; no safe API exposes this"
)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "wired into main.rs in task #3 (DRM direct-open path + initramfs branching)"
    )
)]
pub fn set_argv0_marker() {
    // glibc exports `__progname_full` as a public data symbol pointing
    // at argv[0] in the argv strings region of the process's stack
    // (set up by __libc_init_first at startup). `nm -D libc.so.6 |
    // grep progname` confirms it's available on NixOS's glibc. Writing
    // to *__progname_full mutates argv[0] memory, which
    // /proc/self/cmdline reflects via mm_struct's arg_start.
    unsafe extern "C" {
        static __progname_full: *mut std::os::raw::c_char;
    }

    // SAFETY: glibc populates __progname_full at startup with a pointer
    // to argv[0] in the argv strings region of the process's main-thread
    // stack. That memory is writable per Linux's process layout. Writing
    // one byte to argv[0][0] is the documented mechanism for systemd's
    // @-survival convention per <https://systemd.io/ROOT_STORAGE_DAEMONS/>.
    // Plymouth and other storage daemons use this exact technique.
    unsafe {
        let argv0_ptr: *mut std::os::raw::c_char = __progname_full;
        *argv0_ptr = b'@'.cast_signed();
    }
}

#[cfg(test)]
mod tests {
    use super::{is_initramfs, is_initramfs_at, set_argv0_marker};

    #[test]
    fn is_initramfs_at_returns_true_when_marker_present() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile creation succeeds");
        assert!(
            is_initramfs_at(tmp.path()),
            "tempfile exists; is_initramfs_at must report true"
        );
    }

    #[test]
    fn is_initramfs_at_returns_false_when_marker_absent() {
        let mut path = std::env::temp_dir();
        path.push("halmasuit-context-test-this-file-does-not-exist");
        assert!(
            !path.exists(),
            "test precondition: the absent path must be absent"
        );
        assert!(
            !is_initramfs_at(&path),
            "absent path; is_initramfs_at must report false"
        );
    }

    #[test]
    fn is_initramfs_returns_false_in_rootfs_test_environment() {
        // The cargo-nextest harness runs in the rootfs; the systemd
        // initramfs marker should never be present here.
        assert!(
            !is_initramfs(),
            "expected /etc/initrd-release absent in the test runner's rootfs"
        );
    }

    #[test]
    fn set_argv0_marker_mutates_cmdline_first_byte() {
        // Pre-call cmdline first byte (before any '@' mutation).
        let before = std::fs::read("/proc/self/cmdline").expect("read /proc/self/cmdline");
        let first_before = before.first().copied();
        assert_ne!(
            first_before,
            Some(b'@'),
            "test precondition: cmdline must not already start with '@'; \
             got {first_before:?}. nextest gives each test its own process so \
             this should hold."
        );

        set_argv0_marker();

        let after = std::fs::read("/proc/self/cmdline").expect("re-read /proc/self/cmdline");
        assert_eq!(
            after.first().copied(),
            Some(b'@'),
            "set_argv0_marker must rewrite argv[0][0] to '@'; got {:?}",
            after.first().copied()
        );
    }

    #[test]
    fn set_argv0_marker_is_idempotent() {
        set_argv0_marker();
        set_argv0_marker();
        // No panic + cmdline still starts with '@' is the contract.
        let cmdline = std::fs::read("/proc/self/cmdline").expect("read /proc/self/cmdline");
        assert_eq!(cmdline.first().copied(), Some(b'@'));
    }
}
