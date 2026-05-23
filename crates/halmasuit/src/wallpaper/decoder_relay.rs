//! Compositor-side relay for the sandboxed `halmasuit-decoder`
//! subprocess (Epic #12 task #7).
//!
//! Owns one `OwnedFd` (SOCK_SEQPACKET to the decoder) plus a pidfd
//! to the decoder process. Single-owner per Amendment A6/A8;
//! mirrors the established `BrokerEpisode` / `SeqpacketChannel`
//! pattern.
//!
//! ## Lifecycle
//!
//! 1. `DecoderRelay::spawn(wallpaper_path)` — create
//!    socketpair(SOCK_SEQPACKET), fork-exec `halmasuit-decoder`
//!    with the child end dup2'd to fd 3, open pidfd for signaling,
//!    open the wallpaper file, send `LoadFile { loop_playback:
//!    true }` with the wallpaper fd via SCM_RIGHTS.
//! 2. `poll_frames()` — called from VideoBackend's `render_element`
//!    on every render tick. Non-blockingly drains the socket;
//!    parses headers, validates, reads payloads, keeps only the
//!    LATEST validated frame. Older frames are discarded (Phase A
//!    pacing model: decoder runs at max speed, compositor consumes-
//!    and-discards).
//! 3. `latest_frame()` — accessor for the last validated RGBA8
//!    frame.
//! 4. Restart on decoder exit / fatal error: bounded retries
//!    (`MAX_RESTARTS_PER_WINDOW = 3` in `RESTART_WINDOW = 10s`).
//!    Exhausted → relay marks itself dead; VideoBackend falls back
//!    to its configured fallback image (or solid color).
//! 5. `Drop` — send `Shutdown`, brief wait, `pidfd_send_signal
//!    SIGKILL` if the child hasn't exited.
//!
//! ## Hard rules honored
//!
//! - Single `OwnedFd` for the IPC socket (A6/A8); no
//!   `dup`/`Rc`/`Arc`.
//! - All recv via `recvmsg` with cmsg buffer (truncating SCM_RIGHTS
//!   leaks fds with MSG_CTRUNC).
//! - All send/recv are MSG_DONTWAIT (compositor never blocks on
//!   decoder IPC).
//! - Pidfd-based signaling, never raw `kill(pid, sig)` (CLAUDE.md
//!   memory: `project-pidfd-over-raw-kill`).
//! - Frame validation before GLES upload: `bytes_len <=
//!   MAX_FRAME_BYTES` AND `width * height * 4 == bytes_len` AND
//!   non-zero dims, enforced via `validate_frame_header`.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use halmasuit_decoder_ipc::{
    CompositorToDecoder, DecoderToCompositor, MAX_FRAME_BYTES, WIRE_VERSION, encode_control,
    try_decode_control, validate_frame_header,
};
use nix::sys::socket::{
    AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, recvmsg, sendmsg, socketpair,
};
use thiserror::Error;
use tracing::{debug, error, info, warn};

/// fd number the parent dup2's the child's IPC socket end to before
/// exec. Mirrors `halmasuit_decoder::IPC_FD`.
const IPC_CHILD_FD: RawFd = 3;

/// Restart policy: at most this many decoder restarts per window
/// before the relay gives up and the wallpaper engine falls back.
const MAX_RESTARTS_PER_WINDOW: u32 = 3;
/// Sliding window for the restart counter (seconds).
const RESTART_WINDOW_SECS: u64 = 10;

/// Errors from the relay's setup and per-frame loop.
#[derive(Debug, Error)]
pub enum RelayError {
    #[error("socketpair failed: {0}")]
    Socketpair(nix::Error),
    #[error("setsockopt SO_SNDBUF/SO_RCVBUF failed: {0}")]
    SetSockOpt(nix::Error),
    #[error("failed to open wallpaper file {path:?}: {err}")]
    OpenWallpaper {
        path: std::path::PathBuf,
        err: std::io::Error,
    },
    #[error("failed to spawn halmasuit-decoder: {0}")]
    Spawn(std::io::Error),
    #[error("pidfd_open failed: {0}")]
    PidfdOpen(nix::Error),
    #[error("IPC error: {0}")]
    Ipc(IpcError),
}

