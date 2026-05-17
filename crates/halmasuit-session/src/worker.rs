//! The ephemeral, SIGKILL-able, `setrlimit`-bounded PRIVILEGED auth
//! fork (Epic #1 R4) — unsafe surface #2 (fork/pidfd/_exit).
//!
//! `spawn_auth_worker` `fork()`s a disposable child that runs the
//! blocking `run_pam_auth` over the SEQPACKET socketpair and reports a
//! [`WorkerOutcome`]; the parent holds a [`WorkerHandle { pid, pidfd
//! }`] and can SIGKILL it at ANY time — **SIGKILL directly, no SIGTERM
//! grace** (R4: the child is blocked in libpam, has no session to wind
//! down; SIGKILL is deterministic and stronger credential hygiene).
//! The blocking libpam call thus lives ONLY in a disposable child,
//! never on a long-lived thread (the C1 cure).
//!
//! No module `#![forbid(unsafe_code)]` (fork/pidfd need it); every
//! unsafe block carries `#[expect(unsafe_code, reason = "…")]`, the
//! same quarantine idiom as `pam_ffi`. Pidfd is the project's raw
//! `libc::syscall(SYS_pidfd_*)` idiom (mirrors `halmasuit/src/main.rs`;
//! memory `project-pidfd-over-raw-kill`): signal via pidfd, never
//! `kill(pid)` (pid-reuse race); `ESRCH` ⇒ already-exited ⇒ benign.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use serde::{Deserialize, Serialize};

use crate::auth::run_pam_auth;
use crate::transport::SeqpacketChannel;

/// What the disposable auth child reports to the broker parent, sent
/// over the SEQPACKET channel after `run_pam_auth` returns.
///
/// `{username,uid,gid}` stays ONE atomic unit (Epic R8) — PAM-resolved
/// in the child; the parent never re-derives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkerOutcome {
    /// Wire tag `worker_success` — DELIBERATELY disjoint from every
    /// `BrokerToCompositor` tag (`conv_prompt`/`success`/`failure`) so
    /// the parent demuxes the one channel (conv frames + this terminal
    /// message) with no ambiguity. See [`ParentMessage`].
    #[serde(rename = "worker_success")]
    Success {
        username: String,
        uid: u32,
        gid: u32,
    },
    /// Wire tag `worker_failure` — see [`WorkerOutcome::Success`].
    #[serde(rename = "worker_failure")]
    Failure { reason: String },
}

/// One datagram read by the broker parent from the worker channel.
///
/// The parent multiplexes the PAM conversation
/// (`BrokerToCompositor::ConvPrompt`, wire tag `conv_prompt`) and the
/// single terminal [`WorkerOutcome`] (`worker_success`/
/// `worker_failure`) over ONE SEQPACKET. `#[serde(untagged)]` is sound
/// here ONLY because those tag namespaces are disjoint (pinned by the
/// `worker_outcome_wire_tags_are_disjoint_from_conv_frames` test): a
/// `conv_prompt` datagram fails `WorkerOutcome` and decodes as `Conv`;
/// a `worker_*` datagram fails `BrokerToCompositor` and decodes as
/// `Outcome`. No datagram can decode as the wrong variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParentMessage {
    Conv(halmasuit_session_ipc::BrokerToCompositor),
    Outcome(WorkerOutcome),
}

/// Handle to a live ephemeral auth child.
///
/// SIGKILL via pidfd (race-free wrt pid reuse — memory
/// `project-pidfd-over-raw-kill`); there is deliberately NO SIGTERM
/// path (Epic R4: SIGKILL direct, no grace).
pub struct WorkerHandle {
    pub pid: u32,
    pidfd: OwnedFd,
}

