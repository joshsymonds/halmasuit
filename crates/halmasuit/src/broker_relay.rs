//! Pure compositor-side relay adapter (Epic R3 / Amendments A4 & A5).
//!
//! Translates between the `halmasuit-greetd` `PamSession` conversation
//! vocabulary (`PamStep::{Challenge,Success,Failure}`) and the frozen
//! `halmasuit-session-ipc` wire frames. This is `halmasuit-pam`'s
//! successor: the compositor stops running PAM in-process and instead
//! relays the conversation to the privileged `halmasuit-session`
//! broker, which owns the one `pam_handle_t` for the whole lifecycle
//! (Epic R1/R2/R3).
//!
//! **Pure**: no socket, no libpam, no process control, no `unsafe`,
//! no pidfd. [`crate::broker_session::BrokerEpisode`] wraps this brain
//! with the actual `SOCK_SEQPACKET` I/O — it is the live `PamSession`
//! and implements the Amendment-A5 two-key flash-free swap + the
//! SCM_RIGHTS pidfd backstop; none of that lives here.
//!
//! Amendment A5: session-lifecycle is one-way broker→compositor. This
//! adapter only ever *consumes* [`BrokerToCompositor::SessionOpened`]/
//! [`BrokerToCompositor::SessionEnded`]; it never emits a lifecycle
//! frame (the type system makes that impossible — there is no such
//! `CompositorToBroker` variant). Frame ordering is enforced by an
//! explicit phase machine that fails closed on anything out of
//! sequence.

use std::fmt;

use halmasuit_greetd::{AuthMessageType, PamStep};
use halmasuit_session_ipc::{
    BrokerToCompositor, CompositorToBroker, DisplayStyle, PromptStyle, Secret, SessionOutcome,
};

/// Where the compositor↔broker relay is in the Amendment-A1 sequence.
/// Pure ordering state; every transition is checked so an
/// out-of-sequence frame fails closed rather than advancing auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing sent yet; the next `PamSession::step` (always `None`
    /// from the greetd state machine) emits `BeginAuth`.
    PreAuth,
    /// `BeginAuth`/`ConvResponse` sent; awaiting
    /// `ConvPrompt`/`Success`/`Failure`.
    Authing,
    /// Broker reported `Success`; greetd is in `AuthSuccess`. Awaiting
    /// the greeter's `StartSession` to forward (Amendment A1: the spec
    /// is sent only AFTER auth success).
    Authed,
    /// `StartSession` forwarded; awaiting `SessionOpened`.
    Starting,
    /// `SessionOpened` seen; the session leader runs. Awaiting
    /// `SessionEnded`.
    Running,
    /// Terminal: `Failure`, `SessionEnded`, or `Cancel`.
    Done,
}

/// What an inbound broker frame means to the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEvent {
    /// Feed this to the `halmasuit-greetd` state machine as the
    /// outcome of a `PamSession::step` (the auth conversation).
    Pam(PamStep),
    /// Amendment A5.2/A5.3: the broker forked+exec'd the session
    /// leader. This is the AUTHORIZATION key only — the *visible*
    /// greeter→session swap is gated separately on the compositor's
    /// own first-non-empty-frame observation (the later socket step),
    /// never on this event alone.
    SessionOpened,
    /// Amendment A5.5: the session ended (the broker, its parent and
    /// sole reaper, `waitpid`'d it). Revert to the greeter. `outcome`
    /// is the clean-vs-crash distinction for revert UX.
    SessionEnded(SessionOutcome),
}

/// Relay protocol violation. Fail closed — the caller tears the
/// connection down; auth never advances on an out-of-sequence frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// A `PamSession::step`, `start_session`, or broker frame arrived
    /// in a phase where the Amendment-A1 sequence does not permit it.
    OutOfPhase,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfPhase => {
                f.write_str("broker relay: frame out of sequence for the current phase")
            }
        }
    }
}

impl std::error::Error for RelayError {}