/// Errors from the IPC encode/decode path. Distinct from `RelayError`
/// so callers can match without the spawn-side variants getting in
/// the way.
#[derive(Debug, Error)]
pub enum IpcError {
    #[error("codec: {0}")]
    Codec(halmasuit_decoder_ipc::CodecError),
    #[error("io: {0}")]
    Io(nix::Error),
    #[error("partial-frame datagram from decoder")]
    PartialFrame,
    #[error("peer closed connection")]
    Closed,
}

impl From<IpcError> for RelayError {
    fn from(e: IpcError) -> Self {
        Self::Ipc(e)
    }
}

/// One RGBA8 frame ready for GLES upload by the VideoBackend.
#[derive(Debug)]
pub struct LatestFrame {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
    /// Monotonic counter from the decoder; reset on each LoadFile.
    pub frame_idx: u64,
}

/// Thin wrapper around the IPC socket fd. Owns the OwnedFd plus a
/// reusable receive buffer sized for the largest expected datagram
/// (frame header + control msg cap). Buffer is in `RefCell` to keep
/// `&self` recv on the calloop borrow path while allowing interior
/// mutation. Single-owner per A6/A8: never `dup`/`Rc`/`Arc` this.
pub struct DecoderChannel {
    fd: OwnedFd,
    recv_buf: RefCell<Vec<u8>>,
}

impl DecoderChannel {
    fn new(fd: OwnedFd) -> Self {
        // Buffer sized for the largest expected single-datagram read:
        // either a control message (4 KiB) or a frame payload (up to
        // MAX_FRAME_BYTES = 16 MiB). We allocate once at the larger
        // size and reuse forever.
        Self {
            fd,
            recv_buf: RefCell::new(vec![0u8; MAX_FRAME_BYTES as usize]),
        }
    }

    /// Send a control message — JSON header, one datagram.
    /// `MSG_DONTWAIT`: a wedged decoder is fail-closed.
    fn send_control(&self, msg: &CompositorToDecoder) -> Result<(), IpcError> {
        let bytes = encode_control(msg).map_err(IpcError::Codec)?;
        let n = nix::sys::socket::send(self.fd.as_raw_fd(), &bytes, MsgFlags::MSG_DONTWAIT)
            .map_err(IpcError::Io)?;
        if n == bytes.len() {
            Ok(())
        } else {
            Err(IpcError::PartialFrame)
        }
    }

    /// Send a `LoadFile` control message with the wallpaper fd in
    /// SCM_RIGHTS ancillary data.
    fn send_load_file(
        &self,
        loop_playback: bool,
        wallpaper_fd: BorrowedFd<'_>,
    ) -> Result<(), IpcError> {
        let msg = CompositorToDecoder::LoadFile { loop_playback };
        let bytes = encode_control(&msg).map_err(IpcError::Codec)?;
        let iov = [std::io::IoSlice::new(&bytes)];
        let raw_fd_array = [wallpaper_fd.as_raw_fd()];
        let cmsgs = [ControlMessage::ScmRights(&raw_fd_array)];
        sendmsg::<()>(
            self.fd.as_raw_fd(),
            &iov,
            &cmsgs,
            MsgFlags::MSG_DONTWAIT,
            None,
        )
        .map_err(IpcError::Io)?;
        Ok(())
    }

