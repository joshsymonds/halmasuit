//! `halmasuit-session` — the privileged PAM-lifecycle broker of the
//! unified session/pamd epic.
//!
//! This slice is **transport only**: a `SOCK_SEQPACKET` framed channel
//! that carries `halmasuit-session-ipc` messages, plus a `SO_PEERCRED`
//! peer-uid primitive. It links no PAM, forks nothing, and makes no
//! policy decision — it is the byte tier the broker and the compositor
//! relay both build on. PAM, the ephemeral auth fork, evict-old, and the
//! socket-activated unit land in later tasks.
//!
//! `#![forbid(unsafe_code)]`: every syscall goes through `nix`'s safe
//! wrappers, same posture as `halmasuit-greetd`'s SO_PEERCRED path.

#![forbid(unsafe_code)]

pub mod conv;

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use halmasuit_session_ipc::{CodecError, MAX_MESSAGE_SIZE, encode, try_decode};
use nix::sys::socket::{MsgFlags, recv, send, sockopt};
use thiserror::Error;

/// Errors from the SEQPACKET transport. Framing/codec, peer closure, or
/// the underlying syscall — never a panic on hostile bytes.
#[derive(Debug, Error)]
pub enum TransportError {
    /// A syscall failed (`send`/`recv`/`getsockopt`).
    #[error("socket syscall failed: {0}")]
    Io(#[from] nix::Error),

    /// The datagram body failed framing/JSON decode (includes an
    /// oversized length prefix).
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),

    /// The peer closed the socket (zero-length read).
    #[error("peer closed the connection")]
    Closed,

    /// A whole datagram arrived but did not contain exactly one
    /// complete message. On SEQPACKET there is no "more to come", so an
    /// incomplete or trailing-garbage datagram is malformed, not a wait
    /// condition.
    #[error("malformed datagram (not exactly one framed message)")]
    Malformed,
}

/// One end of a connected `SOCK_SEQPACKET` socket, carrying
/// `halmasuit-session-ipc` messages one datagram per logical message.
///
/// The length prefix from the codec is redundant on SEQPACKET (the
/// kernel preserves message boundaries) but kept so the framing is
/// identical to `halmasuit-greetd` and a single decode path serves
/// both transports.
#[derive(Debug)]
pub struct SeqpacketChannel {
    fd: OwnedFd,
}

impl SeqpacketChannel {
    pub const fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Encode `msg` and write it as exactly one datagram.
    ///
    /// # Errors
    ///
    /// [`TransportError::Codec`] if encoding fails/overflows;
    /// [`TransportError::Io`] on `send`; [`TransportError::Malformed`]
    /// if the kernel accepted only part of the datagram (a SEQPACKET
    /// message cannot be meaningfully continued).
    pub fn send<M: serde::Serialize>(&self, msg: &M) -> Result<(), TransportError> {
        let bytes = encode(msg)?;
        let n = send(self.fd.as_raw_fd(), &bytes, MsgFlags::empty())?;
        if n == bytes.len() {
            Ok(())
        } else {
            Err(TransportError::Malformed)
        }
    }

    /// Read exactly one datagram and decode it to `T`.
    ///
    /// # Errors
    ///
    /// [`TransportError::Closed`] on peer hangup;
    /// [`TransportError::Codec`] on an oversized prefix or bad JSON;
    /// [`TransportError::Malformed`] if the datagram is not exactly one
    /// complete message; [`TransportError::Io`] on `recv`.
    pub fn recv<T: serde::de::DeserializeOwned>(&self) -> Result<T, TransportError> {
        // SEQPACKET delivers one datagram per recv; size the buffer to
        // the largest framed message the codec will ever accept so a
        // valid message is never truncated.
        let mut buf = vec![0u8; std::mem::size_of::<u32>() + MAX_MESSAGE_SIZE as usize];
        let n = recv(self.fd.as_raw_fd(), &mut buf, MsgFlags::empty())?;
        if n == 0 {
            return Err(TransportError::Closed);
        }
        match try_decode::<T>(&buf[..n])? {
            Some((msg, consumed)) if consumed == n => Ok(msg),
            // Short prefix (`None`) or trailing bytes after one message:
            // the whole datagram is here, so this is malformed.
            _ => Err(TransportError::Malformed),
        }
    }
}

