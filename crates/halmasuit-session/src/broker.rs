//! The per-connection broker handler (Epic R6 core; Amendment A1
//! frame routing).
//!
//! Composes the already-built pieces — `AuthSlot` (R5 global single
//! slot, evict-old, SO_PEERCRED greeter gate, churn throttle),
//! `spawn_session_worker` (the Amendment-A1 one-handle `run_session`
//! fork), and the `ParentMessage` relay — into ONE lifecycle per
//! accepted compositor↔broker connection. Pure composition:
//! `#![forbid(unsafe_code)]`; the only unsafe in the crate stays in
//! `pam_ffi`/`worker`.
//!
//! The relay is strictly turn-based, mirroring the PAM conversation:
//! the worker prompts and the broker pumps the answer back, then the
//! worker reports `AuthOk` and BLOCKS for the greeter's
//! `StartSession` (Amendment A1.1 — the spec is read post-auth), then
//! `SessionOpened`/`SessionEnded`. The real libpam path is proven by
//! the flagship VM gate (Epic R12 forbids PAM mocks); the relay state
//! machine here is exercised PAM-free with a scripted worker child
//! (process/relay testing is not a PAM mock).
#![forbid(unsafe_code)]

use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker};
use thiserror::Error;

use crate::slot::{AuthSlot, SlotError};
use crate::transport::{SeqpacketChannel, TransportError, peer_uid};
use crate::worker::{ParentMessage, WorkerOutcome, spawn_session_worker};

/// How one accepted connection's lifecycle ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// The session ran and the leader exited with this status code.
    Completed { code: i32 },
    /// PAM rejected the attempt (no session opened); `reason` was
    /// relayed to the greeter as [`BrokerToCompositor::Failure`].
    AuthFailed { reason: String },
    /// The greeter sent [`CompositorToBroker::Cancel`]; the worker was
    /// SIGKILLed + reaped (Epic R4/R5).
    Cancelled,
}

/// Failure handling one broker connection.
#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("channel: {0}")]
    Transport(#[from] TransportError),
    /// `AuthSlot::create` refused (non-greeter peer / churn throttle /
    /// spawn errno). `SlotError` has no `Display`; shown via `Debug`.
    #[error("auth slot refused: {0:?}")]
    Slot(SlotError),
    /// A frame arrived that the protocol does not permit at this point
    /// (e.g. a non-`StartSession` after `AuthOk`). Fail closed.
    #[error("protocol: unexpected frame for the current phase")]
    UnexpectedFrame,
    /// The in-flight worker vanished from the slot mid-relay.
    #[error("worker is no longer in the slot")]
    WorkerGone,
}