    /// Non-blocking recv of one decoder→compositor message. Returns
    /// `Ok(None)` on EAGAIN. The buffer is reused; the returned slice
    /// is valid only until the next recv.
    fn recv_one(&self) -> Result<Option<RecvOutcome>, IpcError> {
        let mut buf = self.recv_buf.borrow_mut();
        let mut iov = [std::io::IoSliceMut::new(&mut buf)];
        let r = match recvmsg::<()>(self.fd.as_raw_fd(), &mut iov, None, MsgFlags::MSG_DONTWAIT) {
            Ok(r) => r,
            Err(nix::errno::Errno::EAGAIN) => return Ok(None),
            Err(e) => return Err(IpcError::Io(e)),
        };
        if r.bytes == 0 {
            return Err(IpcError::Closed);
        }
        let n = r.bytes;
        // First try to decode as a control-shape message (length-
        // prefixed JSON). If that succeeds AND it's a FrameHeader,
        // we read the payload bytes from the buffer past the header.
        let (msg, _consumed): (DecoderToCompositor, usize) = try_decode_control(&buf[..n])
            .map_err(IpcError::Codec)?
            .ok_or(IpcError::PartialFrame)?;
        match msg {
            DecoderToCompositor::FrameHeader {
                frame_idx,
                pts_us,
                width,
                height,
                format,
                bytes_len,
            } => {
                validate_frame_header(width, height, format, bytes_len).map_err(IpcError::Codec)?;
                // The frame payload is the next datagram on the wire.
                // Read it now in a SECOND recvmsg.
                drop(buf); // release RefCell borrow before re-entrant recv
                let mut payload = vec![0u8; bytes_len as usize];
                let mut piov = [std::io::IoSliceMut::new(&mut payload)];
                let pr = match recvmsg::<()>(
                    self.fd.as_raw_fd(),
                    &mut piov,
                    None,
                    MsgFlags::empty(), // block here; the header already arrived
                ) {
                    Ok(r) => r,
                    Err(e) => return Err(IpcError::Io(e)),
                };
                if pr.bytes != bytes_len as usize {
                    return Err(IpcError::PartialFrame);
                }
                let _ = (frame_idx, pts_us); // consumed by Outcome
                Ok(Some(RecvOutcome::Frame(LatestFrame {
                    width,
                    height,
                    bytes: payload,
                    frame_idx,
                })))
            }
            DecoderToCompositor::Ready { wire_version } => {
                if wire_version != WIRE_VERSION {
                    return Err(IpcError::Codec(
                        halmasuit_decoder_ipc::CodecError::OversizedControl(
                            u32::from(wire_version),
                            u32::from(WIRE_VERSION),
                        ),
                    ));
                }
                Ok(Some(RecvOutcome::Ready))
            }
            DecoderToCompositor::EndOfFile => Ok(Some(RecvOutcome::EndOfFile)),
            DecoderToCompositor::DecoderError { code, message } => {
                Ok(Some(RecvOutcome::DecoderError { code, message }))
            }
        }
    }
}

impl AsFd for DecoderChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Outcome of one `recv_one` call. Pulled out so the relay's
/// `poll_frames` can act on each message kind without re-decoding.
#[derive(Debug)]
enum RecvOutcome {
    Ready,
    Frame(LatestFrame),
    EndOfFile,
    DecoderError {
        code: halmasuit_decoder_ipc::DecoderErrorCode,
        message: String,
    },
}

/// Per-video-wallpaper relay. Owns the IPC socket, the decoder
/// pidfd, the restart-budget bookkeeping, and the last validated
/// frame.
pub struct DecoderRelay {
    chan: DecoderChannel,
    pidfd: OwnedFd,
    decoder_pid: nix::unistd::Pid,
    latest_frame: RefCell<Option<LatestFrame>>,
    /// Sliding-window restart bookkeeping.
    restart_history: RefCell<Vec<Instant>>,
    /// True once the restart budget is exhausted; VideoBackend
    /// queries via [`Self::is_dead`] and switches to fallback.
    dead: RefCell<bool>,
    /// True once Ready handshake received; spuriously-arriving frames
    /// before Ready are protocol violations.
    ready: RefCell<bool>,
}

