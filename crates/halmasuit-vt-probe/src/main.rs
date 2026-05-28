// halmasuit-vt-probe — Epic #71 Phase 0 research probe.
//
// Empirically validates that a non-root process can satisfy the kernel's
// `perm` check for `VT_RELDISP` on an inherited VT fd by calling
// `TIOCSCTTY` on it — without holding `CAP_SYS_TTY_CONFIG`.
//
// The kernel check in `drivers/tty/vt/vt_ioctl.c`:
//     if (current->signal->tty == tty || capable(CAP_SYS_TTY_CONFIG))
//         perm = 1;
// If `TIOCSCTTY` on the inherited fd makes it the calling process's
// controlling TTY, the first arm is satisfied and the process can call
// `VT_RELDISP` without the cap. That's the design assumption underlying
// Epic #71's broker-mediated VT switching: the broker (privileged) opens
// the VT and calls `VT_SETMODE`; the compositor (unprivileged) receives
// the fd, makes it its controlling TTY, and handles `VT_RELDISP` itself.
// If this probe answers YES, the design's load-bearing assumption holds.
// If NO, the design needs the fallback path (broker also brokers
// `VT_RELDISP`) and that's a meaningful protocol change to plan for.
//
// Test mechanics:
//
// 1. Probe runs as root (via the NixOS test's systemd unit). Opens
//    /dev/tty2. Calls `VT_SETMODE PROCESS` with relsig=SIGUSR1,
//    acqsig=SIGUSR2 — same shape the production compositor would use.
// 2. Probe forks. The parent waitpid's the child and exits with the
//    child's status. The fd is preserved across fork (inherited by
//    child).
// 3. Child:
//    a. `setsid()` — new session, no controlling TTY (precondition
//       for TIOCSCTTY).
//    b. `setresgid` + `setresuid` to the target user (halmasuit-
//       compositor, uid 998). This mirrors halmasuit's existing
//       in-process privilege-drop pattern.
//    c. Drop bounding set entirely; capset to {} (zero effective,
//       zero permitted) — even tighter than halmasuit-the-compositor
//       which retains CAP_KILL. The probe needs NO caps.
//    d. Verify caps are empty; log the state.
//    e. `sigprocmask` block SIGUSR1 + SIGUSR2; create a `signalfd`
//       for both.
//    f. `TIOCSCTTY` on the inherited fd — this is the first
//       load-bearing call. Log result + errno on failure.
//    g. Block on signalfd waiting for SIGUSR1 (the kernel's "release
//       this VT, the user wants to switch away" signal). Timeout
//       30s with diagnostic log.
//    h. On SIGUSR1: call `VT_RELDISP(1)` to permit the switch. This
//       is the second load-bearing call. Log result + errno on
//       failure.
//    i. Exit 0 on success, exit 1 on any load-bearing failure.
//
// External trigger: the NixOS test calls `chvt 1` to switch away
// from tty2 (where the probe has set itself as VT_PROCESS controller).
// Kernel sends SIGUSR1 to the probe.
//
// Log format: timestamped lines to the file passed via --log.
// Single source of truth for the test's pass/fail assertion.

#![deny(unsafe_code)]

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Pid, Uid, fork, setresgid, setresuid, setsid};

// ── linux/vt.h constants/types ─────────────────────────────────────
// nix doesn't expose VT_SETMODE / VT_RELDISP / TIOCSCTTY directly.
// These are stable kernel uAPI numbers from linux/vt.h and
// asm-generic/ioctls.h; we wrap them by hand via libc::ioctl.

const VT_SETMODE: u64 = 0x5602;
const VT_RELDISP: u64 = 0x5605;
const TIOCSCTTY: u64 = 0x540E;

const VT_PROCESS: u8 = 0x01;
/// `VT_RELDISP` arg: 1 = permit the switch the kernel asked for via
/// the release signal. (0 = refuse; 2 = ACKACQ, used only for the
/// acquire half of the cooperative-switching handshake.)
const VT_RELDISP_PERMIT: libc::c_long = 1;

#[repr(C)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: u16,
    acqsig: u16,
    frsig: u16,
}

// ── CLI ────────────────────────────────────────────────────────────

