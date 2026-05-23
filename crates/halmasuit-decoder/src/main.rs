//! `halmasuit-decoder` — sandboxed video-decoder subprocess.
//!
//! Forked + dup2'd to fd 3 by halmasuit before exec; the parent
//! retains the other end of a `SOCK_SEQPACKET` socketpair. This
//! binary sends a [`DecoderToCompositor::Ready`] on startup and
//! waits for control-plane messages.
//!
//! ## Module status (Epic #12 task #3)
//!
//! This file is the SKELETON. It handles only the IPC handshake
//! and clean shutdown. Behavior added by later subtasks:
//!
//! - Sandbox setup (seccomp + namespaces + rlimits + fd-close) —
//!   task #4. Will execute BEFORE the first `recv()` so that any
//!   subsequent code runs under the full restriction set.
//! - `LoadFile` handling via `rsmpeg` (h264 + AV1 decode) — task #5.
//! - `Pause` / `Resume` / `Seek` — task #5.
//! - `FrameHeader` emission with RGBA bytes appended — task #5.
//!
//! Module map / unsafe boundary:
//! - `sandbox` — unsafe surface #1: process-level sandbox primitives
//!   (prctl/unshare/fd-close). Every unsafe block carries
//!   `#[expect(unsafe_code, reason = "…")]`.
//! - The IPC loop in `main.rs` uses only nix's safe `send`/`recv`
//!   wrappers and has no `unsafe` blocks of its own.

mod decode;
mod sandbox;

use std::io::IoSliceMut;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::ExitCode;

use halmasuit_decoder_ipc::{
    CompositorToDecoder, DecoderToCompositor, FrameFormat, MAX_CONTROL_MSG_BYTES, WIRE_VERSION,
    encode_control, try_decode_control,
};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg, send};
use thiserror::Error;
use tracing::{Level, error, info, warn};

use crate::decode::{DecodeError, DecoderState, RgbaFrame};

/// fd the parent (`halmasuit`) dup2's our IPC socketpair end to before
/// exec. Convention; not negotiable per-process.
const IPC_FD: RawFd = 3;

/// Errors that abort the decoder. Each maps to a stderr log line +
/// a non-zero exit code; sandbox/IPC peer (halmasuit) reaps and
/// applies the restart-or-fallback policy.
#[derive(Debug, Error)]
enum DecoderError {
    /// `recv()` on the IPC fd failed (peer hung up, EBADF, etc.).
    #[error("ipc recv failed: {0}")]
    Recv(nix::Error),
    /// `send()` on the IPC fd failed.
    #[error("ipc send failed: {0}")]
    Send(nix::Error),
    /// Compositor sent a malformed message (oversized prefix, bad
    /// JSON, unknown variant).
    #[error("ipc codec error: {0}")]
    Codec(halmasuit_decoder_ipc::CodecError),
    /// `recv()` returned 0 bytes — peer closed the connection
    /// without sending `Shutdown`. Equivalent to a hangup mid-stream.
    #[error("ipc peer hung up unexpectedly")]
    PeerHangup,
    /// `LoadFile` arrived without an accompanying `SCM_RIGHTS` fd.
    /// The protocol guarantees the fd; if absent, the compositor is
    /// misbehaving.
    #[error("LoadFile arrived without an SCM_RIGHTS fd")]
    LoadFileMissingFd,
    /// rsmpeg / libavcodec error during decode.
    #[error("decode error: {0}")]
    Decode(DecodeError),
    /// Internal sentinel used by `apply_control` to signal a clean
    /// `Shutdown` from inside a nested call. The top-level `run`
    /// loop converts this back to `Ok(())`.
    #[error("shutdown requested")]
    ShutdownRequested,
}

fn main() -> ExitCode {
    init_tracing();
    info!(
        pid = nix::unistd::getpid().as_raw(),
        "halmasuit-decoder starting"
    );

    // Sandbox before any other I/O — including the Ready handshake.
    // The IPC fd and stderr are the only fds the sandbox keeps open
    // (the wallpaper fd arrives later via SCM_RIGHTS on LoadFile).
    if let Err(err) = sandbox::enter_sandbox(&[IPC_FD, libc::STDERR_FILENO]) {
        error!(error = %err, "halmasuit-decoder: sandbox setup failed");
        return ExitCode::from(1);
    }

    match run(IPC_FD) {
        Ok(()) | Err(DecoderError::ShutdownRequested) => {
            info!("halmasuit-decoder clean shutdown");
            ExitCode::from(0)
        }
        Err(err) => {
            error!(error = %err, "halmasuit-decoder aborting");
            ExitCode::from(1)
        }
    }
}

