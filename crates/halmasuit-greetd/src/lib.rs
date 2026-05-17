//! greetd wire-protocol state machine.
//!
//! This crate is the daemon-side state machine for the greetd protocol
//! (<https://man.sr.ht/~kennylevinsen/greetd/protocol.md>) plus the
//! Rust types that model its wire format.
//!
//! # Why we own the wire types
//!
//! The upstream [`greetd_ipc`] crate is GPL-3.0-only. Linking it would
//! make every halmasuit binary GPL-3.0; halmasuit is dual MIT-OR-Apache,
//! matching the Rust-Wayland infrastructure tier (smithay, wlroots,
//! Weston) it sits in. To preserve permissive licensing without giving
//! up wire-protocol compatibility, the type definitions below are a
//! clean-room reimplementation derived from the public protocol spec
//! (the URL above), not from `greetd_ipc`'s source. The types look
//! similar because the wire format dictates the field names and the
//! serde idiom for tagged JSON unions is mechanically determined —
//! there is essentially one way to write this correctly. The
//! `wire_format_roundtrip` test pins the JSON shape against canonical
//! payloads from the spec to catch any future drift.
//!
//! # State-machine invariant
//!
//! > `StartSession` is only valid after a `CreateSession` whose PAM
//! > round completed with `PamStep::Success`. This is ARCHITECTURE.md
//! > threat model row 2.
//!
//! # Fully sans-IO (Amendment A7)
//!
//! This crate performs NO PAM call and NO socket I/O. It is the pure
//! protocol brain. When a PAM round is required [`SessionState`] EMITS
//! [`Action::Pam`] and SUSPENDS; the compositor episode loop — which
//! owns both the greeter fd and the privileged broker fd as calloop
//! sources — runs exactly one broker round-trip and RESUMES the state
//! machine by feeding the resulting [`PamStep`] back via
//! [`SessionState::on_pam_result`] / [`server::Connection::resume_pam`].
//! libpam links in exactly one crate (`halmasuit-session`, the
//! privileged broker); `halmasuit-greetd` links none and never blocks.
//! This is the canonical sans-IO shape (h11 `NEED_DATA` /
//! rustls `process_new_packets`); a synchronous `step()` that hides a
//! send-then-blocking-recv is the named anti-pattern. Everything
//! observable from outside is covered by unit + property tests.

#![forbid(unsafe_code)]

pub mod server;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Wire types ──────────────────────────────────────────────────────────
//
// JSON shape: outer object tagged by `type` (snake_case discriminant),
// field names in snake_case. Mirrors the spec at
// https://man.sr.ht/~kennylevinsen/greetd/protocol.md.

/// A greetd protocol request, sent greeter → daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Begin a login attempt for `username`. The daemon responds with
    /// `AuthMessage` (challenge), `Success` (no challenge needed), or
    /// `Error` (PAM rejected before any conversation).
    CreateSession { username: String },
    /// Answer the most recent `AuthMessage` challenge. `response` is
    /// `None` for `Info`/`Error` challenge kinds where the spec
    /// doesn't require a reply.
    PostAuthMessageResponse { response: Option<String> },
    /// Start the user's session after PAM completed. `cmd` is the
    /// argv to exec; `env` is `KEY=VALUE` strings to set.
    StartSession { cmd: Vec<String>, env: Vec<String> },
    /// Abort the in-flight session. Valid in any non-Idle state.
    CancelSession,
}

/// A greetd protocol response, sent daemon → greeter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// The request was accepted.
    Success,
    /// The request failed. `error_type` distinguishes generic protocol
    /// errors from authentication-specific failures.
    Error {
        error_type: ErrorType,
        description: String,
    },
    /// PAM is asking the greeter to display a prompt and (usually)
    /// collect a response.
    AuthMessage {
        auth_message_type: AuthMessageType,
        auth_message: String,
    },
}

/// Classifies an `Error` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// Generic protocol error (malformed request, invalid state, etc.).
    Error,
    /// PAM authentication failed.
    AuthError,
}

/// Classifies an `AuthMessage` so the greeter can pick the right UI
/// (echoing input box, hidden input box, info banner, error banner).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    /// Plain prompt; greeter should echo the user's typing.
    Visible,
    /// Password-style prompt; greeter must NOT echo.
    Secret,
    /// Informational message; greeter displays but does not collect.
    Info,
    /// Error message; greeter displays prominently but does not collect.
    Error,
}

// ── State machine ───────────────────────────────────────────────────────

