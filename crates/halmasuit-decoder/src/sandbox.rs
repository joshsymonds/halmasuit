//! Process-level sandbox primitives for `halmasuit-decoder` (Epic
//! #12 task #4).
//!
//! Called once at startup, BEFORE the decoder sends its `Ready`
//! handshake. All subsequent code — including the `recv()` loop and
//! (in later subtasks) `rsmpeg` decode operations — runs under the
//! restriction set established here.
//!
//! ## What this module does
//!
//! In strict order (each step depends on the preceding one not
//! locking us out of the next):
//!
//! 1. `prctl(PR_SET_NO_NEW_PRIVS, 1, …)` — forecloses any
//!    `execve(setuid_binary)` from later upgrading privileges. Once
//!    set on a thread it cannot be cleared.
//! 2. **fd-close**: iterate `/proc/self/fd`; close every fd not in
//!    the caller's `keep_fds` allowlist. Defense against accidental
//!    fd leakage from the parent (`halmasuit`).
//! 3. **Namespace isolation** (best-effort):
//!    - `unshare(CLONE_NEWUSER)` — unprivileged user namespace.
//!      Within it we hold full caps; without it the subsequent NET /
//!      NS unshares would EPERM since we're at uid 998
//!      (`compositor`), not root.
//!    - Write trivial `uid_map` / `gid_map` so we appear as
//!      `nobody`-equivalent inside the namespace.
//!    - `unshare(CLONE_NEWNET | CLONE_NEWNS)` — no network, private
//!      mount namespace.
//!
//!    If `CLONE_NEWUSER` fails (kernel sysctl
//!    `kernel.unprivileged_userns_clone=0`), we log a warning and
//!    skip the NET/NS isolation. The seccomp filter installed by a
//!    later subtask is our primary syscall-level defense; namespaces
//!    are defense-in-depth.
//! 4. **Rlimits**:
//!    - `RLIMIT_NPROC = 0/0` — cannot fork. Combined with seccomp
//!      this is the primary "no more processes" guarantee.
//!    - `RLIMIT_AS = 512 MiB / 512 MiB` — bounded virtual memory.
//!      rsmpeg's working set for 1080p decode is far below this; if
//!      the bound is ever exceeded the decoder dies and the
//!      compositor restarts it.
//!    - `RLIMIT_NOFILE = 32 / 32` — small file-table size. The IPC
//!      fd + stderr + wallpaper fd + rsmpeg's small overhead is well
//!      below.
//!    - `RLIMIT_CORE = 0/0` — no core dumps (decoder state could
//!      leak post-mortem).
//!
//! ## What this module deliberately does NOT do
//!
//! - **seccomp-bpf filter installation** — task #5 (lands with
//!   rsmpeg integration; we need the rsmpeg syscall list before we
//!   can write a tight allowlist).
//! - **chroot / pivot_root** — the mount namespace + RLIMIT_AS +
//!   later seccomp obviate it for this use case.
//! - **setuid / setresuid** — `halmasuit-decoder` is forked from
//!   halmasuit at uid 998 (`compositor`); no further drop needed.

#![expect(
    unsafe_code,
    reason = "this module owns the syscall-level sandbox primitives; \
              every unsafe block is the prctl/unshare/write-uid-map \
              FFI boundary, justified per-block"
)]

use std::fs;
use std::io::Write;
use std::os::fd::RawFd;

use nix::sys::resource::{Resource, setrlimit};
use thiserror::Error;
use tracing::{info, warn};

/// Bound on the decoder's virtual-memory size. 512 MiB is generous
/// for 1080p RGBA8 decode (rsmpeg working set is well under 100 MiB
/// for h264/AV1 at that resolution).
const RLIMIT_AS_BYTES: u64 = 512 * 1024 * 1024;
/// Small file-table size. IPC fd + stderr + wallpaper fd + rsmpeg's
/// few internal fds fit comfortably.
const RLIMIT_NOFILE_COUNT: u64 = 32;

/// Errors from sandbox setup. Any non-`UserNsUnavailable` failure
/// terminates the decoder before `Ready` is sent — the parent's
/// restart-or-fallback policy applies.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("prctl(PR_SET_NO_NEW_PRIVS) failed: {0}")]
    NoNewPrivs(nix::Error),
    #[error("opendir /proc/self/fd failed: {0}")]
    OpenProcFd(std::io::Error),
    #[error("unshare({flag}) failed: {err}")]
    Unshare { flag: &'static str, err: nix::Error },
    #[error("write {file}: {err}")]
    WriteIdMap {
        file: &'static str,
        err: std::io::Error,
    },
    #[error("setrlimit({resource}) failed: {err}")]
    Rlimit {
        resource: &'static str,
        err: nix::Error,
    },
}