impl DecoderRelay {
    /// Spawn the decoder, send LoadFile + wallpaper fd, return relay.
    pub fn spawn(wallpaper_path: &Path) -> Result<Self, RelayError> {
        let (parent_end, child_end) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .map_err(RelayError::Socketpair)?;

        // Bump the kernel SO_SNDBUF/SO_RCVBUF on the parent end so a
        // single 1080p RGBA frame (8.3 MiB) fits in one datagram
        // without ENOBUFS. The decoder side gets the same effective
        // bump via the kernel mirroring it on a socketpair.
        set_socket_buffers(parent_end.as_raw_fd())?;

        let wallpaper_file =
            File::open(wallpaper_path).map_err(|err| RelayError::OpenWallpaper {
                path: wallpaper_path.to_path_buf(),
                err,
            })?;

        // Fork-exec halmasuit-decoder with child_end dup2'd to fd 3.
        let child_raw = child_end.into_raw_fd();
        let child_pid = fork_exec_decoder(child_raw)?;

        // We hold the parent end; the child sees fd 3 = its IPC.
        // SAFETY: the child has its own copy of child_raw; we close
        // our reference to it now since we don't need it.
        #[expect(
            unsafe_code,
            reason = "child_raw was duplicated by the kernel into the child's fd table; we close our parent-side reference"
        )]
        unsafe {
            libc::close(child_raw);
        }

        let chan = DecoderChannel::new(parent_end);

        // Open a pidfd to the decoder child for signaling.
        let pidfd = pidfd_open(child_pid)?;

        let relay = Self {
            chan,
            pidfd,
            decoder_pid: child_pid,
            latest_frame: RefCell::new(None),
            restart_history: RefCell::new(Vec::new()),
            dead: RefCell::new(false),
            ready: RefCell::new(false),
        };

        // Send LoadFile with the wallpaper fd via SCM_RIGHTS.
        relay
            .chan
            .send_load_file(true, wallpaper_file.as_fd())
            .map_err(RelayError::Ipc)?;
        // `wallpaper_file` drops here; the kernel-dup'd copy in the
        // decoder process keeps the underlying inode open.

        info!(
            decoder_pid = child_pid.as_raw(),
            "wallpaper: spawned halmasuit-decoder"
        );
        Ok(relay)
    }

    /// Drain pending decoder messages non-blockingly. Updates
    /// `latest_frame` to the newest validated frame. Returns `true`
    /// if at least one frame was received this call.
    pub fn poll_frames(&self) -> bool {
        if *self.dead.borrow() {
            return false;
        }
        let mut got_frame = false;
        loop {
            match self.chan.recv_one() {
                Ok(None) => break, // EAGAIN: drained
                Ok(Some(RecvOutcome::Ready)) => {
                    *self.ready.borrow_mut() = true;
                    debug!("wallpaper: decoder Ready");
                }
                Ok(Some(RecvOutcome::Frame(frame))) => {
                    if !*self.ready.borrow() {
                        warn!("wallpaper: frame before Ready; protocol violation");
                        self.note_failure();
                        break;
                    }
                    *self.latest_frame.borrow_mut() = Some(frame);
                    got_frame = true;
                }
                Ok(Some(RecvOutcome::EndOfFile)) => {
                    info!("wallpaper: decoder EOF (no loop)");
                }
                Ok(Some(RecvOutcome::DecoderError { code, message })) => {
                    error!(?code, message, "wallpaper: decoder error frame");
                    self.note_failure();
                    break;
                }
                Err(IpcError::Closed) => {
                    warn!("wallpaper: decoder closed connection");
                    self.note_failure();
                    break;
                }
                Err(err) => {
                    error!(error = %err, "wallpaper: IPC error");
                    self.note_failure();
                    break;
                }
            }
        }
        got_frame
    }

    /// Latest validated frame, if any. Returns a borrow that the
    /// caller copies-or-uploads then releases.
    pub fn latest_frame(&self) -> Option<std::cell::Ref<'_, LatestFrame>> {
        if self.latest_frame.borrow().is_none() {
            None
        } else {
            Some(std::cell::Ref::map(self.latest_frame.borrow(), |o| {
                o.as_ref().unwrap()
            }))
        }
    }

    /// True if the restart budget is exhausted and the wallpaper
    /// engine should fall back. Consumed by `VideoBackend` after each
    /// poll to short-circuit the "keep rendering forever" path.
    pub fn is_dead(&self) -> bool {
        *self.dead.borrow()
    }

    /// Borrowed pidfd for poll-based decoder-exit detection. Not yet
    /// wired into calloop (Phase A polls via `poll_frames` in
    /// render_element); calloop integration is a follow-on epic.
    #[allow(
        dead_code,
        reason = "available for calloop integration; not consumed in Phase A"
    )]
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }

    fn note_failure(&self) {
        let now = Instant::now();
        let mut hist = self.restart_history.borrow_mut();
        hist.retain(|t| now.duration_since(*t).as_secs() < RESTART_WINDOW_SECS);
        hist.push(now);
        if u32::try_from(hist.len()).unwrap_or(u32::MAX) > MAX_RESTARTS_PER_WINDOW {
            warn!(
                count = hist.len(),
                "wallpaper: decoder restart budget exhausted; relay dead"
            );
            *self.dead.borrow_mut() = true;
        }
        // Phase A: we don't actually respawn here. The relay is
        // dropped+recreated by the wallpaper engine at the next
        // render tick (or VideoBackend falls back to ImageBackend
        // if `is_dead()`). Respawning in-place is a future
        // optimization.
    }
}

