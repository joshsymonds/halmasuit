//! The per-connection broker handler (Epic R6 core; Amendment A1
//! frame routing).
//!
//! Composes the already-built pieces — `AuthSlot` (R5 global single
//! slot, evict-old, SO_PEERCRED relay-peer gate, churn throttle),
//! `spawn_session_worker` (the Amendment-A1 one-handle `run_session`
//! fork), and the `ParentMessage` relay — into ONE lifecycle per
//! accepted compositor↔broker connection. Pure composition:
//! `#![forbid(unsafe_code)]`; the only unsafe in the crate stays in
//! `pam_ffi`/`worker`.
//!
//! R9 teardown note (for R13's ARCHITECTURE.md/CLAUDE.md rewrite):
//! the Epic-R9 clause "extend the COMPOSITOR's existing single
//! SIGCHLD `waitpid(WNOHANG)` reaper with a `PamWorker` `ReapOutcome`
//! variant" is SUPERSEDED by this out-of-process broker and is N/A
//! here. The compositor neither spawns nor reaps the PAM worker; the
//! broker's `AuthSlot` owns `WorkerHandle{pid,pidfd}` and reaps it
//! synchronously (pidfd-kill + `waitpid`) at every connection-terminal
//! point — success/auth-fail (`reap_current`), greeter-cancel /
//! out-of-phase (`cancel_current`), and transport-error (the R9
//! invariant added below in `relay`/`BrokerLoop::step`). No second
//! SIGCHLD/`waitid(P_PIDFD)` reaper exists (R9 anti-pattern); reaping
//! stays slot-owned and synchronous. The superseded compositor-reaper
//! extension goes away with `halmasuit-pam` in the successor
//! (compositor→broker) epic (Amendment A3).
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

use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, RegistrationToken};
use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, SessionOutcome};
use thiserror::Error;

use crate::slot::{AuthSlot, SlotError};
use crate::transport::{SeqpacketChannel, TransportError, peer_uid};
use crate::worker::{
    BorrowedSourceFd, ParentMessage, WorkerOutcome, accept_seqpacket, own_raw_fd,
    spawn_session_worker,
};

/// Idle window (Amendment A2.2): with no connection in flight and the
/// slot empty for this long, the broker `exit(0)`s so the unit
/// deactivates — systemd's PID1-retained socket re-activates it on the
/// next connection. Interactive-scale; a real greeter reconnects in
/// well under this.
const IDLE_EXIT: Duration = Duration::from_secs(30);

/// Whether the broker should idle-exit now (Amendment A2.2). Pure so
/// the policy is unit-tested without the loop: exit ONLY when there is
/// no in-flight connection AND the idle window has elapsed. An active
/// connection never idle-exits regardless of elapsed time.
#[must_use]
pub(crate) const fn should_idle_exit(has_active_conn: bool, idle_window_elapsed: bool) -> bool {
    !has_active_conn && idle_window_elapsed
}

