//! The GLOBAL single auth slot (Epic #1 R5).
//!
//! At most ONE `spawn_auth_worker` is in flight system-wide. A new
//! `create` from the SO_PEERCRED-verified relay peer EVICTS the
//! in-flight attempt — SIGKILL its fork (R4 `WorkerHandle::kill`, no
//! SIGTERM) + reap it — then installs the fresh one. Two mitigations
//! are BOTH non-negotiable (Epic R5; without either, evict-old is a
//! legitimate-user-preemption DoS):
//!
//! 1. **SO_PEERCRED relay-peer gate** — only the configured relay-peer
//!    uid may drive `create` at all; any other peer is rejected and
//!    any in-flight worker is left UNTOUCHED. The relay peer is the
//!    unprivileged compositor in the live topology (it owns its own
//!    SO_PEERCRED greeter gate on the greetd socket); in the
//!    standalone/test topology it is whatever drives the broker
//!    directly. (SO_PEERCRED authenticates the peer; it never
//!    authorizes the action — identity is independently PAM-derived,
//!    Epic R8; the caller obtains `peer_uid` via `transport::peer_uid`
//!    and passes it here.)
//! 2. **Churn throttle** — a sliding window bounds create/evict
//!    cycling; over-rate is rejected with the in-flight worker
//!    UNTOUCHED.
//!
//! greetd's single `configuring` slot REJECTS a second create;
//! halmasuit deliberately EVICTS-old (retry UX) — sound only with the
//! two mitigations above.
//!
//! `#![forbid(unsafe_code)]` — composes the safe `worker`/`transport`
//! APIs; the only unsafe in the crate stays in `pam_ffi`/`worker`.
#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use nix::sys::wait::WaitStatus;

use crate::transport::SeqpacketChannel;
use crate::worker::WorkerHandle;

/// Default churn bound: max create/evict cycles per [`DEFAULT_WINDOW`].
///
/// A human retrying auth (typo, re-enter) operates at human speed — a
/// handful per ~10s — so this comfortably allows legitimate retries
/// while bounding the malicious create/cancel reconnect-churn loop
/// (the C1 attack) to an O(1) rate.
pub const DEFAULT_MAX_PER_WINDOW: usize = 5;
/// Sliding window for [`DEFAULT_MAX_PER_WINDOW`].
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(10);

/// Why a [`AuthSlot::create`] was refused. On BOTH refusals any
/// in-flight worker is left strictly UNTOUCHED (Epic R5).
#[derive(Debug)]
pub enum SlotError {
    /// The requesting peer's SO_PEERCRED uid is not the configured
    /// relay-peer uid. The action is not authorized (the peer may be
    /// authenticated as *someone*, but only the trusted relay peer —
    /// the compositor in the live topology — drives auth).
    Unauthorized,
    /// The create/evict churn bound for the window was exceeded —
    /// the reconnect-churn DoS guard (Epic R5).
    Throttled,
    /// The spawn closure or the evicted worker's reap failed
    /// (fork/socketpair/pidfd/waitpid errno).
    Worker(io::Error),
}

/// The single in-flight auth worker held by the slot.
pub struct InFlight {
    /// The worker child pid (diagnostic / test).
    pub pid: u32,
    handle: WorkerHandle,
    chan: SeqpacketChannel,
}

impl InFlight {
    /// Parent end of the worker's SEQPACKET channel — the broker
    /// relays the PAM conversation here and reads the terminal
    /// outcome.
    #[must_use]
    pub const fn channel(&self) -> &SeqpacketChannel {
        &self.chan
    }
}

/// Epic #1 R5: the GLOBAL single auth slot.
///
/// One in-flight `spawn_auth_worker` system-wide; a new create from
/// the verified relay peer (the compositor in the live topology)
/// evicts the prior one (SIGKILL + reap) under a churn bound.
pub struct AuthSlot {
    relay_peer_uid: u32,
    max_per_window: usize,
    window: Duration,
    inflight: Option<InFlight>,
    recent: VecDeque<Instant>,
}

impl AuthSlot {
    #[must_use]
    pub const fn new(relay_peer_uid: u32, max_per_window: usize, window: Duration) -> Self {
        Self {
            relay_peer_uid,
            max_per_window,
            window,
            inflight: None,
            recent: VecDeque::new(),
        }
    }

    /// Production constructor with the default churn bound.
    #[must_use]
    pub const fn with_defaults(relay_peer_uid: u32) -> Self {
        Self::new(relay_peer_uid, DEFAULT_MAX_PER_WINDOW, DEFAULT_WINDOW)
    }

    /// The in-flight worker, if any.
    #[must_use]
    pub const fn current(&self) -> Option<&InFlight> {
        self.inflight.as_ref()
    }