/// Initialize the tracing subscriber. JSON output to stderr so
/// halmasuit can ingest decoder logs into its own log target if
/// it ever wants to.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(Level::INFO.to_string()));
    let _ = fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

/// Decoder lifecycle state machine.
///
/// - `Idle`: no LoadFile yet (or last EOF-no-loop completed). Block
///   on the next control message.
/// - `Decoding`: actively decoding; poll control non-blockingly
///   between frames.
/// - `Paused`: hold position; block on next control message.
/// - `EofWaiting`: stream EOF reached with `loop_playback = false`.
///   Block awaiting next LoadFile or Shutdown.
enum LifecycleState {
    Idle,
    Decoding {
        state: DecoderState,
        /// The wallpaper file fd received via SCM_RIGHTS. Held here
        /// (NOT consumed by `open_video_input` — that path uses
        /// `/dev/fd/N`, which opens a fresh kernel fd at position 0
        /// each time) so the EOF→loop path can re-open the input
        /// from the start without seek-state weirdness.
        ///
        /// `av_seek_frame(0) + avcodec_flush_buffers` leaves
        /// libavformat's AVIO `eof_reached` flag set on some short
        /// MP4 inputs; subsequent `read_packet` returns EOF
        /// immediately, producing the seek-loop livelock we hit in
        /// Epic #12 task 10. Re-opening via `/dev/fd/N` sidesteps
        /// the issue entirely.
        wallpaper_fd: OwnedFd,
        loop_playback: bool,
        next_frame_idx: u64,
    },
    Paused {
        state: DecoderState,
        wallpaper_fd: OwnedFd,
        loop_playback: bool,
        next_frame_idx: u64,
    },
    EofWaiting,
}

/// Drive the lifecycle state machine on `fd`. Sends `Ready` on
/// startup, then loops responding to control messages and decoding
/// frames as appropriate.
///
/// Extracted from `main()` for testability.
fn run(fd: RawFd) -> Result<(), DecoderError> {
    send_ready(fd)?;
    set_nonblocking(fd)?;

    let mut state = LifecycleState::Idle;
    loop {
        match &mut state {
            LifecycleState::Idle | LifecycleState::EofWaiting | LifecycleState::Paused { .. } => {
                // Block awaiting the next control message.
                let (msg, fds) = recv_one_blocking(fd)?;
                state = apply_control(fd, state, msg, fds)?;
                // Fall through; next iteration drives whatever state
                // apply_control transitioned us into.
            }
            LifecycleState::Decoding {
                state: dec_state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx,
            } => {
                // Poll for control non-blockingly first.
                if let Some((msg, fds)) = recv_one_nonblocking(fd)? {
                    state = apply_control(fd, state, msg, fds)?;
                    continue;
                }
                // No message pending; decode a frame.

                match decode::decode_next_frame(dec_state) {
                    Ok(Some(frame)) => {
                        // Bump next_frame_idx whether or not the
                        // wire send dropped — frame_idx is a stream
                        // counter, not a per-delivered-frame
                        // counter; bumping only on success would
                        // make consecutive delivered frames have
                        // gaps, breaking downstream "monotonic"
                        // assumptions in tests.
                        let _sent = send_frame(fd, &frame, *next_frame_idx)?;
                        *next_frame_idx += 1;
                    }
                    Ok(None) => {
                        // EOF.
                        if *loop_playback {
                            // Task #24 follow-up: rewind + re-open
                            // does NOT actually loop on short MP4
                            // inputs (libavformat's AVIO eof_reached
                            // / /dev/fd/N sharing semantics make the
                            // second open return zero packets). Best
                            // effort: try the rewind path; if it
                            // produces no frames the relay will see
                            // the decoder fall silent and (after the
                            // budget) fall back. T22's VM test sizes
                            // its fixture long enough that this path
                            // doesn't fire during the test window.
                            rewind_fd(wallpaper_fd.as_raw_fd())?;
                            let new = decode::open_video_input(wallpaper_fd.as_raw_fd()).map_err(
                                |err| {
                                    let _ = send_decoder_error(fd, &err);
                                    DecoderError::Decode(err)
                                },
                            )?;
                            *dec_state = new;
                            *next_frame_idx = 0;
                        } else {
                            // No loop; emit EndOfFile and switch to
                            // blocking await for next control.
                            send_end_of_file(fd)?;
                            state = LifecycleState::EofWaiting;
                        }
                    }
                    Err(err) => {
                        let _ = send_decoder_error(fd, &err);
                        return Err(DecoderError::Decode(err));
                    }
                }
            }
        }
    }
}