struct Args {
    vt: PathBuf,
    target_user: String,
    log: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut vt = None;
    let mut target_user = None;
    let mut log = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--vt" => {
                vt = Some(PathBuf::from(
                    it.next().ok_or_else(|| anyhow!("--vt needs a value"))?,
                ));
            }
            "--target-user" => {
                target_user = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--target-user needs a value"))?,
                );
            }
            "--log" => {
                log = Some(PathBuf::from(
                    it.next().ok_or_else(|| anyhow!("--log needs a value"))?,
                ));
            }
            "-h" | "--help" => {
                eprintln!(
                    "Usage: halmasuit-vt-probe --vt <path> --target-user <name> --log <path>\n\
                     \n\
                     Epic #71 Phase 0 probe. Open the VT as root, set\n\
                     VT_SETMODE PROCESS, drop privileges to <target-user>,\n\
                     call TIOCSCTTY + VT_RELDISP on the inherited fd, and\n\
                     log the kernel's verdict to --log."
                );
                process::exit(2);
            }
            other => return Err(anyhow!("unknown arg: {other}")),
        }
    }

    Ok(Args {
        vt: vt.ok_or_else(|| anyhow!("--vt is required"))?,
        target_user: target_user.ok_or_else(|| anyhow!("--target-user is required"))?,
        log: log.ok_or_else(|| anyhow!("--log is required"))?,
    })
}

// ── Logging ────────────────────────────────────────────────────────

struct Log {
    path: PathBuf,
    start: Instant,
}

impl Log {
    fn open(path: PathBuf) -> Result<Self> {
        // Create-or-truncate; the probe is one-shot.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("open log {}", path.display()))?;
        Ok(Self {
            path,
            start: Instant::now(),
        })
    }

    fn line(&self, msg: &str) {
        // Append (probe is single-process at any moment; parent
        // writes during setup; child writes during the run loop;
        // they don't race). Best-effort: log errors here are
        // themselves unrecoverable, so just print to stderr.
        let elapsed = self.start.elapsed().as_secs_f64();
        let mut f = match OpenOptions::new().append(true).open(&self.path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("vt-probe: failed to append log: {e}");
                return;
            }
        };
        let line = format!("[{:>7.3}s] [pid={}] {}\n", elapsed, process::id(), msg);
        let _ = f.write_all(line.as_bytes());
    }
}

// ── Main ───────────────────────────────────────────────────────────

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vt-probe: {e:#}");
            process::exit(2);
        }
    };

    let log = match Log::open(args.log.clone()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vt-probe: {e:#}");
            process::exit(2);
        }
    };

    log.line(&format!(
        "START: vt={} target_user={} log={}",
        args.vt.display(),
        args.target_user,
        args.log.display(),
    ));

    // Probe must start as root — we need to open /dev/ttyN with
    // O_RDWR and then setresuid down.
    if !Uid::current().is_root() {
        log.line(&format!(
            "FAIL: probe must be invoked as root (current uid={})",
            Uid::current()
        ));
        process::exit(1);
    }

    match orchestrate(&args, &log) {
        Ok(()) => {
            log.line("END: probe completed successfully");
            process::exit(0);
        }
        Err(e) => {
            log.line(&format!("FAIL: {e:#}"));
            process::exit(1);
        }
    }
}