/// Apply the full sandbox in the order documented at the module
/// head. `keep_fds` is the allowlist for the fd-close step — every
/// other fd open at call time is closed.
pub fn enter_sandbox(keep_fds: &[RawFd]) -> Result<(), SandboxError> {
    set_no_new_privs()?;
    close_fds_except(keep_fds)?;
    unshare_namespaces()?;
    set_rlimits()?;
    info!("sandbox: process-level restrictions in place");
    Ok(())
}

/// Sets `PR_SET_NO_NEW_PRIVS` on the current thread. Idempotent;
/// once set, cannot be cleared.
fn set_no_new_privs() -> Result<(), SandboxError> {
    // SAFETY: prctl with PR_SET_NO_NEW_PRIVS is a documented thread-
    // local op with no pointer arguments; the 0s are required filler
    // per the prctl(2) man page.
    let rc = unsafe {
        libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            1_i32 as libc::c_ulong,
            0_i32 as libc::c_ulong,
            0_i32 as libc::c_ulong,
            0_i32 as libc::c_ulong,
        )
    };
    if rc == -1 {
        return Err(SandboxError::NoNewPrivs(nix::Error::last()));
    }
    Ok(())
}

/// Iterate `/proc/self/fd`; close every fd not in `keep_fds`.
///
/// Done BEFORE `unshare` so we still have access to the procfs
/// view. (`/proc/self/fd` is per-thread, not per-namespace, so the
/// later mount-ns unshare doesn't affect it — but the iteration is
/// cheaper before the kernel sets up the new ns regardless.)
fn close_fds_except(keep_fds: &[RawFd]) -> Result<(), SandboxError> {
    let entries = fs::read_dir("/proc/self/fd").map_err(SandboxError::OpenProcFd)?;
    // Collect first so we don't close the dirfd mid-iteration.
    let mut to_close: Vec<RawFd> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(fd) = name.parse::<RawFd>() else {
            continue;
        };
        if keep_fds.contains(&fd) {
            continue;
        }
        // The procfs dirfd itself appears in the listing; we don't
        // want to close it while iterating. Skip; it closes when the
        // `entries` iterator drops.
        to_close.push(fd);
    }
    for fd in to_close {
        // SAFETY: each fd came from /proc/self/fd; we own it
        // implicitly (inherited at fork or opened during process
        // startup). The kernel will refuse to close fds we don't
        // own (EBADF), which we tolerate as best-effort.
        let _ = unsafe { libc::close(fd) };
    }
    Ok(())
}

/// Best-effort namespace isolation. The user-namespace step is the
/// load-bearing one; if it fails (sysctl-disabled kernel), we log
/// and continue without NET/NS isolation. The seccomp filter
/// (separate task) is the primary syscall-level defense.
fn unshare_namespaces() -> Result<(), SandboxError> {
    use nix::sched::{CloneFlags, unshare};

    // Snapshot identity before entering the user namespace so we can
    // write a uid_map / gid_map that maps OUR outer uid/gid to the
    // same value inside (an "identity" mapping that doesn't surprise
    // any code reading getuid() inside the sandbox).
    let outer_uid = nix::unistd::geteuid().as_raw();
    let outer_gid = nix::unistd::getegid().as_raw();

    match unshare(CloneFlags::CLONE_NEWUSER) {
        Ok(()) => {
            // Inside the new user namespace we hold all capabilities.
            // Write the mapping files. Kernel requires writing to
            // setgroups BEFORE gid_map (CVE-2014-8989 mitigation).
            write_id_map_files(outer_uid, outer_gid)?;

            // With caps inside the new user ns, we can now do
            // NET + NS unshares.
            unshare(CloneFlags::CLONE_NEWNET).map_err(|err| SandboxError::Unshare {
                flag: "CLONE_NEWNET",
                err,
            })?;
            unshare(CloneFlags::CLONE_NEWNS).map_err(|err| SandboxError::Unshare {
                flag: "CLONE_NEWNS",
                err,
            })?;
            info!("sandbox: entered user/net/mount namespaces");
        }
        Err(err) => {
            // kernel.unprivileged_userns_clone=0 or other policy
            // forbidding unprivileged user-ns creation. Continue
            // without namespace isolation; log loudly so a future
            // sysadmin/auditor sees the gap.
            warn!(
                error = %err,
                "sandbox: unshare(CLONE_NEWUSER) failed; continuing without \
                 net/mount namespace isolation (seccomp + rlimits remain)"
            );
        }
    }
    Ok(())
}