/// Compositor-side state of an in-flight greetd session.
///
/// Transitions are driven by [`SessionState::on_request`] (a greeter
/// `Request`) and [`SessionState::on_pam_result`] (the broker's
/// [`PamStep`], fed back after a suspended PAM round). The PAM-success
/// invariant is enforced by match arms: `StartSession` is only accepted
/// in [`SessionState::AuthSuccess`], which is reachable only via an
/// `on_pam_result` that returned [`PamStep::Success`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionState {
    /// No session in flight. Ready to accept `CreateSession`.
    #[default]
    Idle,
    /// A PAM round has been emitted; the conversation is SUSPENDED
    /// awaiting the broker's [`PamStep`] via [`Self::on_pam_result`].
    /// Holds the client-supplied `CreateSession` name purely as the
    /// in-flight label for a subsequent `Challenge` echo — NEVER the
    /// spawn identity (that is PAM's resolved name from `Success`).
    AuthPending { username: String },
    /// PAM issued a challenge; the greeter must answer via
    /// `PostAuthMessageResponse`.
    Authenticating { username: String },
    /// PAM completed successfully. `StartSession` is now valid. The
    /// `username`, uid, and gid all come from PAM's post-auth canonical
    /// name (`pam_get_user` → `pwent` lookup), NOT the pre-auth
    /// client-supplied `CreateSession` string. All three are handed to
    /// the broker together so its `initgroups(3)` resolves the same
    /// identity the uid/gid came from.
    AuthSuccess {
        username: String,
        uid: u32,
        gid: u32,
    },
    /// `StartSession` received; the I/O layer forwards the
    /// [`server::SpawnRequest`] to the broker. From the state machine's
    /// perspective, no more wire requests are accepted in this state —
    /// the session ends when the spawned child exits.
    Spawning {
        username: String,
        uid: u32,
        gid: u32,
        cmd: Vec<String>,
        env: Vec<String>,
    },
}

/// One round of the PAM conversation, fed back into the suspended state
/// machine by the I/O layer after it completed a broker round-trip.
///
/// `halmasuit-greetd` never produces a `PamStep` itself — it is the
/// resume payload. The compositor's `BrokerRelay` (Amendment A6/A7)
/// translates broker wire frames into `PamStep` and back; greetd stays
/// ignorant of the broker wire format and of libpam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PamStep {
    /// PAM wants the user to answer a challenge. The state machine
    /// translates this into a `Response::AuthMessage` for the greeter
    /// and parks in [`SessionState::Authenticating`].
    Challenge {
        kind: AuthMessageType,
        prompt: String,
    },
    /// PAM succeeded. The state machine transitions to
    /// [`SessionState::AuthSuccess`] with the resolved username/uid/gid.
    ///
    /// `username` is PAM's canonical name (`pam_get_user` after the
    /// stack ran), NOT the client-supplied `CreateSession` string. It
    /// is the name the uid/gid were resolved from, so it is also the
    /// name `initgroups(3)` must use in the broker — carrying the
    /// pre-auth client string there instead would let a
    /// username-rewriting PAM stack pair one user's uid with another
    /// user's supplementary groups.
    Success {
        username: String,
        uid: u32,
        gid: u32,
    },
    /// PAM failed (bad password, locked account, etc.). The state
    /// machine returns to [`SessionState::Idle`] after sending the
    /// error to the greeter.
    Failure { reason: String },
}

/// What the I/O layer must do in response to a greeter `Request`.
///
/// `halmasuit-greetd` is fully sans-IO (Amendment A7): it performs no
/// PAM call and no socket I/O. When a PAM round is required it EMITS
/// [`Action::Pam`] and SUSPENDS; the compositor episode loop (which
/// owns both the greeter fd and the privileged broker fd) runs exactly
/// one broker round-trip and feeds the resulting [`PamStep`] back via
/// [`SessionState::on_pam_result`] / [`server::Connection::resume_pam`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this `Response` to the greeter. Non-terminal.
    Reply(Response),
    /// SUSPEND. Drive exactly one PAM round with `response` (`None` =
    /// the initial round right after `CreateSession`; `Some` = the
    /// answer to the last `Challenge`). Resume with the broker's
    /// [`PamStep`].
    Pam { response: Option<String> },
    /// PAM completed and the greeter sent `StartSession`. The I/O layer
    /// sends `Response::Success` to the greeter, forwards this as
    /// `StartSession` to the broker, then closes the greeter
    /// connection.
    Spawn(server::SpawnRequest),
}

/// State-machine errors.
///
/// These are NOT wire-level errors (which are `Response::Error`);
/// they're invariant violations caught at the state-machine layer.
/// The caller decides whether to translate them to `Response::Error`
/// for the wire (yes) or treat them as bugs (no).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StateMachineError {
    /// `StartSession` was received before PAM completed. This is the
    /// ARCHITECTURE.md threat model row 2 invariant — refused at the
    /// state-machine level even before PAM gets a chance to weigh in.
    #[error("start_session received before PAM success")]
    StartBeforeAuth,

    /// `PostAuthMessageResponse` was received while not in
    /// [`SessionState::Authenticating`] — there's no challenge pending.
    #[error("post_auth_message_response received without pending challenge")]
    ResponseWithoutPendingChallenge,

    /// `CreateSession` was received in a non-Idle state. The greeter
    /// must `CancelSession` first.
    #[error("create_session received with a session already in flight")]
    DoubleCreate,

    /// `CancelSession` was received in [`SessionState::Idle`] — no
    /// session to cancel.
    #[error("cancel_session received with no session in flight")]
    CancelWithoutSession,
}

impl StateMachineError {
    /// Translate to a wire-level `Response::Error`. The greeter sees
    /// these and surfaces them to the user (or hard-fails the
    /// conversation).
    #[must_use]
    pub fn to_response(&self) -> Response {
        Response::Error {
            error_type: ErrorType::Error,
            description: self.to_string(),
        }
    }
}

