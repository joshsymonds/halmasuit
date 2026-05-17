//! The full one-handle PAM lifecycle (Epic R1 + Amendment A1) — the
//! §0.2 reason for being: ONE `pam_handle_t` spanning auth→session so
//! credential-passing modules (pam_mount/gnome-keyring/krb5) work.
//!
//! `run_session` keeps the SAME `pam_ffi::Pam` from `pam_start`
//! through `authenticate → acct_mgmt → get_user → setcred(ESTABLISH)
//! →` (Amendment A1.1: report `AuthOk`, then BLOCK reading the
//! `StartSession` spec from the channel) `→` (A1.2: `pam_putenv` the
//! session env into the handle) `→ open_session →` (A1.3: leader env
//! is `pam_getenvlist`, allowlist-filtered — NEVER the raw greeter
//! env) `→` (fork the non-setuid session leader, R7) `→ wait →
//! close_session → setcred(DELETE) →` (Drop) `pam_end`. The handle
//! owner NEVER `execve`s (R1); the only exec is in the forked
//! `spawn_session_leader` child.
//!
//! The greetd sequencing (spec read AFTER `setcred(ESTABLISH)` and
//! BEFORE `pam_open_session`) is derived from primary source: greetd
//! `worker.rs` reads `Args{env,cmd}` between `setcred(ESTABLISH_CRED)`
//! and `open_session` so the env reaches the handle before
//! pam_systemd/logind register the session (Epic Amendment A1).
//!
//! `#![forbid(unsafe_code)]` — composes the safe `pam_ffi`/
//! `session_leader`/`worker` APIs; the only unsafe (fork/pidfd/exec)
//! stays quarantined in `worker`.
//!
//! Real one-handle auth→session is proven by the flagship pam_mount
//! VM gate (epic headline) and the new PAM-env-survives gate —
//! `open_session`/`putenv`/`getenvlist` need host-ns root and Epic
//! R12 forbids PAM mocks, so this is NOT unit-testable here. This
//! module composes only already-tested+safe pieces; the lifecycle
//! ORDERING is asserted by construction + the VM gates.
#![forbid(unsafe_code)]

use halmasuit_session_ipc::{CompositorToBroker, SessionOutcome};
use thiserror::Error;

use crate::auth::{AuthError, run_auth_phase};
use crate::pam_ffi::{self, PamError};
use crate::responder::ChannelResponder;
use crate::session_leader::{self, SpecError};
use crate::transport::{SeqpacketChannel, TransportError};
use crate::worker::{WorkerOutcome, spawn_session_leader};

