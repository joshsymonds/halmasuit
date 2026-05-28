//! Epic #71 R1.3 — compositor-side VT-switching IPC.
//!
//! The compositor connects to the privileged broker, sends
//! `RequestVtSwitch`, drives the cooperative-switching setup on the
//! inherited VT fd, and reports the broker's verdict back. This
//! module covers the **IPC dance only** — keyboard chord interception
//! (R2.1) and live DRM pause/resume (R2.2) layer on top once the
//! broker↔compositor protocol is sound.
//!
//! ## Protocol sequence (mirrors `halmasuit-session-ipc`'s module doc)
//!
//! ```text
//! C → B  RequestVtSwitch { target_vt }
//! B → C  VtSwitchPrepare           (fd via SCM_RIGHTS)
//!        │   compositor: hooks.before_drop_master()
//!        │   compositor: setsid (best-effort) + TIOCSCTTY + VT_SETMODE
//!        │   compositor: hooks.after_drop_master()
//! C → B  VtSwitchMasterDropped
//! B → C  VtSwitchActivated  OR  VtSwitchRejected { reason }
//!        │   compositor: on Activated → hooks.on_activated(fd)
//! ```
//!
//! The `VtSwitchHooks` callback bundle is the seam R2 plugs into for
//! real DRM master release / acquire. R1.3 tests pass no-op stubs.
//!
//! Like `broker_spawn_greeter`, the dance is **synchronous**: it fires
//! at one specific moment (the user pressed Ctrl+Alt+F<n>), not on the
//! render-loop hot path. The CLAUDE.md "compositor never blocks the
//! render thread on broker IPC" rule applies to the relay (greeter
//! conversation) — transient one-shot requests on dedicated
//! connections are fine to block on, same posture as the existing
//! `RequestRootFd` and `SpawnGreeter` paths.
//!
//! ## What this module deliberately does NOT do
//!
//! - Wire keyboard chord interception (Ctrl+Alt+F1..F12). That's R2.1.
//! - Drive real DRM pause/resume. The compositor's existing
//!   `DrmBackend` doesn't expose a pause/resume API yet; that's R2.2.
//!   This module accepts callbacks so R2.2 can plug in without
//!   reshaping the IPC layer.
//! - Run the signalfd loop for SIGUSR1/SIGUSR2 after `VtSwitchActivated`.
//!   The on-activated callback receives the fd and is expected to
//!   register the signalfd source with calloop; that wiring is R2.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, VtSwitchRejectReason};

use crate::broker_session::{SeqpacketChannel, WireError, connect_broker};

/// Outcome of a `VtSwitcher::request_switch` call.
#[derive(Debug)]
#[allow(dead_code, reason = "fields consumed by R2.1 keybind handler")]
pub enum VtSwitchOutcome {
    /// Broker performed `VT_ACTIVATE`; kernel switched. `vt_fd` is the
    /// inherited VT fd, already TIOCSCTTY'd and VT_SETMODE'd. R2's
    /// `on_activated` hook receives this and registers the signalfd
    /// source. Tests use the fd to assert the dance reached the
    /// activated state.
    Activated { vt_fd: OwnedFd },
    /// Broker refused the request. The wire-protocol reason is
    /// preserved so the caller can route into "log + back off"
    /// (RateLimited) vs "log + escalate" (BrokerInternal) etc.
    Rejected { reason: VtSwitchRejectReason },
}

/// Failure modes for the IPC dance itself (orthogonal to a broker
/// `VtSwitchRejected` reply, which is signalled via
/// [`VtSwitchOutcome::Rejected`]).
#[derive(Debug, thiserror::Error)]
pub enum VtSwitchError {
    /// Socket I/O error (connect, send, recv).
    #[error("io: {source}")]
    Io {
        #[from]
        source: io::Error,
    },
    /// Codec error on the wire format.
    #[error("wire: {source}")]
    Wire { source: WireError },
    /// Broker spoke an unexpected frame, omitted a required SCM_RIGHTS
    /// fd, or peer-closed mid-dance.
    #[error("protocol: {0}")]
    Protocol(String),
}