/// Relay ONE full lifecycle between the greeter (`compositor`) and the
/// in-flight worker held by `slot` (Amendment A1 frame routing).
///
/// Strictly turn-based, mirroring the PAM conversation. The worker
/// channel is re-borrowed from `slot` per step so a `Cancel` can take
/// `&mut slot` to SIGKILL the worker.
///
/// # Errors
///
/// [`BrokerError`] on transport failure, an out-of-phase frame, or a
/// vanished worker.
pub(crate) fn relay(
    slot: &mut AuthSlot,
    compositor: &SeqpacketChannel,
) -> Result<Disposition, BrokerError> {
    loop {
        let pm: ParentMessage = slot
            .current()
            .ok_or(BrokerError::WorkerGone)?
            .channel()
            .recv()?;
        match pm {
            ParentMessage::Conv(prompt @ BrokerToCompositor::ConvPrompt { .. }) => {
                compositor.send(&prompt)?;
                match compositor.recv::<CompositorToBroker>()? {
                    resp @ CompositorToBroker::ConvResponse { .. } => {
                        slot.current()
                            .ok_or(BrokerError::WorkerGone)?
                            .channel()
                            .send(&resp)?;
                    }
                    CompositorToBroker::Cancel => {
                        let _ = slot.cancel_current();
                        return Ok(Disposition::Cancelled);
                    }
                    _ => {
                        let _ = slot.cancel_current();
                        return Err(BrokerError::UnexpectedFrame);
                    }
                }
            }
            ParentMessage::Outcome(WorkerOutcome::AuthOk { username, uid, gid }) => {
                // R8: identity is the worker's PAM-resolved one.
                compositor.send(&BrokerToCompositor::Success { username, uid, gid })?;
                // Amendment A1.1: the greeter now sends the session
                // spec; forward it verbatim to the blocked worker.
                match compositor.recv::<CompositorToBroker>()? {
                    start @ CompositorToBroker::StartSession { .. } => {
                        slot.current()
                            .ok_or(BrokerError::WorkerGone)?
                            .channel()
                            .send(&start)?;
                    }
                    CompositorToBroker::Cancel => {
                        let _ = slot.cancel_current();
                        return Ok(Disposition::Cancelled);
                    }
                    _ => {
                        let _ = slot.cancel_current();
                        return Err(BrokerError::UnexpectedFrame);
                    }
                }
            }
            // Session is live; nothing to relay — wait for the end.
            ParentMessage::Outcome(WorkerOutcome::SessionOpened { .. }) => {}
            ParentMessage::Outcome(WorkerOutcome::SessionEnded { code }) => {
                let _ = slot.reap_current();
                return Ok(Disposition::Completed { code });
            }
            ParentMessage::Outcome(WorkerOutcome::Failure { reason }) => {
                compositor.send(&BrokerToCompositor::Failure {
                    reason: reason.clone(),
                })?;
                let _ = slot.reap_current();
                return Ok(Disposition::AuthFailed { reason });
            }
            // Out of phase: the worker only ever relays a ConvPrompt
            // (Success/Failure on the BrokerToCompositor side are
            // broker→greeter only); and `Success` is the auth-only
            // worker variant `spawn_session_worker` never emits.
            ParentMessage::Conv(_) | ParentMessage::Outcome(WorkerOutcome::Success { .. }) => {
                let _ = slot.cancel_current();
                return Err(BrokerError::UnexpectedFrame);
            }
        }
    }
}