/// The pure compositor↔broker translation state machine.
///
/// Constructed per greeter `CreateSession` with the `pam_start` hint
/// (`service` + the client-supplied `username`). The authoritative
/// identity is NEVER this `username`: it is the broker's PAM-resolved
/// `Success{username,uid,gid}`, passed through verbatim (Epic R8).
///
/// Epic #24 invariant: `awaiting_display_ack` names whether the last
/// frame we forwarded to the greeter was a [`BrokerToCompositor::ConvDisplay`]
/// (display-class, one-way on the broker wire). When set, the next
/// `on_pam_step` MUST return `None` — DMS sends a greetd
/// `PostAuthMessageResponse` for every `auth_message` regardless of
/// type, and the compositor's job is to swallow that response for
/// display-class messages so it never crosses the broker boundary
/// (the broker stays in `AwaitWorker`; see `crates/halmasuit-session/
/// src/broker.rs` `on_worker_readable`'s `ConvDisplay` arm).
#[derive(Debug)]
pub struct BrokerRelay {
    service: String,
    username: String,
    phase: Phase,
    /// One-shot: `true` iff the LAST frame forwarded to the greeter
    /// was a [`BrokerToCompositor::ConvDisplay`]. Set on `ConvDisplay`
    /// forward, cleared on (a) a `ConvPrompt` forward, or (b) the
    /// next `on_pam_step` swallow. A boolean (not a counter) is
    /// sufficient because greetd's own state machine enforces ONE
    /// outstanding `auth_message` at a time on the compositor↔greeter
    /// wire: `crates/halmasuit-greetd/src/server.rs::resume_pam`
    /// fail-closes (`"resume_pam called without an outstanding PAM
    /// round"`) the moment a second broker auth_message arrives
    /// before the greeter's `post_auth_message_response` for the
    /// first. So the pathological ordering D₁ D₂ R₁ R₂ — where two
    /// `ConvDisplay` frames would be in flight simultaneously and
    /// the flag would need to count to 2 — cannot occur in
    /// halmasuit's broker↔compositor protocol stack.
    awaiting_display_ack: bool,
}

impl BrokerRelay {
    /// `service` = the PAM service (`/etc/pam.d/<service>`);
    /// `username` = the client-supplied `CreateSession` name, used
    /// ONLY as the `pam_start` hint (never as the spawn identity).
    #[must_use]
    pub const fn new(service: String, username: String) -> Self {
        Self {
            service,
            username,
            phase: Phase::PreAuth,
            awaiting_display_ack: false,
        }
    }

    /// Translate a `halmasuit-greetd` `PamSession::step(response)` call
    /// into the outbound broker frame, OR `None` if the response is
    /// for a display-class message and must be swallowed.
    ///
    /// - `PreAuth`: emits `Some(BeginAuth)` (the greetd state
    ///   machine's initial `step(None)`); `response` is unused here.
    /// - `Authing`:
    ///   - If `awaiting_display_ack` is set (the last frame the
    ///     compositor forwarded to the greeter was a `ConvDisplay`),
    ///     returns `None` — Epic #24 R5: the broker stays in
    ///     `AwaitWorker`, and DMS's `respond("")` MUST NOT be
    ///     forwarded as a `ConvResponse`. The flag is cleared.
    ///   - Otherwise emits `Some(ConvResponse{...})`. `None` content
    ///     (a greeter that sends a no-payload response for an
    ///     unrecognized prompt) maps to an empty `Secret`.
    ///
    /// # Errors
    /// [`RelayError::OutOfPhase`] if called after auth completed.
    pub fn on_pam_step(
        &mut self,
        response: Option<String>,
    ) -> Result<Option<CompositorToBroker>, RelayError> {
        match self.phase {
            Phase::PreAuth => {
                self.phase = Phase::Authing;
                Ok(Some(CompositorToBroker::BeginAuth {
                    service: self.service.clone(),
                    username: self.username.clone(),
                }))
            }
            Phase::Authing => {
                if self.awaiting_display_ack {
                    // Epic #24 R5: swallow the greetd-side response to
                    // a display-class auth_message. The broker stays
                    // in AwaitWorker; the worker (which is NOT blocked
                    // — `display()` returned immediately) drives the
                    // next conv message or PAM step.
                    self.awaiting_display_ack = false;
                    return Ok(None);
                }
                Ok(Some(CompositorToBroker::ConvResponse {
                    response: Secret::new(response.unwrap_or_default()),
                }))
            }
            Phase::Authed | Phase::Starting | Phase::Running | Phase::Done => {
                Err(RelayError::OutOfPhase)
            }
        }
    }