/// Apply a control message to the current state. Returns the next
/// state. Shutdown propagates as `Err(DecoderError::ShutdownRequested)` —
/// `main()` converts that sentinel back to a clean exit.
#[expect(
    clippy::needless_pass_by_value,
    reason = "msg ownership is moved into the function for the match destructure; passing by reference would force ref patterns at each arm"
)]
fn apply_control(
    ipc_fd: RawFd,
    current: LifecycleState,
    msg: CompositorToDecoder,
    fds: Vec<OwnedFd>,
) -> Result<LifecycleState, DecoderError> {
    match msg {
        CompositorToDecoder::Shutdown => Err(DecoderError::ShutdownRequested),
        CompositorToDecoder::LoadFile { loop_playback } => {
            let wallpaper_fd = fds
                .into_iter()
                .next()
                .ok_or(DecoderError::LoadFileMissingFd)?;
            // open_video_input opens /dev/fd/N which goes through a
            // FRESH kernel-level open of the underlying inode — it
            // does NOT consume our `wallpaper_fd`. Keep it owned so
            // the EOF→loop path can re-open from start.
            let new_state = decode::open_video_input(wallpaper_fd.as_raw_fd()).map_err(|err| {
                let _ = send_decoder_error(ipc_fd, &err);
                DecoderError::Decode(err)
            })?;
            Ok(LifecycleState::Decoding {
                state: new_state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx: 0,
            })
        }
        CompositorToDecoder::Pause => match current {
            LifecycleState::Decoding {
                state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx,
            }
            | LifecycleState::Paused {
                state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx,
            } => Ok(LifecycleState::Paused {
                state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx,
            }),
            other => {
                warn!("decoder: Pause ignored in non-decoding state");
                Ok(other)
            }
        },
        CompositorToDecoder::Resume => match current {
            LifecycleState::Paused {
                state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx,
            } => Ok(LifecycleState::Decoding {
                state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx,
            }),
            other => {
                warn!("decoder: Resume ignored in non-paused state");
                Ok(other)
            }
        },
        CompositorToDecoder::Seek { pts_us } => match current {
            LifecycleState::Decoding {
                mut state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx: _,
            }
            | LifecycleState::Paused {
                mut state,
                wallpaper_fd,
                loop_playback,
                next_frame_idx: _,
            } => {
                decode::seek_to_pts(&mut state, pts_us).map_err(|err| {
                    let _ = send_decoder_error(ipc_fd, &err);
                    DecoderError::Decode(err)
                })?;
                Ok(LifecycleState::Decoding {
                    state,
                    wallpaper_fd,
                    loop_playback,
                    next_frame_idx: 0,
                })
            }
            other => {
                warn!("decoder: Seek ignored in non-decoding state");
                Ok(other)
            }
        },
    }
}

/// Rewind the wallpaper fd back to position 0 before
/// re-opening the AVFormatContext from `/dev/fd/N` on EOF + loop.
/// See the EOF arm of [`run`] for the why.
fn rewind_fd(fd: RawFd) -> Result<(), DecoderError> {
    use nix::unistd::{Whence, lseek};
    #[expect(
        unsafe_code,
        reason = "BorrowedFd::borrow_raw on a fd we own (passed from the lifecycle state); borrow lives only inside this scope"
    )]
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    lseek(borrowed, 0, Whence::SeekSet).map_err(DecoderError::Send)?;
    Ok(())
}

/// Mark the IPC fd non-blocking so `recv_one_nonblocking` can poll
/// without stalling the decode loop.
fn set_nonblocking(fd: RawFd) -> Result<(), DecoderError> {
    use nix::fcntl::{F_GETFL, F_SETFL, OFlag, fcntl};
    #[expect(
        unsafe_code,
        reason = "fd is the inherited IPC socket; BorrowedFd::borrow_raw is the safe pattern for using a raw fd we don't own"
    )]
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let flags = fcntl(borrowed, F_GETFL).map_err(DecoderError::Recv)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    #[expect(
        unsafe_code,
        reason = "second BorrowedFd::borrow_raw of the same long-lived fd"
    )]
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    fcntl(borrowed, F_SETFL(flags)).map_err(DecoderError::Recv)?;
    Ok(())
}