impl From<WireError> for VtSwitchError {
    fn from(source: WireError) -> Self {
        Self::Wire { source }
    }
}

/// Callbacks the IPC dance invokes at the well-defined cooperative
/// points. R1.3 tests pass no-ops; R2.2 will plug in real DRM
/// pause/resume here without reshaping this module.
pub struct VtSwitchHooks<'a> {
    /// Called immediately after the broker's `VtSwitchPrepare` frame
    /// arrives with the VT fd, BEFORE the compositor TIOCSCTTYs +
    /// VT_SETMODEs. R2.2's hook calls `drm.pause()` here.
    pub before_drop_master: &'a dyn Fn() -> io::Result<()>,
    /// Called immediately AFTER TIOCSCTTY + VT_SETMODE succeed and
    /// BEFORE the `VtSwitchMasterDropped` ack is sent to the broker.
    /// R2.2's hook ensures the render loop is paused and the DRM fd
    /// is released here.
    pub after_drop_master: &'a dyn Fn() -> io::Result<()>,
    /// Called on `VtSwitchActivated`, with the inherited VT fd (now
    /// the compositor's controlling TTY). R2's hook installs a
    /// signalfd source on this fd via calloop.
    pub on_activated: &'a dyn Fn(BorrowedFd<'_>) -> io::Result<()>,
}

/// Trait abstraction over the TIOCSCTTY + VT_SETMODE setup on the
/// inherited fd. Production uses [`RealVtFdSetup`] which issues the
/// real ioctls; unit tests pass a no-op so the IPC dance can be
/// driven without root + an actual TTY.
pub trait VtFdSetup {
    /// Called on the inherited VT fd after `VtSwitchPrepare` arrives,
    /// before the compositor's `after_drop_master` hook.
    ///
    /// # Errors
    /// Any errno from `setsid(2)` / `ioctl(TIOCSCTTY)` /
    /// `ioctl(VT_SETMODE)`.
    fn tiocsctty_and_setmode(&self, fd: BorrowedFd<'_>) -> io::Result<()>;
}

/// Production implementation: real `setsid` + `TIOCSCTTY` +
/// `VT_SETMODE` on the inherited VT fd. Per Phase 0's verdict
/// (`crates/halmasuit-vt-probe/README.md`), the compositor (running
/// as a non-root system user) can do all three without
/// `CAP_SYS_TTY_CONFIG`, because `TIOCSCTTY` makes the inherited fd
/// the controlling TTY which satisfies the kernel's `perm` check for
/// subsequent VT ioctls.
#[allow(
    dead_code,
    reason = "wired into VtSwitcher production path in R2.1 keybind handler"
)]
pub struct RealVtFdSetup;

impl VtFdSetup for RealVtFdSetup {
    fn tiocsctty_and_setmode(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        // `setsid()` is best-effort. The compositor is typically a
        // session leader already (systemd unit, no controlling TTY
        // inherited from PID 1's session); the EPERM-on-already-leader
        // case is fine.
        match nix::unistd::setsid() {
            Ok(_) | Err(nix::errno::Errno::EPERM) => {}
            Err(e) => return Err(io::Error::other(format!("setsid: {e}"))),
        }
        tiocsctty(fd.as_raw_fd())?;
        vt_setmode_process(fd.as_raw_fd(), libc::SIGUSR1, libc::SIGUSR2)?;
        Ok(())
    }
}

/// No-op fd-setup used by unit tests. The IPC dance is driven against
/// a non-TTY fd (a pipe end, typically), so the real ioctls would
/// fail; this skips them.
#[allow(
    dead_code,
    reason = "constructed only by #[cfg(test)] tests in this module"
)]
pub struct NoopVtFdSetup;

impl VtFdSetup for NoopVtFdSetup {
    fn tiocsctty_and_setmode(&self, _fd: BorrowedFd<'_>) -> io::Result<()> {
        Ok(())
    }
}