    /// Translate an inbound `BrokerToCompositor` frame into a
    /// [`RelayEvent`], advancing the phase. Fails closed on any frame
    /// that does not belong in the current phase.
    ///
    /// # Errors
    /// [`RelayError::OutOfPhase`] on an out-of-sequence frame.
    pub fn on_broker_frame(&mut self, frame: BrokerToCompositor) -> Result<RelayEvent, RelayError> {
        match (self.phase, frame) {
            // Auth conversation: a prompt keeps us in Authing.
            // Greeter MUST answer (greetd post_auth_message_response).
            // Epic #24 R5: clear the display-ack flag — the next
            // greeter response is a real ConvResponse to be forwarded.
            (Phase::Authing, BrokerToCompositor::ConvPrompt { style, message }) => {
                self.awaiting_display_ack = false;
                Ok(RelayEvent::Pam(PamStep::Challenge {
                    kind: prompt_to_kind(style),
                    prompt: message,
                }))
            }
            // Auth conversation: a display-only message keeps us in
            // Authing too. The greetd wire protocol still mandates a
            // post_auth_message_response from the greeter (DMS sends
            // `respond("")`); Epic #24 R5: arm the display-ack swallow
            // so the next `on_pam_step` returns None instead of
            // forwarding a stale ConvResponse to the broker (which is
            // in AwaitWorker — see broker.rs's ConvDisplay arm).
            (Phase::Authing, BrokerToCompositor::ConvDisplay { style, message }) => {
                self.awaiting_display_ack = true;
                Ok(RelayEvent::Pam(PamStep::Challenge {
                    kind: display_to_kind(style),
                    prompt: message,
                }))
            }
            // Success: pass the broker's PAM-resolved identity through
            // VERBATIM (Epic R8 — never re-derive / substitute).
            (Phase::Authing, BrokerToCompositor::Success { username, uid, gid }) => {
                self.phase = Phase::Authed;
                Ok(RelayEvent::Pam(PamStep::Success { username, uid, gid }))
            }
            (Phase::Authing, BrokerToCompositor::Failure { reason }) => {
                self.phase = Phase::Done;
                Ok(RelayEvent::Pam(PamStep::Failure { reason }))
            }
            // Lifecycle (Amendment A5): consumed only, never emitted.
            (Phase::Starting, BrokerToCompositor::SessionOpened) => {
                self.phase = Phase::Running;
                Ok(RelayEvent::SessionOpened)
            }
            (Phase::Running, BrokerToCompositor::SessionEnded { outcome }) => {
                self.phase = Phase::Done;
                Ok(RelayEvent::SessionEnded(outcome))
            }
            // Anything else is out of sequence — fail closed.
            _ => Err(RelayError::OutOfPhase),
        }
    }

    /// Translate the greetd `Spawning { cmd, env }` terminal (reached
    /// only from `AuthSuccess`) into the broker `StartSession` frame.
    /// Valid ONLY after `Success` (Amendment A1: the spec is sent
    /// post-auth). greetd `env` entries are `KEY=VALUE` strings; they
    /// are split at the first `=` into the broker's `(key, value)`
    /// pairs (a missing `=` yields an empty value).
    ///
    /// # Errors
    /// [`RelayError::OutOfPhase`] unless the phase is `Authed`.
    pub fn start_session(
        &mut self,
        cmd: Vec<String>,
        env: &[String],
    ) -> Result<CompositorToBroker, RelayError> {
        if self.phase != Phase::Authed {
            return Err(RelayError::OutOfPhase);
        }
        self.phase = Phase::Starting;
        Ok(CompositorToBroker::StartSession {
            cmd,
            env: env.iter().map(|e| split_env_pair(e)).collect(),
        })
    }