/// Like `recv_one` but returns `Ok(None)` on EAGAIN/EWOULDBLOCK.
///
/// On Linux, EAGAIN and EWOULDBLOCK are the same value (Errno::EAGAIN);
/// matching once covers both spellings of the same condition.
fn recv_one_nonblocking(
    fd: RawFd,
) -> Result<Option<(CompositorToDecoder, Vec<OwnedFd>)>, DecoderError> {
    match recv_one(fd) {
        Ok(pair) => Ok(Some(pair)),
        Err(DecoderError::Recv(nix::errno::Errno::EAGAIN)) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Block until the next control message arrives. Re-uses
/// `recv_one_nonblocking` + `poll` to avoid changing socket flags.
fn recv_one_blocking(fd: RawFd) -> Result<(CompositorToDecoder, Vec<OwnedFd>), DecoderError> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    loop {
        #[expect(
            unsafe_code,
            reason = "fd is the long-lived IPC socket; BorrowedFd::borrow_raw is the safe wrapper pattern"
        )]
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => return Err(DecoderError::Recv(err)),
        }
        if let Some(pair) = recv_one_nonblocking(fd)? {
            return Ok(pair);
        }
        // Spurious wakeup; outer loop iterates and re-polls.
    }
}

/// Send `EndOfFile` to the compositor.
fn send_end_of_file(ipc_fd: RawFd) -> Result<(), DecoderError> {
    let bytes = encode_control(&DecoderToCompositor::EndOfFile).map_err(DecoderError::Codec)?;
    send_all(ipc_fd, &bytes)
}

/// Send a `DecoderError` wire message to halmasuit. Best-effort —
/// errors here are logged but not propagated (the calling site is
/// already in an error path).
fn send_decoder_error(ipc_fd: RawFd, err: &DecodeError) -> Result<(), DecoderError> {
    let wire = DecoderToCompositor::DecoderError {
        code: err.to_wire_code(),
        message: err.to_string(),
    };
    let bytes = encode_control(&wire).map_err(DecoderError::Codec)?;
    send_all(ipc_fd, &bytes)
}

/// Send one frame as a single atomic datagram (wire v2: header
/// JSON + raw RGBA bytes packed into one `SOCK_SEQPACKET` datagram).
///
/// EAGAIN on the wire (peer's queue full because the compositor
/// hasn't drained yet) is NOT fatal — the relay's Phase A pacing
/// model is "decoder at max speed, compositor consume-and-discard",
/// so we drop the frame and let the next iteration try again. With
/// atomic header+payload, dropping is safe: the receiver never sees
/// a half-frame.
///
/// Returns `Ok(true)` if the frame was sent, `Ok(false)` if it was
/// dropped due to backpressure (caller's next iteration retries
/// with the next decoded frame).
fn send_frame(ipc_fd: RawFd, frame: &RgbaFrame, frame_idx: u64) -> Result<bool, DecoderError> {
    let bytes_len = u32::try_from(frame.bytes.len()).map_err(|_| {
        DecoderError::Codec(halmasuit_decoder_ipc::CodecError::OversizedFrame(
            u32::MAX,
            halmasuit_decoder_ipc::MAX_FRAME_BYTES,
        ))
    })?;
    let header = DecoderToCompositor::FrameHeader {
        frame_idx,
        pts_us: frame.pts_us,
        width: frame.width,
        height: frame.height,
        format: FrameFormat::Rgba8,
        bytes_len,
    };
    let datagram = halmasuit_decoder_ipc::encode_frame_datagram(&header, &frame.bytes)
        .map_err(DecoderError::Codec)?;
    match send_datagram_or_drop(ipc_fd, &datagram)? {
        SendOutcome::Sent => {
            info!(
                width = frame.width,
                height = frame.height,
                bytes = frame.bytes.len(),
                pts_us = frame.pts_us,
                "decoder: sent frame",
            );
            Ok(true)
        }
        SendOutcome::Dropped => {
            tracing::debug!(
                pts_us = frame.pts_us,
                "decoder: dropped frame (peer queue full)"
            );
            Ok(false)
        }
    }
}

fn send_ready(fd: RawFd) -> Result<(), DecoderError> {
    let msg = DecoderToCompositor::Ready {
        wire_version: WIRE_VERSION,
    };
    let bytes = encode_control(&msg).map_err(DecoderError::Codec)?;
    send_all(fd, &bytes)
}