    /// Create a new auth worker, evicting any in-flight one.
    ///
    /// `peer_uid` is the SO_PEERCRED-attested uid of the requesting
    /// relay-peer connection (obtain via [`crate::peer_uid`]). Returns
    /// the reaped `WaitStatus` of the evicted prior worker, or `None`
    /// if the slot was empty.
    ///
    /// # Errors
    ///
    /// [`SlotError::Unauthorized`] if `peer_uid` is not the relay-peer
    /// uid; [`SlotError::Throttled`] if the churn bound is exceeded;
    /// [`SlotError::Worker`] on a spawn/reap errno. On the first two,
    /// the in-flight worker is left untouched.
    pub fn create<F>(&mut self, peer_uid: u32, spawn_fn: F) -> Result<Option<WaitStatus>, SlotError>
    where
        F: FnOnce() -> io::Result<(WorkerHandle, SeqpacketChannel)>,
    {
        self.create_at(Instant::now(), peer_uid, spawn_fn)
    }

    fn create_at<F>(
        &mut self,
        now: Instant,
        peer_uid: u32,
        spawn_fn: F,
    ) -> Result<Option<WaitStatus>, SlotError>
    where
        F: FnOnce() -> io::Result<(WorkerHandle, SeqpacketChannel)>,
    {
        // 1. SO_PEERCRED relay-peer gate — FIRST, so a non-relay-peer never
        //    perturbs the in-flight worker or the throttle state.
        if peer_uid != self.relay_peer_uid {
            return Err(SlotError::Unauthorized);
        }
        // 2. Churn throttle — prune entries older than the window,
        //    reject if it is already full. In-flight UNTOUCHED.
        while let Some(&front) = self.recent.front() {
            if now.duration_since(front) >= self.window {
                self.recent.pop_front();
            } else {
                break;
            }
        }
        if self.recent.len() >= self.max_per_window {
            return Err(SlotError::Throttled);
        }
        // 3. Evict any in-flight worker FIRST (strict single-slot —
        //    never two live): SIGKILL (R4 — no SIGTERM) + reap. A
        //    post-evict spawn failure leaves the slot empty; the
        //    greeter's new CreateSession already abandoned the old.
        let evicted = match self.inflight.take() {
            Some(prev) => {
                let _ = prev.handle.kill();
                Some(prev.handle.wait().map_err(SlotError::Worker)?)
            }
            None => None,
        };
        // 4. Spawn the fresh worker; record the churn event.
        let (handle, chan) = spawn_fn().map_err(SlotError::Worker)?;
        let pid = handle.pid;
        self.recent.push_back(now);
        self.inflight = Some(InFlight { pid, handle, chan });
        Ok(evicted)
    }

    /// Reap the in-flight worker and clear the slot.
    ///
    /// Called by the broker once it has read the worker's terminal
    /// `WorkerOutcome` over [`InFlight::channel`] (the worker `_exit`s
    /// after reporting). `None` if the slot is empty; otherwise the
    /// `waitpid` result for the worker.
    ///
    /// # Errors
    ///
    /// (Inside the `Some`) any errno from `waitpid(2)`.
    pub fn reap_current(&mut self) -> Option<io::Result<WaitStatus>> {
        self.inflight.take().map(|i| i.handle.wait())
    }

    /// Greeter-driven cancel of the in-flight worker (Epic R4/R5):
    /// SIGKILL — never SIGTERM — then reap, clearing the slot. `None`
    /// if the slot was empty; otherwise the worker's `waitpid` result.
    /// Distinct from [`Self::reap_current`], which only waits (the
    /// worker already `_exit`ed after reporting); this is for an abort
    /// mid-conversation where the worker is still blocked in libpam.
    ///
    /// # Errors
    ///
    /// (Inside the `Some`) any errno from `waitpid(2)`. The SIGKILL
    /// itself is best-effort (`ESRCH` ⇒ already gone ⇒ benign).
    pub fn cancel_current(&mut self) -> Option<io::Result<WaitStatus>> {
        self.inflight.take().map(|i| {
            let _ = i.handle.kill();
            i.handle.wait()
        })
    }

    #[cfg(test)]
    pub(crate) fn take_handle_for_test(&mut self) -> Option<WorkerHandle> {
        self.inflight.take().map(|i| i.handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::spawn_worker;
    use nix::sys::signal::Signal;
    use nix::sys::wait::WaitStatus;
    use std::time::{Duration, Instant};

    const GREETER: u32 = 1000;

    /// Non-PAM in-flight payload: a child that blocks until SIGKILLed.
    /// (Process-supervision testing is NOT a PAM mock — Epic R12.)
    fn sleeper() -> std::io::Result<(WorkerHandle, SeqpacketChannel)> {
        spawn_worker(|_chan| {
            loop {
                std::thread::park();
            }
        })
    }

    fn drain(slot: &mut AuthSlot) {
        if let Some(h) = slot.take_handle_for_test() {
            let _ = h.kill();
            let _ = h.wait();
        }
    }

    #[test]
    fn empty_slot_create_installs_and_evicts_nothing() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        let evicted = slot
            .create_at(Instant::now(), GREETER, sleeper)
            .expect("authorized create");
        assert!(evicted.is_none(), "empty slot evicts nothing");
        assert!(slot.current().is_some());
        drain(&mut slot);
    }

    #[test]
    fn authorized_create_evicts_inflight_sigkill_single_slot() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        let t0 = Instant::now();
        slot.create_at(t0, GREETER, sleeper).unwrap();
        let pid1 = slot.current().unwrap().pid;

        let evicted = slot
            .create_at(t0, GREETER, sleeper)
            .expect("authorized re-create");
        // The prior worker was SIGKILLed (R4 kill — never SIGTERM) and
        // reaped by the slot.
        match evicted {
            Some(WaitStatus::Signaled(_, Signal::SIGKILL, _)) => {}
            other => panic!("expected evicted worker SIGKILLed, got {other:?}"),
        }
        let pid2 = slot.current().unwrap().pid;
        assert_ne!(pid1, pid2, "a fresh worker replaced the evicted one");
        drain(&mut slot);
    }

