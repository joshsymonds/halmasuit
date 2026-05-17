//! The in-process PAM auth transaction composition.
//!
//! `run_pam_auth` wires `ChannelResponder` → `ConvCtx` → `pam_ffi::Pam`
//! and runs `pam_start → pam_authenticate → pam_acct_mgmt →
//! pam_get_user`, then resolves uid/gid from the pwent of the
//! PAM-resolved name (Epic R8: identity is one atomic unit from the
//! resolved name, never the pre-auth client string). No fork here
//! (Epic R4, later). `#![forbid(unsafe_code)]` — composes pam_ffi's
//! safe API only.
//!
//! The real `pam_start → authenticate` path is NOT unit-testable
//! (Epic R12 / CLAUDE.md forbid mocking PAM); its first real assertion
//! is the real-PAM VM gate, task #8. This module unit-tests only the
//! PAM-free parts (pwent resolution + error mapping).
#![forbid(unsafe_code)]

use thiserror::Error;

use crate::pam_ffi::{self, ConvCtx, PamError};
use crate::responder::ChannelResponder;
use crate::transport::SeqpacketChannel;

/// PAM's canonical post-stack identity.
///
/// Resolved as ONE unit from `pam_get_user` → pwent (Epic R8):
/// `uid`/`gid` come from the pwent of `username`; the pre-auth client
/// string is never substituted at any field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
}

/// Failure of the auth transaction.
#[derive(Debug, Error)]
pub enum AuthError {
    /// A libpam step failed (`pam_start`/`authenticate`/`acct_mgmt`/
    /// `get_user`).
    #[error("PAM step failed: {0}")]
    Pam(#[from] PamError),
    /// Post-auth pwent lookup of the PAM-resolved name failed (absent
    /// or NSS error). The message is intentionally identical for both
    /// causes — distinguishing them would leak whether the name exists
    /// in the local passwd db vs only in the PAM stack (account
    /// enumeration). Mirrors halmasuit-pam's posture.
    #[error("post-auth account lookup failed")]
    AccountLookup,
}

/// Resolve a PAM-canonical name to its pwent identity. Factored out so
/// it is unit-testable without real PAM.
///
/// # Errors
///
/// [`AuthError::AccountLookup`] if the name has no pwent or NSS errors
/// (same message either way — no account enumeration).
fn resolve_identity(resolved_name: &str) -> Result<ResolvedIdentity, AuthError> {
    let pw = nix::unistd::User::from_name(resolved_name)
        .map_err(|_| AuthError::AccountLookup)?
        .ok_or(AuthError::AccountLookup)?;
    Ok(ResolvedIdentity {
        username: resolved_name.to_owned(),
        uid: pw.uid.as_raw(),
        gid: pw.gid.as_raw(),
    })
}

/// Run one PAM auth transaction over `ch`, returning PAM's resolved
/// identity (Epic R8). The conversation is relayed to the
/// compositor/greeter via [`ChannelResponder`].
///
/// The real libpam path is asserted by the real-PAM VM gate (task #8);
/// it cannot be unit-tested (Epic R12 forbids PAM mocks).
///
/// # Errors
///
/// [`AuthError::Pam`] for any libpam-step failure (wrong password is
/// `pam_authenticate` failure); [`AuthError::AccountLookup`] if the
/// resolved name has no pwent.
pub fn run_pam_auth(
    ch: &SeqpacketChannel,
    service: &str,
    username: &str,
) -> Result<ResolvedIdentity, AuthError> {
    let mut responder = ChannelResponder::new(ch);
    let mut ctx = ConvCtx {
        responder: &mut responder,
    };
    let mut pam = pam_ffi::Pam::start(service, username, &mut ctx)?;
    pam.authenticate()?;
    pam.acct_mgmt()?;
    let resolved = pam.get_user()?;
    resolve_identity(&resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_an_existing_account_to_its_pwent_identity() {
        // root is universally present; its uid/gid are 0/0 on Linux.
        let id = resolve_identity("root").expect("root resolves");
        assert_eq!(id.username, "root");
        assert_eq!(id.uid, 0);
        assert_eq!(id.gid, 0);

        // Identity is one unit FROM the resolved name: cross-check the
        // ids against an independent pwent lookup of that same name.
        let pw = nix::unistd::User::from_name("root").unwrap().unwrap();
        assert_eq!(id.uid, pw.uid.as_raw());
        assert_eq!(id.gid, pw.gid.as_raw());
    }

    #[test]
    fn absent_account_is_account_lookup_error_without_enumeration() {
        let err =
            resolve_identity("halmasuit_no_such_user_x9q7").expect_err("absent account must fail");
        assert!(matches!(err, AuthError::AccountLookup));
        // No-enumeration: the message must not reveal whether the name
        // exists / why the lookup failed.
        assert_eq!(err.to_string(), "post-auth account lookup failed");
    }

    #[test]
    fn pam_error_maps_into_auth_error() {
        // PamError → AuthError::Pam via #[from]; Display does not leak
        // anything secret (just the status code).
        let e: AuthError = PamError::Authenticate(7).into();
        assert!(matches!(e, AuthError::Pam(PamError::Authenticate(7))));
        let shown = e.to_string();
        assert!(shown.contains("PAM step failed"));
        assert!(!shown.to_lowercase().contains("password"));
    }
}