/// Driver for the compositor↔broker VT-switching IPC.
///
/// Owns the broker socket path; each `request_switch` opens a fresh
/// transient connection (same one-shot model as `broker_spawn_greeter`
/// and the cross-pivot `RequestRootFd` retry path).
#[allow(
    dead_code,
    reason = "constructed by R2.1 keybind handler; R1.3 lands the dance only"
)]
pub struct VtSwitcher {
    broker_socket: PathBuf,
}

impl VtSwitcher {
    /// New switcher configured with the broker's seqpacket socket
    /// path. Typically set from halmasuit's existing
    /// `broker_socket_path_from_env()` or the equivalent config knob.
    #[allow(dead_code, reason = "called by R2.1 keybind handler")]
    #[must_use]
    pub const fn new(broker_socket: PathBuf) -> Self {
        Self { broker_socket }
    }

    /// Drive the full IPC dance to switch to `target_vt`.
    ///
    /// Blocks the caller. Intended to fire at one specific moment
    /// (the user-triggered keychord — not yet wired), NOT on the
    /// render loop's hot path.
    ///
    /// # Errors
    ///
    /// Returns [`VtSwitchError`] on socket / wire / protocol
    /// failures. A broker `VtSwitchRejected` is NOT an error — it's
    /// returned as `Ok(VtSwitchOutcome::Rejected{reason})`. The
    /// caller distinguishes "transport failed" (Err) from "broker
    /// said no" (Ok-with-Rejected).
    #[allow(dead_code, reason = "called by R2.1 keybind handler")]
    pub fn request_switch(
        &self,
        target_vt: u8,
        hooks: &VtSwitchHooks<'_>,
        fd_setup: &dyn VtFdSetup,
    ) -> Result<VtSwitchOutcome, VtSwitchError> {
        let chan = connect_broker(&self.broker_socket).map_err(|e| {
            VtSwitchError::Protocol(format!("connect_broker for RequestVtSwitch: {e:?}"))
        })?;

        run_dance_on_channel(&chan, target_vt, hooks, fd_setup)
    }
}

/// The IPC dance, factored out of `VtSwitcher::request_switch` so unit
/// tests can drive it on a pre-connected socketpair without touching
/// the filesystem.
fn run_dance_on_channel(
    chan: &SeqpacketChannel,
    target_vt: u8,
    hooks: &VtSwitchHooks<'_>,
    fd_setup: &dyn VtFdSetup,
) -> Result<VtSwitchOutcome, VtSwitchError> {
    chan.send(&CompositorToBroker::RequestVtSwitch { target_vt })?;

    // First reply: either VtSwitchPrepare (with fd) or
    // VtSwitchRejected (no fd; validation failed at the broker).
    let prepare_fd = match recv_with_poll(chan, Duration::from_secs(8))? {
        (BrokerToCompositor::VtSwitchPrepare, Some(fd)) => fd,
        (BrokerToCompositor::VtSwitchPrepare, None) => {
            return Err(VtSwitchError::Protocol(
                "broker sent VtSwitchPrepare without SCM_RIGHTS fd".to_owned(),
            ));
        }
        (BrokerToCompositor::VtSwitchRejected { reason }, _) => {
            return Ok(VtSwitchOutcome::Rejected { reason });
        }
        (other, _) => {
            return Err(VtSwitchError::Protocol(format!(
                "broker reply was not VtSwitchPrepare or VtSwitchRejected: {other:?}"
            )));
        }
    };

    (hooks.before_drop_master)()
        .map_err(|e| VtSwitchError::Protocol(format!("before_drop_master hook failed: {e}")))?;

    fd_setup
        .tiocsctty_and_setmode(prepare_fd.as_fd())
        .map_err(|e| VtSwitchError::Protocol(format!("fd_setup failed: {e}")))?;

    (hooks.after_drop_master)()
        .map_err(|e| VtSwitchError::Protocol(format!("after_drop_master hook failed: {e}")))?;

    chan.send(&CompositorToBroker::VtSwitchMasterDropped)?;

    match recv_with_poll(chan, Duration::from_secs(8))? {
        (BrokerToCompositor::VtSwitchActivated, _) => {
            (hooks.on_activated)(prepare_fd.as_fd())
                .map_err(|e| VtSwitchError::Protocol(format!("on_activated hook failed: {e}")))?;
            Ok(VtSwitchOutcome::Activated { vt_fd: prepare_fd })
        }
        (BrokerToCompositor::VtSwitchRejected { reason }, _) => {
            Ok(VtSwitchOutcome::Rejected { reason })
        }
        (other, _) => Err(VtSwitchError::Protocol(format!(
            "broker post-ack reply was not VtSwitchActivated or VtSwitchRejected: {other:?}"
        ))),
    }
}