/// Failure of the full session lifecycle.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("PAM: {0}")]
    Pam(#[from] PamError),
    #[error("auth: {0}")]
    Auth(#[from] AuthError),
    #[error("session spec: {0}")]
    Spec(#[from] SpecError),
    #[error("channel: {0}")]
    Transport(#[from] TransportError),
    #[error("session leader: {0}")]
    Io(#[from] std::io::Error),
    #[error("group capture failed: {0}")]
    Groups(String),
    /// The greeter cancelled after auth, before sending a session spec
    /// (`CompositorToBroker::Cancel`). Clean abort — the handle drops
    /// (`pam_end`); no session was opened.
    #[error("session cancelled by the greeter before StartSession")]
    Aborted,
    /// Where Amendment A1.1 requires a `StartSession`, the channel
    /// produced a different `CompositorToBroker` frame. Fail closed.
    #[error("protocol: expected StartSession after AuthOk, got a different frame")]
    UnexpectedFrame,
}

/// Run the FULL one-handle lifecycle over `ch` (Epic R1 + Amendment A1).
///
/// The session program + environment are NOT parameters: they
/// arrive as a `CompositorToBroker::StartSession` frame read on `ch`
/// AFTER auth success (greetd sequencing). Identity is ALWAYS the
/// PAM-resolved one (R8), never a caller/compositor-asserted value.
///
/// Emits [`WorkerOutcome::AuthOk`] once auth + `setcred(ESTABLISH)`
/// succeed and the spec is awaited, [`WorkerOutcome::SessionOpened`]
/// once the session leader is running, and
/// [`WorkerOutcome::SessionEnded`] after teardown.
///
/// # Errors
///
/// [`SessionError`] for any step. On the error path the caller
/// (`spawn_session_worker`) reports [`WorkerOutcome::Failure`].
pub fn run_session(
    ch: &SeqpacketChannel,
    service: &str,
    username: &str,
) -> Result<(), SessionError> {
    let mut responder = ChannelResponder::new(ch);
    let mut ctx = pam_ffi::ConvCtx {
        responder: &mut responder,
    };
    // ONE handle for the whole lifecycle (R1). It is NEVER execve'd
    // (the only exec is the forked session leader); it drops LAST,
    // after close_session + setcred(DELETE), running pam_end.
    let mut pam = pam_ffi::Pam::start(service, username, &mut ctx)?;
    let id = run_auth_phase(&mut pam)?;

    // setcred(ESTABLISH) on the SAME handle, BEFORE the spec is read
    // and BEFORE open_session (greetd worker.rs ordering: setcred at
    // :145, recv Args at :157, open_session at :232).
    pam.set_cred_established()?;

    // Amendment A1.1: report auth success, then BLOCK reading the
    // session spec from the SAME channel. The broker parent forwards
    // the greeter's StartSession down here (same relay path as a
    // ConvResponse). The spec is NOT known up front.
    ch.send(&WorkerOutcome::AuthOk {
        username: id.username.clone(),
        uid: id.uid,
        gid: id.gid,
    })?;
    let cmd = match ch.recv::<CompositorToBroker>()? {
        CompositorToBroker::StartSession { cmd, env } => {
            // Amendment A1.2: push the StartSession env into the PAM
            // handle BEFORE open_session so pam_systemd/logind (and
            // pam_mount) register the session against the right env.
            for (k, v) in &env {
                pam.putenv(&format!("{k}={v}"))?;
            }
            cmd
        }
        CompositorToBroker::Cancel => return Err(SessionError::Aborted),
        // BeginAuth / ConvResponse here is a protocol violation.
        _ => return Err(SessionError::UnexpectedFrame),
    };

    pam.open_session()?;

    // Supplementary groups pam_group/pam_systemd established on THIS
    // process during setcred/open_session. merged_groups unions them
    // with getgrouplist so the leader child keeps them — a blind
    // initgroups would clobber them (R7/R11).
    let established: Vec<u32> = nix::unistd::getgroups()
        .map_err(|e| SessionError::Groups(e.to_string()))?
        .iter()
        .map(|g| g.as_raw())
        .collect();

    // Amendment A1.3: the leader's env is `pam_getenvlist` — the
    // StartSession env this process putenv'd UNION whatever
    // pam_env/pam_systemd/pam_mount added during setcred/open_session
    // — allowlist-filtered by `validate` (the mandatory R11 LD_*
    // defense). Passing the raw greeter env here would clobber the
    // module-added vars: the forbidden env analogue of a blind
    // initgroups.
    let pam_env = pam.getenvlist()?;
    let spec = session_leader::validate(&id.username, id.uid, id.gid, cmd, pam_env)?;
    let groups = session_leader::merged_groups(&id.username, id.gid, &established)?;

    ch.send(&WorkerOutcome::SessionOpened {
        username: id.username.clone(),
        uid: id.uid,
        gid: id.gid,
    })?;

    // Fork-not-exec the privilege-dropped session leader (R7). The
    // handle owner (this process) does NOT exec — it waits.
    let handle = spawn_session_leader(&spec, &groups)?;
    let status = handle.wait()?;

    // Teardown on the SAME handle, in order (R7): close_session →
    // setcred(DELETE) → (Drop) pam_end.
    pam.close_session()?;
    pam.set_cred_deleted()?;

    // Preserve the leader's crash-vs-clean distinction to the wire
    // (Amendment A5.2). `handle.wait()` blocked until termination, so
    // status is Exited/Signaled; `_` is unreachable-defensive.
    let outcome = match status {
        nix::sys::wait::WaitStatus::Exited(_, c) => SessionOutcome::Exited { code: c },
        nix::sys::wait::WaitStatus::Signaled(_, s, _) => {
            SessionOutcome::Signaled { signal: s as i32 }
        }
        _ => SessionOutcome::Exited { code: -1 },
    };
    ch.send(&WorkerOutcome::SessionEnded { outcome })?;
    Ok(())
    // `pam` drops here → pam_end. Handle closed LAST (R1).
}