/// Handle ONE accepted compositor↔broker connection end to end.
///
/// SO_PEERCRED-gated (via [`AuthSlot::create`]'s greeter gate),
/// reads [`CompositorToBroker::BeginAuth`], spawns the
/// Amendment-A1 full-lifecycle worker into the global single slot,
/// then [`relay`]s. The `service`/`username` from `BeginAuth` are
/// only the `pam_start` hint; the authoritative identity is the
/// worker's PAM-resolved one (Epic R8).
///
/// # Errors
///
/// [`BrokerError`] — peer/slot refusal, an out-of-phase frame, or
/// transport failure.
pub fn handle_connection(
    slot: &mut AuthSlot,
    compositor: &SeqpacketChannel,
) -> Result<Disposition, BrokerError> {
    let puid = peer_uid(compositor)?;
    let CompositorToBroker::BeginAuth { service, username } =
        compositor.recv::<CompositorToBroker>()?
    else {
        return Err(BrokerError::UnexpectedFrame);
    };
    slot.create(puid, || spawn_session_worker(&service, &username))
        .map_err(BrokerError::Slot)?;
    relay(slot, compositor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::AuthSlot;
    use crate::transport::SeqpacketChannel;
    use crate::worker::{WorkerOutcome, spawn_worker};
    use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, PromptStyle, Secret};
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    use std::thread;
    use std::time::Duration;

    const GREETER: u32 = 1000;

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

    /// Drive the relay with a scripted worker child (real fork, NO
    /// libpam — process/relay testing, Epic R12 intact) that walks the
    /// full Amendment-A1 sequence, and a greeter thread on the
    /// compositor end.
    #[test]
    fn relay_full_lifecycle_auth_then_session() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|w| {
                // worker → broker → greeter: one prompt.
                w.send(&BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "PW: ".into(),
                })
                .unwrap();
                let resp: CompositorToBroker = w.recv().unwrap();
                assert!(matches!(resp, CompositorToBroker::ConvResponse { .. }));
                // A1.1: auth ok, then BLOCK for the spec.
                w.send(&WorkerOutcome::AuthOk {
                    username: "alice".into(),
                    uid: 1000,
                    gid: 1000,
                })
                .unwrap();
                let spec: CompositorToBroker = w.recv().unwrap();
                assert!(matches!(spec, CompositorToBroker::StartSession { .. }));
                w.send(&WorkerOutcome::SessionOpened {
                    username: "alice".into(),
                    uid: 1000,
                    gid: 1000,
                })
                .unwrap();
                w.send(&WorkerOutcome::SessionEnded { code: 0 }).unwrap();
            })
        })
        .unwrap();

        let (greeter, broker_end) = pair();
        let gt = thread::spawn(move || {
            let p: BrokerToCompositor = greeter.recv().unwrap();
            assert_eq!(
                p,
                BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "PW: ".into()
                }
            );
            greeter
                .send(&CompositorToBroker::ConvResponse {
                    response: Secret::new("pw".into()),
                })
                .unwrap();
            // Amendment A1: Success carries the worker's PAM-resolved
            // identity; the greeter then sends StartSession.
            let s: BrokerToCompositor = greeter.recv().unwrap();
            assert_eq!(
                s,
                BrokerToCompositor::Success {
                    username: "alice".into(),
                    uid: 1000,
                    gid: 1000
                }
            );
            greeter
                .send(&CompositorToBroker::StartSession {
                    cmd: vec!["/bin/sh".into()],
                    env: vec![],
                })
                .unwrap();
        });

        let disp = relay(&mut slot, &broker_end).expect("relay ok");
        assert_eq!(disp, Disposition::Completed { code: 0 });
        assert!(slot.current().is_none(), "slot reaped after SessionEnded");
        gt.join().unwrap();
    }

    #[test]
    fn relay_auth_failure_is_relayed_then_reaped() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|w| {
                w.send(&WorkerOutcome::Failure {
                    reason: "PAM step failed: status 7".into(),
                })
                .unwrap();
            })
        })
        .unwrap();

        let (greeter, broker_end) = pair();
        let gt = thread::spawn(move || {
            let f: BrokerToCompositor = greeter.recv().unwrap();
            assert_eq!(
                f,
                BrokerToCompositor::Failure {
                    reason: "PAM step failed: status 7".into()
                }
            );
        });

        let disp = relay(&mut slot, &broker_end).expect("relay ok");
        assert!(matches!(disp, Disposition::AuthFailed { .. }));
        assert!(slot.current().is_none());
        gt.join().unwrap();
    }

    #[test]
    fn relay_greeter_cancel_sigkills_the_worker() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|w| {
                w.send(&BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "PW: ".into(),
                })
                .unwrap();
                // Greeter cancels instead of answering; the worker is
                // SIGKILLed, so this recv never returns — block.
                let _: Result<CompositorToBroker, _> = w.recv();
                loop {
                    std::thread::park();
                }
            })
        })
        .unwrap();

        let (greeter, broker_end) = pair();
        let gt = thread::spawn(move || {
            let _p: BrokerToCompositor = greeter.recv().unwrap();
            greeter.send(&CompositorToBroker::Cancel).unwrap();
        });

        let disp = relay(&mut slot, &broker_end).expect("relay ok");
        assert_eq!(disp, Disposition::Cancelled);
        assert!(
            slot.current().is_none(),
            "cancelled worker SIGKILLed + reaped, slot cleared"
        );
        gt.join().unwrap();
    }

    #[test]
    fn handle_connection_rejects_non_greeter_peer() {
        // socketpair peers are the test process's own uid; configure a
        // DIFFERENT greeter uid so the SO_PEERCRED gate (in
        // AuthSlot::create) refuses without spawning anything.
        let my_uid = nix::unistd::getuid().as_raw();
        let mut slot = AuthSlot::new(my_uid.wrapping_add(1), 5, Duration::from_secs(10));
        let (greeter, broker_end) = pair();
        let gt = thread::spawn(move || {
            greeter
                .send(&CompositorToBroker::BeginAuth {
                    service: "halmasuit".into(),
                    username: "alice".into(),
                })
                .unwrap();
        });
        let r = handle_connection(&mut slot, &broker_end);
        assert!(
            matches!(r, Err(BrokerError::Slot(_))),
            "non-greeter peer refused: {r:?}"
        );
        assert!(slot.current().is_none(), "no worker spawned");
        gt.join().unwrap();
    }
}