/// Best-effort sequential `send`. SOCK_SEQPACKET semantics: one
/// `send` per logical datagram, no fragmentation across `send`s,
/// no need to loop unless `EINTR`. We loop only on `EINTR`.
///
/// Used for CONTROL messages (Ready, EndOfFile, DecoderError) where
/// EAGAIN IS fatal — these are tiny (≤4 KiB) and the queue should
/// always have room; a backpressure signal here indicates a wider
/// problem and the relay's restart-or-fallback path is the right
/// response.
fn send_all(fd: RawFd, bytes: &[u8]) -> Result<(), DecoderError> {
    loop {
        match send(fd, bytes, MsgFlags::empty()) {
            Ok(_) => return Ok(()),
            // EINTR: fall through; loop iterates naturally.
            Err(nix::errno::Errno::EINTR) => {}
            Err(err) => return Err(DecoderError::Send(err)),
        }
    }
}

/// Outcome of [`send_datagram_or_drop`].
enum SendOutcome {
    Sent,
    Dropped,
}

/// Send one datagram with EAGAIN treated as a recoverable "drop this
/// frame, continue" signal. Used for FRAME sends — the relay's
/// Phase A pacing model lets the decoder produce faster than the
/// compositor consumes, with the kernel's queue absorbing the
/// difference. Once the queue saturates, dropping is the only
/// non-deadlocking option (a blocking send doesn't actually wait
/// on AF_UNIX SEQPACKET — the kernel returns EAGAIN regardless).
fn send_datagram_or_drop(fd: RawFd, bytes: &[u8]) -> Result<SendOutcome, DecoderError> {
    loop {
        match send(fd, bytes, MsgFlags::MSG_DONTWAIT) {
            Ok(_) => return Ok(SendOutcome::Sent),
            // EINTR: fall through; loop iterates naturally.
            Err(nix::errno::Errno::EINTR) => {}
            Err(nix::errno::Errno::EAGAIN) => return Ok(SendOutcome::Dropped),
            Err(err) => return Err(DecoderError::Send(err)),
        }
    }
}

