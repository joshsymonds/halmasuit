//! The full one-handle PAM lifecycle (Epic R1) — the §0.2 reason
//! for being: ONE `pam_handle_t` spanning auth→session so credential-
//! passing modules (pam_mount/gnome-keyring/krb5) work.
//!
//! `run_session` keeps the SAME `pam_ffi::Pam` from `pam_start`
//! through `authenticate → acct_mgmt → get_user → setcred(ESTABLISH)
//! → open_session →` (fork the non-setuid session leader, R7) `→
//! wait → close_session → setcred(DELETE) →` (Drop) `pam_end`. The
//! handle owner NEVER `execve`s (R1); the only exec is in the forked
//! `spawn_session_leader` child.
//!
//! `#![forbid(unsafe_code)]` — composes the safe `pam_ffi`/
//! `session_leader`/`worker` APIs; the only unsafe (fork/pidfd/exec)
//! stays quarantined in `worker`.
//!
//! Real one-handle auth→session is proven by the flagship pam_mount
//! VM gate (next task / epic headline) — `open_session` needs host-ns
//! root and Epic R12 forbids PAM mocks, so it is NOT unit-testable
//! here. This module composes only already-tested+safe pieces; the
//! lifecycle ORDERING is asserted by construction + the VM gate.
#![forbid(unsafe_code)]

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
}

/// Run the FULL one-handle lifecycle over `ch` (Epic R1).
///
/// `cmd`/`env`
/// are the session program + environment (the greeter's StartSession;
/// the wire frame that delivers them over `ch` lands with the R6
/// broker accept loop — here they are parameters so the lifecycle is
/// the focus). Identity is ALWAYS the PAM-resolved one (R8), never a
/// caller/compositor-asserted value.
///
/// Emits [`WorkerOutcome::SessionOpened`] once the session leader is
/// running and [`WorkerOutcome::SessionEnded`] after teardown.
///
/// # Errors
///
/// [`SessionError`] for any step. On the error path the caller
/// (`spawn_session_worker`) reports [`WorkerOutcome::Failure`].
pub fn run_session(
    ch: &SeqpacketChannel,
    service: &str,
    username: &str,
    cmd: Vec<String>,
    env: Vec<(String, String)>,
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

    // Session phase, SAME handle (R1). setcred(ESTABLISH) before
    // open_session (greetd-canonical ordering).
    pam.set_cred_established()?;
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

    let spec = session_leader::validate(&id.username, id.uid, id.gid, cmd, env)?;
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

    let code = match status {
        nix::sys::wait::WaitStatus::Exited(_, c) => c,
        nix::sys::wait::WaitStatus::Signaled(_, s, _) => 128 + s as i32,
        _ => -1,
    };
    ch.send(&WorkerOutcome::SessionEnded { code })?;
    Ok(())
    // `pam` drops here → pam_end. Handle closed LAST (R1).
}