/// Write `/proc/self/setgroups`, `/proc/self/uid_map`,
/// `/proc/self/gid_map` with an identity mapping. Must be done
/// after entering a new user ns and before unshare(NEWNET/NEWNS).
fn write_id_map_files(outer_uid: u32, outer_gid: u32) -> Result<(), SandboxError> {
    // setgroups MUST be "deny" before gid_map can be written without
    // CAP_SETGID in the outer ns (which we don't have).
    fs::write("/proc/self/setgroups", "deny").map_err(|err| SandboxError::WriteIdMap {
        file: "/proc/self/setgroups",
        err,
    })?;
    // uid_map: "inside outside count" — map our outer uid to itself
    // inside (count=1).
    let uid_line = format!("{outer_uid} {outer_uid} 1\n");
    let mut file =
        fs::File::create("/proc/self/uid_map").map_err(|err| SandboxError::WriteIdMap {
            file: "/proc/self/uid_map",
            err,
        })?;
    file.write_all(uid_line.as_bytes())
        .map_err(|err| SandboxError::WriteIdMap {
            file: "/proc/self/uid_map",
            err,
        })?;
    let gid_line = format!("{outer_gid} {outer_gid} 1\n");
    let mut file =
        fs::File::create("/proc/self/gid_map").map_err(|err| SandboxError::WriteIdMap {
            file: "/proc/self/gid_map",
            err,
        })?;
    file.write_all(gid_line.as_bytes())
        .map_err(|err| SandboxError::WriteIdMap {
            file: "/proc/self/gid_map",
            err,
        })?;
    Ok(())
}

/// Apply the rlimit set documented at the module head.
fn set_rlimits() -> Result<(), SandboxError> {
    let pairs: &[(&'static str, Resource, u64)] = &[
        ("RLIMIT_NPROC", Resource::RLIMIT_NPROC, 0),
        ("RLIMIT_AS", Resource::RLIMIT_AS, RLIMIT_AS_BYTES),
        (
            "RLIMIT_NOFILE",
            Resource::RLIMIT_NOFILE,
            RLIMIT_NOFILE_COUNT,
        ),
        ("RLIMIT_CORE", Resource::RLIMIT_CORE, 0),
    ];
    for &(name, res, bound) in pairs {
        setrlimit(res, bound, bound).map_err(|err| SandboxError::Rlimit {
            resource: name,
            err,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_no_new_privs` is idempotent and one-way; safe to call in
    /// the test process. Just verify it returns Ok.
    #[test]
    fn set_no_new_privs_succeeds() {
        set_no_new_privs().expect("PR_SET_NO_NEW_PRIVS");
        // Verify the flag actually stuck.
        // SAFETY: prctl GET path with PR_GET_NO_NEW_PRIVS is read-only.
        let got = unsafe {
            libc::prctl(
                libc::PR_GET_NO_NEW_PRIVS,
                0_i32 as libc::c_ulong,
                0_i32 as libc::c_ulong,
                0_i32 as libc::c_ulong,
                0_i32 as libc::c_ulong,
            )
        };
        assert_eq!(got, 1, "PR_GET_NO_NEW_PRIVS should return 1");
    }

    // NOTE: `close_fds_except`, `unshare_namespaces`, and
    // `set_rlimits` are one-way operations whose semantics affect
    // the entire test PROCESS (cargo nextest runs unit tests
    // serially within one binary; closing fds or unsharing
    // namespaces would break subsequent tests). They're verified
    // end-to-end in the VM test (Epic #12 task #9): a real
    // halmasuit-decoder is forked under halmasuit, attempts to
    // open a network socket and an out-of-allowlist fd, and the
    // assertion is that those attempts fail. The sub-functions
    // themselves are small wrappers over well-documented syscalls
    // (libc::prctl, nix::sched::unshare, nix::sys::resource::setrlimit,
    // fs::read_dir + libc::close); inline unit testing here would
    // amount to testing the syscalls.
}