impl SessionState {
    /// Apply a greeter `Request` to the current state.
    ///
    /// Returns an [`Action`] the I/O layer must carry out, or a
    /// [`StateMachineError`] for a protocol violation (the I/O layer
    /// translates that via [`StateMachineError::to_response`] and keeps
    /// the connection open — the greeter may retry).
    ///
    /// On `Ok`, `self` is mutated to the new state. On `Err`, `self` is
    /// **unchanged** — the violation does not advance the machine.
    /// This call performs NO I/O and NO PAM work: a PAM round is
    /// requested by returning [`Action::Pam`] and suspending in
    /// [`SessionState::AuthPending`].
    #[allow(
        clippy::match_same_arms,
        reason = "match arms are organized by (state, request) — \
                  the ARCHITECTURE.md row-2 invariant is easier to \
                  audit when every state×request pair is listed \
                  explicitly. Collapsing arms with `|` would obscure \
                  which transitions are forbidden in which states."
    )]
    pub fn on_request(&mut self, request: Request) -> Result<Action, StateMachineError> {
        match (&*self, request) {
            // ── Idle ────────────────────────────────────────────────────
            (Self::Idle, Request::CreateSession { username }) => {
                *self = Self::AuthPending { username };
                Ok(Action::Pam { response: None })
            }
            (Self::Idle, Request::StartSession { .. }) => Err(StateMachineError::StartBeforeAuth),
            (Self::Idle, Request::PostAuthMessageResponse { .. }) => {
                Err(StateMachineError::ResponseWithoutPendingChallenge)
            }
            (Self::Idle, Request::CancelSession) => Err(StateMachineError::CancelWithoutSession),

            // ── AuthPending ─────────────────────────────────────────────
            // SUSPENDED awaiting on_pam_result. A well-behaved greeter
            // sends nothing until it sees the next Response; Cancel is
            // the one permitted abort.
            (Self::AuthPending { .. }, Request::CancelSession) => {
                *self = Self::Idle;
                Ok(Action::Reply(Response::Success))
            }
            (Self::AuthPending { .. }, Request::CreateSession { .. }) => {
                Err(StateMachineError::DoubleCreate)
            }
            (Self::AuthPending { .. }, Request::StartSession { .. }) => {
                Err(StateMachineError::StartBeforeAuth)
            }
            (Self::AuthPending { .. }, Request::PostAuthMessageResponse { .. }) => {
                Err(StateMachineError::ResponseWithoutPendingChallenge)
            }

            // ── Authenticating ──────────────────────────────────────────
            (Self::Authenticating { username }, Request::PostAuthMessageResponse { response }) => {
                let username = username.clone();
                *self = Self::AuthPending { username };
                Ok(Action::Pam { response })
            }
            (Self::Authenticating { .. }, Request::CancelSession) => {
                *self = Self::Idle;
                Ok(Action::Reply(Response::Success))
            }
            (Self::Authenticating { .. }, Request::CreateSession { .. }) => {
                Err(StateMachineError::DoubleCreate)
            }
            (Self::Authenticating { .. }, Request::StartSession { .. }) => {
                Err(StateMachineError::StartBeforeAuth)
            }

            // ── AuthSuccess ─────────────────────────────────────────────
            (Self::AuthSuccess { username, uid, gid }, Request::StartSession { cmd, env }) => {
                let spawn = server::SpawnRequest {
                    username: username.clone(),
                    uid: *uid,
                    gid: *gid,
                    cmd: cmd.clone(),
                    env: env.clone(),
                };
                *self = Self::Spawning {
                    username: spawn.username.clone(),
                    uid: spawn.uid,
                    gid: spawn.gid,
                    cmd,
                    env,
                };
                Ok(Action::Spawn(spawn))
            }
            (Self::AuthSuccess { .. }, Request::CancelSession) => {
                *self = Self::Idle;
                Ok(Action::Reply(Response::Success))
            }
            (Self::AuthSuccess { .. }, Request::CreateSession { .. }) => {
                Err(StateMachineError::DoubleCreate)
            }
            (Self::AuthSuccess { .. }, Request::PostAuthMessageResponse { .. }) => {
                Err(StateMachineError::ResponseWithoutPendingChallenge)
            }

            // ── Spawning ────────────────────────────────────────────────
            // Once a session has been spawned, no further requests are
            // meaningful. The greeter `wl_client` is killed by the I/O
            // layer at this point; any stray request — regardless of
            // kind — is a protocol violation by a confused greeter.
            // All four kinds collapse to `DoubleCreate` for consistency.
            (Self::Spawning { .. }, _) => Err(StateMachineError::DoubleCreate),
        }
    }

    /// Resume the suspended conversation with the broker's PAM result.
    ///
    /// Only meaningful in [`SessionState::AuthPending`] — the
    /// [`server::Connection`] layer guarantees that by tracking the
    /// suspend. If called in any other state this is a driver-contract
    /// violation; we fail closed with a generic protocol error rather
    /// than panicking.
    pub fn on_pam_result(&mut self, step: PamStep) -> Response {
        let Self::AuthPending { username } = std::mem::take(self) else {
            *self = Self::Idle;
            return Response::Error {
                error_type: ErrorType::Error,
                description: "pam result fed while not awaiting a PAM round".into(),
            };
        };
        match step {
            PamStep::Challenge { kind, prompt } => {
                *self = Self::Authenticating { username };
                Response::AuthMessage {
                    auth_message_type: kind,
                    auth_message: prompt,
                }
            }
            PamStep::Success {
                username: resolved,
                uid,
                gid,
            } => {
                // Use PAM's canonical name, not the client-supplied
                // `username` (which only fed the in-flight
                // `Authenticating` echo). uid/gid were resolved from
                // `resolved`; the supplementary-group lookup downstream
                // in the broker must use the same name or it can pair
                // one identity's uid with another's groups.
                *self = Self::AuthSuccess {
                    username: resolved,
                    uid,
                    gid,
                };
                Response::Success
            }
            PamStep::Failure { reason } => {
                *self = Self::Idle;
                // Cap the description length so a misbehaving broker
                // can't produce a reason longer than the wire-format
                // MAX_MESSAGE_SIZE and silently wedge the greeter
                // (encode would fail; the I/O layer would close the
                // connection). 4 KiB is plenty for any PAM error.
                Response::Error {
                    error_type: ErrorType::AuthError,
                    description: truncate_description(reason),
                }
            }
        }
    }
}

