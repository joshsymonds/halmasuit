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

/// Which side the relay is waiting on next (Amendment A2: the relay is
/// a step-driven state machine the calloop loop pumps on FD readiness,
/// NOT a blocking loop that owns the thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayPhase {
    /// Expect a `ParentMessage` from the in-flight worker.
    AwaitWorker,
    /// Sent `ConvPrompt` to the greeter; expect `ConvResponse`/`Cancel`.
    AwaitGreeterConvResponse,
    /// Sent `Success` to the greeter (A1.1); expect
    /// `StartSession`/`Cancel`.
    AwaitGreeterStartSession,
    /// `SessionOpened` seen; the leader runs — expect `SessionEnded`
    /// from the worker (the worker side is still the next read).
    SessionRunning,
}

/// Outcome of one relay step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayStep {
    /// More frames to relay; keep the sources armed.
    Continue,
    /// Terminal: the connection's lifecycle ended this way.
    Finished(Disposition),
}

/// The step-driven relay state machine (Amendment A1 frame routing,
/// Amendment A2 shape). One channel action per call; the worker
/// channel is re-borrowed from `slot` each step so a greeter `Cancel`
/// can take `&mut slot` to SIGKILL the worker.
#[derive(Debug)]
pub(crate) struct Relay {
    phase: RelayPhase,
}

impl Relay {
    pub(crate) const fn new() -> Self {
        Self {
            phase: RelayPhase::AwaitWorker,
        }
    }

    pub(crate) const fn phase(&self) -> RelayPhase {
        self.phase
    }

    /// Drive one step from the WORKER side (call when the worker
    /// channel is readable). Valid in `AwaitWorker`/`SessionRunning`;
    /// a readiness in a greeter-await phase is an out-of-phase
    /// protocol violation (fail closed).
    ///
    /// # Errors
    /// [`BrokerError`] on transport failure / out-of-phase frame /
    /// vanished worker.
    pub(crate) fn on_worker_readable(
        &mut self,
        slot: &mut AuthSlot,
        compositor: &SeqpacketChannel,
    ) -> Result<RelayStep, BrokerError> {
        if !matches!(
            self.phase,
            RelayPhase::AwaitWorker | RelayPhase::SessionRunning
        ) {
            let _ = slot.cancel_current();
            return Err(BrokerError::UnexpectedFrame);
        }
        let pm: ParentMessage = slot
            .current()
            .ok_or(BrokerError::WorkerGone)?
            .channel()
            .recv()?;
        match pm {
            ParentMessage::Conv(prompt @ BrokerToCompositor::ConvPrompt { .. }) => {
                compositor.send(&prompt)?;
                self.phase = RelayPhase::AwaitGreeterConvResponse;
                Ok(RelayStep::Continue)
            }
            ParentMessage::Outcome(WorkerOutcome::AuthOk { username, uid, gid }) => {
                // R8: identity is the worker's PAM-resolved one.
                compositor.send(&BrokerToCompositor::Success { username, uid, gid })?;
                self.phase = RelayPhase::AwaitGreeterStartSession;
                Ok(RelayStep::Continue)
            }
            // Session is live; nothing to relay — keep awaiting the end.
            ParentMessage::Outcome(WorkerOutcome::SessionOpened { .. }) => {
                self.phase = RelayPhase::SessionRunning;
                Ok(RelayStep::Continue)
            }
            ParentMessage::Outcome(WorkerOutcome::SessionEnded { code }) => {
                let _ = slot.reap_current();
                Ok(RelayStep::Finished(Disposition::Completed { code }))
            }
            ParentMessage::Outcome(WorkerOutcome::Failure { reason }) => {
                compositor.send(&BrokerToCompositor::Failure {
                    reason: reason.clone(),
                })?;
                let _ = slot.reap_current();
                Ok(RelayStep::Finished(Disposition::AuthFailed { reason }))
            }
            // Out of phase: the worker only ever relays a ConvPrompt
            // (Success/Failure on the BrokerToCompositor side are
            // broker→greeter only); and `Success` is the auth-only
            // worker variant `spawn_session_worker` never emits.
            ParentMessage::Conv(_) | ParentMessage::Outcome(WorkerOutcome::Success { .. }) => {
                let _ = slot.cancel_current();
                Err(BrokerError::UnexpectedFrame)
            }
        }
    }

    /// Drive one step from the GREETER side (call when the compositor
    /// channel is readable). Valid only in the greeter-await phases;
    /// a readiness otherwise is out-of-phase (fail closed).
    ///
    /// # Errors
    /// [`BrokerError`] on transport failure / out-of-phase frame /
    /// vanished worker.
    pub(crate) fn on_greeter_readable(
        &mut self,
        slot: &mut AuthSlot,
        compositor: &SeqpacketChannel,
    ) -> Result<RelayStep, BrokerError> {
        let expect_start = match self.phase {
            RelayPhase::AwaitGreeterConvResponse => false,
            RelayPhase::AwaitGreeterStartSession => true,
            RelayPhase::AwaitWorker | RelayPhase::SessionRunning => {
                let _ = slot.cancel_current();
                return Err(BrokerError::UnexpectedFrame);
            }
        };
        let frame = compositor.recv::<CompositorToBroker>()?;
        match (&frame, expect_start) {
            (CompositorToBroker::ConvResponse { .. }, false)
            | (CompositorToBroker::StartSession { .. }, true) => {
                slot.current()
                    .ok_or(BrokerError::WorkerGone)?
                    .channel()
                    .send(&frame)?;
                self.phase = RelayPhase::AwaitWorker;
                Ok(RelayStep::Continue)
            }
            (CompositorToBroker::Cancel, _) => {
                let _ = slot.cancel_current();
                Ok(RelayStep::Finished(Disposition::Cancelled))
            }
            _ => {
                let _ = slot.cancel_current();
                Err(BrokerError::UnexpectedFrame)
            }
        }
    }
}

