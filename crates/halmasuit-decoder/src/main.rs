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

mod sandbox;

use std::os::fd::RawFd;
use std::process::ExitCode;

use halmasuit_decoder_ipc::{
    CompositorToDecoder, DecoderToCompositor, MAX_CONTROL_MSG_BYTES, WIRE_VERSION, encode_control,
    try_decode_control,
};
use nix::sys::socket::{MsgFlags, recv, send};
use thiserror::Error;
use tracing::{Level, error, info, warn};

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
        Ok(()) => {
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

/// Drive the IPC loop on `fd`.
///
/// 1. Send `Ready { wire_version }`.
/// 2. Loop: receive a control message; act on it.
///    - `Shutdown` → return Ok(()).
///    - Anything else → log "skeleton: ignoring {msg:?}" and continue.
///
/// Extracted from `main()` for unit-testing against a `socketpair`.
fn run(fd: RawFd) -> Result<(), DecoderError> {
    send_ready(fd)?;

    loop {
        match recv_one(fd)? {
            CompositorToDecoder::Shutdown => return Ok(()),
            other => {
                warn!(message = ?other, "skeleton: ignoring control message (not yet implemented)");
            }
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

/// Receive ONE control-plane message. SOCK_SEQPACKET delivers one
/// datagram per `recv`; we size the buffer for [`MAX_CONTROL_MSG_BYTES`]
/// plus the 4-byte length prefix so the codec sees a complete frame.
fn recv_one(fd: RawFd) -> Result<CompositorToDecoder, DecoderError> {
    let mut buf = vec![0u8; MAX_CONTROL_MSG_BYTES as usize + 4];
    let n = loop {
        match recv(fd, &mut buf, MsgFlags::empty()) {
            Ok(n) => break n,
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
    Ok(msg)
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
            // Run completes Ok(()).
            join.join().expect("thread").expect("run ok");
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