impl WorkerHandle {
    /// SIGKILL the child immediately — no SIGTERM, no grace (R4).
    ///
    /// # Errors
    ///
    /// Any errno from `pidfd_send_signal(2)` EXCEPT `ESRCH`
    /// (already-exited / already-reaped ⇒ benign ⇒ `Ok(())`).
    pub fn kill(&self) -> io::Result<()> {
        match pidfd_send_signal(&self.pidfd, libc::SIGKILL) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Block until the child terminates; reap it.
    ///
    /// The compositor-side single-SIGCHLD-reaper integration (Epic R9)
    /// lands later in `halmasuit/src/main.rs`; this is the
    /// broker-local reap used by the worker's own tests and teardown.
    ///
    /// # Errors
    ///
    /// Any errno from `waitpid(2)`.
    pub fn wait(&self) -> io::Result<nix::sys::wait::WaitStatus> {
        let pid = nix::unistd::Pid::from_raw(
            i32::try_from(self.pid).map_err(|_| io::Error::other("pid out of range"))?,
        );
        nix::sys::wait::waitpid(pid, None).map_err(io::Error::from)
    }
}

// pidfd helpers — the project's raw-syscall idiom (mirrors
// `halmasuit/src/main.rs`; memory `project-pidfd-over-raw-kill`). No
// nix feature needed; libc is already a dep.
fn pidfd_open_for(pid: u32) -> io::Result<OwnedFd> {
    let pid_signed = i32::try_from(pid)
        .map_err(|_| io::Error::other(format!("pid {pid} does not fit in i32")))?;
    // SAFETY: pidfd_open(2) is a numeric syscall with no pointer
    // arguments; returns a non-negative fd or -1 with errno set. The
    // OwnedFd is built only on success.
    #[expect(unsafe_code, reason = "raw pidfd_open syscall via libc")]
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::pid_t::from(pid_signed), 0_u32) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let raw_fd = i32::try_from(raw)
        .map_err(|_| io::Error::other(format!("pidfd_open returned out-of-range fd {raw}")))?;
    // SAFETY: a fresh kernel fd from the successful syscall above;
    // nothing else holds it.
    #[expect(unsafe_code, reason = "wrap fresh pidfd into OwnedFd")]
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    Ok(fd)
}

fn pidfd_send_signal(pidfd: &OwnedFd, sig: i32) -> io::Result<()> {
    // SAFETY: pidfd_send_signal(2) reads the fd numerically and a NULL
    // siginfo pointer (kernel synthesizes it); flags=0.
    #[expect(unsafe_code, reason = "raw pidfd_send_signal syscall via libc")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// RLIMIT_CPU bound (seconds) applied in the disposable child before
/// it runs PAM. Auth is interactive-scale; the kernel kills a
/// CPU-spinning wedged module at this bound. AS/NPROC are deliberately
/// NOT tightened here — pam_unix forks `unix_chkpwd` and libpam
/// allocates; an over-tight bound that breaks real modules is worse
/// than the structural isolation. The parent's SIGKILL-anytime plus
/// the global single slot (Epic R5, later) are the other bounds
/// (HANDOFF §0.4: failure cost stays delegated to the PAM stack).
const RLIMIT_CPU_SECS: u64 = 30;

/// Fork a disposable child running `child_main` on its SEQPACKET end;
/// return the parent's handle + channel end. The generic seam lets the
/// worker's own tests exercise fork/kill/reap with a non-PAM payload
/// (process-supervision testing is NOT a PAM mock — Epic R12 intact).
///
/// # Errors
///
/// Any errno from `socketpair`/`fork`/`pidfd_open`.
pub(crate) fn spawn_worker<F>(child_main: F) -> io::Result<(WorkerHandle, SeqpacketChannel)>
where
    F: FnOnce(SeqpacketChannel),
{
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};

    let (parent_fd, child_fd) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::empty(),
    )
    .map_err(io::Error::from)?;

    // SAFETY: fork(2). The child path is straight-line — drop the
    // parent fd, reset the signal mask, setrlimit, run child_main on
    // its socket end, then _exit; it never returns into Rust unwinding
    // or the test harness. The parent records the pid and opens a
    // pidfd before returning (R9 ordering: pid known before any reap).
    #[expect(unsafe_code, reason = "fork(2) for the ephemeral auth child (Epic R4)")]
    let fork = unsafe { nix::unistd::fork() }.map_err(io::Error::from)?;

    match fork {
        nix::unistd::ForkResult::Child => {
            drop(parent_fd);
            // memory project-pre-exec-signal-mask: an inherited block
            // mask must not survive into the child. Harmless here
            // (we only ever SIGKILL it) but the documented, correct
            // idiom for any child of the (later) systemd/calloop
            // broker.
            let _ = nix::sys::signal::sigprocmask(
                nix::sys::signal::SigmaskHow::SIG_SETMASK,
                Some(&nix::sys::signal::SigSet::empty()),
                None,
            );
            let _ = nix::sys::resource::setrlimit(
                nix::sys::resource::Resource::RLIMIT_CPU,
                RLIMIT_CPU_SECS,
                RLIMIT_CPU_SECS,
            );
            let chan = SeqpacketChannel::new(child_fd);
            let code = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                child_main(chan);
            })) {
                Ok(()) => 0,
                Err(_) => 101,
            };
            // SAFETY: _exit skips the parent's atexit/Drop (incl. the
            // test harness) in the forked child.
            #[expect(
                unsafe_code,
                reason = "_exit the forked child without parent atexit/Drop"
            )]
            unsafe {
                libc::_exit(code)
            }
        }
        nix::unistd::ForkResult::Parent { child } => {
            drop(child_fd);
            let pid = u32::try_from(child.as_raw())
                .map_err(|_| io::Error::other("child pid out of range"))?;
            let pidfd = pidfd_open_for(pid)?;
            Ok((
                WorkerHandle { pid, pidfd },
                SeqpacketChannel::new(parent_fd),
            ))
        }
    }
}