    #[test]
    fn non_authorized_peer_cannot_evict_inflight_untouched() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        let t0 = Instant::now();
        slot.create_at(t0, GREETER, sleeper).unwrap();
        let pid1 = slot.current().unwrap().pid;

        let denied = slot.create_at(t0, 9999, sleeper);
        assert!(matches!(denied, Err(SlotError::Unauthorized)));
        // In-flight worker UNTOUCHED: same pid, still the live one —
        // proven by it being the worker the next authorized evict
        // SIGKILLs.
        assert_eq!(slot.current().unwrap().pid, pid1);
        let evicted = slot.create_at(t0, GREETER, sleeper).unwrap();
        assert!(
            matches!(evicted, Some(WaitStatus::Signaled(_, Signal::SIGKILL, _))),
            "the untouched in-flight worker is the one finally evicted: {evicted:?}"
        );
        drain(&mut slot);
    }

    #[test]
    fn non_authorized_peer_cannot_create_on_empty_slot() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        let denied = slot.create_at(Instant::now(), 9999, sleeper);
        assert!(matches!(denied, Err(SlotError::Unauthorized)));
        assert!(
            slot.current().is_none(),
            "no worker spawned for a non-authorized peer"
        );
    }

    #[test]
    fn churn_throttle_blocks_over_rate_inflight_untouched() {
        let window = Duration::from_secs(10);
        let mut slot = AuthSlot::new(GREETER, 2, window);
        let t0 = Instant::now();

        slot.create_at(t0, GREETER, sleeper).unwrap(); // 1
        slot.create_at(t0, GREETER, sleeper).unwrap(); // 2
        let pid2 = slot.current().unwrap().pid;

        // 3rd within the window → throttled; the 2nd worker UNTOUCHED.
        let r = slot.create_at(t0, GREETER, sleeper);
        assert!(matches!(r, Err(SlotError::Throttled)));
        assert_eq!(
            slot.current().unwrap().pid,
            pid2,
            "throttled create must not evict the in-flight worker"
        );

        // After the window slides, create succeeds again (evicting #2).
        let evicted = slot
            .create_at(t0 + window + Duration::from_millis(1), GREETER, sleeper)
            .expect("create after window slid");
        assert!(matches!(
            evicted,
            Some(WaitStatus::Signaled(_, Signal::SIGKILL, _))
        ));
        drain(&mut slot);
    }

    #[test]
    fn within_bound_repeated_creates_succeed() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        let t0 = Instant::now();
        for _ in 0..3 {
            slot.create_at(t0, GREETER, sleeper)
                .expect("within-bound create");
        }
        drain(&mut slot);
    }

    #[test]
    fn cancel_current_sigkills_reaps_and_clears_the_slot() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        slot.create_at(Instant::now(), GREETER, sleeper).unwrap();
        assert!(slot.current().is_some());
        // Greeter Cancel mid-flight (Epic R4/R5): SIGKILL — never
        // SIGTERM — then reap; slot cleared.
        let status = slot
            .cancel_current()
            .expect("a worker was in flight")
            .expect("waitpid ok");
        assert!(
            matches!(status, WaitStatus::Signaled(_, Signal::SIGKILL, _)),
            "cancel_current must SIGKILL the in-flight worker: {status:?}"
        );
        assert!(slot.current().is_none(), "slot cleared after cancel");
        // Cancelling an empty slot is None, not an error.
        assert!(slot.cancel_current().is_none());
    }

    #[test]
    fn reap_current_waits_and_clears_the_slot() {
        let mut slot = AuthSlot::new(GREETER, 5, Duration::from_secs(10));
        // A child that exits 0 promptly (the "worker finished and
        // reported" shape the broker reaps after reading the outcome).
        slot.create_at(Instant::now(), GREETER, || {
            spawn_worker(|_chan| { /* return → _exit(0) */ })
        })
        .unwrap();
        assert!(slot.current().is_some());
        let status = slot
            .reap_current()
            .expect("a worker was in flight")
            .expect("waitpid ok");
        assert!(
            matches!(status, WaitStatus::Exited(_, 0)),
            "reap_current returns the worker's wait status: {status:?}"
        );
        assert!(slot.current().is_none(), "slot cleared after reap");
        // Reaping an empty slot is None, not an error.
        assert!(slot.reap_current().is_none());
    }
}