    /// Force the terminal phase WITHOUT emitting a frame.
    ///
    /// The episode (`BrokerEpisode`) calls this on any transport/relay
    /// error so every subsequent `on_pam_step`/`on_broker_frame`
    /// returns [`RelayError::OutOfPhase`]; the episode maps that to
    /// greetd's `broker_closed()` fail-closed auth failure (A7.4). The
    /// latch lives in the episode-owned relay, so it survives across
    /// the calloop readiness callbacks that drive one episode.
    pub const fn poison(&mut self) {
        self.phase = Phase::Done;
    }
}

/// Map a libpam **prompt-class** conv style to the greetd
/// auth-message type the state machine hands the greeter.
///
/// Exhaustive over [`PromptStyle`] — no `_ =>` default. Epic #24 R4
/// invariant: a future variant addition fails compile here, never
/// silently absorbs.
const fn prompt_to_kind(style: PromptStyle) -> AuthMessageType {
    match style {
        PromptStyle::Visible => AuthMessageType::Visible,
        PromptStyle::Secret => AuthMessageType::Secret,
    }
}

/// Map a libpam **display-class** conv style to the greetd
/// auth-message type the state machine hands the greeter.
///
/// Exhaustive over [`DisplayStyle`] — no `_ =>` default. Same Epic #24
/// R4 rationale as [`prompt_to_kind`].
const fn display_to_kind(style: DisplayStyle) -> AuthMessageType {
    match style {
        DisplayStyle::Info => AuthMessageType::Info,
        DisplayStyle::Error => AuthMessageType::Error,
    }
}