/// Blocking driver over [`Relay`] — turn-based, used by
/// [`handle_connection`] and the relay tests. The Amendment-A2 calloop
/// loop (next task) calls the step methods directly from FD callbacks
/// and DELETES this driver + the serial binary loop (no shim then).
///
/// # Errors
///
/// [`BrokerError`] on transport failure, an out-of-phase frame, or a
/// vanished worker.
pub(crate) fn relay(
    slot: &mut AuthSlot,
    compositor: &SeqpacketChannel,
) -> Result<Disposition, BrokerError> {
    let mut r = Relay::new();
    loop {
        let step = match r.phase() {
            RelayPhase::AwaitWorker | RelayPhase::SessionRunning => {
                r.on_worker_readable(slot, compositor)?
            }
            RelayPhase::AwaitGreeterConvResponse | RelayPhase::AwaitGreeterStartSession => {
                r.on_greeter_readable(slot, compositor)?
            }
        };
        if let RelayStep::Finished(d) = step {
            return Ok(d);
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

    // ── Amendment A2: the step-driven state machine ──────────────────
    // Same scripted-worker socketpair harness (NOT a PAM mock — R12);
    // these drive the step methods directly, the shape the calloop
    // event loop will call them in.

    #[test]
    fn step_machine_conv_then_response_then_end_transitions_phases() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|w| {
                w.send(&BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "PW: ".into(),
                })
                .unwrap();
                let r: CompositorToBroker = w.recv().unwrap();
                assert!(matches!(r, CompositorToBroker::ConvResponse { .. }));
                w.send(&WorkerOutcome::SessionEnded { code: 0 }).unwrap();
            })
        })
        .unwrap();

        let (greeter, broker_end) = pair();
        let gt = thread::spawn(move || {
            let p: BrokerToCompositor = greeter.recv().unwrap();
            assert!(matches!(p, BrokerToCompositor::ConvPrompt { .. }));
            greeter
                .send(&CompositorToBroker::ConvResponse {
                    response: Secret::new("pw".into()),
                })
                .unwrap();
        });

        let mut r = Relay::new();
        assert_eq!(r.phase(), RelayPhase::AwaitWorker);
        // worker prompt → forwarded to greeter; await its response.
        assert_eq!(
            r.on_worker_readable(&mut slot, &broker_end).unwrap(),
            RelayStep::Continue
        );
        assert_eq!(r.phase(), RelayPhase::AwaitGreeterConvResponse);
        // greeter response → forwarded to worker; back to await worker.
        assert_eq!(
            r.on_greeter_readable(&mut slot, &broker_end).unwrap(),
            RelayStep::Continue
        );
        assert_eq!(r.phase(), RelayPhase::AwaitWorker);
        // worker SessionEnded → terminal, reaped.
        assert_eq!(
            r.on_worker_readable(&mut slot, &broker_end).unwrap(),
            RelayStep::Finished(Disposition::Completed { code: 0 })
        );
        assert!(slot.current().is_none(), "reaped after SessionEnded");
        gt.join().unwrap();
    }

    #[test]
    fn step_machine_greeter_cancel_at_conv_finishes_cancelled() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|w| {
                w.send(&BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "PW: ".into(),
                })
                .unwrap();
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

        let mut r = Relay::new();
        assert_eq!(
            r.on_worker_readable(&mut slot, &broker_end).unwrap(),
            RelayStep::Continue
        );
        assert_eq!(r.phase(), RelayPhase::AwaitGreeterConvResponse);
        assert_eq!(
            r.on_greeter_readable(&mut slot, &broker_end).unwrap(),
            RelayStep::Finished(Disposition::Cancelled)
        );
        assert!(
            slot.current().is_none(),
            "greeter Cancel SIGKILLed + reaped the worker"
        );
        gt.join().unwrap();
    }

    #[test]
    fn step_machine_out_of_phase_worker_frame_fails_closed() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|w| {
                // `Success` is the auth-only variant; the full
                // lifecycle worker never emits it — out of phase.
                w.send(&WorkerOutcome::Success {
                    username: "x".into(),
                    uid: 1,
                    gid: 2,
                })
                .unwrap();
            })
        })
        .unwrap();
        let (_greeter, broker_end) = pair();
        let mut r = Relay::new();
        assert!(matches!(
            r.on_worker_readable(&mut slot, &broker_end),
            Err(BrokerError::UnexpectedFrame)
        ));
        assert!(
            slot.current().is_none(),
            "out-of-phase frame fails closed: worker cancelled + reaped"
        );
    }
}
