//! `ChannelResponder` — a [`crate::pam_ffi::ConvResponder`] that relays
//! each PAM prompt over the broker's SEQPACKET channel and blocks for
//! the compositor/greeter's response.
//!
//! This is the glue between the libpam conv trampoline and the
//! transport: the trampoline asks for a response, the responder sends
//! the `ConvPrompt` frame and waits for a `ConvResponse`. No PAM, no
//! fork here — `#![forbid(unsafe_code)]` (the FFI lives in `pam_ffi`).
//! Fail-closed: anything other than a well-formed `ConvResponse`
//! (greeter `Cancel`, an unexpected variant, a closed/garbled socket)
//! aborts the conversation so libpam gets `PAM_CONV_ERR`.
#![forbid(unsafe_code)]

use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, Secret};

use crate::pam_ffi::{ConvResponder, ResponderError};
use crate::transport::SeqpacketChannel;

/// A [`ConvResponder`] backed by the broker's SEQPACKET channel.
///
/// Borrows the channel for the transaction; the broker owns the
/// [`SeqpacketChannel`] and the `pam_handle_t`.
pub struct ChannelResponder<'c> {
    ch: &'c SeqpacketChannel,
}

impl<'c> ChannelResponder<'c> {
    pub const fn new(ch: &'c SeqpacketChannel) -> Self {
        Self { ch }
    }
}

impl ConvResponder for ChannelResponder<'_> {
    fn respond(&mut self, prompt: &BrokerToCompositor) -> Result<Secret, ResponderError> {
        // Relay the prompt; a transport failure aborts the conv.
        self.ch.send(prompt).map_err(|_| ResponderError)?;
        // Block for exactly one well-formed ConvResponse. Greeter
        // Cancel, any other variant, or a closed/garbled socket all
        // fail closed → conv_trampoline returns PAM_CONV_ERR.
        match self.ch.recv::<CompositorToBroker>() {
            Ok(CompositorToBroker::ConvResponse { response }) => Ok(response),
            _ => Err(ResponderError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_session_ipc::PromptStyle;
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    use std::os::fd::{AsFd, AsRawFd};
    use std::thread;

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

    fn a_prompt() -> BrokerToCompositor {
        BrokerToCompositor::ConvPrompt {
            style: PromptStyle::Secret,
            message: "Password: ".into(),
        }
    }

    #[test]
    fn relays_prompt_and_returns_the_response_secret() {
        let (broker, peer) = pair();
        let sent = a_prompt();
        let expect = sent.clone();
        let peer_thread = thread::spawn(move || {
            let got: BrokerToCompositor = peer.recv().expect("peer recv");
            assert_eq!(got, expect);
            peer.send(&CompositorToBroker::ConvResponse {
                response: Secret::new("hunter2".into()),
            })
            .expect("peer send");
        });

        let mut r = ChannelResponder::new(&broker);
        let secret = r.respond(&sent).expect("respond ok");
        assert_eq!(secret.expose(), "hunter2");
        peer_thread.join().unwrap();
    }

    #[test]
    fn greeter_cancel_aborts_the_conversation() {
        let (broker, peer) = pair();
        let t = thread::spawn(move || {
            let _: BrokerToCompositor = peer.recv().expect("peer recv");
            peer.send(&CompositorToBroker::Cancel).expect("peer send");
        });
        let mut r = ChannelResponder::new(&broker);
        assert!(r.respond(&a_prompt()).is_err());
        t.join().unwrap();
    }

    #[test]
    fn unexpected_variant_aborts_the_conversation() {
        let (broker, peer) = pair();
        let t = thread::spawn(move || {
            let _: BrokerToCompositor = peer.recv().expect("peer recv");
            peer.send(&CompositorToBroker::BeginAuth {
                service: "x".into(),
                username: "y".into(),
            })
            .expect("peer send");
        });
        let mut r = ChannelResponder::new(&broker);
        assert!(r.respond(&a_prompt()).is_err());
        t.join().unwrap();
    }

    #[test]
    fn peer_close_is_err_not_hang() {
        let (broker, peer) = pair();
        drop(peer); // compositor gone before answering
        let mut r = ChannelResponder::new(&broker);
        assert!(r.respond(&a_prompt()).is_err());
    }

    #[test]
    fn garbage_reply_is_err_not_panic() {
        let (broker, peer) = pair();
        let t = thread::spawn(move || {
            let _: BrokerToCompositor = peer.recv().expect("peer recv");
            // Raw non-frame bytes back at the broker. `send` is a safe
            // nix wrapper (same call transport.rs uses under forbid).
            nix::sys::socket::send(
                peer.as_fd().as_raw_fd(),
                &[0xde, 0xad, 0xbe, 0xef, 0x01],
                nix::sys::socket::MsgFlags::empty(),
            )
            .expect("raw send");
        });
        let mut r = ChannelResponder::new(&broker);
        assert!(r.respond(&a_prompt()).is_err());
        t.join().unwrap();
    }
}