/// Upper bound on `Response::Error::description` chosen well below
/// [`MAX_MESSAGE_SIZE`] so a broker that produces a pathologically
/// long reason can't make a Response that fails to encode. Truncates
/// at a UTF-8 char boundary to keep the resulting string valid.
const MAX_ERROR_DESCRIPTION: usize = 4 * 1024;

fn truncate_description(mut reason: String) -> String {
    if reason.len() <= MAX_ERROR_DESCRIPTION {
        return reason;
    }
    // Find the last char boundary at or before MAX_ERROR_DESCRIPTION
    // so the truncated string remains valid UTF-8.
    let mut cut = MAX_ERROR_DESCRIPTION;
    while !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    reason.truncate(cut);
    reason.push_str(" [truncated]");
    reason
}

// ── Wire codec ──────────────────────────────────────────────────────────
//
// greetd's wire format (per the spec at man.sr.ht):
//
//     [4-byte native-endian unsigned length][JSON body of that length]
//
// "Native endian" is the spec's choice; on Linux x86_64 / aarch64
// that's little-endian in practice. Cross-architecture connections
// aren't expected (greeter and daemon both run on the same host).
//
// These functions are pure — no I/O. The socket loop layer reads bytes
// from a Unix socket, buffers them, and calls `try_decode` in a loop
// until it returns `None` (not enough bytes yet). On the write side it
// calls `encode` and hands the bytes to the socket.

/// Maximum permitted message body size (1 MiB). Rejected before we
/// allocate a body buffer — defends against a malicious peer pushing
/// a 4 GiB length prefix.
pub const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;

const LENGTH_PREFIX_SIZE: usize = std::mem::size_of::<u32>();

/// Errors from the wire codec. Framing or JSON, never I/O.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The length prefix exceeded [`MAX_MESSAGE_SIZE`]. Rejected
    /// before any allocation.
    #[error("message length {0} exceeds MAX_MESSAGE_SIZE ({1})")]
    OversizedMessage(u32, u32),

    /// The body wasn't valid JSON, didn't deserialize to the expected
    /// type, or the encode side couldn't serialize the message.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encode a `Request` or `Response` to wire bytes. The result is a
/// `Vec<u8>` of `[length_prefix:4][json_body:N]`.
///
/// # Errors
///
/// [`CodecError::Json`] if `serde_json::to_vec` fails (shouldn't
/// happen for our types; included for completeness).
pub fn encode<M: serde::Serialize>(msg: &M) -> Result<Vec<u8>, CodecError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len()).map_err(|_| {
        // Vec<u8> longer than u32::MAX is implausible but guard
        // anyway so we never silently truncate the prefix.
        CodecError::OversizedMessage(u32::MAX, MAX_MESSAGE_SIZE)
    })?;
    if len > MAX_MESSAGE_SIZE {
        return Err(CodecError::OversizedMessage(len, MAX_MESSAGE_SIZE));
    }
    let mut out = Vec::with_capacity(LENGTH_PREFIX_SIZE + body.len());
    out.extend_from_slice(&len.to_ne_bytes());
    out.extend(body);
    Ok(out)
}