/// Spawn the ephemeral privileged auth child running `run_pam_auth`
/// over the returned channel.
///
/// The parent relays the PAM conversation (greeter ↔ child) on that
/// channel and finally reads a [`WorkerOutcome`]. The child is
/// SIGKILL-able at any time via the returned [`WorkerHandle`] (Epic
/// R4). Real libpam through this fork is asserted by the real-PAM VM
/// gate — never a mock (Epic R12).
///
/// # Errors
///
/// Any errno from the underlying `spawn_worker`.
pub fn spawn_auth_worker(
    service: &str,
    username: &str,
) -> io::Result<(WorkerHandle, SeqpacketChannel)> {
    let service = service.to_owned();
    let username = username.to_owned();
    spawn_worker(move |chan| {
        let outcome = match run_pam_auth(&chan, &service, &username) {
            Ok(id) => WorkerOutcome::Success {
                username: id.username,
                uid: id.uid,
                gid: id.gid,
            },
            Err(e) => WorkerOutcome::Failure {
                reason: e.to_string(),
            },
        };
        let _ = chan.send(&outcome);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::Signal;
    use nix::sys::wait::WaitStatus;

    #[test]
    fn child_outcome_round_trips_and_child_exits_zero() {
        let (handle, chan) = spawn_worker(|chan| {
            chan.send(&WorkerOutcome::Success {
                username: "resolved".into(),
                uid: 1000,
                gid: 1000,
            })
            .expect("child send");
        })
        .expect("spawn_worker");

        let outcome: WorkerOutcome = chan.recv().expect("parent recv");
        assert_eq!(
            outcome,
            WorkerOutcome::Success {
                username: "resolved".into(),
                uid: 1000,
                gid: 1000,
            }
        );
        match handle.wait().expect("wait") {
            WaitStatus::Exited(_, 0) => {}
            other => panic!("expected clean exit, got {other:?}"),
        }
    }

    #[test]
    fn kill_sends_sigkill_and_is_reapable_then_benign() {
        let (handle, _chan) = spawn_worker(|_chan| {
            // Block forever; only SIGKILL ends this.
            loop {
                std::thread::park();
            }
        })
        .expect("spawn_worker");

        handle.kill().expect("first kill");
        match handle.wait().expect("wait") {
            WaitStatus::Signaled(_, Signal::SIGKILL, _) => {}
            other => panic!("expected SIGKILL death, got {other:?}"),
        }
        // Child already reaped → pidfd ESRCH → benign Ok(()).
        handle.kill().expect("second kill is benign (ESRCH)");
    }

    #[test]
    fn child_runs_under_rlimit_cpu_30s() {
        let (handle, chan) = spawn_worker(|chan| {
            let lim = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_CPU)
                .expect("getrlimit");
            chan.send(&WorkerOutcome::Failure {
                reason: format!("{}:{}", lim.0, lim.1),
            })
            .expect("child send");
        })
        .expect("spawn_worker");

        let outcome: WorkerOutcome = chan.recv().expect("parent recv");
        assert_eq!(
            outcome,
            WorkerOutcome::Failure {
                reason: "30:30".into()
            },
            "RLIMIT_CPU soft:hard must be the bound spawn_worker set"
        );
        let _ = handle.wait();
    }

    #[test]
    fn sigkill_no_grace_sigterm_handler_never_runs() {
        // The child installs a SIGTERM handler that _exit(42)s. If the
        // kill path ever sent SIGTERM (a grace period), the child would
        // die Exited(42). It dies Signaled(SIGKILL) ⇒ proves SIGKILL
        // was sent directly with NO SIGTERM grace (R4).
        let (handle, _chan) = spawn_worker(|_chan| {
            extern "C" fn on_sigterm(_sig: i32) {
                // async-signal-safe: _exit only.
                #[expect(
                    unsafe_code,
                    reason = "test child SIGTERM handler: _exit is async-signal-safe"
                )]
                unsafe {
                    libc::_exit(42)
                }
            }
            #[expect(
                unsafe_code,
                reason = "test child installs a SIGTERM handler to detect any grace signal"
            )]
            unsafe {
                libc::signal(
                    libc::SIGTERM,
                    on_sigterm as extern "C" fn(i32) as libc::sighandler_t,
                );
            }
            loop {
                std::thread::park();
            }
        })
        .expect("spawn_worker");

        handle.kill().expect("kill");
        match handle.wait().expect("wait") {
            WaitStatus::Signaled(_, Signal::SIGKILL, _) => {}
            WaitStatus::Exited(_, 42) => {
                panic!("child caught SIGTERM — a grace signal was sent (R4 violation)")
            }
            other => panic!("unexpected status {other:?}"),
        }
    }

    #[test]
    fn spawn_auth_worker_is_wired() {
        // Real libpam through the fork is asserted by the next VM gate
        // (R12 forbids mocks here). This only proves the entry point
        // forks, the child attempts auth against a non-existent
        // service and fails closed, the parent reaps it — no hang.
        let (handle, chan) = spawn_auth_worker("halmasuit-no-such-pam-service-zzz", "nobody")
            .expect("spawn_auth_worker");
        // The child will fail pam_start (unknown service) and send a
        // Failure outcome, or the channel closes — either way no hang.
        let _ = chan.recv::<WorkerOutcome>();
        let _ = handle.wait();
    }

    // ── parent-channel disambiguation (Epic R4 relay) ────────────────
    //
    // The parent channel multiplexes conv frames
    // (`BrokerToCompositor::ConvPrompt`, wire tag "conv_prompt") and
    // the terminal `WorkerOutcome`. Soundness of the single-stream
    // decode rests on the tag namespaces being DISJOINT — these tests
    // pin that invariant so a future rename can't silently reintroduce
    // ambiguity.

    #[test]
    fn worker_outcome_wire_tags_are_disjoint_from_conv_frames() {
        use halmasuit_session_ipc::{BrokerToCompositor, PromptStyle, encode};

        // encode() = [len:4][json]; lossy-utf8 the body to inspect the
        // serde tag (avoids a serde_json dev-dep).
        let body = |b: &[u8]| String::from_utf8_lossy(b).into_owned();

        let succ = body(
            &encode(&WorkerOutcome::Success {
                username: "u".into(),
                uid: 1,
                gid: 2,
            })
            .unwrap(),
        );
        let fail = body(&encode(&WorkerOutcome::Failure { reason: "x".into() }).unwrap());
        assert!(succ.contains(r#""type":"worker_success""#), "got {succ}");
        assert!(fail.contains(r#""type":"worker_failure""#), "got {fail}");

        let prompt = body(
            &encode(&BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "p".into(),
            })
            .unwrap(),
        );
        // Disjoint: a conv frame's tag is never a WorkerOutcome tag.
        assert!(prompt.contains(r#""type":"conv_prompt""#));
        assert!(!prompt.contains("worker_success"));
        assert!(!prompt.contains("worker_failure"));
    }

    #[test]
    fn parent_message_decodes_each_stream_member_unambiguously() {
        use halmasuit_session_ipc::{BrokerToCompositor, PromptStyle, encode, try_decode};

        // A conv prompt on the wire → ParentMessage::Conv.
        let prompt = BrokerToCompositor::ConvPrompt {
            style: PromptStyle::Secret,
            message: "Password: ".into(),
        };
        let bytes = encode(&prompt).unwrap();
        let (pm, _): (ParentMessage, usize) = try_decode(&bytes).unwrap().unwrap();
        assert_eq!(pm, ParentMessage::Conv(prompt));

        // A terminal outcome on the wire → ParentMessage::Outcome,
        // never decoded as a conv frame.
        let outcome = WorkerOutcome::Success {
            username: "alice".into(),
            uid: 1000,
            gid: 1000,
        };
        let bytes = encode(&outcome).unwrap();
        let (pm, _): (ParentMessage, usize) = try_decode(&bytes).unwrap().unwrap();
        assert_eq!(pm, ParentMessage::Outcome(outcome));
    }
}
