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
//! PAM itself is abstracted as a trait ([`PamSession`]). The concrete
//! implementation (pam-sys / pam-client / bindgen) lands in a follow-up
//! task. I/O integration (socket binding, length-prefixed JSON codec,
//! halmasuit event-loop hookup) is yet another task. This crate is
//! pure logic; everything observable from outside is covered by unit
//! + property tests.

#![forbid(unsafe_code)]

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
/// Transitions are driven by [`SessionState::handle`]. The PAM-success
/// invariant is enforced by match arms: `StartSession` is only accepted
/// in [`SessionState::AuthSuccess`], which can only be reached via a
/// PAM step that returned [`PamStep::Success`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionState {
    /// No session in flight. Ready to accept `CreateSession`.
    #[default]
    Idle,
    /// `CreateSession` received; PAM has issued a challenge that the
    /// greeter must answer via `PostAuthMessageResponse`.
    Authenticating { username: String },
    /// PAM completed successfully. `StartSession` is now valid. The
    /// resolved uid/gid come from PAM (via the `pwent` lookup the real
    /// PAM stack runs); they will be handed to `halmasuit-spawn`.
    AuthSuccess {
        username: String,
        uid: u32,
        gid: u32,
    },
    /// `StartSession` received; the I/O layer is responsible for
    /// invoking `halmasuit-spawn` with these arguments. From the state
    /// machine's perspective, no more wire requests are accepted in
    /// this state — the session ends when the spawned child exits.
    Spawning {
        username: String,
        uid: u32,
        gid: u32,
        cmd: Vec<String>,
        env: Vec<String>,
    },
}

/// One round of the PAM conversation.
///
/// The state machine drives PAM forward by calling [`PamSession::step`]
/// after each greeter response; the concrete impl translates between
/// greetd's text-based response model and `pam_authenticate`'s
/// conversation callbacks.
pub trait PamSession {
    /// Drive PAM by one round.
    ///
    /// `response` is `None` for the initial step (right after
    /// `CreateSession`) and `Some(text)` for subsequent rounds (in
    /// response to `PostAuthMessageResponse`).
    fn step(&mut self, response: Option<String>) -> PamStep;
}

/// Outcome of one [`PamSession::step`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PamStep {
    /// PAM wants the user to answer a challenge. The state machine
    /// translates this into a `Response::AuthMessage` for the greeter.
    Challenge {
        kind: AuthMessageType,
        prompt: String,
    },
    /// PAM succeeded. The state machine transitions to
    /// [`SessionState::AuthSuccess`] with the resolved uid/gid.
    Success { uid: u32, gid: u32 },
    /// PAM failed (bad password, locked account, etc.). The state
    /// machine returns to [`SessionState::Idle`] after sending the
    /// error to the greeter.
    Failure { reason: String },
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
    /// Apply a `Request` to the current state, calling PAM as needed.
    ///
    /// Returns the outbound `Response` (which the I/O layer serializes
    /// and writes to the greeter socket) or a `StateMachineError`
    /// (which the I/O layer translates via [`StateMachineError::to_response`]).
    ///
    /// On `Ok(Response)`, `self` is mutated to the new state. On
    /// `Err(...)`, `self` is **unchanged** — the protocol violation
    /// doesn't advance the state machine.
    #[allow(
        clippy::match_same_arms,
        reason = "match arms are organized by (state, request) — \
                  the ARCHITECTURE.md row-2 invariant is easier to \
                  audit when every state×request pair is listed \
                  explicitly. Collapsing arms with `|` would obscure \
                  which transitions are forbidden in which states."
    )]
    pub fn handle<P: PamSession>(
        &mut self,
        request: Request,
        pam: &mut P,
    ) -> Result<Response, StateMachineError> {
        match (&*self, request) {
            // ── Idle ────────────────────────────────────────────────────
            (Self::Idle, Request::CreateSession { username }) => {
                Ok(self.advance_pam(username, pam))
            }
            (Self::Idle, Request::StartSession { .. }) => Err(StateMachineError::StartBeforeAuth),
            (Self::Idle, Request::PostAuthMessageResponse { .. }) => {
                Err(StateMachineError::ResponseWithoutPendingChallenge)
            }
            (Self::Idle, Request::CancelSession) => Err(StateMachineError::CancelWithoutSession),

            // ── Authenticating ──────────────────────────────────────────
            (Self::Authenticating { .. }, Request::PostAuthMessageResponse { response }) => {
                Ok(self.advance_pam_with_response(response, pam))
            }
            (Self::Authenticating { .. }, Request::CancelSession) => {
                *self = Self::Idle;
                Ok(Response::Success)
            }
            (Self::Authenticating { .. }, Request::CreateSession { .. }) => {
                Err(StateMachineError::DoubleCreate)
            }
            (Self::Authenticating { .. }, Request::StartSession { .. }) => {
                Err(StateMachineError::StartBeforeAuth)
            }

            // ── AuthSuccess ─────────────────────────────────────────────
            (Self::AuthSuccess { username, uid, gid }, Request::StartSession { cmd, env }) => {
                let username = username.clone();
                let uid = *uid;
                let gid = *gid;
                *self = Self::Spawning {
                    username,
                    uid,
                    gid,
                    cmd,
                    env,
                };
                Ok(Response::Success)
            }
            (Self::AuthSuccess { .. }, Request::CancelSession) => {
                *self = Self::Idle;
                Ok(Response::Success)
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
            // layer at this point; any stray requests are protocol
            // violations from a confused greeter.
            (
                Self::Spawning { .. },
                Request::CreateSession { .. } | Request::PostAuthMessageResponse { .. },
            ) => Err(StateMachineError::DoubleCreate),
            (Self::Spawning { .. }, Request::StartSession { .. }) => {
                Err(StateMachineError::DoubleCreate)
            }
            (Self::Spawning { .. }, Request::CancelSession) => {
                Err(StateMachineError::CancelWithoutSession)
            }
        }
    }

    fn advance_pam<P: PamSession>(&mut self, username: String, pam: &mut P) -> Response {
        *self = Self::Authenticating {
            username: username.clone(),
        };
        translate_pam_step(self, pam.step(None), username)
    }

    fn advance_pam_with_response<P: PamSession>(
        &mut self,
        response: Option<String>,
        pam: &mut P,
    ) -> Response {
        let Self::Authenticating { username } = self else {
            unreachable!("advance_pam_with_response called outside Authenticating");
        };
        let username = username.clone();
        translate_pam_step(self, pam.step(response), username)
    }
}