fn orchestrate(args: &Args, log: &Log) -> Result<()> {
    // 1. Resolve target user (must do this BEFORE forking; getpwnam
    //    isn't fork-safe in glibc and we want a clean lookup error
    //    path before we go heroic).
    //
    // The probe's target is the compositor's SYSTEM user
    // (halmasuit-compositor, uid 998) — NOT a logged-in human user.
    // CLAUDE.md's UID_MIN ≥ 1000 floor applies to the broker's
    // session-leader child (the eventual session uid), not to the
    // compositor system user itself. Sanity-check only that we're
    // not asked to drop to root (which would be a no-op).
    let user = nix::unistd::User::from_name(&args.target_user)
        .with_context(|| format!("getpwnam({})", args.target_user))?
        .ok_or_else(|| anyhow!("user {} not found", args.target_user))?;

    if user.uid.as_raw() == 0 {
        return Err(anyhow!(
            "target user {} resolves to uid 0; refusing to drop to root \
             (probe MUST end up unprivileged to test the no-cap path)",
            args.target_user
        ));
    }

    log.line(&format!(
        "RESOLVED: user={} uid={} gid={}",
        user.name, user.uid, user.gid
    ));

    // chown the log file to the target user so the post-drop child
    // can append to it. The parent created it as root, default mode
    // ~0644; without chown the child gets EACCES on every log.line.
    nix::unistd::chown(&args.log, Some(user.uid), Some(user.gid))
        .with_context(|| format!("chown {} to {}:{}", args.log.display(), user.uid, user.gid))?;
    log.line(&format!(
        "LOG-CHOWN: {} → {}:{}",
        args.log.display(),
        user.uid,
        user.gid
    ));

    // 2. Open the VT. The broker (= parent in this probe) opens the
    //    fd because /dev/ttyN is mode 0660 root:tty — the compositor
    //    user typically isn't in the tty group. The broker does NOT
    //    call VT_SETMODE here; the kernel records the VT_PROCESS
    //    controller as the calling pid, and we want that pid to be
    //    the compositor's, not the broker's.
    let vt_fd = open_vt(&args.vt).with_context(|| format!("open {}", args.vt.display()))?;
    log.line(&format!(
        "OPENED: {} fd={}",
        args.vt.display(),
        vt_fd.as_raw_fd()
    ));

    // 4. Fork. Parent waits; child does the unprivileged work.
    // SAFETY: fork is safe in single-threaded Rust binaries; we
    // haven't spawned any threads, and we don't use anything in the
    // child between fork and the explicit work that requires
    // post-fork-safety. The closures below ensure we don't
    // accidentally hold a lock from before the fork.
    let pid = unsafe_fork(log).context("fork")?;
    match pid {
        ForkResult::Parent { child } => {
            log.line(&format!("FORK: parent waiting for child pid={child}"));
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, 0)) => {
                    log.line("CHILD: exited 0");
                    Ok(())
                }
                Ok(WaitStatus::Exited(_, code)) => Err(anyhow!("child exited {code}")),
                Ok(WaitStatus::Signaled(_, sig, _)) => Err(anyhow!("child killed by {sig:?}")),
                Ok(other) => Err(anyhow!("child unexpected wait status: {other:?}")),
                Err(e) => Err(anyhow!("waitpid: {e}")),
            }
        }
        ForkResult::Child => {
            // The child returns a Result; on Err, log and exit 1
            // INSIDE the child (the parent will see exit-1 via
            // waitpid and propagate the failure).
            let result = child_main(vt_fd, &user, log);
            match result {
                Ok(()) => {
                    log.line("CHILD: success path complete");
                    process::exit(0);
                }
                Err(e) => {
                    log.line(&format!("CHILD-FAIL: {e:#}"));
                    process::exit(1);
                }
            }
        }
    }
}

enum ForkResult {
    Parent { child: Pid },
    Child,
}

// nix::unistd::fork is unsafe; wrap it once.
fn unsafe_fork(log: &Log) -> Result<ForkResult> {
    // SAFETY: see rationale in caller — this is a single-threaded
    // probe binary; fork is safe here. We immediately do well-defined
    // work in the child (no async-signal-unsafe operations between
    // fork and the explicit child path).
    #[expect(unsafe_code, reason = "fork(2) is inherently unsafe in Rust")]
    let r = unsafe { fork() };
    match r {
        Ok(nix::unistd::ForkResult::Parent { child }) => Ok(ForkResult::Parent { child }),
        Ok(nix::unistd::ForkResult::Child) => Ok(ForkResult::Child),
        Err(e) => {
            log.line(&format!("FORK-FAIL: {e}"));
            Err(anyhow!("fork: {e}"))
        }
    }
}

