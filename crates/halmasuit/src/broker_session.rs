//! `BrokerSession` — the compositor's `PamSession` backed by the
//! privileged `halmasuit-session` broker (Epic R3 / Amendments A4/A5).
//!
//! This is `halmasuit-pam`'s successor on the live path: instead of
//! running PAM in-process, the compositor relays the greetd auth
//! conversation to the broker over a `SOCK_SEQPACKET` channel speaking
//! the frozen `halmasuit-session-ipc` contract. The pure translation
//! brain is [`crate::broker_relay::BrokerRelay`] (task #27); this file
//! is only the I/O shell.
//!
//! Scope (task #29): the AUTH conversation only —
//! `PamStep::{Challenge,Success,Failure}`. The greetd `Spawning` →
//! `StartSession` seam, the A5 `SessionOpened`/`SessionEnded`
//! consumption + two-key flash-free swap, and the SCM_RIGHTS pidfd
//! backstop are later R3 sub-tasks. The compositor links no libpam and
//! never depends on the `halmasuit-session` crate (R2/R3/R14); only
//! the pure `halmasuit-session-ipc` codec is shared (the SEQPACKET
//! syscall wrapper is reimplemented locally, not the codec).
//!
//! Built behind the `PamSessionFactory` trait and NOT constructed in
//! `main()` yet — the live `PamThreadFactory`→`BrokerSessionFactory`
//! swap is atomic with deleting `halmasuit-pam`/`halmasuit-spawn`
//! (R10/R14/R15; the next task), never before.

// reason: the live PamSessionFactory swap that constructs/drives this
// is the next (atomic-with-deletion) R3 task; until then it is
// exercised only by this module's tests, so the non-test build sees
// it as unused. Removed when that swap lands (Amendment A4).
#![allow(dead_code)]

use std::fmt;
use std::os::fd::{AsRawFd, OwnedFd};

use halmasuit_greetd::server::PamSessionFactory;
use halmasuit_greetd::{PamSession, PamStep};
use halmasuit_session_ipc::{
    BrokerToCompositor, CodecError, CompositorToBroker, MAX_MESSAGE_SIZE, encode, try_decode,
};
use nix::sys::socket::{MsgFlags, recv, send};

use crate::broker_relay::{BrokerRelay, RelayEvent};

/// SEQPACKET framing error. Internal — every variant is turned into a
/// fail-closed [`PamStep::Failure`] by [`BrokerSession::step`]; it is
/// never surfaced to the greeter as anything but an auth failure.
#[derive(Debug)]
pub enum WireError {
    /// `send`/`recv` syscall failed.
    Io(nix::Error),
    /// Body failed framing/JSON decode (incl. an oversized prefix).
    Codec(CodecError),
    /// Peer closed the socket (zero-length read).
    Closed,
    /// A datagram arrived that was not exactly one framed message. On
    /// SEQPACKET there is no "more to come" — this is malformed.
    Malformed,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "broker channel syscall failed: {e}"),
            Self::Codec(e) => write!(f, "broker channel codec error: {e}"),
            Self::Closed => f.write_str("broker closed the connection"),
            Self::Malformed => f.write_str("broker sent a malformed datagram"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<nix::Error> for WireError {
    fn from(e: nix::Error) -> Self {
        Self::Io(e)
    }
}

impl From<CodecError> for WireError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

/// One end of a connected `SOCK_SEQPACKET` socket carrying
/// `halmasuit-session-ipc` messages, one datagram per logical message.
///
/// The codec ([`encode`]/[`try_decode`]) is shared with the broker via
/// the pure contract crate; only this ~thin syscall wrapper is local
/// (the compositor must not depend on the libpam-linking
/// `halmasuit-session` crate — R14).
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
    /// [`WireError::Codec`] on encode overflow; [`WireError::Io`] on
    /// `send`; [`WireError::Malformed`] if the kernel accepted only
    /// part of the datagram (a SEQPACKET message cannot be continued).
    pub fn send(&self, msg: &CompositorToBroker) -> Result<(), WireError> {
        let bytes = encode(msg)?;
        let n = send(self.fd.as_raw_fd(), &bytes, MsgFlags::empty())?;
        if n == bytes.len() {
            Ok(())
        } else {
            Err(WireError::Malformed)
        }
    }

    /// Read exactly one datagram and decode it.
    ///
    /// # Errors
    /// [`WireError::Closed`] on peer hangup; [`WireError::Codec`] on a
    /// bad/oversized body; [`WireError::Malformed`] if the datagram is
    /// not exactly one complete message; [`WireError::Io`] on `recv`.
    pub fn recv(&self) -> Result<BrokerToCompositor, WireError> {
        let mut buf = vec![0u8; std::mem::size_of::<u32>() + MAX_MESSAGE_SIZE as usize];
        let n = recv(self.fd.as_raw_fd(), &mut buf, MsgFlags::empty())?;
        if n == 0 {
            return Err(WireError::Closed);
        }
        match try_decode::<BrokerToCompositor>(&buf[..n])? {
            Some((msg, consumed)) if consumed == n => Ok(msg),
            _ => Err(WireError::Malformed),
        }
    }
}