/// How one accepted connection's lifecycle ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// The session ran; `outcome` is the leader's crash-vs-clean
    /// `WaitStatus` (relayed to the greeter as
    /// [`BrokerToCompositor::SessionEnded`], Amendment A5.2).
    Completed { outcome: SessionOutcome },
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
    /// `AuthSlot::create` refused (non-relay-peer / churn throttle /
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
        slot: &AuthSlot,
        compositor: &SeqpacketChannel,
    ) -> Result<RelayStep, BrokerError> {
        if !matches!(
            self.phase,
            RelayPhase::AwaitWorker | RelayPhase::SessionRunning
        ) {
            // R9: the caller reaps on this terminal error AFTER tearing
            // down the calloop sources (A8.2) — the relay never reaps.
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
            // Amendment A5: forward the session-lifecycle outcome to
            // the greeter as a one-way BrokerToCompositor frame (the
            // compositor never emits these). SessionOpened is the
            // authorization key of the two-key flash-free swap; the
            // visible swap is gated compositor-side on the first
            // session-client frame (a later R3 step), not here.
            ParentMessage::Outcome(WorkerOutcome::SessionOpened { .. }) => {
                compositor.send(&BrokerToCompositor::SessionOpened)?;
                self.phase = RelayPhase::SessionRunning;
                Ok(RelayStep::Continue)
            }
            ParentMessage::Outcome(WorkerOutcome::SessionEnded { outcome }) => {
                compositor.send(&BrokerToCompositor::SessionEnded { outcome })?;
                Ok(RelayStep::Finished(Disposition::Completed { outcome }))
            }
            ParentMessage::Outcome(WorkerOutcome::Failure { reason }) => {
                compositor.send(&BrokerToCompositor::Failure {
                    reason: reason.clone(),
                })?;
                Ok(RelayStep::Finished(Disposition::AuthFailed { reason }))
            }
            // Out of phase: the worker only ever relays a ConvPrompt
            // (Success/Failure on the BrokerToCompositor side are
            // broker→greeter only); and `Success` is the auth-only
            // worker variant `spawn_session_worker` never emits.
            ParentMessage::Conv(_) | ParentMessage::Outcome(WorkerOutcome::Success { .. }) => {
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
        slot: &AuthSlot,
        compositor: &SeqpacketChannel,
    ) -> Result<RelayStep, BrokerError> {
        let expect_start = match self.phase {
            RelayPhase::AwaitGreeterConvResponse => false,
            RelayPhase::AwaitGreeterStartSession => true,
            RelayPhase::AwaitWorker | RelayPhase::SessionRunning => {
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
            (CompositorToBroker::Cancel, _) => Ok(RelayStep::Finished(Disposition::Cancelled)),
            _ => Err(BrokerError::UnexpectedFrame),
        }
    }
}

/// Reap the in-flight worker for a terminal disposition (R9: reaped
/// exactly once at every connection-terminal point, no transient
/// zombie). The relay no longer reaps in-step — the caller reaps here,
/// AFTER tearing down the calloop sources, so the source token is gone
/// before the worker fd closes (Amendment A8.2; this is what lets the
/// loop watch the worker fd via a NON-owning borrowed-fd source with
/// no `dup`). `Completed`/`AuthFailed`: the worker `_exit`s after
/// reporting → `reap_current` (waitpid). `Cancelled`: the worker is
/// still blocked (greeter cancelled mid-conversation) → `cancel_current`
/// (SIGKILL + reap). Both idempotent (a no-op if the slot is empty).
fn reap_for_disposition(slot: &mut AuthSlot, disp: &Disposition) {
    match disp {
        Disposition::Completed { .. } | Disposition::AuthFailed { .. } => {
            let _ = slot.reap_current();
        }
        Disposition::Cancelled => {
            let _ = slot.cancel_current();
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
                r.on_worker_readable(slot, compositor)
            }
            RelayPhase::AwaitGreeterConvResponse | RelayPhase::AwaitGreeterStartSession => {
                r.on_greeter_readable(slot, compositor)
            }
        };
        match step {
            Ok(RelayStep::Continue) => {}
            Ok(RelayStep::Finished(d)) => {
                // R9: reap exactly once at the terminal point. (No
                // calloop sources here — this blocking driver is used
                // by handle_connection + the relay tests, not the loop;
                // A8.2 ordering is the loop's concern, in `step`.)
                reap_for_disposition(slot, &d);
                return Ok(d);
            }
            Err(e) => {
                // R9: the connection ended for ANY reason (transport
                // failure / out-of-phase / vanished worker) ⇒ SIGKILL +
                // reap exactly once before returning — no transient
                // zombie. The relay no longer reaps in-step, so this is
                // the sole reaper for every error path; idempotent if
                // the worker is already gone.
                let _ = slot.cancel_current();
                return Err(e);
            }
        }
    }
}

/// Handle ONE accepted compositor↔broker connection end to end.
///
/// SO_PEERCRED-gated (via [`AuthSlot::create`]'s relay-peer gate),
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

/// The in-flight connection's per-conn state (Amendment A2): the
/// broker OWNS the greeter channel (the relay's send/recv go through
/// it); the two calloop sources are level-triggered readability
/// wakeups over NON-owning [`BorrowedSourceFd`]s (A8.3 — no dup of a
/// privilege-crossing fd) — the real I/O is on the owned channels.
/// A8.2: `drop_active` removes both source tokens (epoll del) BEFORE
/// the slot reap closes the owning fd, so the borrow never dangles.
struct Active {
    greeter: SeqpacketChannel,
    relay: Relay,
    greeter_token: RegistrationToken,
    worker_token: RegistrationToken,
}

/// Broker event-loop state (Amendment A2.1): ONE loop multiplexing the
/// systemd listener, the in-flight worker, the greeter, and an
/// idle-exit timer. `slot` is the R5 global single slot; a new
/// connection's `AuthSlot::create` evicts any in-flight worker
/// (A2.3) — reachable ONLY because the listener stays armed while a
/// connection is in flight (the serial loop could not do this).
struct BrokerLoop {
    slot: AuthSlot,
    listener_fd: RawFd,
    active: Option<Active>,
    /// `Some(t)` since the slot went idle; `None` while a connection
    /// is in flight. Drives the A2.2 idle-exit.
    idle_since: Option<Instant>,
    loop_handle: calloop::LoopHandle<'static, Self>,
    loop_signal: LoopSignal,
    running: bool,
}

impl BrokerLoop {
    /// Tear down the in-flight connection's calloop sources (epoll
    /// del) and clear `active`. A8.2: the source tokens are removed
    /// HERE, strictly before the worker fd is closed — `step()` calls
    /// this immediately before the slot reap (`reap_for_disposition`/
    /// `cancel_current`), and `admit_one` calls it before the
    /// `AuthSlot::create` evict. The worker itself is SIGKILLed+reaped
    /// by the slot, never here. Arms the idle timer.
    fn drop_active(&mut self) {
        if let Some(a) = self.active.take() {
            self.loop_handle.remove(a.greeter_token);
            self.loop_handle.remove(a.worker_token);
        }
        self.idle_since = Some(Instant::now());
    }

    /// Drive one relay step for whichever side fired; on a terminal
    /// step tear the connection down.
    fn step(&mut self, worker_side: bool) {
        let res = {
            let Self { slot, active, .. } = self;
            let Some(a) = active.as_mut() else { return };
            if worker_side {
                a.relay.on_worker_readable(slot, &a.greeter)
            } else {
                a.relay.on_greeter_readable(slot, &a.greeter)
            }
        };
        match res {
            Ok(RelayStep::Continue) => {}
            Ok(RelayStep::Finished(disp)) => {
                tracing_log(&format!("connection ended: {disp:?}"));
                // A8.2 ORDER: deregister the calloop sources (epoll
                // del) BEFORE the slot reap closes the worker fd the
                // `BorrowedSourceFd` aliases — token gone before the
                // owning fd drops, so the borrow never dangles.
                self.drop_active();
                reap_for_disposition(&mut self.slot, &disp);
            }
            Err(e) => {
                tracing_log(&format!("connection error: {e}"));
                // R9 invariant: connection ended for ANY reason ⇒ slot
                // worker reaped exactly once. The relay no longer reaps
                // in-step (A8.6); this is the SOLE reaper for every
                // transport-error path. A8.2 order: sources gone
                // (epoll del) before the worker fd close.
                // `cancel_current` is idempotent (`take()` → `None`).
                self.drop_active();
                let _ = self.slot.cancel_current();
            }
        }
    }
}

// Stderr is the unit's journal (Type=notify service). No tracing dep
// in this crate; a thin eprintln keeps the broker dependency-light.
fn tracing_log(msg: &str) {
    eprintln!("halmasuit-session: {msg}");
}

/// Accept and admit ONE pending connection on `listener_fd`.
///
/// `listener_fd` is already O_NONBLOCK. `Ok(true)` = a connection was
/// admitted (or cleanly rejected); `Ok(false)` = nothing pending
/// (`EWOULDBLOCK`).
///
/// Admitting calls [`AuthSlot::create`] which EVICTS any in-flight
/// worker (Amendment A2.3); the prior connection's loop sources are
/// torn down first so they cannot fire against the evicted worker.
fn admit_one(bl: &mut BrokerLoop) -> io::Result<bool> {
    let greeter = match accept_seqpacket(bl.listener_fd) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
        Err(e) => return Err(e),
    };
    let puid = match peer_uid(&greeter) {
        Ok(u) => u,
        Err(e) => {
            tracing_log(&format!("peer_uid failed; dropping connection: {e}"));
            return Ok(true);
        }
    };
    let begin = match greeter.recv::<CompositorToBroker>() {
        Ok(CompositorToBroker::BeginAuth { service, username }) => (service, username),
        Ok(_) => {
            tracing_log("first frame was not BeginAuth; dropping connection");
            return Ok(true);
        }
        Err(e) => {
            tracing_log(&format!("reading BeginAuth failed: {e}"));
            return Ok(true);
        }
    };
    // A new connection supersedes any in-flight one. Drop the old
    // loop sources BEFORE create() so a stale readiness can't fire
    // against the worker create() is about to SIGKILL.
    bl.drop_active();
    let (service, username) = begin;
    if let Err(e) = bl
        .slot
        .create(puid, || spawn_session_worker(&service, &username))
    {
        tracing_log(&format!("auth slot refused: {e:?}"));
        return Ok(true);
    }
    // A8.3: hand calloop NON-owning borrowed-fd sources — the broker
    // never dups a privilege-crossing fd. The slot's worker channel
    // and the broker-owned greeter channel remain the sole owners;
    // `drop_active` removes these source tokens before either fd is
    // closed (A8.2), so the raw fds the sources alias never dangle.
    let Some(worker_raw) = bl.slot.current().map(|i| i.channel().as_fd().as_raw_fd()) else {
        tracing_log("worker vanished immediately after create");
        let _ = bl.slot.cancel_current();
        return Ok(true);
    };
    let worker_src = BorrowedSourceFd::new(worker_raw);
    let greeter_src = BorrowedSourceFd::new(greeter.as_fd().as_raw_fd());
    let worker_token = bl
        .loop_handle
        .insert_source(
            Generic::new(worker_src, Interest::READ, Mode::Level),
            |_, _, bl: &mut BrokerLoop| {
                bl.step(true);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| io::Error::other(format!("insert worker source: {e}")))?;
    let greeter_token = bl
        .loop_handle
        .insert_source(
            Generic::new(greeter_src, Interest::READ, Mode::Level),
            |_, _, bl: &mut BrokerLoop| {
                bl.step(false);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| {
            bl.loop_handle.remove(worker_token);
            io::Error::other(format!("insert greeter source: {e}"))
        })?;
    bl.idle_since = None;
    bl.active = Some(Active {
        greeter,
        relay: Relay::new(),
        greeter_token,
        worker_token,
    });
    Ok(true)
}

/// Run the Epic-R6 / Amendment-A2 broker event loop.
///
/// `listener_fd` is the validated systemd activation socket;
/// `relay_peer_uid` is the SO_PEERCRED-authorized peer. Returns when
/// SIGTERM/SIGINT or the idle-exit fires — the process then exits 0
/// and the unit deactivates (no standing root; PID1's retained socket
/// re-activates on the next connection).
///
/// # Errors
///
/// [`io::Error`] on event-loop construction / source registration /
/// a fatal `accept` errno.
pub fn run_broker(listener_fd: RawFd, relay_peer_uid: u32) -> io::Result<()> {
    let mut event_loop: EventLoop<'static, BrokerLoop> =
        EventLoop::try_new().map_err(io::Error::other)?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Adopt the systemd listener fd (the only fd adoption — quarantined
    // in `worker`), then make it non-blocking via its safe `AsFd` so
    // the listener callback and the idle race-drain never block in
    // accept(2). No unsafe here.
    let listener_src = own_raw_fd(listener_fd);
    nix::fcntl::fcntl(
        listener_src.as_fd(),
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .map_err(io::Error::from)?;
    loop_handle
        .insert_source(
            Generic::new(listener_src, Interest::READ, Mode::Level),
            |_, _, bl: &mut BrokerLoop| {
                // Drain all pending connections; each admit may evict
                // the prior (Amendment A2.3).
                loop {
                    match admit_one(bl) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(e) => {
                            tracing_log(&format!("accept failed: {e}"));
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| io::Error::other(format!("insert listener source: {e}")))?;

    loop_handle
        .insert_source(
            calloop::signals::Signals::new(&[
                calloop::signals::Signal::SIGTERM,
                calloop::signals::Signal::SIGINT,
            ])
            .map_err(io::Error::other)?,
            |_, &mut (), bl: &mut BrokerLoop| {
                bl.running = false;
                bl.loop_signal.stop();
            },
        )
        .map_err(|e| io::Error::other(format!("insert signal source: {e}")))?;

    let mut bl = BrokerLoop {
        slot: AuthSlot::with_defaults(relay_peer_uid),
        listener_fd,
        active: None,
        idle_since: Some(Instant::now()),
        loop_handle,
        loop_signal,
        running: true,
    };

    while bl.running {
        event_loop
            .dispatch(Some(Duration::from_secs(1)), &mut bl)
            .map_err(io::Error::other)?;
        let elapsed = bl.idle_since.is_some_and(|t| t.elapsed() >= IDLE_EXIT);
        if should_idle_exit(bl.active.is_some(), elapsed) {
            // Race-drain (Amendment A2.2): one last non-blocking
            // accept; if a connection slipped in, service it and stay.
            match admit_one(&mut bl) {
                Ok(true) => {}
                Ok(false) => {
                    tracing_log("idle window elapsed; exiting (unit deactivates)");
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::AuthSlot;
    use crate::transport::SeqpacketChannel;
    use crate::worker::{WorkerOutcome, spawn_worker};
    use halmasuit_session_ipc::{
        BrokerToCompositor, CompositorToBroker, PromptStyle, Secret, SessionOutcome,
    };
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
                w.send(&WorkerOutcome::SessionEnded {
                    outcome: SessionOutcome::Exited { code: 0 },
                })
                .unwrap();
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
            // Amendment A5: the broker forwards the lifecycle frames
            // one-way; the greeter (compositor) consumes them.
            assert_eq!(
                greeter.recv::<BrokerToCompositor>().unwrap(),
                BrokerToCompositor::SessionOpened
            );
            assert_eq!(
                greeter.recv::<BrokerToCompositor>().unwrap(),
                BrokerToCompositor::SessionEnded {
                    outcome: SessionOutcome::Exited { code: 0 },
                }
            );
        });

        let disp = relay(&mut slot, &broker_end).expect("relay ok");
        assert_eq!(
            disp,
            Disposition::Completed {
                outcome: SessionOutcome::Exited { code: 0 }
            }
        );
        assert!(slot.current().is_none(), "slot reaped after SessionEnded");
        gt.join().unwrap();
    }

    /// Amendment A5: the broker FORWARDS the session-lifecycle outcomes
    /// to the greeter as one-way `BrokerToCompositor::SessionOpened` /
    /// `SessionEnded{outcome}` frames (crash-vs-clean preserved). The
    /// compositor is a pure sink — it never sends a lifecycle frame.
    /// Scripted-worker socketpair (NOT a PAM mock — Epic R12).
    #[test]
    fn relay_emits_session_lifecycle_frames_to_greeter() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|w| {
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
                // Signalled exit → crash-vs-clean must survive to the
                // wire frame (GDM SESSION_DIED; not collapsed).
                w.send(&WorkerOutcome::SessionEnded {
                    outcome: SessionOutcome::Signaled { signal: 9 },
                })
                .unwrap();
            })
        })
        .unwrap();

        let (greeter, broker_end) = pair();
        let gt = thread::spawn(move || {
            let s: BrokerToCompositor = greeter.recv().unwrap();
            assert_eq!(
                s,
                BrokerToCompositor::Success {
                    username: "alice".into(),
                    uid: 1000,
                    gid: 1000,
                }
            );
            greeter
                .send(&CompositorToBroker::StartSession {
                    cmd: vec!["/bin/sh".into()],
                    env: vec![],
                })
                .unwrap();
            assert_eq!(
                greeter.recv::<BrokerToCompositor>().unwrap(),
                BrokerToCompositor::SessionOpened
            );
            assert_eq!(
                greeter.recv::<BrokerToCompositor>().unwrap(),
                BrokerToCompositor::SessionEnded {
                    outcome: SessionOutcome::Signaled { signal: 9 },
                }
            );
        });

        let disp = relay(&mut slot, &broker_end).expect("relay ok");
        assert_eq!(
            disp,
            Disposition::Completed {
                outcome: SessionOutcome::Signaled { signal: 9 }
            }
        );
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
    fn relay_worker_dies_mid_relay_is_reaped_no_zombie() {
        // R9 closure: a worker that exits abruptly mid-relay (drops
        // its channel) makes the broker's next recv error. The
        // connection ends for a transport reason — the slot worker
        // MUST still be reaped exactly once, with no transient zombie
        // lingering until the next create-evict or the ≤30s
        // idle-exit. Scripted-worker socketpair, NOT a PAM mock
        // (Epic R12 — process-supervision testing is not PAM mocking).
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create(GREETER, || {
            spawn_worker(|_w| {
                // Return immediately → _exit(0), dropping the worker
                // channel end. The broker's first recv sees the peer
                // closed (TransportError::Closed) — the worker-died-
                // mid-relay path.
            })
        })
        .unwrap();
        assert!(slot.current().is_some());

        let (_greeter, broker_end) = pair();
        let r = relay(&mut slot, &broker_end);
        assert!(
            matches!(r, Err(BrokerError::Transport(_))),
            "worker death mid-relay surfaces as a transport error: {r:?}"
        );
        assert!(
            slot.current().is_none(),
            "R9: the dead worker was reaped on the transport-error \
             path (no transient zombie)"
        );
    }

    #[test]
    fn handle_connection_rejects_non_authorized_peer() {
        // socketpair peers are the test process's own uid; configure a
        // DIFFERENT relay-peer uid so the SO_PEERCRED gate (in
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
            "non-authorized peer refused: {r:?}"
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
                w.send(&WorkerOutcome::SessionEnded {
                    outcome: SessionOutcome::Exited { code: 0 },
                })
                .unwrap();
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
            // Amendment A5: drain the one-way SessionEnded frame so
            // the broker's send completes (and validate it).
            assert_eq!(
                greeter.recv::<BrokerToCompositor>().unwrap(),
                BrokerToCompositor::SessionEnded {
                    outcome: SessionOutcome::Exited { code: 0 },
                }
            );
        });

        let mut r = Relay::new();
        assert_eq!(r.phase(), RelayPhase::AwaitWorker);
        // worker prompt → forwarded to greeter; await its response.
        assert_eq!(
            r.on_worker_readable(&slot, &broker_end).unwrap(),
            RelayStep::Continue
        );
        assert_eq!(r.phase(), RelayPhase::AwaitGreeterConvResponse);
        // greeter response → forwarded to worker; back to await worker.
        assert_eq!(
            r.on_greeter_readable(&slot, &broker_end).unwrap(),
            RelayStep::Continue
        );
        assert_eq!(r.phase(), RelayPhase::AwaitWorker);
        // worker SessionEnded → terminal. A8.6 caller-reaps contract:
        // the relay step does NOT reap; the loop's `step()` reaps via
        // `reap_for_disposition` AFTER tearing the sources down (A8.2).
        let fin = r.on_worker_readable(&slot, &broker_end).unwrap();
        assert_eq!(
            fin,
            RelayStep::Finished(Disposition::Completed {
                outcome: SessionOutcome::Exited { code: 0 }
            })
        );
        assert!(
            slot.current().is_some(),
            "relay step must NOT reap in-step (A8.6)"
        );
        let RelayStep::Finished(disp) = fin else {
            unreachable!()
        };
        reap_for_disposition(&mut slot, &disp);
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
            r.on_worker_readable(&slot, &broker_end).unwrap(),
            RelayStep::Continue
        );
        assert_eq!(r.phase(), RelayPhase::AwaitGreeterConvResponse);
        let fin = r.on_greeter_readable(&slot, &broker_end).unwrap();
        assert_eq!(fin, RelayStep::Finished(Disposition::Cancelled));
        // A8.6: relay step does not reap; caller reaps post-teardown.
        let RelayStep::Finished(disp) = fin else {
            unreachable!()
        };
        reap_for_disposition(&mut slot, &disp);
        assert!(
            slot.current().is_none(),
            "greeter Cancel SIGKILLed + reaped the worker"
        );
        gt.join().unwrap();
    }

    #[test]
    fn idle_exit_only_when_no_active_conn_and_window_elapsed() {
        // Amendment A2.2: the broker exits (→ unit deactivates, no
        // standing root) ONLY when there is no in-flight connection
        // AND the idle window has elapsed. An active connection (auth
        // or session in flight) NEVER triggers idle-exit regardless of
        // elapsed time.
        assert!(should_idle_exit(false, true), "idle + elapsed → exit");
        assert!(
            !should_idle_exit(true, true),
            "active conn must never idle-exit"
        );
        assert!(
            !should_idle_exit(false, false),
            "idle but window not elapsed → stay"
        );
        assert!(
            !should_idle_exit(true, false),
            "active + not elapsed → stay"
        );
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
            r.on_worker_readable(&slot, &broker_end),
            Err(BrokerError::UnexpectedFrame)
        ));
        // A8.6: relay step no longer reaps on error; the caller
        // (`relay()`/loop `step()`) is the sole error-path reaper.
        let _ = slot.cancel_current();
        assert!(
            slot.current().is_none(),
            "out-of-phase frame fails closed: worker cancelled + reaped"
        );
    }
}