fn translate_pam_step(state: &mut SessionState, step: PamStep, username: String) -> Response {
    match step {
        PamStep::Challenge { kind, prompt } => {
            *state = SessionState::Authenticating { username };
            Response::AuthMessage {
                auth_message_type: kind,
                auth_message: prompt,
            }
        }
        PamStep::Success { uid, gid } => {
            *state = SessionState::AuthSuccess { username, uid, gid };
            Response::Success
        }
        PamStep::Failure { reason } => {
            *state = SessionState::Idle;
            Response::Error {
                error_type: ErrorType::AuthError,
                description: reason,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;

    struct MockPam {
        steps: VecDeque<PamStep>,
    }

    impl MockPam {
        fn scripted(steps: Vec<PamStep>) -> Self {
            Self {
                steps: steps.into(),
            }
        }
        fn empty() -> Self {
            Self {
                steps: VecDeque::new(),
            }
        }
    }

    impl PamSession for MockPam {
        fn step(&mut self, _response: Option<String>) -> PamStep {
            self.steps.pop_front().unwrap_or_else(|| PamStep::Failure {
                reason: "no more scripted steps".into(),
            })
        }
    }

    // ── happy-path single-round ─────────────────────────────────────────

    #[test]
    fn create_then_immediate_success() {
        let mut state = SessionState::default();
        let mut pam = MockPam::scripted(vec![PamStep::Success {
            uid: 1000,
            gid: 1000,
        }]);
        let response = state
            .handle(
                Request::CreateSession {
                    username: "alice".into(),
                },
                &mut pam,
            )
            .unwrap();
        assert_eq!(response, Response::Success);
        assert_eq!(
            state,
            SessionState::AuthSuccess {
                username: "alice".into(),
                uid: 1000,
                gid: 1000,
            }
        );
    }

    #[test]
    fn create_challenge_response_success() {
        let mut state = SessionState::default();
        let mut pam = MockPam::scripted(vec![
            PamStep::Challenge {
                kind: AuthMessageType::Secret,
                prompt: "password:".into(),
            },
            PamStep::Success {
                uid: 1000,
                gid: 1000,
            },
        ]);
        let r1 = state
            .handle(
                Request::CreateSession {
                    username: "alice".into(),
                },
                &mut pam,
            )
            .unwrap();
        assert_eq!(
            r1,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "password:".into(),
            }
        );
        let r2 = state
            .handle(
                Request::PostAuthMessageResponse {
                    response: Some("hunter2".into()),
                },
                &mut pam,
            )
            .unwrap();
        assert_eq!(r2, Response::Success);
        assert!(matches!(state, SessionState::AuthSuccess { .. }));
    }

    #[test]
    fn auth_failure_returns_to_idle() {
        let mut state = SessionState::default();
        let mut pam = MockPam::scripted(vec![PamStep::Failure {
            reason: "bad password".into(),
        }]);
        let r = state
            .handle(
                Request::CreateSession {
                    username: "alice".into(),
                },
                &mut pam,
            )
            .unwrap();
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
        let mut pam = MockPam::empty();
        let r = state
            .handle(
                Request::StartSession {
                    cmd: vec!["niri".into()],
                    env: vec!["XDG_SESSION_TYPE=wayland".into()],
                },
                &mut pam,
            )
            .unwrap();
        assert_eq!(r, Response::Success);
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
        let mut pam = MockPam::empty();
        let err = state
            .handle(
                Request::StartSession {
                    cmd: vec!["niri".into()],
                    env: vec![],
                },
                &mut pam,
            )
            .unwrap_err();
        assert_eq!(err, StateMachineError::StartBeforeAuth);
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn start_session_in_authenticating_refused() {
        let mut state = SessionState::Authenticating {
            username: "alice".into(),
        };
        let mut pam = MockPam::empty();
        let err = state
            .handle(
                Request::StartSession {
                    cmd: vec!["niri".into()],
                    env: vec![],
                },
                &mut pam,
            )
            .unwrap_err();
        assert_eq!(err, StateMachineError::StartBeforeAuth);
    }

    #[test]
    fn double_create_session_refused() {
        let mut state = SessionState::Authenticating {
            username: "alice".into(),
        };
        let mut pam = MockPam::empty();
        let err = state
            .handle(
                Request::CreateSession {
                    username: "bob".into(),
                },
                &mut pam,
            )
            .unwrap_err();
        assert_eq!(err, StateMachineError::DoubleCreate);
    }

    #[test]
    fn response_in_idle_refused() {
        let mut state = SessionState::Idle;
        let mut pam = MockPam::empty();
        let err = state
            .handle(
                Request::PostAuthMessageResponse {
                    response: Some("hunter2".into()),
                },
                &mut pam,
            )
            .unwrap_err();
        assert_eq!(err, StateMachineError::ResponseWithoutPendingChallenge);
    }

    #[test]
    fn cancel_in_idle_refused() {
        let mut state = SessionState::Idle;
        let mut pam = MockPam::empty();
        let err = state.handle(Request::CancelSession, &mut pam).unwrap_err();
        assert_eq!(err, StateMachineError::CancelWithoutSession);
    }

    #[test]
    fn cancel_from_authenticating_returns_to_idle() {
        let mut state = SessionState::Authenticating {
            username: "alice".into(),
        };
        let mut pam = MockPam::empty();
        let r = state.handle(Request::CancelSession, &mut pam).unwrap();
        assert_eq!(r, Response::Success);
        assert_eq!(state, SessionState::Idle);
    }

    #[test]
    fn cancel_from_auth_success_returns_to_idle() {
        let mut state = SessionState::AuthSuccess {
            username: "alice".into(),
            uid: 1000,
            gid: 1000,
        };
        let mut pam = MockPam::empty();
        let r = state.handle(Request::CancelSession, &mut pam).unwrap();
        assert_eq!(r, Response::Success);
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

    struct AlwaysSucceedPam;
    impl PamSession for AlwaysSucceedPam {
        fn step(&mut self, _response: Option<String>) -> PamStep {
            PamStep::Success {
                uid: 1000,
                gid: 1000,
            }
        }
    }

    struct EndlessChallengePam;
    impl PamSession for EndlessChallengePam {
        fn step(&mut self, _response: Option<String>) -> PamStep {
            PamStep::Challenge {
                kind: AuthMessageType::Secret,
                prompt: "again:".into(),
            }
        }
    }

    proptest! {
        /// The invariant: `StartSession` only succeeds from `AuthSuccess`.
        #[test]
        fn start_session_requires_auth_success(
            sequence in prop::collection::vec(arb_request(), 0..20),
        ) {
            let mut state = SessionState::default();
            let mut pam = AlwaysSucceedPam;
            for req in sequence {
                let is_start = matches!(req, Request::StartSession { .. });
                let pre_state = state.clone();
                let result = state.handle(req, &mut pam);
                if is_start && result.is_ok() {
                    prop_assert!(
                        matches!(pre_state, SessionState::AuthSuccess { .. }),
                        "start_session succeeded from pre-state {:?}", pre_state
                    );
                }
            }
        }

        /// Adversarial sequences must never panic the state machine.
        #[test]
        fn garbage_sequences_do_not_panic(
            sequence in prop::collection::vec(arb_request(), 0..50),
        ) {
            let mut state = SessionState::default();
            let mut pam = AlwaysSucceedPam;
            for req in sequence {
                let _ = state.handle(req, &mut pam);
            }
        }

        /// Endless challenges keep state in Authenticating.
        #[test]
        fn endless_challenges_stay_in_authenticating(
            responses in prop::collection::vec(prop::option::of(".*"), 1..10),
        ) {
            let mut state = SessionState::default();
            let mut pam = EndlessChallengePam;
            state.handle(
                Request::CreateSession { username: "alice".into() },
                &mut pam,
            ).unwrap();
            for resp in responses {
                state.handle(
                    Request::PostAuthMessageResponse { response: resp },
                    &mut pam,
                ).unwrap();
                prop_assert!(
                    matches!(state, SessionState::Authenticating { .. }),
                    "state should remain Authenticating, got {:?}", state
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
    }
}