fn child_main(vt_fd: OwnedFd, user: &nix::unistd::User, log: &Log) -> Result<()> {
    // 1. setsid — become session leader of a new session with NO
    //    controlling TTY. TIOCSCTTY requires this.
    setsid().context("setsid")?;
    log.line("SETSID: OK");

    // 2. Drop bounding set entirely. This must happen BEFORE setresuid
    //    while we still have CAP_SETPCAP in effective. Matches
    //    halmasuit's drop_privileges pattern.
    drop_bounding_caps(log).context("drop bounding caps")?;

    // 3. setresgid + setresuid. All three components set to the same
    //    value so saved-set is also dropped.
    setresgid(user.gid, user.gid, user.gid).context("setresgid")?;
    setresuid(user.uid, user.uid, user.uid).context("setresuid")?;
    log.line(&format!(
        "PRIVILEGES-DROPPED: uid={} gid={}",
        user.uid, user.gid
    ));

    // 4. Empty caps entirely (no CAP_KILL, no nothing). The probe
    //    must hold zero caps so the empirical question — does
    //    TIOCSCTTY + VT_RELDISP work WITHOUT any cap — is testing
    //    what it claims to test.
    drop_all_active_caps(log).context("drop active caps")?;

    // 5. Verify caps are empty — load-bearing assertion of the probe's
    //    "I'm testing the no-cap path" claim.
    verify_caps_empty(log).context("verify caps empty")?;

    // 6. Block SIGUSR1/SIGUSR2 via sigprocmask; create signalfd.
    //    Done BEFORE the ioctls below so we can't miss an early
    //    signal (kernel sends acqsig on VT_SETMODE if the VT is
    //    currently active).
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGUSR1);
    mask.add(Signal::SIGUSR2);
    mask.thread_block().context("sigprocmask block SIGUSR1/2")?;
    let sfd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC).context("signalfd")?;
    log.line("SIGNALFD: blocked SIGUSR1+SIGUSR2, signalfd ready");

    // 7. TIOCSCTTY on the inherited fd. THIS IS THE FIRST LOAD-BEARING
    //    CALL. After this, tty2 is THIS process's controlling TTY,
    //    which gives us `perm` for subsequent VT_SETMODE/VT_RELDISP
    //    without holding CAP_SYS_TTY_CONFIG.
    let raw_fd = vt_fd.as_raw_fd();
    let tiocsctty_res = ioctl_int(raw_fd, TIOCSCTTY, 0);
    match tiocsctty_res {
        Ok(_) => log.line("TIOCSCTTY: success"),
        Err(errno) => {
            log.line(&format!(
                "TIOCSCTTY: FAILED errno={errno} ({})",
                errno_string(errno)
            ));
            return Err(anyhow!(
                "TIOCSCTTY failed (errno {errno}): broker-passes-fd model not viable, design must use fallback (broker also handles VT setup + RELDISP)"
            ));
        }
    }

    // 8. VT_SETMODE PROCESS. THIS IS THE SECOND LOAD-BEARING CALL.
    //    Now that the fd is our controlling TTY, perm=1 via
    //    (current->signal->tty == tty) — no cap needed. The kernel
    //    records THIS process's pid as the VT_PROCESS controller,
    //    so SIGUSR1/SIGUSR2 will be sent to us.
    let sigusr1 = u16::try_from(libc::SIGUSR1).expect("SIGUSR1 fits in u16");
    let sigusr2 = u16::try_from(libc::SIGUSR2).expect("SIGUSR2 fits in u16");
    let vtmode_res = set_vt_mode_process_via_fd(raw_fd, sigusr1, sigusr2);
    match vtmode_res {
        Ok(_) => log.line(
            "VT_SETMODE: PROCESS, relsig=SIGUSR1, acqsig=SIGUSR2 — OK (unprivileged, via TIOCSCTTY-derived perm)",
        ),
        Err(errno) => {
            log.line(&format!(
                "VT_SETMODE: FAILED errno={errno} ({})",
                errno_string(errno)
            ));
            return Err(anyhow!(
                "VT_SETMODE failed (errno {errno}) after TIOCSCTTY succeeded — \
                 unexpected; perm check should have passed via controlling TTY"
            ));
        }
    }

    // 9. Run the signal-handling loop. Receives SIGUSR2 (acquire) and
    //    SIGUSR1 (release), acks each with the appropriate VT_RELDISP
    //    variant, logs the VERDICT line.
    run_signal_loop(raw_fd, &sfd, log)?;

    // Drop the fd explicitly (Drop would do this anyway, but
    // explicit close on success is clearer).
    drop(vt_fd);
    Ok(())
}