/// Attempt to decode one message from the front of `buf`.
///
/// Returns:
/// - `Ok(Some((msg, consumed)))` — successfully parsed; the caller
///   should advance its read buffer by `consumed` bytes.
/// - `Ok(None)` — `buf` doesn't yet contain a complete message; the
///   caller should read more bytes and retry.
/// - `Err(_)` — framing error (oversized prefix) or JSON error. The
///   connection should generally be closed at this point.
///
/// # Errors
///
/// See [`CodecError`].
pub fn try_decode<T: serde::de::DeserializeOwned>(
    buf: &[u8],
) -> Result<Option<(T, usize)>, CodecError> {
    if buf.len() < LENGTH_PREFIX_SIZE {
        return Ok(None);
    }
    let mut len_bytes = [0u8; LENGTH_PREFIX_SIZE];
    len_bytes.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
    let len = u32::from_ne_bytes(len_bytes);
    if len > MAX_MESSAGE_SIZE {
        return Err(CodecError::OversizedMessage(len, MAX_MESSAGE_SIZE));
    }
    let len = len as usize;
    let needed = LENGTH_PREFIX_SIZE + len;
    if buf.len() < needed {
        return Ok(None);
    }
    let body = &buf[LENGTH_PREFIX_SIZE..needed];
    let msg: T = serde_json::from_slice(body)?;
    Ok(Some((msg, needed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── helpers ─────────────────────────────────────────────────────────

    /// Drive `CreateSession` → suspended PAM round, asserting the
    /// emitted action, and return the state ready for `on_pam_result`.
    fn created(username: &str) -> SessionState {
        let mut state = SessionState::default();
        let action = state
            .on_request(Request::CreateSession {
                username: username.into(),
            })
            .unwrap();
        assert_eq!(action, Action::Pam { response: None });
        assert_eq!(
            state,
            SessionState::AuthPending {
                username: username.into()
            }
        );
        state
    }

    // ── happy-path single-round ─────────────────────────────────────────

    #[test]
    fn create_then_immediate_success() {
        // PAM canonicalizes "alice" → "alice.canonical" (a
        // username-rewriting stack, e.g. pam_username/pam_mapfile).
        // AuthSuccess must carry PAM's resolved name, NOT the
        // client-supplied CreateSession string — that name is what the
        // broker hands to initgroups(3). (F1 regression.)
        let mut state = created("alice");
        let response = state.on_pam_result(PamStep::Success {
            username: "alice.canonical".into(),
            uid: 1000,
            gid: 1000,
        });
        assert_eq!(response, Response::Success);
        assert_eq!(
            state,
            SessionState::AuthSuccess {
                username: "alice.canonical".into(),
                uid: 1000,
                gid: 1000,
            }
        );
    }

    #[test]
    fn create_challenge_response_success() {
        let mut state = created("alice");
        let r1 = state.on_pam_result(PamStep::Challenge {
            kind: AuthMessageType::Secret,
            prompt: "password:".into(),
        });
        assert_eq!(
            r1,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            }
        );
        assert_eq!(
            state,
            SessionState::Authenticating {
                username: "alice".into()
            }
        );

        // greeter answers the challenge → another suspended PAM round.
        let action = state
            .on_request(Request::PostAuthMessageResponse {
                response: Some("hunter2".into()),
            })
            .unwrap();
        assert_eq!(
            action,
            Action::Pam {
                response: Some("hunter2".into())
            }
        );
        let r2 = state.on_pam_result(PamStep::Success {
            username: "alice".into(),
            uid: 1000,
            gid: 1000,
        });
        assert_eq!(r2, Response::Success);
        assert!(matches!(state, SessionState::AuthSuccess { .. }));
    }

    #[test]
    fn auth_failure_returns_to_idle() {
        let mut state = created("alice");
        let r = state.on_pam_result(PamStep::Failure {
            reason: "bad password".into(),
        });
        assert_eq!(
            r,
            Response::Error {
                error_type: ErrorType::AuthError,
                description: "bad password".into(),
            }
        );
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn auth_success_then_start_session() {
        let mut state = SessionState::AuthSuccess {
            username: "alice".into(),
            uid: 1000,
            gid: 1000,
        };
        let action = state
            .on_request(Request::StartSession {
                cmd: vec!["niri".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            })
            .unwrap();
        assert_eq!(
            action,
            Action::Spawn(server::SpawnRequest {
                username: "alice".into(),
                uid: 1000,
                gid: 1000,
                cmd: vec!["niri".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            })
        );
        assert_eq!(
            state,
            SessionState::Spawning {
                username: "alice".into(),
                uid: 1000,
                gid: 1000,
                cmd: vec!["niri".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            }
        );
    }

    // ── error paths (the load-bearing invariants) ───────────────────────

    #[test]
    fn start_session_in_idle_refused() {
        let mut state = SessionState::Idle;
        let err = state
            .on_request(Request::StartSession {
                cmd: vec!["niri".into()],
                env: vec![],
            })
            .unwrap_err();
        assert_eq!(err, StateMachineError::StartBeforeAuth);
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn start_session_in_auth_pending_refused() {
        // A PAM round is in flight — StartSession must NOT slip past
        // the row-2 gate just because the machine is mid-conversation.
        let mut state = created("alice");
        let err = state
            .on_request(Request::StartSession {
                cmd: vec!["niri".into()],
                env: vec![],
            })
            .unwrap_err();
        assert_eq!(err, StateMachineError::StartBeforeAuth);
        assert_eq!(
            state,
            SessionState::AuthPending {
                username: "alice".into()
            }
        );
    }

    #[test]
    fn start_session_in_authenticating_refused() {
        let mut state = SessionState::Authenticating {
            username: "alice".into(),
        };
        let err = state
            .on_request(Request::StartSession {
                cmd: vec!["niri".into()],
                env: vec![],
            })
            .unwrap_err();
        assert_eq!(err, StateMachineError::StartBeforeAuth);
    }

    #[test]
    fn double_create_session_refused() {
        let mut state = SessionState::Authenticating {
            username: "alice".into(),
        };
        let err = state
            .on_request(Request::CreateSession {
                username: "bob".into(),
            })
            .unwrap_err();
        assert_eq!(err, StateMachineError::DoubleCreate);
    }

    #[test]
    fn response_in_idle_refused() {
        let mut state = SessionState::Idle;
        let err = state
            .on_request(Request::PostAuthMessageResponse {
                response: Some("hunter2".into()),
            })
            .unwrap_err();
        assert_eq!(err, StateMachineError::ResponseWithoutPendingChallenge);
    }

    #[test]
    fn cancel_in_idle_refused() {
        let mut state = SessionState::Idle;
        let err = state.on_request(Request::CancelSession).unwrap_err();
        assert_eq!(err, StateMachineError::CancelWithoutSession);
    }

    #[test]
    fn cancel_from_auth_pending_returns_to_idle() {
        let mut state = created("alice");
        let action = state.on_request(Request::CancelSession).unwrap();
        assert_eq!(action, Action::Reply(Response::Success));
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn cancel_from_authenticating_returns_to_idle() {
        let mut state = SessionState::Authenticating {
            username: "alice".into(),
        };
        let action = state.on_request(Request::CancelSession).unwrap();
        assert_eq!(action, Action::Reply(Response::Success));
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn cancel_from_auth_success_returns_to_idle() {
        let mut state = SessionState::AuthSuccess {
            username: "alice".into(),
            uid: 1000,
            gid: 1000,
        };
        let action = state.on_request(Request::CancelSession).unwrap();
        assert_eq!(action, Action::Reply(Response::Success));
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn pam_result_outside_auth_pending_fails_closed() {
        // Driver-contract violation: on_pam_result must only be called
        // while suspended. It must NOT panic and must NOT advance auth.
        let mut state = SessionState::Idle;
        let r = state.on_pam_result(PamStep::Success {
            username: "root".into(),
            uid: 0,
            gid: 0,
        });
        assert!(matches!(
            r,
            Response::Error {
                error_type: ErrorType::Error,
                ..
            }
        ));
        assert_eq!(state, SessionState::Idle);
    }

    // ── error-to-wire translation ───────────────────────────────────────

    #[test]
    fn error_translates_to_response_error() {
        let err = StateMachineError::StartBeforeAuth;
        let wire = err.to_response();
        assert_eq!(
            wire,
            Response::Error {
                error_type: ErrorType::Error,
                description: "start_session received before PAM success".into(),
            }
        );
    }

    #[test]
    fn on_pam_result_truncates_oversized_failure_reason() {
        // A misbehaving broker could feed back an enormous reason
        // string; on_pam_result caps it so the resulting Response::Error
        // stays well below MAX_MESSAGE_SIZE and never fails to encode.
        let mut state = created("alice");
        let huge_reason: String = "z".repeat(MAX_MESSAGE_SIZE as usize);
        let resp = state.on_pam_result(PamStep::Failure {
            reason: huge_reason,
        });
        let Response::Error { description, .. } = resp else {
            panic!("expected Error response");
        };
        assert!(
            description.len() < MAX_MESSAGE_SIZE as usize,
            "description {} exceeded MAX_MESSAGE_SIZE",
            description.len()
        );
        assert!(
            description.ends_with(" [truncated]"),
            "expected truncation marker, got tail: {:?}",
            &description[description.len().saturating_sub(20)..]
        );
    }

    // ── wire-format canonical-payload tests ─────────────────────────────
    //
    // Pins the JSON shape against payloads taken verbatim from the
    // greetd protocol spec (https://man.sr.ht/~kennylevinsen/greetd/
    // protocol.md). If the upstream protocol ever changes shape and we
    // don't notice, these tests break — that's the intended drift
    // mitigation now that we own the types instead of borrowing them.

    #[test]
    fn wire_format_request_create_session() {
        let json = r#"{"type":"create_session","username":"alice"}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            Request::CreateSession {
                username: "alice".into()
            }
        );
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(reserialized, json);
    }

    #[test]
    fn wire_format_request_post_auth_message_response_some() {
        let json = r#"{"type":"post_auth_message_response","response":"hunter2"}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            Request::PostAuthMessageResponse {
                response: Some("hunter2".into())
            }
        );
    }

    #[test]
    fn wire_format_request_post_auth_message_response_none() {
        let json = r#"{"type":"post_auth_message_response","response":null}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, Request::PostAuthMessageResponse { response: None });
    }

    #[test]
    fn wire_format_request_start_session() {
        let json = r#"{"type":"start_session","cmd":["niri"],"env":["XDG_SESSION_TYPE=wayland"]}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            Request::StartSession {
                cmd: vec!["niri".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            }
        );
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(reserialized, json);
    }

    #[test]
    fn wire_format_request_cancel_session() {
        let json = r#"{"type":"cancel_session"}"#;
        let parsed: Request = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, Request::CancelSession);
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(reserialized, json);
    }

    #[test]
    fn wire_format_response_success() {
        let json = r#"{"type":"success"}"#;
        let parsed: Response = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, Response::Success);
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(reserialized, json);
    }

    #[test]
    fn wire_format_response_error() {
        let json = r#"{"type":"error","error_type":"auth_error","description":"bad password"}"#;
        let parsed: Response = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            Response::Error {
                error_type: ErrorType::AuthError,
                description: "bad password".into(),
            }
        );
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(reserialized, json);
    }

    #[test]
    fn wire_format_response_auth_message() {
        let json =
            r#"{"type":"auth_message","auth_message_type":"secret","auth_message":"password:"}"#;
        let parsed: Response = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            }
        );
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(reserialized, json);
    }

    #[test]
    fn wire_format_auth_message_type_all_variants() {
        for (raw, variant) in [
            (r#""visible""#, AuthMessageType::Visible),
            (r#""secret""#, AuthMessageType::Secret),
            (r#""info""#, AuthMessageType::Info),
            (r#""error""#, AuthMessageType::Error),
        ] {
            let parsed: AuthMessageType = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, variant);
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
        }
    }

    #[test]
    fn wire_format_error_type_all_variants() {
        for (raw, variant) in [
            (r#""error""#, ErrorType::Error),
            (r#""auth_error""#, ErrorType::AuthError),
        ] {
            let parsed: ErrorType = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, variant);
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
        }
    }

    // ── codec ───────────────────────────────────────────────────────────

    #[test]
    fn codec_roundtrip_each_request_variant() {
        for req in [
            Request::CreateSession {
                username: "alice".into(),
            },
            Request::PostAuthMessageResponse {
                response: Some("hunter2".into()),
            },
            Request::PostAuthMessageResponse { response: None },
            Request::StartSession {
                cmd: vec!["niri".into()],
                env: vec!["XDG_SESSION_TYPE=wayland".into()],
            },
            Request::CancelSession,
        ] {
            let bytes = encode(&req).expect("encode");
            let (decoded, consumed): (Request, usize) =
                try_decode(&bytes).expect("decode").expect("complete");
            assert_eq!(decoded, req);
            assert_eq!(consumed, bytes.len());
        }
    }

    #[test]
    fn codec_roundtrip_each_response_variant() {
        for resp in [
            Response::Success,
            Response::Error {
                error_type: ErrorType::AuthError,
                description: "bad password".into(),
            },
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            },
        ] {
            let bytes = encode(&resp).expect("encode");
            let (decoded, consumed): (Response, usize) =
                try_decode(&bytes).expect("decode").expect("complete");
            assert_eq!(decoded, resp);
            assert_eq!(consumed, bytes.len());
        }
    }

    #[test]
    fn codec_decode_returns_none_for_short_prefix() {
        let r: Result<Option<(Request, usize)>, CodecError> = try_decode(&[0u8, 0, 0]);
        assert!(matches!(r, Ok(None)));
    }

    #[test]
    fn codec_decode_returns_none_for_partial_body() {
        let bytes = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        // Truncate one byte off the body so length prefix says N but only N-1 bytes follow.
        let truncated = &bytes[..bytes.len() - 1];
        let r: Result<Option<(Request, usize)>, CodecError> = try_decode(truncated);
        assert!(matches!(r, Ok(None)), "got: {r:?}");
    }

    #[test]
    fn codec_decode_consumes_one_message_at_a_time() {
        let a = encode(&Request::CreateSession {
            username: "alice".into(),
        })
        .unwrap();
        let b = encode(&Request::CancelSession).unwrap();
        let mut combined = Vec::new();
        combined.extend(&a);
        combined.extend(&b);

        let (first, consumed): (Request, usize) = try_decode(&combined).unwrap().unwrap();
        assert_eq!(
            first,
            Request::CreateSession {
                username: "alice".into()
            }
        );
        assert_eq!(consumed, a.len());

        let (second, consumed2): (Request, usize) =
            try_decode(&combined[consumed..]).unwrap().unwrap();
        assert_eq!(second, Request::CancelSession);
        assert_eq!(consumed2, b.len());
    }

    #[test]
    fn codec_decode_rejects_oversized_prefix() {
        let oversized: u32 = MAX_MESSAGE_SIZE + 1;
        let mut buf = Vec::new();
        buf.extend_from_slice(&oversized.to_ne_bytes());
        let r: Result<Option<(Request, usize)>, CodecError> = try_decode(&buf);
        match r {
            Err(CodecError::OversizedMessage(got, max)) => {
                assert_eq!(got, oversized);
                assert_eq!(max, MAX_MESSAGE_SIZE);
            }
            other => panic!("expected OversizedMessage, got {other:?}"),
        }
    }

    #[test]
    fn codec_decode_rejects_invalid_json() {
        // 5 bytes of body: `xxxxx` — not JSON.
        let len: u32 = 5;
        let mut buf = Vec::new();
        buf.extend_from_slice(&len.to_ne_bytes());
        buf.extend_from_slice(b"xxxxx");
        let r: Result<Option<(Request, usize)>, CodecError> = try_decode(&buf);
        assert!(matches!(r, Err(CodecError::Json(_))), "got: {r:?}");
    }

    #[test]
    fn codec_roundtrips_near_max_size_payload() {
        // Build a CreateSession with a username large enough that the
        // encoded message approaches MAX_MESSAGE_SIZE. JSON overhead
        // for `{"type":"create_session","username":"..."}` is ~40
        // bytes; pick a length that leaves room.
        let big: String = "a".repeat(MAX_MESSAGE_SIZE as usize - 200);
        let req = Request::CreateSession { username: big };
        let bytes = encode(&req).expect("encode near-max payload");
        let (decoded, consumed): (Request, usize) =
            try_decode(&bytes).expect("decode").expect("complete");
        assert_eq!(decoded, req);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn codec_rejects_encode_of_oversized_payload() {
        // Push past MAX_MESSAGE_SIZE so encode itself errors before
        // any wire transmission. The username alone exceeds the cap.
        let huge: String = "x".repeat(MAX_MESSAGE_SIZE as usize + 1);
        let req = Request::CreateSession { username: huge };
        let r = encode(&req);
        assert!(
            matches!(r, Err(CodecError::OversizedMessage(_, _))),
            "got: {r:?}"
        );
    }

    // ── property tests ──────────────────────────────────────────────────

    fn arb_request() -> impl Strategy<Value = Request> {
        prop_oneof![
            "[a-z][a-z0-9_-]{0,15}".prop_map(|u| Request::CreateSession { username: u }),
            prop::option::of(".*").prop_map(|r| Request::PostAuthMessageResponse { response: r }),
            (
                prop::collection::vec(".*", 0..4),
                prop::collection::vec(".*", 0..4),
            )
                .prop_map(|(cmd, env)| Request::StartSession { cmd, env }),
            Just(Request::CancelSession),
        ]
    }

    /// Resolve an [`Action::Pam`] suspend by feeding a scripted
    /// [`PamStep`] back, mirroring what the compositor episode loop
    /// does after one broker round-trip. `pam` decides the result.
    fn settle(state: &mut SessionState, action: &Action, pam: impl Fn() -> PamStep) {
        if let Action::Pam { .. } = action {
            let _ = state.on_pam_result(pam());
        }
    }

    proptest! {
        /// The invariant: `StartSession` only succeeds from
        /// `AuthSuccess`. PAM is scripted to always succeed (the most
        /// permissive adversary for this property) and is fed back via
        /// `on_pam_result`, exactly as the sans-IO driver would.
        #[test]
        fn start_session_requires_auth_success(
            sequence in prop::collection::vec(arb_request(), 0..20),
        ) {
            let mut state = SessionState::default();
            for req in sequence {
                let is_start = matches!(req, Request::StartSession { .. });
                let pre_state = state.clone();
                let result = state.on_request(req);
                if is_start && let Ok(Action::Spawn(_)) = result {
                    prop_assert!(
                        matches!(pre_state, SessionState::AuthSuccess { .. }),
                        "start_session spawned from pre-state {:?}", pre_state
                    );
                }
                if let Ok(action) = &result {
                    settle(&mut state, action, || PamStep::Success {
                        username: "resolved".into(),
                        uid: 1000,
                        gid: 1000,
                    });
                }
            }
        }

        /// Adversarial sequences must never panic the state machine,
        /// with arbitrary PAM outcomes interleaved.
        #[test]
        fn garbage_sequences_do_not_panic(
            sequence in prop::collection::vec(arb_request(), 0..50),
            fail_flags in prop::collection::vec(any::<bool>(), 50),
        ) {
            let mut state = SessionState::default();
            for (i, req) in sequence.into_iter().enumerate() {
                if let Ok(action) = state.on_request(req) {
                    settle(&mut state, &action, || {
                        if fail_flags[i] {
                            PamStep::Failure { reason: "no".into() }
                        } else {
                            PamStep::Challenge {
                                kind: AuthMessageType::Secret,
                                prompt: "again:".into(),
                            }
                        }
                    });
                }
            }
        }

        /// Endless challenges never reach AuthSuccess/Spawning: the
        /// machine ping-pongs Authenticating ⇄ AuthPending forever and
        /// `StartSession` can never be honored.
        #[test]
        fn endless_challenges_never_authorize_spawn(
            responses in prop::collection::vec(prop::option::of(".*"), 1..10),
        ) {
            let mut state = SessionState::default();
            let a = state.on_request(Request::CreateSession {
                username: "alice".into(),
            }).unwrap();
            prop_assert_eq!(a, Action::Pam { response: None });
            state.on_pam_result(PamStep::Challenge {
                kind: AuthMessageType::Secret,
                prompt: "again:".into(),
            });
            prop_assert!(
                matches!(state, SessionState::Authenticating { .. }),
                "expected Authenticating after first challenge"
            );
            for resp in responses {
                let action = state.on_request(
                    Request::PostAuthMessageResponse { response: resp },
                ).unwrap();
                prop_assert!(
                    matches!(action, Action::Pam { .. }),
                    "answering a challenge must request another PAM round"
                );
                prop_assert!(
                    matches!(state, SessionState::AuthPending { .. }),
                    "must suspend in AuthPending awaiting the PAM result"
                );
                // A spawn attempt here is rejected — never authorized.
                let mut probe = state.clone();
                prop_assert_eq!(
                    probe.on_request(Request::StartSession {
                        cmd: vec![], env: vec![],
                    }),
                    Err(StateMachineError::StartBeforeAuth)
                );
                state.on_pam_result(PamStep::Challenge {
                    kind: AuthMessageType::Secret,
                    prompt: "again:".into(),
                });
                prop_assert!(
                    matches!(state, SessionState::Authenticating { .. }),
                    "endless challenge keeps the machine in Authenticating"
                );
            }
        }

        /// Wire types round-trip: JSON → Request → JSON is stable.
        #[test]
        fn request_json_roundtrip(req in arb_request()) {
            let json = serde_json::to_string(&req).unwrap();
            let parsed: Request = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(parsed, req);
        }

        /// Codec round-trip: encode → try_decode is identity, and the
        /// consumed-byte count matches the encoded length exactly.
        #[test]
        fn codec_request_roundtrip(req in arb_request()) {
            let bytes = encode(&req).unwrap();
            let (decoded, consumed): (Request, usize) =
                try_decode(&bytes).unwrap().unwrap();
            prop_assert_eq!(decoded, req);
            prop_assert_eq!(consumed, bytes.len());
        }
    }
}