impl AsFd for SeqpacketChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// The peer's uid via `SO_PEERCRED`.
///
/// The kernel-attested identity of whoever is on the other end — the
/// building block for the Epic R5/R8 "only the verified greeter peer
/// may evict / be trusted as the conversation driver" gate. This
/// primitive only reads it; it makes no policy decision.
///
/// # Errors
///
/// [`TransportError::Io`] if `getsockopt(SO_PEERCRED)` fails.
pub fn peer_uid<F: AsFd>(sock: &F) -> Result<u32, TransportError> {
    let creds = nix::sys::socket::getsockopt(sock, sockopt::PeerCredentials)?;
    Ok(creds.uid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, PromptStyle, Secret};
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};

    fn pair() -> (SeqpacketChannel, SeqpacketChannel) {
        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");
        (SeqpacketChannel::new(a), SeqpacketChannel::new(b))
    }

    #[test]
    fn roundtrips_every_compositor_to_broker_variant() {
        let (tx, rx) = pair();
        for msg in [
            CompositorToBroker::BeginAuth {
                service: "halmasuit".into(),
                username: "alice".into(),
            },
            CompositorToBroker::ConvResponse {
                response: Secret::new("hunter2".into()),
            },
            CompositorToBroker::Cancel,
        ] {
            tx.send(&msg).expect("send");
            let got: CompositorToBroker = rx.recv().expect("recv");
            assert_eq!(got, msg);
        }
    }

    #[test]
    fn roundtrips_every_broker_to_compositor_variant() {
        let (tx, rx) = pair();
        for msg in [
            BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "Password: ".into(),
            },
            BrokerToCompositor::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            },
            BrokerToCompositor::Failure {
                reason: "denied".into(),
            },
        ] {
            tx.send(&msg).expect("send");
            let got: BrokerToCompositor = rx.recv().expect("recv");
            assert_eq!(got, msg);
        }
    }

    #[test]
    fn secret_survives_the_socket_intact() {
        let (tx, rx) = pair();
        tx.send(&CompositorToBroker::ConvResponse {
            response: Secret::new("p@ss w0rd ☃".into()),
        })
        .expect("send");
        match rx.recv::<CompositorToBroker>().expect("recv") {
            CompositorToBroker::ConvResponse { response } => {
                assert_eq!(response.expose(), "p@ss w0rd ☃");
            }
            other => panic!("expected ConvResponse, got {other:?}"),
        }
    }

    #[test]
    fn recv_on_short_datagram_is_malformed_not_panic() {
        let (raw_tx, raw_rx) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");
        // A whole datagram arrived but it is shorter than the length
        // prefix — on SEQPACKET there is no "more to come", so this is
        // a malformed message, not a wait condition.
        nix::sys::socket::send(
            std::os::fd::AsRawFd::as_raw_fd(&raw_tx),
            &[0u8, 0, 0],
            nix::sys::socket::MsgFlags::empty(),
        )
        .expect("raw send");
        let rx = SeqpacketChannel::new(raw_rx);
        let r = rx.recv::<CompositorToBroker>();
        assert!(matches!(r, Err(TransportError::Malformed)), "got: {r:?}");
    }

    #[test]
    fn recv_on_oversized_prefix_is_error_not_panic() {
        let (raw_tx, raw_rx) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");
        let oversized: u32 = halmasuit_session_ipc::MAX_MESSAGE_SIZE + 1;
        nix::sys::socket::send(
            std::os::fd::AsRawFd::as_raw_fd(&raw_tx),
            &oversized.to_ne_bytes(),
            nix::sys::socket::MsgFlags::empty(),
        )
        .expect("raw send");
        let rx = SeqpacketChannel::new(raw_rx);
        let r = rx.recv::<CompositorToBroker>();
        assert!(matches!(r, Err(TransportError::Codec(_))), "got: {r:?}");
    }

    #[test]
    fn recv_on_garbage_never_panics() {
        // One datagram per socket → exactly one recv per socket.
        // `recv` is a blocking syscall by design (synchronous
        // transport); a second recv with nothing more sent would
        // block forever — that is correct transport behaviour, so the
        // test must not do it. Alternate the decode target across
        // iterations to cover both generic parametrizations.
        for seed in 0u8..64 {
            let (raw_tx, raw_rx) = socketpair(
                AddressFamily::Unix,
                SockType::SeqPacket,
                None,
                SockFlag::empty(),
            )
            .expect("socketpair");
            let body: Vec<u8> = (0..=seed)
                .map(|i| i.wrapping_mul(seed).wrapping_add(13))
                .collect();
            nix::sys::socket::send(
                std::os::fd::AsRawFd::as_raw_fd(&raw_tx),
                &body,
                nix::sys::socket::MsgFlags::empty(),
            )
            .expect("raw send");
            let rx = SeqpacketChannel::new(raw_rx);
            // Must return a Result (an Err for arbitrary bytes), never
            // panic.
            if seed % 2 == 0 {
                assert!(rx.recv::<CompositorToBroker>().is_err());
            } else {
                assert!(rx.recv::<BrokerToCompositor>().is_err());
            }
        }
    }

    #[test]
    fn recv_after_peer_close_is_closed() {
        let (tx, rx) = pair();
        drop(tx);
        let r = rx.recv::<CompositorToBroker>();
        assert!(matches!(r, Err(TransportError::Closed)), "got: {r:?}");
    }

    #[test]
    fn peer_uid_matches_self_over_socketpair() {
        let (a, _b) = pair();
        let uid = peer_uid(&a).expect("peer_uid");
        assert_eq!(uid, nix::unistd::getuid().as_raw());
    }
}