/// The signalfd-driven cooperative-VT loop. Extracted from child_main
/// to keep the function under the workspace's per-fn line limit.
///
/// Returns Ok(()) when VERDICT: PASS has been logged (a full SIGUSR1
/// cycle completed with VT_RELDISP returning success). Returns Err
/// when the VERDICT was FAIL (any ioctl failed) or on a 30s timeout.
fn run_signal_loop(raw_fd: RawFd, sfd: &SignalFd, log: &Log) -> Result<()> {
    log.line(
        "WAITING: signalfd loop (timeout 30s); expecting SIGUSR2 (acquire) then SIGUSR1 (release)",
    );
    let mut received_acqsig = false;
    let deadline = Instant::now() + std::time::Duration::from_secs(30);

    loop {
        if Instant::now() >= deadline {
            log.line(
                "WAITING: TIMEOUT — neither SIGUSR1 nor SIGUSR2 sequence completed within 30s",
            );
            return Err(anyhow!("timeout waiting for SIGUSR1/SIGUSR2"));
        }
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u128::from(u16::MAX));
        let remaining = u16::try_from(remaining_ms).expect("clamped to u16::MAX");
        let timeout = PollTimeout::from(remaining);
        let mut pollfd = [PollFd::new(sfd.as_fd(), PollFlags::POLLIN)];
        let n = poll(&mut pollfd, timeout).context("poll signalfd")?;
        if n == 0 {
            continue;
        }
        let siginfo = sfd
            .read_signal()
            .context("read signalfd")?
            .ok_or_else(|| anyhow!("signalfd readable but no signal"))?;
        log.line(&format!("SIGNAL: received signo={}", siginfo.ssi_signo));

        let sigusr1 = u32::try_from(libc::SIGUSR1).expect("SIGUSR1 fits in u32");
        let sigusr2 = u32::try_from(libc::SIGUSR2).expect("SIGUSR2 fits in u32");

        if siginfo.ssi_signo == sigusr2 {
            handle_acqsig(raw_fd, log)?;
            received_acqsig = true;
            continue;
        }
        if siginfo.ssi_signo == sigusr1 {
            return handle_relsig(raw_fd, log, received_acqsig);
        }

        log.line(&format!(
            "SIGNAL: unexpected signo {}; ignoring",
            siginfo.ssi_signo
        ));
    }
}

/// Handle SIGUSR2 (acquire — kernel switched TO our VT).
/// Ack with VT_RELDISP(VT_ACKACQ).
fn handle_acqsig(raw_fd: RawFd, log: &Log) -> Result<()> {
    const VT_ACKACQ: libc::c_long = 2;
    match ioctl_int(raw_fd, VT_RELDISP, VT_ACKACQ) {
        Ok(_) => {
            log.line("VT_RELDISP(VT_ACKACQ): success — ack'd acquire (unprivileged)");
            Ok(())
        }
        Err(errno) => {
            log.line(&format!(
                "VT_RELDISP(VT_ACKACQ): FAILED errno={errno} ({})",
                errno_string(errno)
            ));
            log.line("VERDICT: FAIL — VT_RELDISP(VT_ACKACQ) requires CAP_SYS_TTY_CONFIG. Design must use fallback.");
            Err(anyhow!("VT_RELDISP(VT_ACKACQ) failed (errno {errno})"))
        }
    }
}

/// Handle SIGUSR1 (release — kernel switching AWAY from our VT).
/// Ack with VT_RELDISP(1). Logs the final VERDICT line.
fn handle_relsig(raw_fd: RawFd, log: &Log, received_acqsig: bool) -> Result<()> {
    match ioctl_int(raw_fd, VT_RELDISP, VT_RELDISP_PERMIT) {
        Ok(_) => {
            log.line("VT_RELDISP(permit): success — ack'd release (unprivileged)");
            if received_acqsig {
                log.line("VERDICT: PASS — TIOCSCTTY + VT_SETMODE + VT_RELDISP(both arg variants) work without CAP_SYS_TTY_CONFIG. Broker-passes-fd model is fully viable.");
            } else {
                log.line("VERDICT: PASS — TIOCSCTTY + VT_SETMODE + VT_RELDISP(permit) work without CAP_SYS_TTY_CONFIG. (Note: SIGUSR2/acquire path was not exercised in this run; broker-passes-fd model viable for the release path.)");
            }
            Ok(())
        }
        Err(errno) => {
            log.line(&format!(
                "VT_RELDISP(permit): FAILED errno={errno} ({})",
                errno_string(errno)
            ));
            log.line("VERDICT: FAIL — VT_RELDISP(permit) requires CAP_SYS_TTY_CONFIG. Design must use fallback (broker also brokers VT_RELDISP).");
            Err(anyhow!("VT_RELDISP(permit) failed (errno {errno})"))
        }
    }
}

// ── VT helpers ─────────────────────────────────────────────────────