/// A `PamSession` whose conversation is relayed to the broker.
///
/// `Broken` is the fail-closed state: a failed connect (or any
/// transport/relay error mid-auth) makes every subsequent `step`
/// return [`PamStep::Failure`] — never a panic, never a partial
/// success. The greetd state machine treats that exactly like a PAM
/// rejection.
enum Inner {
    Connected {
        chan: SeqpacketChannel,
        relay: BrokerRelay,
    },
    Broken(String),
}

pub struct BrokerSession {
    inner: Inner,
}

impl BrokerSession {
    /// Wrap an already-connected channel (the seam unit tests inject;
    /// production builds it via [`BrokerSessionFactory::build`]).
    pub const fn new(chan: SeqpacketChannel, service: String, username: String) -> Self {
        Self {
            inner: Inner::Connected {
                chan,
                relay: BrokerRelay::new(service, username),
            },
        }
    }

    /// A session that fails closed on first `step` with `reason`
    /// (e.g. the broker socket could not be reached).
    pub const fn broken(reason: String) -> Self {
        Self {
            inner: Inner::Broken(reason),
        }
    }
}

fn fail(reason: &str) -> PamStep {
    PamStep::Failure {
        reason: reason.to_owned(),
    }
}

impl PamSession for BrokerSession {
    fn step(&mut self, response: Option<String>) -> PamStep {
        let (chan, relay) = match &mut self.inner {
            Inner::Connected { chan, relay } => (chan, relay),
            Inner::Broken(reason) => return fail(reason),
        };
        let outbound = match relay.on_pam_step(response) {
            Ok(frame) => frame,
            Err(e) => {
                let reason = e.to_string();
                self.inner = Inner::Broken(reason.clone());
                return fail(&reason);
            }
        };
        if let Err(e) = chan.send(&outbound) {
            let reason = e.to_string();
            self.inner = Inner::Broken(reason.clone());
            return fail(&reason);
        }
        let frame = match chan.recv() {
            Ok(f) => f,
            Err(e) => {
                let reason = e.to_string();
                self.inner = Inner::Broken(reason.clone());
                return fail(&reason);
            }
        };
        match relay.on_broker_frame(frame) {
            Ok(RelayEvent::Pam(step)) => step,
            Ok(_) => {
                // A lifecycle frame during the auth conversation is a
                // protocol violation here (those are handled by the
                // later Spawning/lifecycle seam, not by `step`).
                let reason = "broker sent a non-conversation frame during auth".to_owned();
                self.inner = Inner::Broken(reason.clone());
                fail(&reason)
            }
            Err(e) => {
                let reason = e.to_string();
                self.inner = Inner::Broken(reason.clone());
                fail(&reason)
            }
        }
    }
}

/// `PamSessionFactory` that connects to the broker socket per greeter
/// `CreateSession`. A failed connect yields a [`BrokerSession::broken`]
/// — `build` cannot fail (the trait returns the boxed session), so the
/// failure surfaces as a clean auth rejection on first `step`.
pub struct BrokerSessionFactory {
    /// PAM service hint forwarded in `BeginAuth`.
    service: String,
    /// Broker `SOCK_SEQPACKET` socket path
    /// (`/run/halmasuit-session.sock`; overridable via the same env
    /// the unit sets).
    sock_path: std::path::PathBuf,
}

impl BrokerSessionFactory {
    pub const fn new(service: String, sock_path: std::path::PathBuf) -> Self {
        Self { service, sock_path }
    }

    /// Connect a client `SOCK_SEQPACKET` socket to `sock_path`.
    ///
    /// # Errors
    /// [`WireError::Io`] if `socket`/`connect` fails.
    fn connect(&self) -> Result<SeqpacketChannel, WireError> {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
        let fd = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::empty(),
            None,
        )?;
        let addr = UnixAddr::new(&self.sock_path).map_err(WireError::Io)?;
        connect(fd.as_raw_fd(), &addr)?;
        Ok(SeqpacketChannel::new(fd))
    }
}