/// Receive one frame from the compositor-side channel with a bounded
/// poll. SeqpacketChannel's `recv_with_fd` is MSG_DONTWAIT so we need
/// to poll first to know data is ready.
fn recv_with_poll(
    chan: &SeqpacketChannel,
    timeout: Duration,
) -> Result<(BrokerToCompositor, Option<OwnedFd>), VtSwitchError> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    let timeout_ms =
        u16::try_from(timeout.as_millis().min(u128::from(u16::MAX))).expect("clamped to u16::MAX");
    let mut pollfd = [PollFd::new(chan.as_fd(), PollFlags::POLLIN)];
    let n = poll(&mut pollfd, PollTimeout::from(timeout_ms))
        .map_err(|e| VtSwitchError::Protocol(format!("poll broker: {e}")))?;
    if n == 0 {
        return Err(VtSwitchError::Protocol(
            "broker did not reply within timeout".to_owned(),
        ));
    }
    let (frame, fd) = chan
        .recv_with_fd()?
        .ok_or_else(|| VtSwitchError::Protocol("broker closed mid-dance".to_owned()))?;
    Ok((frame, fd))
}

// ── FFI quarantine — TIOCSCTTY + VT_SETMODE ─────────────────────────
//
// Same #[expect(unsafe_code)] pattern as `pidfd_send_signal` and
// `capset_permitted_effective_cap_kill` in main.rs. The workspace-
// level `unsafe_code = "warn"` lint (denied by -D warnings) is the
// gate; each unsafe block carries an explicit reason.

/// TIOCSCTTY ioctl number, per `asm-generic/ioctls.h`.
#[allow(dead_code, reason = "wired in R2.1 production path via RealVtFdSetup")]
const TIOCSCTTY: u64 = 0x540E;
/// VT_SETMODE ioctl number, per `linux/vt.h`.
#[allow(dead_code, reason = "wired in R2.1 production path via RealVtFdSetup")]
const VT_SETMODE: u64 = 0x5602;
/// VT_SETMODE mode value: cooperative-switching with kernel signals.
#[allow(dead_code, reason = "wired in R2.1 production path via RealVtFdSetup")]
const VT_PROCESS: u8 = 0x01;

#[repr(C)]
#[allow(dead_code, reason = "wired in R2.1 production path via RealVtFdSetup")]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: u16,
    acqsig: u16,
    frsig: u16,
}