fn open_vt(path: &Path) -> Result<OwnedFd> {
    let cs = CString::new(path.as_os_str().as_encoded_bytes()).context("CString from path")?;
    // O_RDWR | O_NOCTTY: don't accidentally make this our controlling
    // TTY here; that's the child's job after setsid().
    // SAFETY: standard libc::open call with valid CString and known
    // flag constants; returns -1 on error which we check.
    #[expect(unsafe_code, reason = "libc::open FFI")]
    let fd = unsafe { libc::open(cs.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(anyhow!("open: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fd is a valid open file descriptor returned by open(2);
    // wrapping it in OwnedFd transfers ownership to the Rust value.
    #[expect(unsafe_code, reason = "OwnedFd::from_raw_fd transfers fd ownership")]
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(owned)
}

/// Like the canonical `set_vt_mode_process`, but takes a raw fd and
/// returns errno on failure (so the caller can distinguish "no perm"
/// from other errors). Used by the child after TIOCSCTTY makes the
/// fd its controlling TTY.
fn set_vt_mode_process_via_fd(fd: RawFd, relsig: u16, acqsig: u16) -> Result<libc::c_int, i32> {
    let mode = VtMode {
        mode: VT_PROCESS,
        waitv: 0,
        relsig,
        acqsig,
        frsig: 0,
    };
    // SAFETY: VT_SETMODE takes a pointer to a vt_mode struct of known
    // layout (verified against linux/vt.h). The kernel reads the
    // struct; we pass a valid pointer to a stack-allocated VtMode.
    #[expect(unsafe_code, reason = "libc::ioctl FFI for VT_SETMODE")]
    let rc = unsafe {
        libc::ioctl(
            fd,
            VT_SETMODE as _,
            std::ptr::addr_of!(mode).cast::<libc::c_void>(),
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(rc)
    }
}

/// Wrapper for ioctls that take a single integer arg (TIOCSCTTY,
/// VT_RELDISP). Returns kernel return value on success or errno on
/// failure.
fn ioctl_int(fd: RawFd, req: u64, arg: libc::c_long) -> Result<libc::c_int, i32> {
    // SAFETY: libc::ioctl with a known request number and an integer
    // arg. The kernel interprets `arg` per-ioctl; TIOCSCTTY and
    // VT_RELDISP both take an int.
    #[expect(unsafe_code, reason = "libc::ioctl FFI")]
    let rc = unsafe { libc::ioctl(fd, req as _, arg) };
    if rc < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(rc)
    }
}

fn errno_string(errno: i32) -> String {
    std::io::Error::from_raw_os_error(errno).to_string()
}

// ── Capability handling ───────────────────────────────────────────

fn drop_bounding_caps(log: &Log) -> Result<()> {
    use caps::CapSet;
    for cap in caps::all() {
        caps::drop(None, CapSet::Bounding, cap).with_context(|| format!("bounding drop {cap}"))?;
    }
    log.line("CAPS: bounding set emptied");
    Ok(())
}

fn drop_all_active_caps(log: &Log) -> Result<()> {
    use caps::CapSet;
    let empty = caps::CapsHashSet::new();
    caps::set(None, CapSet::Effective, &empty).context("clear effective caps")?;
    caps::set(None, CapSet::Permitted, &empty).context("clear permitted caps")?;
    caps::set(None, CapSet::Inheritable, &empty).context("clear inheritable caps")?;
    log.line("CAPS: effective/permitted/inheritable cleared");
    Ok(())
}

fn verify_caps_empty(log: &Log) -> Result<()> {
    use caps::CapSet;
    let eff = caps::read(None, CapSet::Effective).context("read effective")?;
    let perm = caps::read(None, CapSet::Permitted).context("read permitted")?;
    let bnd = caps::read(None, CapSet::Bounding).context("read bounding")?;
    log.line(&format!(
        "CAPS-VERIFY: effective={} permitted={} bounding={}",
        eff.len(),
        perm.len(),
        bnd.len()
    ));
    if !eff.is_empty() || !perm.is_empty() || !bnd.is_empty() {
        return Err(anyhow!(
            "caps not fully empty: effective={eff:?} permitted={perm:?} bounding={bnd:?}"
        ));
    }
    log.line("CAPS-VERIFY: confirmed all-empty (no CAP_SYS_TTY_CONFIG, no anything)");
    Ok(())
}
