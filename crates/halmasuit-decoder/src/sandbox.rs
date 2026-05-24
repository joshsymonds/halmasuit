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
    #[error("seccompiler: {0}")]
    SeccompCompile(seccompiler::Error),
    #[error("seccompiler load: {0}")]
    SeccompLoad(seccompiler::Error),
}

/// Apply the full sandbox in the order documented at the module
/// head. `keep_fds` is the allowlist for the fd-close step — every
/// other fd open at call time is closed.
///
/// Order matters — each step depends on the preceding one not
/// locking us out of the next. seccomp is LAST because the
/// filter blocks syscalls that the preceding steps need
/// (`unshare`, `setrlimit`, `openat`, `close`, `write` to
/// /proc/self/{setgroups,uid_map,gid_map}).
pub fn enter_sandbox(keep_fds: &[RawFd]) -> Result<(), SandboxError> {
    set_no_new_privs()?;
    close_fds_except(keep_fds)?;
    unshare_namespaces()?;
    set_rlimits()?;
    install_seccomp_filter()?;
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
    use nix::dir::Dir;
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;
    use std::os::fd::AsRawFd;

    // Open /proc/self/fd with nix::dir::Dir so we can read the
    // dirfd's raw value via AsRawFd — std::fs::ReadDir doesn't
    // expose it.
    let mut dir = Dir::open(
        "/proc/self/fd",
        OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|err| SandboxError::OpenProcFd(std::io::Error::from_raw_os_error(err as i32)))?;
    // The dirfd backing the iterator appears in its own listing. We
    // explicitly skip it from `to_close` because the close loop below
    // runs AFTER `dir` is dropped — at which point the dirfd's
    // number can be recycled by the kernel for an unrelated fd, and
    // a stale `libc::close(old_dirfd)` would close the wrong file
    // descriptor. Race window is tiny in single-threaded sandbox
    // setup but the explicit skip is correct.
    let dirfd_raw = dir.as_raw_fd();
    let mut to_close: Vec<RawFd> = Vec::new();
    for entry in dir.iter().flatten() {
        let bytes = entry.file_name().to_bytes();
        let Ok(name) = std::str::from_utf8(bytes) else {
            continue;
        };
        let Ok(fd) = name.parse::<RawFd>() else {
            continue;
        };
        if keep_fds.contains(&fd) || fd == dirfd_raw {
            continue;
        }
        to_close.push(fd);
    }
    // Drop the Dir BEFORE the close loop so the dirfd is released
    // (the kernel may then recycle its number, but we've already
    // excluded it from to_close above).
    drop(dir);
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

/// Compile + install the seccomp-bpf allowlist filter.
///
/// Default action is `KillProcess` — any syscall NOT on the
/// allowlist immediately terminates the decoder with SIGSYS. The
/// relay reaps via pidfd and applies the restart-or-fallback
/// policy; the wire surface includes a `DecoderErrorCode::SeccompTrap`
/// the operator can correlate (the dying decoder doesn't get to
/// send it, but the relay can infer it from the exit signal).
///
/// Allow-list rationale: minimal set required to (a) decode video
/// frames via libavcodec + sws_scale, (b) deliver them via
/// SOCK_SEQPACKET sendmsg, (c) receive control messages via
/// recvmsg + poll, (d) sleep between frames for PTS pacing, plus
/// (e) the usual Rust/libc startup + futex + memory-management
/// syscalls. Anything we didn't enumerate (network, filesystem
/// path opens, fork/clone, ptrace, kernel modules, etc.) → SIGSYS.
///
/// The list is conservative: a few entries (e.g. `mprotect`,
/// `rt_sigaction`) are NOT in epic Req #3's documented allowlist
/// but ARE required for the decoder to function (allocator memory
/// protection updates and Rust's panic handler respectively).
/// Documented inline; a future audit pass can tighten further.
fn install_seccomp_filter() -> Result<(), SandboxError> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    use std::collections::BTreeMap;

    // x86_64 syscall numbers + a `TargetArch::x86_64` filter head.
    // Hard-fail the build on any other arch so a future
    // `aarch64-linux` flake build doesn't silently produce a
    // decoder that SIGSYSes on first syscall.
    #[cfg(not(target_arch = "x86_64"))]
    compile_error!(
        "halmasuit-decoder's seccomp filter is x86_64-only \
         (ALLOWED_SYSCALLS uses libc::SYS_* constants whose values \
         differ on aarch64). Add an aarch64 syscall table and \
         conditionally select TargetArch before re-enabling."
    );
    const ALLOWED_SYSCALLS: &[(i64, &str)] = &[
        // Spec allowlist (epic Req #3).
        (libc::SYS_read, "read"),
        (libc::SYS_write, "write"),
        (libc::SYS_mmap, "mmap"),
        (libc::SYS_munmap, "munmap"),
        (libc::SYS_mremap, "mremap"),
        (libc::SYS_futex, "futex"),
        (libc::SYS_clock_gettime, "clock_gettime"),
        (libc::SYS_exit, "exit"),
        (libc::SYS_exit_group, "exit_group"),
        (libc::SYS_rt_sigreturn, "rt_sigreturn"),
        (libc::SYS_brk, "brk"),
        (libc::SYS_restart_syscall, "restart_syscall"),
        (libc::SYS_madvise, "madvise"),
        // IPC: SOCK_SEQPACKET send/recv on the inherited fd.
        // sendmsg/recvmsg for SCM_RIGHTS frames + the gather-write
        // frame path; sendto/recvfrom because libc's send()/recv()
        // are thin wrappers over those syscalls (NULL dest_addr).
        (libc::SYS_sendmsg, "sendmsg"),
        (libc::SYS_recvmsg, "recvmsg"),
        (libc::SYS_sendto, "sendto"),
        (libc::SYS_recvfrom, "recvfrom"),
        // PTS pacing: wait until the next frame's presentation
        // time on the IPC fd (poll + control interrupt).
        (libc::SYS_ppoll, "ppoll"),
        (libc::SYS_poll, "poll"),
        (libc::SYS_nanosleep, "nanosleep"),
        (libc::SYS_clock_nanosleep, "clock_nanosleep"),
        // Memory allocator (glibc malloc family). brk + mmap are
        // already on the list; mprotect updates page protections.
        (libc::SYS_mprotect, "mprotect"),
        // Rust panic infrastructure + libc signal setup. The
        // decoder doesn't install custom handlers, but Rust's
        // backtrace machinery may call these on a panic.
        (libc::SYS_rt_sigaction, "rt_sigaction"),
        (libc::SYS_rt_sigprocmask, "rt_sigprocmask"),
        (libc::SYS_sigaltstack, "sigaltstack"),
        // libc startup + identity calls (logging includes pid).
        (libc::SYS_getpid, "getpid"),
        // For the OwnedFd Drop on the original SCM_RIGHTS fd after
        // open_video_input dups + mmaps.
        (libc::SYS_close, "close"),
        // libc may call this on startup (random pool init).
        (libc::SYS_getrandom, "getrandom"),
        // Rust's threading runtime allocates new pages for thread
        // stacks (we don't spawn threads, but rsmpeg may use
        // pthread_self / TLS init).
        (libc::SYS_writev, "writev"),
        (libc::SYS_readv, "readv"),
        // set_nonblocking on the IPC fd + decoder fd dup
        // (decode::open_video_input uses libc::dup which is a thin
        // wrapper over fcntl on some libc impls).
        (libc::SYS_fcntl, "fcntl"),
        (libc::SYS_dup, "dup"),
        // /proc/self/fd open during enter_sandbox happens BEFORE
        // the filter installs, but rsmpeg / libavutil / libav* may
        // touch the filesystem for codec config (e.g. read CPU
        // capability files). openat is given an argument-filter
        // below — entry kept here only to seat the syscall in the
        // rules map; the empty Vec is REPLACED before SeccompFilter
        // build with rdonly_only_rules. Bare allowlist would also
        // permit O_WRONLY/O_RDWR/O_CREAT/O_TMPFILE on this
        // mount-ns-inherited host filesystem view.
        (libc::SYS_openat, "openat"),
        (libc::SYS_newfstatat, "newfstatat"),
        (libc::SYS_fstat, "fstat"),
        (libc::SYS_statx, "statx"),
        // ioctl on the IPC fd for socket options + maybe TIOCGWINSZ
        // from tracing-subscriber's terminal detection.
        (libc::SYS_ioctl, "ioctl"),
        // pread for mmap'd region read-ahead by libavformat
        // (custom AVIO uses our read callback, but libavutil may
        // also pread the underlying file when discovering codec
        // parameters).
        (libc::SYS_pread64, "pread64"),
        // Generic fd-table operations.
        (libc::SYS_lseek, "lseek"),
        // libc startup probes (some libc init uses this).
        (libc::SYS_set_robust_list, "set_robust_list"),
        (libc::SYS_rseq, "rseq"),
        // Allocator may use these for arena management.
        (libc::SYS_membarrier, "membarrier"),
    ];

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for &(nr, _) in ALLOWED_SYSCALLS {
        rules.insert(nr, vec![]);
    }

    // openat: restrict to read-only. AND of two MaskedEq
    // conditions on arg 2 (flags):
    //   (flags & O_ACCMODE) == O_RDONLY     (== 0; rejects O_WRONLY/O_RDWR)
    //   (flags & (O_CREAT|O_TMPFILE)) == 0  (rejects file creation)
    // Conditions within a single SeccompRule are AND'd; if BOTH
    // hold the rule matches (Allow), else the default action
    // (KillProcess) fires. libavformat's config probes use openat
    // for read-only path lookups (codec capability files, etc.) —
    // this allow-list keeps them working while denying any write
    // path into the mount-ns-inherited host filesystem view.
    let o_accmode: u64 = u64::from(libc::O_ACCMODE as u32);
    let o_rdonly: u64 = u64::from(libc::O_RDONLY as u32);
    let create_flags: u64 = u64::from((libc::O_CREAT | libc::O_TMPFILE) as u32);
    let openat_readonly = SeccompRule::new(vec![
        SeccompCondition::new(
            2,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(o_accmode),
            o_rdonly,
        )
        .map_err(|e| SandboxError::SeccompCompile(seccompiler::Error::from(e)))?,
        SeccompCondition::new(
            2,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(create_flags),
            0,
        )
        .map_err(|e| SandboxError::SeccompCompile(seccompiler::Error::from(e)))?,
    ])
    .map_err(|e| SandboxError::SeccompCompile(seccompiler::Error::from(e)))?;
    rules.insert(libc::SYS_openat, vec![openat_readonly]);

    let filter = SeccompFilter::new(
        rules,
        // Default: kill the whole process on any syscall not above.
        SeccompAction::KillProcess,
        // Allow listed syscalls.
        SeccompAction::Allow,
        TargetArch::x86_64,
    )
    .map_err(|e| SandboxError::SeccompCompile(seccompiler::Error::from(e)))?;
    let program: BpfProgram = filter
        .try_into()
        .map_err(|e| SandboxError::SeccompCompile(seccompiler::Error::from(e)))?;
    seccompiler::apply_filter(&program).map_err(SandboxError::SeccompLoad)?;
    info!(
        allowed = ALLOWED_SYSCALLS.len(),
        "sandbox: seccomp-bpf filter installed (default: KILL_PROCESS)"
    );
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