/// Split a greetd `KEY=VALUE` env entry at the FIRST `=`. A missing
/// `=` yields `(entry, "")` (malformed-but-fail-soft, matching the
/// broker side's `split_env_pair`).
fn split_env_pair(entry: &str) -> (String, String) {
    entry.split_once('=').map_or_else(
        || (entry.to_owned(), String::new()),
        |(k, v)| (k.to_owned(), v.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> BrokerRelay {
        BrokerRelay::new("halmasuit".into(), "alice".into())
    }

    fn begun() -> BrokerRelay {
        let mut r = relay();
        r.on_pam_step(None).unwrap();
        r
    }

    /// Drive to `Authed` (post-`Success`), returning the relay.
    fn authed() -> BrokerRelay {
        let mut r = begun();
        r.on_broker_frame(BrokerToCompositor::Success {
            username: "alice.canonical".into(),
            uid: 1001,
            gid: 1001,
        })
        .unwrap();
        r
    }

    // ── outbound: greetd step → CompositorToBroker ───────────────────

    #[test]
    fn pre_auth_step_emits_begin_auth() {
        let mut r = relay();
        assert_eq!(
            r.on_pam_step(None).unwrap(),
            Some(CompositorToBroker::BeginAuth {
                service: "halmasuit".into(),
                username: "alice".into(),
            })
        );
    }

    #[test]
    fn step_with_response_emits_conv_response() {
        // Plain prompt path: no display has been forwarded, so the
        // greeter's response IS a real ConvResponse to be forwarded.
        let mut r = begun();
        // Drive a prompt through `on_broker_frame` first so the
        // awaiting_display_ack flag is in its post-prompt state
        // (false). This models the real flow: broker → ConvPrompt →
        // greeter → response → on_pam_step.
        r.on_broker_frame(BrokerToCompositor::ConvPrompt {
            style: PromptStyle::Secret,
            message: "pw".into(),
        })
        .unwrap();
        let frame = r.on_pam_step(Some("hunter2".into())).unwrap();
        match frame {
            Some(CompositorToBroker::ConvResponse { response }) => {
                assert_eq!(response.expose(), "hunter2");
            }
            other => panic!("expected Some(ConvResponse), got {other:?}"),
        }
    }

    #[test]
    fn display_response_is_swallowed_not_forwarded_to_broker() {
        // Epic #24 R5: after the broker has forwarded a ConvDisplay
        // to the greeter, the greeter's mandated greetd-side response
        // MUST NOT cross the broker boundary. `on_pam_step` returns
        // `Ok(None)` (swallow); the broker is in `AwaitWorker` and
        // sending a ConvResponse would trigger UnexpectedFrame.
        //
        // This test pins the EXACT behaviour the gen-399 production
        // bug was caused by drifting from.
        let mut r = begun();
        r.on_broker_frame(BrokerToCompositor::ConvDisplay {
            style: DisplayStyle::Info,
            message: "Please touch the device".into(),
        })
        .unwrap();
        // DMS sends `Greetd.respond("")` for every greetd auth_message
        // regardless of type. The compositor lifts that as
        // `Action::Pam { response: Some("") }` (or None depending on
        // the greeter), and `on_pam_step` MUST swallow it.
        assert_eq!(
            r.on_pam_step(Some(String::new())).unwrap(),
            None,
            "ConvDisplay response MUST be swallowed (R5)"
        );
        // After the swallow, the flag is cleared: the next conv
        // (e.g. a real prompt) is forwarded normally.
        r.on_broker_frame(BrokerToCompositor::ConvPrompt {
            style: PromptStyle::Secret,
            message: "Password: ".into(),
        })
        .unwrap();
        assert!(matches!(
            r.on_pam_step(Some("hunter2".into())).unwrap(),
            Some(CompositorToBroker::ConvResponse { .. })
        ));
    }

    #[test]
    fn display_response_swallowed_even_if_greeter_sends_response_none() {
        // Defensive: some greeters could send a None-content response
        // for a display message rather than an empty string. The
        // swallow MUST trigger on the awaiting_display_ack flag, not
        // on the content of the response.
        let mut r = begun();
        r.on_broker_frame(BrokerToCompositor::ConvDisplay {
            style: DisplayStyle::Error,
            message: "Authentication failure".into(),
        })
        .unwrap();
        assert_eq!(r.on_pam_step(None).unwrap(), None);
    }

    #[test]
    fn display_then_prompt_then_display_clears_and_rearms_flag() {
        // Epic #24 R5: re-arm semantics for the awaiting_display_ack
        // one-shot flag across a D→P→D interleave (gen-400 review
        // Q-I1). Each ConvDisplay arms the swallow; the intervening
        // ConvPrompt clears it (its greeter response is a real
        // ConvResponse to forward); the second ConvDisplay re-arms
        // it; the second greeter "respond" must again be swallowed.
        // Without re-arm, the second display's response would be
        // forwarded as a ConvResponse against AwaitWorker.
        let mut r = begun();

        // D₁: arm swallow.
        r.on_broker_frame(BrokerToCompositor::ConvDisplay {
            style: DisplayStyle::Info,
            message: "Please touch the device".into(),
        })
        .unwrap();
        assert_eq!(r.on_pam_step(Some(String::new())).unwrap(), None);

        // P: clear (no-op here; swallow already cleared) and forward.
        r.on_broker_frame(BrokerToCompositor::ConvPrompt {
            style: PromptStyle::Visible,
            message: "login: ".into(),
        })
        .unwrap();
        assert!(matches!(
            r.on_pam_step(Some("alice".into())).unwrap(),
            Some(CompositorToBroker::ConvResponse { .. })
        ));

        // D₂: re-arm swallow AFTER the prompt cleared it.
        r.on_broker_frame(BrokerToCompositor::ConvDisplay {
            style: DisplayStyle::Error,
            message: "Touch timed out, retrying".into(),
        })
        .unwrap();
        assert_eq!(
            r.on_pam_step(Some(String::new())).unwrap(),
            None,
            "second ConvDisplay's response MUST be swallowed (re-arm)"
        );
    }

    // ── inbound: BrokerToCompositor → RelayEvent ─────────────────────

    #[test]
    fn conv_prompt_maps_to_challenge() {
        let mut r = begun();
        let ev = r
            .on_broker_frame(BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "Password: ".into(),
            })
            .unwrap();
        assert_eq!(
            ev,
            RelayEvent::Pam(PamStep::Challenge {
                kind: AuthMessageType::Secret,
                prompt: "Password: ".into(),
            })
        );
    }

    #[test]
    fn success_passes_pam_identity_through_verbatim_r8() {
        // R8: the {username,uid,gid} from the broker's post-stack
        // pam_get_user → pwent is passed through UNCHANGED. The
        // client-supplied "alice" is never substituted; a
        // username-rewriting stack ("alice.canonical") survives.
        let mut r = begun();
        let ev = r
            .on_broker_frame(BrokerToCompositor::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            })
            .unwrap();
        assert_eq!(
            ev,
            RelayEvent::Pam(PamStep::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            })
        );
    }

    #[test]
    fn failure_maps_to_pam_failure_and_is_terminal() {
        let mut r = begun();
        let ev = r
            .on_broker_frame(BrokerToCompositor::Failure {
                reason: "authentication failed".into(),
            })
            .unwrap();
        assert_eq!(
            ev,
            RelayEvent::Pam(PamStep::Failure {
                reason: "authentication failed".into(),
            })
        );
        // Terminal: a further step is out of phase.
        assert_eq!(r.on_pam_step(None), Err(RelayError::OutOfPhase));
    }

    #[test]
    fn full_lifecycle_success_start_opened_ended() {
        let mut r = begun();
        // challenge → response → success
        r.on_broker_frame(BrokerToCompositor::ConvPrompt {
            style: PromptStyle::Secret,
            message: "pw".into(),
        })
        .unwrap();
        let cr = r.on_pam_step(Some("pw".into())).unwrap();
        assert!(matches!(cr, Some(CompositorToBroker::ConvResponse { .. })));
        r.on_broker_frame(BrokerToCompositor::Success {
            username: "alice.canonical".into(),
            uid: 1001,
            gid: 1001,
        })
        .unwrap();
        // start_session (A1: only after Success)
        let ss = r
            .start_session(
                vec!["niri".into()],
                &["XDG_SESSION_TYPE=wayland".to_string()],
            )
            .unwrap();
        assert_eq!(
            ss,
            CompositorToBroker::StartSession {
                cmd: vec!["niri".into()],
                env: vec![("XDG_SESSION_TYPE".into(), "wayland".into())],
            }
        );
        // opened → running
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::SessionOpened)
                .unwrap(),
            RelayEvent::SessionOpened
        );
        // ended → terminal
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Exited { code: 0 },
            })
            .unwrap(),
            RelayEvent::SessionEnded(SessionOutcome::Exited { code: 0 })
        );
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::SessionOpened),
            Err(RelayError::OutOfPhase)
        );
    }

    #[test]
    fn session_ended_signaled_outcome_preserved() {
        let mut r = authed();
        r.start_session(vec!["niri".into()], &[]).unwrap();
        r.on_broker_frame(BrokerToCompositor::SessionOpened)
            .unwrap();
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Signaled { signal: 9 },
            })
            .unwrap(),
            RelayEvent::SessionEnded(SessionOutcome::Signaled { signal: 9 })
        );
    }

    // ── start_session env splitting ──────────────────────────────────

    #[test]
    fn start_session_splits_env_at_first_equals() {
        let mut r = authed();
        let ss = r
            .start_session(
                vec!["sh".into(), "-c".into()],
                &[
                    "A=b".to_string(),
                    "NOEQ".to_string(),
                    "K=v=w".to_string(),
                    "EMPTY=".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(
            ss,
            CompositorToBroker::StartSession {
                cmd: vec!["sh".into(), "-c".into()],
                env: vec![
                    ("A".into(), "b".into()),
                    ("NOEQ".into(), String::new()),
                    ("K".into(), "v=w".into()),
                    ("EMPTY".into(), String::new()),
                ],
            }
        );
    }

    // ── style mapping (Epic #24: split prompt + display, exhaustive) ─

    #[test]
    fn prompt_style_maps_both_kinds() {
        // Post-Epic-#24: PromptStyle is narrowed to {Visible, Secret}.
        // Each must land on the matching greetd AuthMessageType.
        for (style, kind) in [
            (PromptStyle::Visible, AuthMessageType::Visible),
            (PromptStyle::Secret, AuthMessageType::Secret),
        ] {
            let mut r = begun();
            assert_eq!(
                r.on_broker_frame(BrokerToCompositor::ConvPrompt {
                    style,
                    message: "m".into(),
                })
                .unwrap(),
                RelayEvent::Pam(PamStep::Challenge {
                    kind,
                    prompt: "m".into(),
                })
            );
        }
    }

    #[test]
    fn display_style_maps_both_kinds() {
        // DisplayStyle::Info / Error each route through ConvDisplay and
        // land on the matching greetd AuthMessageType. The state phase
        // STAYS in Authing for display messages (R4 invariant) — they
        // do not move the relay toward Authed or Done.
        for (style, kind) in [
            (DisplayStyle::Info, AuthMessageType::Info),
            (DisplayStyle::Error, AuthMessageType::Error),
        ] {
            let mut r = begun();
            assert_eq!(
                r.on_broker_frame(BrokerToCompositor::ConvDisplay {
                    style,
                    message: "m".into(),
                })
                .unwrap(),
                RelayEvent::Pam(PamStep::Challenge {
                    kind,
                    prompt: "m".into(),
                })
            );
            // Phase invariant: a display frame doesn't advance us out
            // of Authing — a subsequent ConvPrompt or Success must
            // still be accepted.
            assert_eq!(
                r.on_broker_frame(BrokerToCompositor::Success {
                    username: "alice".into(),
                    uid: 1001,
                    gid: 1001,
                })
                .unwrap(),
                RelayEvent::Pam(PamStep::Success {
                    username: "alice".into(),
                    uid: 1001,
                    gid: 1001,
                })
            );
        }
    }

    #[test]
    fn conv_display_after_success_is_out_of_phase() {
        // Mirror of conv_prompt_after_success_is_out_of_phase: a
        // display frame outside Authing is fail-closed.
        let mut r = authed();
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::ConvDisplay {
                style: DisplayStyle::Info,
                message: "stale info".into(),
            }),
            Err(RelayError::OutOfPhase)
        );
    }

    // ── fail-closed: every out-of-phase frame is rejected ────────────

    #[test]
    fn success_before_begin_auth_is_out_of_phase() {
        let mut r = relay();
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::Success {
                username: "x".into(),
                uid: 1,
                gid: 1,
            }),
            Err(RelayError::OutOfPhase)
        );
    }

    #[test]
    fn conv_prompt_after_success_is_out_of_phase() {
        let mut r = authed();
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "m".into(),
            }),
            Err(RelayError::OutOfPhase)
        );
    }

    #[test]
    fn double_success_is_out_of_phase() {
        let mut r = authed();
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::Success {
                username: "x".into(),
                uid: 1,
                gid: 1,
            }),
            Err(RelayError::OutOfPhase)
        );
    }

    #[test]
    fn start_session_before_success_is_out_of_phase() {
        let mut r0 = relay();
        assert_eq!(
            r0.start_session(vec!["niri".into()], &[]),
            Err(RelayError::OutOfPhase)
        );
        let mut r1 = begun();
        assert_eq!(
            r1.start_session(vec!["niri".into()], &[]),
            Err(RelayError::OutOfPhase)
        );
    }

    #[test]
    fn session_opened_before_start_session_is_out_of_phase() {
        // Authed but StartSession not yet forwarded.
        let mut r = authed();
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::SessionOpened),
            Err(RelayError::OutOfPhase)
        );
    }

    #[test]
    fn session_ended_before_opened_is_out_of_phase() {
        let mut r = authed();
        r.start_session(vec!["niri".into()], &[]).unwrap();
        // Starting, but SessionOpened not yet seen.
        assert_eq!(
            r.on_broker_frame(BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Exited { code: 0 },
            }),
            Err(RelayError::OutOfPhase)
        );
    }

    #[test]
    fn step_after_success_is_out_of_phase() {
        let mut r = authed();
        assert_eq!(r.on_pam_step(None), Err(RelayError::OutOfPhase));
    }
}