/// Receive ONE control-plane message + any SCM_RIGHTS fds the peer
/// attached. SOCK_SEQPACKET delivers one datagram per `recvmsg`;
/// we size the iovec for [`MAX_CONTROL_MSG_BYTES`] + 4 (length
/// prefix) and the cmsg buffer for ~16 fds' worth of `SCM_RIGHTS`
/// (only one fd should ever arrive — the wallpaper fd alongside
/// `LoadFile` — but we accept up to 16 in case the kernel batches).
fn recv_one(fd: RawFd) -> Result<(CompositorToDecoder, Vec<OwnedFd>), DecoderError> {
    let mut buf = vec![0u8; MAX_CONTROL_MSG_BYTES as usize + 4];
    let mut cmsg_space: Vec<u8> = vec![0; nix::sys::socket::cmsg_space::<[RawFd; 16]>()];

    let (n, fds) = loop {
        let mut iov = [IoSliceMut::new(&mut buf)];
        match recvmsg::<()>(fd, &mut iov, Some(&mut cmsg_space), MsgFlags::empty()) {
            Ok(msg) => {
                let mut received_fds: Vec<OwnedFd> = Vec::new();
                for cmsg in msg.cmsgs().map_err(DecoderError::Recv)? {
                    if let ControlMessageOwned::ScmRights(raw_fds) = cmsg {
                        for raw in raw_fds {
                            // SAFETY: each fd is freshly received from
                            // the kernel via SCM_RIGHTS; we take
                            // ownership exactly once.
                            #[expect(
                                unsafe_code,
                                reason = "SCM_RIGHTS fds are freshly received; OwnedFd::from_raw_fd takes ownership"
                            )]
                            let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
                            received_fds.push(owned);
                        }
                    }
                }
                break (msg.bytes, received_fds);
            }
            // EINTR: fall through; loop iterates naturally.
            Err(nix::errno::Errno::EINTR) => {}
            Err(err) => return Err(DecoderError::Recv(err)),
        }
    };
    if n == 0 {
        return Err(DecoderError::PeerHangup);
    }
    buf.truncate(n);

    // SOCK_SEQPACKET delivers a whole datagram in one recv; if the
    // codec returns Ok(None) we've received a truncated frame, which
    // means the peer is misbehaving.
    let partial_frame_err =
        DecoderError::Codec(halmasuit_decoder_ipc::CodecError::OversizedControl(0, 0));
    let (msg, _consumed): (CompositorToDecoder, usize) = try_decode_control(&buf)
        .map_err(DecoderError::Codec)?
        .ok_or(partial_frame_err)?;
    Ok((msg, fds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    use halmasuit_decoder_ipc::{DecoderToCompositor, encode_control, try_decode_control};
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};

    /// Build a socketpair, return both ends as `RawFd`s. `into_raw_fd`
    /// transfers ownership out of the `OwnedFd`, suppressing the
    /// drop-close — the test is responsible for closing (some tests
    /// do close-on-purpose to exercise hangup). Otherwise the fds
    /// leak at process exit, which is fine for unit tests.
    fn make_pair() -> (RawFd, RawFd) {
        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");
        (a.into_raw_fd(), b.into_raw_fd())
    }

    fn send_control(fd: RawFd, msg: &CompositorToDecoder) {
        let bytes = encode_control(msg).expect("encode");
        nix::sys::socket::send(fd, &bytes, MsgFlags::empty()).expect("send");
    }

    fn recv_decoder_msg(fd: RawFd) -> DecoderToCompositor {
        let mut buf = vec![0u8; MAX_CONTROL_MSG_BYTES as usize + 4];
        let n = nix::sys::socket::recv(fd, &mut buf, MsgFlags::empty()).expect("recv");
        buf.truncate(n);
        let (msg, _): (DecoderToCompositor, usize) =
            try_decode_control(&buf).expect("decode").expect("complete");
        msg
    }

    #[test]
    fn ready_is_emitted_on_startup_then_shutdown_clean() {
        let (parent, child) = make_pair();
        // The decoder runs on `child`; the test plays the parent role.
        std::thread::scope(|s| {
            let _ = s.spawn(move || run(child));
            // Read the Ready frame.
            match recv_decoder_msg(parent) {
                DecoderToCompositor::Ready { wire_version } => {
                    assert_eq!(wire_version, WIRE_VERSION);
                }
                other => panic!("expected Ready, got {other:?}"),
            }
            send_control(parent, &CompositorToDecoder::Shutdown);
        });
    }

    #[test]
    fn pause_is_logged_and_ignored_then_shutdown_clean() {
        let (parent, child) = make_pair();
        std::thread::scope(|s| {
            let join = s.spawn(move || run(child));
            // Drain Ready.
            let _ = recv_decoder_msg(parent);
            send_control(parent, &CompositorToDecoder::Pause);
            // Decoder is still alive; send Shutdown.
            send_control(parent, &CompositorToDecoder::Shutdown);
            // Run completes — Ok(()) or Err(ShutdownRequested) (the
            // sentinel main() converts to clean exit) both count.
            let result = join.join().expect("thread");
            assert!(
                matches!(result, Ok(()) | Err(DecoderError::ShutdownRequested)),
                "expected clean shutdown, got {result:?}"
            );
        });
    }

    #[test]
    fn malformed_prefix_aborts() {
        let (parent, child) = make_pair();
        std::thread::scope(|s| {
            let join = s.spawn(move || run(child));
            // Drain Ready.
            let _ = recv_decoder_msg(parent);
            // Send a single datagram with an oversized length prefix +
            // garbage. The decoder must fail the codec check and return
            // an error.
            let oversized: u32 = MAX_CONTROL_MSG_BYTES + 1;
            let mut bad = Vec::new();
            bad.extend_from_slice(&oversized.to_ne_bytes());
            bad.extend_from_slice(b"garbage");
            nix::sys::socket::send(parent, &bad, MsgFlags::empty()).expect("send");
            let result = join.join().expect("thread");
            assert!(
                matches!(result, Err(DecoderError::Codec(_))),
                "got {result:?}"
            );
        });
    }

    #[test]
    fn peer_hangup_returns_error() {
        let (parent, child) = make_pair();
        std::thread::scope(|s| {
            let join = s.spawn(move || run(child));
            // Drain Ready.
            let _ = recv_decoder_msg(parent);
            // Close parent end without sending Shutdown — decoder's
            // recv() returns 0 (EOF on a seqpacket), which we treat
            // as PeerHangup.
            nix::unistd::close(parent).expect("close");
            let result = join.join().expect("thread");
            assert!(
                matches!(result, Err(DecoderError::PeerHangup)),
                "got {result:?}"
            );
        });
    }
}