/// Issue `TIOCSCTTY(fd, 0)` to make the inherited fd the calling
/// process's controlling TTY. Caller must be a session leader with no
/// existing controlling TTY (a `setsid()` immediately before is the
/// canonical precondition).
#[allow(dead_code, reason = "wired in R2.1 production path via RealVtFdSetup")]
fn tiocsctty(fd: RawFd) -> io::Result<()> {
    // SAFETY: `ioctl(fd, TIOCSCTTY, 0)` reads only the request number
    // and an integer arg; no memory dereference. fd is a borrowed
    // valid descriptor from the caller. Errno is checked.
    #[expect(unsafe_code, reason = "raw TIOCSCTTY ioctl via libc")]
    let rc = unsafe { libc::ioctl(fd, TIOCSCTTY as _, 0_i32) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Issue `VT_SETMODE(fd, &VtMode{PROCESS, relsig, acqsig})` to switch
/// the kernel into cooperative-switching mode and register the
/// signal numbers the kernel will use for release / acquire events.
#[allow(dead_code, reason = "wired in R2.1 production path via RealVtFdSetup")]
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
    // SAFETY: `ioctl(fd, VT_SETMODE, &mode)` reads the vt_mode struct
    // by pointer. The struct is a valid stack allocation of the
    // correct repr(C) layout for `struct vt_mode` from `linux/vt.h`.
    // Errno is checked.
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

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    use halmasuit_session_ipc::{
        BrokerToCompositor, CompositorToBroker, VtSwitchRejectReason, encode, try_decode,
    };
    use nix::sys::socket::{
        AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, recvmsg, sendmsg, socketpair,
    };

    use super::*;
    use crate::broker_session::SeqpacketChannel;

    /// Connected SOCK_SEQPACKET pair: compositor-side as a real
    /// `SeqpacketChannel`, broker-side as a raw `OwnedFd` so we can
    /// decode `CompositorToBroker` (which the compositor sends) and
    /// encode `BrokerToCompositor` (which the broker replies).
    /// `SeqpacketChannel`'s typed `recv` only decodes the compositor's
    /// inbound direction, so the broker side uses raw nix calls.
    fn pair() -> (SeqpacketChannel, OwnedFd) {
        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .expect("socketpair");
        (SeqpacketChannel::new(a), b)
    }

    /// Receive one `CompositorToBroker` datagram on the broker side
    /// of the test socketpair. No SCM_RIGHTS is expected for any
    /// compositor → broker frame in the VT-switching protocol.
    fn broker_recv(b: &OwnedFd) -> CompositorToBroker {
        let mut buf = vec![0_u8; 4 + 1024];
        let mut iov = [io::IoSliceMut::new(&mut buf)];
        let mut cmsg = nix::cmsg_space!(RawFd);
        let r = recvmsg::<()>(b.as_raw_fd(), &mut iov, Some(&mut cmsg), MsgFlags::empty())
            .expect("broker recvmsg");
        let n = r.bytes;
        let (msg, consumed) = try_decode::<CompositorToBroker>(&buf[..n])
            .expect("decode")
            .expect("complete frame");
        assert_eq!(consumed, n, "broker frame must be exactly one datagram");
        msg
    }

    /// Send a `BrokerToCompositor` frame to the compositor side, with
    /// an optional SCM_RIGHTS fd (for `VtSwitchPrepare`).
    fn broker_send(b: &OwnedFd, msg: &BrokerToCompositor, fd: Option<BorrowedFd<'_>>) {
        let bytes = encode(msg).expect("encode");
        let iov = [io::IoSlice::new(&bytes)];
        let raw_fd_storage = [fd.map_or(-1, |b| b.as_raw_fd())];
        let cmsgs: &[ControlMessage<'_>] = if fd.is_some() {
            &[ControlMessage::ScmRights(&raw_fd_storage)]
        } else {
            &[]
        };
        let n = sendmsg::<()>(b.as_raw_fd(), &iov, cmsgs, MsgFlags::empty(), None)
            .expect("broker sendmsg");
        assert_eq!(n, bytes.len());
    }

    /// Hook recorder: counts invocations of each callback so tests
    /// can assert the dance reached the right state.
    #[derive(Default)]
    struct HookRecorder {
        before: AtomicU32,
        after: AtomicU32,
        activated: AtomicU32,
    }

    impl HookRecorder {
        fn calls(&self) -> (u32, u32, u32) {
            (
                self.before.load(Ordering::SeqCst),
                self.after.load(Ordering::SeqCst),
                self.activated.load(Ordering::SeqCst),
            )
        }
    }

    /// Happy path: broker scripts the full Prepare(+fd) →
    /// MasterDropped → Activated sequence. All three hooks fire in
    /// order; outcome carries the activated fd.
    #[test]
    fn vt_switch_happy_path() {
        let (c_side, b_side) = pair();
        let recorder = Arc::new(HookRecorder::default());

        // The fd we pass through SCM_RIGHTS is a pipe end — not a
        // real TTY. NoopVtFdSetup skips the ioctls that would fail.
        let (pipe_r, pipe_w) = nix::unistd::pipe().expect("pipe");
        let _keep_pipe_w_open = pipe_w;

        let b_thread = thread::spawn(move || {
            let msg = broker_recv(&b_side);
            assert!(
                matches!(msg, CompositorToBroker::RequestVtSwitch { target_vt: 2 }),
                "expected RequestVtSwitch{{target_vt: 2}}, got {msg:?}"
            );
            broker_send(
                &b_side,
                &BrokerToCompositor::VtSwitchPrepare,
                Some(pipe_r.as_fd()),
            );
            let msg = broker_recv(&b_side);
            assert!(
                matches!(msg, CompositorToBroker::VtSwitchMasterDropped),
                "expected VtSwitchMasterDropped, got {msg:?}"
            );
            broker_send(&b_side, &BrokerToCompositor::VtSwitchActivated, None);
        });

        let r1 = Arc::clone(&recorder);
        let before = move || {
            r1.before.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r2 = Arc::clone(&recorder);
        let after = move || {
            r2.after.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r3 = Arc::clone(&recorder);
        let on_activated = move |_fd: BorrowedFd<'_>| {
            r3.activated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let hooks = VtSwitchHooks {
            before_drop_master: &before,
            after_drop_master: &after,
            on_activated: &on_activated,
        };

        let outcome = run_dance_on_channel(&c_side, 2, &hooks, &NoopVtFdSetup).expect("dance ok");
        b_thread.join().unwrap();

        match outcome {
            VtSwitchOutcome::Activated { vt_fd } => {
                assert!(vt_fd.as_raw_fd() >= 0, "fd should be valid");
            }
            VtSwitchOutcome::Rejected { reason } => {
                panic!("expected Activated, got Rejected({reason:?})")
            }
        }
        assert_eq!(
            recorder.calls(),
            (1, 1, 1),
            "all three hooks called exactly once"
        );
    }

    /// Broker rejects the initial RequestVtSwitch (e.g. rate-limit).
    /// No hooks fire — the dance bails before any fd-setup.
    #[test]
    fn vt_switch_broker_rejects_request() {
        let (c_side, b_side) = pair();
        let recorder = Arc::new(HookRecorder::default());

        let b_thread = thread::spawn(move || {
            let msg = broker_recv(&b_side);
            assert!(matches!(msg, CompositorToBroker::RequestVtSwitch { .. }));
            broker_send(
                &b_side,
                &BrokerToCompositor::VtSwitchRejected {
                    reason: VtSwitchRejectReason::RateLimited,
                },
                None,
            );
        });

        let r1 = Arc::clone(&recorder);
        let before = move || {
            r1.before.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r2 = Arc::clone(&recorder);
        let after = move || {
            r2.after.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r3 = Arc::clone(&recorder);
        let on_activated = move |_fd: BorrowedFd<'_>| {
            r3.activated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let hooks = VtSwitchHooks {
            before_drop_master: &before,
            after_drop_master: &after,
            on_activated: &on_activated,
        };

        let outcome = run_dance_on_channel(&c_side, 2, &hooks, &NoopVtFdSetup).expect("dance ok");
        b_thread.join().unwrap();

        match outcome {
            VtSwitchOutcome::Rejected { reason } => {
                assert_eq!(reason, VtSwitchRejectReason::RateLimited);
            }
            VtSwitchOutcome::Activated { .. } => panic!("expected Rejected"),
        }
        assert_eq!(
            recorder.calls(),
            (0, 0, 0),
            "hooks must NOT fire when broker rejects request before Prepare"
        );
    }

    /// Broker accepts the request, sends Prepare, receives
    /// MasterDropped, then rejects (e.g. `VT_ACTIVATE` failed →
    /// `BrokerInternal`). before+after hooks fire (we reached the
    /// drop-master state), but `on_activated` does NOT.
    #[test]
    fn vt_switch_broker_rejects_after_master_drop() {
        let (c_side, b_side) = pair();
        let recorder = Arc::new(HookRecorder::default());
        let (pipe_r, pipe_w) = nix::unistd::pipe().expect("pipe");
        let _keep_pipe_w_open = pipe_w;

        let b_thread = thread::spawn(move || {
            let msg = broker_recv(&b_side);
            assert!(matches!(msg, CompositorToBroker::RequestVtSwitch { .. }));
            broker_send(
                &b_side,
                &BrokerToCompositor::VtSwitchPrepare,
                Some(pipe_r.as_fd()),
            );
            let msg = broker_recv(&b_side);
            assert!(matches!(msg, CompositorToBroker::VtSwitchMasterDropped));
            broker_send(
                &b_side,
                &BrokerToCompositor::VtSwitchRejected {
                    reason: VtSwitchRejectReason::BrokerInternal,
                },
                None,
            );
        });

        let r1 = Arc::clone(&recorder);
        let before = move || {
            r1.before.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r2 = Arc::clone(&recorder);
        let after = move || {
            r2.after.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r3 = Arc::clone(&recorder);
        let on_activated = move |_fd: BorrowedFd<'_>| {
            r3.activated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let hooks = VtSwitchHooks {
            before_drop_master: &before,
            after_drop_master: &after,
            on_activated: &on_activated,
        };

        let outcome = run_dance_on_channel(&c_side, 2, &hooks, &NoopVtFdSetup).expect("dance ok");
        b_thread.join().unwrap();

        match outcome {
            VtSwitchOutcome::Rejected { reason } => {
                assert_eq!(reason, VtSwitchRejectReason::BrokerInternal);
            }
            VtSwitchOutcome::Activated { .. } => panic!("expected Rejected"),
        }
        assert_eq!(
            recorder.calls(),
            (1, 1, 0),
            "before+after fire, on_activated must NOT"
        );
    }

    /// Broker sends `VtSwitchPrepare` WITHOUT an attached fd —
    /// malformed protocol. The dance must return `Err(Protocol)`,
    /// not panic, not silently proceed. No hooks fire because we
    /// never received a valid fd to pass to fd_setup.
    #[test]
    fn vt_switch_missing_fd_is_protocol_error() {
        let (c_side, b_side) = pair();
        let recorder = Arc::new(HookRecorder::default());

        let b_thread = thread::spawn(move || {
            let msg = broker_recv(&b_side);
            assert!(matches!(msg, CompositorToBroker::RequestVtSwitch { .. }));
            broker_send(&b_side, &BrokerToCompositor::VtSwitchPrepare, None);
        });

        let r1 = Arc::clone(&recorder);
        let before = move || {
            r1.before.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r2 = Arc::clone(&recorder);
        let after = move || {
            r2.after.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let r3 = Arc::clone(&recorder);
        let on_activated = move |_fd: BorrowedFd<'_>| {
            r3.activated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let hooks = VtSwitchHooks {
            before_drop_master: &before,
            after_drop_master: &after,
            on_activated: &on_activated,
        };

        let res = run_dance_on_channel(&c_side, 2, &hooks, &NoopVtFdSetup);
        b_thread.join().unwrap();

        match res {
            Err(VtSwitchError::Protocol(msg)) => {
                assert!(
                    msg.contains("VtSwitchPrepare without SCM_RIGHTS fd"),
                    "unexpected protocol error: {msg}"
                );
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
        assert_eq!(
            recorder.calls(),
            (0, 0, 0),
            "no hook fires on missing-fd path"
        );
    }

    /// Smoke test: `VtSwitcher::new` exercises the public constructor
    /// so the API is at least one-call covered without spinning up a
    /// real broker socket (R1.4's VM test exercises `request_switch`
    /// end-to-end).
    #[test]
    fn vt_switcher_new_smoke() {
        let switcher = VtSwitcher::new(PathBuf::from("/run/halmasuit/broker.sock"));
        assert_eq!(
            switcher.broker_socket,
            std::path::Path::new("/run/halmasuit/broker.sock")
        );
    }
}