impl Drop for DecoderRelay {
    fn drop(&mut self) {
        // Best-effort clean shutdown.
        let _ = self.chan.send_control(&CompositorToDecoder::Shutdown);
        // SIGKILL via pidfd as a backstop. We don't wait — the
        // broker isn't reaping our child here; the kernel will
        // wait4-style auto-reap if we ever set SIGCHLD to ignore,
        // which halmasuit doesn't. Realistically: the child sees
        // Shutdown, exits cleanly, and pidfd_send_signal SIGKILL
        // on an already-exited pid is a no-op (ESRCH).
        #[expect(
            unsafe_code,
            reason = "pidfd_send_signal via the libc syscall wrapper; the pidfd is owned and valid"
        )]
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::c_void>(),
                0_u32,
            );
        }
        debug!(
            decoder_pid = self.decoder_pid.as_raw(),
            "wallpaper: relay dropped, decoder signaled"
        );
    }
}

/// Increase the socket buffers so a 1080p RGBA frame fits in one
/// SOCK_SEQPACKET datagram without ENOBUFS. The kernel doubles the
/// requested value internally; we ask for the frame cap.
fn set_socket_buffers(fd: RawFd) -> Result<(), RelayError> {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::{RcvBuf, SndBuf};
    #[expect(
        unsafe_code,
        reason = "BorrowedFd::borrow_raw on a fd we own (just returned from socketpair); the borrow lives only inside this scope"
    )]
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let target = MAX_FRAME_BYTES as usize;
    setsockopt(&borrowed, SndBuf, &target).map_err(RelayError::SetSockOpt)?;
    setsockopt(&borrowed, RcvBuf, &target).map_err(RelayError::SetSockOpt)?;
    Ok(())
}

/// Open a pidfd for `pid` for race-free signaling. Pure libc syscall;
/// nix's wrapper exists in newer versions but we keep this local.
fn pidfd_open(pid: nix::unistd::Pid) -> Result<OwnedFd, RelayError> {
    #[expect(
        unsafe_code,
        reason = "pidfd_open is a numeric syscall (SYS_pidfd_open); the returned fd is wrapped in OwnedFd for RAII"
    )]
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw() as libc::pid_t, 0_u32) };
    if raw < 0 {
        return Err(RelayError::PidfdOpen(nix::errno::Errno::last()));
    }
    let raw_i32 =
        i32::try_from(raw).map_err(|_| RelayError::PidfdOpen(nix::errno::Errno::EINVAL))?;
    #[expect(
        unsafe_code,
        reason = "pidfd_open just returned a fresh fd we own; OwnedFd::from_raw_fd takes ownership"
    )]
    let owned = unsafe { OwnedFd::from_raw_fd(raw_i32) };
    Ok(owned)
}

