//! `ChannelResponder` — a [`crate::pam_ffi::ConvResponder`] that relays
//! each PAM conversation message over the broker's SEQPACKET channel.
//!
//! libpam's conv contract is asymmetric (see [`crate::conv`] module
//! docs):
//!
//! - **Prompts** (`PAM_PROMPT_ECHO_ON`/`PAM_PROMPT_ECHO_OFF`) →
//!   [`Self::respond`]. Sends [`BrokerToCompositor::ConvPrompt`] then
//!   BLOCKS waiting for exactly one well-formed
//!   [`CompositorToBroker::ConvResponse`]. Greeter `Cancel`, any other
//!   variant, or a closed/garbled socket fail closed → libpam gets
//!   `PAM_CONV_ERR`.
//! - **Display-only** (`PAM_TEXT_INFO`/`PAM_ERROR_MSG`) →
//!   [`Self::display`]. Sends [`BrokerToCompositor::ConvDisplay`] and
//!   returns immediately. MUST NOT block; the broker's phase machine
//!   stays in `AwaitWorker` because the worker is already processing
//!   the next conv message (Epic #24 R2/R4). The compositor handles
//!   the greetd-side mandated `post_auth_message_response` and
//!   swallows it (Epic #24 R5) — so for the broker wire, display is
//!   one-way.
//!
//! No PAM, no fork here — `#![forbid(unsafe_code)]` (the FFI lives in
//! `pam_ffi`).
#![forbid(unsafe_code)]

use halmasuit_session_ipc::{
    BrokerToCompositor, CompositorToBroker, DisplayStyle, PromptStyle, Secret,
};

use crate::pam_ffi::{ConvResponder, ResponderError};
use crate::transport::SeqpacketChannel;
use crate::wire_trace;

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
    fn respond(&mut self, style: PromptStyle, message: &str) -> Result<Secret, ResponderError> {
        // Relay the prompt; a transport failure aborts the conv.
        let frame = BrokerToCompositor::ConvPrompt {
            style,
            message: message.to_owned(),
        };
        wire_trace::emit(wire_trace::Direction::Send, &frame);
        self.ch.send(&frame).map_err(|_| ResponderError)?;
        // Block for exactly one well-formed ConvResponse. Greeter
        // Cancel, any other variant, or a closed/garbled socket all
        // fail closed → conv_trampoline returns PAM_CONV_ERR.
        let received = self.ch.recv::<CompositorToBroker>();
        if let Ok(ref incoming) = received {
            wire_trace::emit(wire_trace::Direction::Recv, incoming);
        }
        match received {
            Ok(CompositorToBroker::ConvResponse { response }) => Ok(response),
            _ => Err(ResponderError),
        }
    }

    fn display(&mut self, style: DisplayStyle, message: &str) -> Result<(), ResponderError> {
        // Fire-and-forget. The compositor MUST swallow any greetd-side
        // PostAuthMessageResponse it produces in response to this
        // greetd `auth_message{type=info|error}` (Epic #24 R5); the
        // broker wire never carries a reply for display-class
        // messages, so we MUST NOT read here — doing so would deadlock
        // with the broker's phase machine staying in AwaitWorker.
        let frame = BrokerToCompositor::ConvDisplay {
            style,
            message: message.to_owned(),
        };
        wire_trace::emit(wire_trace::Direction::Send, &frame);
        self.ch.send(&frame).map_err(|_| ResponderError)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_session_ipc::{DisplayStyle, PromptStyle};
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

    #[test]
    fn relays_prompt_and_returns_the_response_secret() {
        let (broker, peer) = pair();
        let peer_thread = thread::spawn(move || {
            let got: BrokerToCompositor = peer.recv().expect("peer recv");
            assert_eq!(
                got,
                BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "Password: ".into(),
                }
            );
            peer.send(&CompositorToBroker::ConvResponse {
                response: Secret::new("hunter2".into()),
            })
            .expect("peer send");
        });

        let mut r = ChannelResponder::new(&broker);
        let secret = r
            .respond(PromptStyle::Secret, "Password: ")
            .expect("respond ok");
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
        assert!(r.respond(PromptStyle::Secret, "Password: ").is_err());
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
        assert!(r.respond(PromptStyle::Secret, "Password: ").is_err());
        t.join().unwrap();
    }

    #[test]
    fn peer_close_is_err_not_hang() {
        let (broker, peer) = pair();
        drop(peer); // compositor gone before answering
        let mut r = ChannelResponder::new(&broker);
        assert!(r.respond(PromptStyle::Secret, "Password: ").is_err());
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
        assert!(r.respond(PromptStyle::Secret, "Password: ").is_err());
        t.join().unwrap();
    }

    #[test]
    fn display_sends_frame_and_returns_immediately() {
        // Epic #24 R2: display MUST NOT block. The peer never replies;
        // display() must still return Ok and the broker must see the
        // ConvDisplay frame on the wire.
        let (broker, peer) = pair();
        let peer_thread = thread::spawn(move || {
            let got: BrokerToCompositor = peer.recv().expect("peer recv");
            assert_eq!(
                got,
                BrokerToCompositor::ConvDisplay {
                    style: DisplayStyle::Info,
                    message: "Please touch the device".into(),
                }
            );
            // Deliberately send NOTHING back. display() MUST NOT block
            // on a response; if it does this test deadlocks.
        });

        let mut r = ChannelResponder::new(&broker);
        r.display(DisplayStyle::Info, "Please touch the device")
            .expect("display ok");
        peer_thread.join().unwrap();
    }

    #[test]
    fn display_with_error_style_sends_the_right_frame() {
        // Symmetric to the info case — the DisplayStyle::Error variant
        // must serialize to wire-type "conv_display" with style "error".
        let (broker, peer) = pair();
        let peer_thread = thread::spawn(move || {
            let got: BrokerToCompositor = peer.recv().expect("peer recv");
            assert_eq!(
                got,
                BrokerToCompositor::ConvDisplay {
                    style: DisplayStyle::Error,
                    message: "Authentication failure".into(),
                }
            );
        });

        let mut r = ChannelResponder::new(&broker);
        r.display(DisplayStyle::Error, "Authentication failure")
            .expect("display ok");
        peer_thread.join().unwrap();
    }

    #[test]
    fn display_peer_closed_is_err_not_panic() {
        // If the broker channel is dead when display fires (e.g. the
        // greeter connection went away mid-conv), display MUST surface
        // the transport failure as a ResponderError so the trampoline
        // returns PAM_CONV_ERR.
        let (broker, peer) = pair();
        drop(peer); // compositor gone before display
        let mut r = ChannelResponder::new(&broker);
        assert!(r.display(DisplayStyle::Info, "anything").is_err());
    }
}