impl PamSessionFactory for BrokerSessionFactory {
    fn build(&self, username: &str) -> Box<dyn PamSession + Send> {
        match self.connect() {
            Ok(chan) => Box::new(BrokerSession::new(
                chan,
                self.service.clone(),
                username.to_owned(),
            )),
            Err(e) => Box::new(BrokerSession::broken(format!(
                "cannot reach the halmasuit-session broker: {e}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_greetd::AuthMessageType;
    use halmasuit_session_ipc::PromptStyle;
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    use std::thread;

    /// (compositor end, broker end) connected SEQPACKET pair.
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

    /// Decode the next compositor→broker frame on the broker end.
    fn broker_recv(end: &SeqpacketChannel) -> CompositorToBroker {
        let mut buf = vec![0u8; std::mem::size_of::<u32>() + MAX_MESSAGE_SIZE as usize];
        let n = recv(end.fd.as_raw_fd(), &mut buf, MsgFlags::empty()).expect("recv");
        assert!(n > 0, "compositor closed");
        let (msg, consumed): (CompositorToBroker, usize) =
            try_decode(&buf[..n]).expect("decode").expect("complete");
        assert_eq!(consumed, n);
        msg
    }

    fn broker_send(end: &SeqpacketChannel, msg: &BrokerToCompositor) {
        let bytes = halmasuit_session_ipc::encode(msg).expect("encode");
        let nn = send(end.fd.as_raw_fd(), &bytes, MsgFlags::empty()).expect("send");
        assert_eq!(nn, bytes.len());
    }

    #[test]
    fn first_step_sends_begin_auth_and_prompt_becomes_challenge() {
        let (comp, broker) = pair();
        let mut s = BrokerSession::new(comp, "halmasuit".into(), "alice".into());
        let bt = thread::spawn(move || {
            assert_eq!(
                broker_recv(&broker),
                CompositorToBroker::BeginAuth {
                    service: "halmasuit".into(),
                    username: "alice".into(),
                }
            );
            broker_send(
                &broker,
                &BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "Password: ".into(),
                },
            );
        });
        assert_eq!(
            s.step(None),
            PamStep::Challenge {
                kind: AuthMessageType::Secret,
                prompt: "Password: ".into(),
            }
        );
        bt.join().unwrap();
    }

    #[test]
    fn response_then_success_passes_identity_through_verbatim_r8() {
        let (comp, broker) = pair();
        let mut s = BrokerSession::new(comp, "halmasuit".into(), "alice".into());
        let bt = thread::spawn(move || {
            assert!(matches!(
                broker_recv(&broker),
                CompositorToBroker::BeginAuth { .. }
            ));
            broker_send(
                &broker,
                &BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "pw".into(),
                },
            );
            match broker_recv(&broker) {
                CompositorToBroker::ConvResponse { response } => {
                    assert_eq!(response.expose(), "hunter2");
                }
                other => panic!("expected ConvResponse, got {other:?}"),
            }
            broker_send(
                &broker,
                &BrokerToCompositor::Success {
                    username: "alice.canonical".into(),
                    uid: 1001,
                    gid: 1001,
                },
            );
        });
        assert!(matches!(s.step(None), PamStep::Challenge { .. }));
        assert_eq!(
            s.step(Some("hunter2".into())),
            PamStep::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            }
        );
        bt.join().unwrap();
    }

    #[test]
    fn broker_failure_frame_becomes_pam_failure() {
        let (comp, broker) = pair();
        let mut s = BrokerSession::new(comp, "halmasuit".into(), "bob".into());
        let bt = thread::spawn(move || {
            assert!(matches!(
                broker_recv(&broker),
                CompositorToBroker::BeginAuth { .. }
            ));
            broker_send(
                &broker,
                &BrokerToCompositor::Failure {
                    reason: "authentication failed".into(),
                },
            );
        });
        assert_eq!(
            s.step(None),
            PamStep::Failure {
                reason: "authentication failed".into(),
            }
        );
        bt.join().unwrap();
    }

    #[test]
    fn broker_hangup_mid_auth_fails_closed() {
        let (comp, broker) = pair();
        let mut s = BrokerSession::new(comp, "halmasuit".into(), "alice".into());
        // Broker reads BeginAuth then drops the socket without
        // answering — recv sees peer-closed.
        let bt = thread::spawn(move || {
            assert!(matches!(
                broker_recv(&broker),
                CompositorToBroker::BeginAuth { .. }
            ));
            drop(broker);
        });
        match s.step(None) {
            PamStep::Failure { .. } => {}
            other => panic!("expected fail-closed Failure, got {other:?}"),
        }
        // Subsequent steps stay failed (Broken latch), never panic.
        assert!(matches!(s.step(None), PamStep::Failure { .. }));
        bt.join().unwrap();
    }

    #[test]
    fn lifecycle_frame_during_auth_is_protocol_violation() {
        let (comp, broker) = pair();
        let mut s = BrokerSession::new(comp, "halmasuit".into(), "alice".into());
        let bt = thread::spawn(move || {
            assert!(matches!(
                broker_recv(&broker),
                CompositorToBroker::BeginAuth { .. }
            ));
            // Out of phase: a session-lifecycle frame before any auth
            // conversation. Relay fails closed.
            broker_send(&broker, &BrokerToCompositor::SessionOpened);
        });
        match s.step(None) {
            PamStep::Failure { .. } => {}
            other => panic!("expected Failure on out-of-phase frame, got {other:?}"),
        }
        bt.join().unwrap();
    }

    #[test]
    fn broken_factory_session_fails_closed_without_panic() {
        let mut s = BrokerSession::broken("cannot reach the broker".into());
        assert_eq!(
            s.step(None),
            PamStep::Failure {
                reason: "cannot reach the broker".into(),
            }
        );
        // Idempotent, never panics.
        assert!(matches!(s.step(Some("x".into())), PamStep::Failure { .. }));
    }
}