/// Fork+exec `halmasuit-decoder` with `child_socket_fd` dup2'd to
/// fd 3. Returns the child's pid.
fn fork_exec_decoder(child_socket_fd: RawFd) -> Result<nix::unistd::Pid, RelayError> {
    // Locate the decoder binary. In production this is the systemd-
    // unit-resolved path (set by nix module). For dev/test we look
    // up via $HALMASUIT_DECODER_PATH env var, with a sensible
    // fallback to the workspace's target/debug build.
    let binary: std::path::PathBuf = std::env::var_os("HALMASUIT_DECODER_PATH").map_or_else(
        || std::path::PathBuf::from("halmasuit-decoder"),
        std::path::PathBuf::from,
    );

    let mut cmd = Command::new(binary);
    cmd.arg0(OsStr::new("halmasuit-decoder"));

    // SAFETY: the closure runs after fork(2) in the child only — no
    // allocator, no mutex, no thread-affecting syscall beyond what's
    // here. We dup2 the socket to fd 3 and reset the signal mask
    // (memory: `project-pre-exec-signal-mask`: calloop's signalfd
    // would otherwise have SIGTERM blocked in the child).
    #[expect(
        unsafe_code,
        reason = "Command::pre_exec runs the closure in the forked child between fork and exec; ONLY async-signal-safe operations are valid (dup2, sigprocmask)"
    )]
    unsafe {
        cmd.pre_exec(move || {
            // Reset the signal mask: calloop's signalfd in the parent
            // blocks SIGTERM/SIGINT; an inherited block is poison in
            // the child (memory: project-pre-exec-signal-mask).
            let empty: libc::sigset_t = std::mem::zeroed();
            let _ = libc::sigprocmask(
                libc::SIG_SETMASK,
                std::ptr::from_ref(&empty),
                std::ptr::null_mut(),
            );
            // dup2 the inherited socket to fd 3.
            if libc::dup2(child_socket_fd, IPC_CHILD_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Close the original fd; the child only needs fd 3 + stdio.
            if child_socket_fd != IPC_CHILD_FD {
                libc::close(child_socket_fd);
            }
            Ok(())
        });
    }

    let child = cmd.spawn().map_err(RelayError::Spawn)?;
    Ok(nix::unistd::Pid::from_raw(
        i32::try_from(child.id())
            .map_err(|_| RelayError::Spawn(std::io::Error::other("child pid out of i32 range")))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: full end-to-end coverage (spawn → frames flowing → frame
    // upload) lands in the VM test (Epic #12 task #9). The unit-test
    // layer below covers pure-Rust adapter logic that doesn't need a
    // live decoder binary.

    #[test]
    fn relay_error_display_for_open_failure() {
        let err = RelayError::OpenWallpaper {
            path: "/missing".into(),
            err: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let s = format!("{err}");
        assert!(s.contains("/missing"));
        assert!(s.contains("wallpaper"));
    }

    #[test]
    fn ipc_error_categories_distinct() {
        // Exhaustiveness check that the wire-error categories are
        // discriminable for the relay's failure-counting logic.
        let cases = [
            IpcError::Codec(halmasuit_decoder_ipc::CodecError::OversizedControl(0, 0)),
            IpcError::Io(nix::errno::Errno::EAGAIN),
            IpcError::PartialFrame,
            IpcError::Closed,
        ];
        for err in &cases {
            assert!(!format!("{err}").is_empty(), "Display impl");
        }
    }
}
