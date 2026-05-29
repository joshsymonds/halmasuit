//! Wire contract for the compositor↔`halmasuit-session` PAM-conversation
//! relay.
//!
//! This crate is **pure**: message types plus a length-prefixed codec.
//! It opens no socket, links no PAM, and runs no process control. It is
//! the frozen seam between the unprivileged compositor relay (C) and the
//! privileged `halmasuit-session` broker (B) of the unified session/pamd
//! epic. The compositor never parses the greeter wire protocol on the
//! broker's behalf; it relays exactly these framed messages.
//!
//! Same clean-room posture as `halmasuit-greetd`: the shapes are owned
//! here and pinned by the `wire_format_*` drift tests, so an accidental
//! change to the contract fails CI.
//!
//! VT switching is NOT on this wire: halmasuit owns its home VT directly
//! (opened in its root startup window, the home-VT model in
//! `halmasuit/src/vt_switch.rs`), so the broker has no VT role.

#![forbid(unsafe_code)]

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use zeroize::Zeroizing;

/// A conversation response carrying user-entered credential material.
///
/// Zeroes its heap on drop (`Zeroizing`) so a relayed password does not
/// linger on either side of the seam (Epic #1 R3/R11). Serializes as a
/// bare JSON string; `Debug` is redacted so the secret never reaches a
/// log line. Deliberately implements no `Display`.
#[derive(Clone)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// Borrow the plaintext for handing to the PAM conversation. The
    /// only place the secret is legitimately read.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

impl Eq for Secret {}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Style of a PAM conversation **prompt** — a message that REQUIRES a greeter response.
///
/// Mirrors libpam's two prompt-class `msg_style` codes:
/// `PAM_PROMPT_ECHO_ON` and `PAM_PROMPT_ECHO_OFF`. Deliberately
/// narrowed to the prompt class; display-only messages
/// (`PAM_TEXT_INFO`/`PAM_ERROR_MSG`) use [`DisplayStyle`] and travel as
/// [`BrokerToCompositor::ConvDisplay`] (one-way, never carries a
/// response). The split is type-enforced — at compile time a
/// `Display`-style message cannot accidentally reach a code path that
/// expects a [`CompositorToBroker::ConvResponse`].
///
/// Reference: `pam_conv(3)` and Linux-PAM Application Developers' Guide
/// §6.2. The asymmetry comes directly from libpam: prompts fill
/// `pam_response_t.resp`; display-only messages MUST set `resp = NULL`
/// and the conv MUST still return `PAM_SUCCESS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStyle {
    /// Echoing prompt (`PAM_PROMPT_ECHO_ON`) — usernames, OTPs.
    Visible,
    /// Non-echoing prompt (`PAM_PROMPT_ECHO_OFF`) — passwords.
    Secret,
}

/// Style of a PAM conversation **display-only message** — one-way to the greeter, never carries a response.
///
/// Mirrors libpam's two display-class `msg_style` codes:
/// `PAM_TEXT_INFO` and `PAM_ERROR_MSG`. Display messages travel as
/// [`BrokerToCompositor::ConvDisplay`] — distinct from
/// [`BrokerToCompositor::ConvPrompt`] precisely so the type system
/// makes a `ConvResponse`-for-a-`ConvDisplay` impossible to construct.
/// The greetd wire protocol DOES mandate a `post_auth_message_response`
/// per `auth_message` — the compositor translates `ConvDisplay` into a
/// greetd info/error `auth_message` for the greeter, receives the
/// greeter's required (but content-empty) response, and **swallows**
/// that response so it never crosses the broker boundary. Inside the
/// broker wire, display is strictly one-way.
///
/// Reference: `pam_conv(3)`; greetd `protocol.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayStyle {
    /// Informational text (`PAM_TEXT_INFO`) — e.g. `pam_u2f cue`'s
    /// "Please touch the device" message.
    Info,
    /// Error text (`PAM_ERROR_MSG`) — e.g. "Incorrect password".
    Error,
}

/// Compositor (relay) → `halmasuit-session` broker.
///
/// The compositor never interprets these beyond framing — it relays the
/// greeter's conversation. The broker owns the `pam_handle_t`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompositorToBroker {
    /// Begin a PAM transaction. `username` is only the `pam_start`
    /// hint; the authoritative identity comes back in
    /// [`BrokerToCompositor::Success`] from post-stack `pam_get_user`
    /// (Epic R8) — it is never trusted as the spawn identity.
    BeginAuth { service: String, username: String },
    /// Answer the most recent [`BrokerToCompositor::ConvPrompt`].
    ConvResponse { response: Secret },
    /// Launch the user session after a successful auth. Sent by the
    /// compositor only after [`BrokerToCompositor::Success`]; the
    /// broker runs it on the SAME `pam_handle_t` (Epic R1) as the
    /// preceding auth. `cmd` is argv (`cmd[0]` is the program); `env`
    /// is the explicit key/value set — the broker does NOT inherit its
    /// own environment, and the session-leader env allowlist (Epic
    /// R7/R11, enforced downstream in `halmasuit-session`) still
    /// filters it. Identity is never carried here: it is always the
    /// PAM-resolved one from `Success`, never re-asserted by the
    /// compositor.
    ///
    /// Wire tag `start_session` is deliberately disjoint from every
    /// other `CompositorToBroker`/`BrokerToCompositor` tag and the
    /// `worker_*` `WorkerOutcome` tags, so the global tag namespace
    /// stays unambiguous (the untagged `ParentMessage` demux and the
    /// broker's frame routing both rely on this).
    ///
    /// A whole frame (cmd+env) is bounded by [`MAX_MESSAGE_SIZE`]
    /// (1 MiB) like every other message; a realistic session
    /// environment is a few KiB, well under the cap.
    StartSession {
        cmd: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// Abort the in-flight auth. The broker SIGKILLs its auth fork and
    /// `pam_end`s the transaction (Epic R4/R5).
    Cancel,
    /// Phase B v2: request the broker's process-root file descriptor
    /// for cross-pivot filesystem-view migration. The broker's
    /// response carries `/proc/self/root` as an SCM_RIGHTS ancillary
    /// fd attached to a [`BrokerToCompositor::RootFd`] frame.
    /// Halmasuit then `fchdir(fd) + chroot(".")` to enter the
    /// broker's process root (= rootfs's view), which makes
    /// `/etc/passwd`, `/nix/store`, `/run/systemd` reachable.
    ///
    /// MUST be sent as the FIRST frame on a fresh broker connection,
    /// before any `BeginAuth`. The broker recognizes it and responds
    /// with the root fd then closes the connection — this connection
    /// is dedicated to the root-fd transfer.
    RequestRootFd,
    /// Epic #47 R1: request the broker to spawn the greeter. The
    /// broker reads its OWN `HALMASUIT_GREETER_UID` and
    /// `HALMASUIT_GREETER_COMMAND` env vars (the compositor is
    /// unprivileged and MUST NOT assert spawn policy) and
    /// fork-then-drops a child to that uid, then `execve`s the
    /// configured command. The broker responds with
    /// [`BrokerToCompositor::GreeterSpawned`] carrying the spawned
    /// pid plus an SCM_RIGHTS pidfd for the compositor to signal at
    /// session-start swap time (the same shape Amendment A5.6 uses
    /// for the session leader pidfd).
    ///
    /// Sent on a TRANSIENT broker connection dedicated to the
    /// greeter spawn — analogous to `RequestRootFd`, NOT the
    /// per-greeter-episode relay connection that carries the
    /// auth/session-lifecycle frames (CLAUDE.md hard rule: one
    /// OwnedFd per episode, no sharing).
    SpawnGreeter,
}

/// How a launched user session ended (Amendment A5.2).
///
/// Crash-vs-clean is preserved (the GDM `SESSION_EXITED` /
/// `SESSION_DIED` distinction; deliberately NOT collapsed the way
/// greetd collapses it) so the compositor can pick the right revert
/// UX/policy. Carries NO pid: the compositor is never the leader's
/// parent and never reaps or signals it (Epic R9 / Amendment A5.4) —
/// the broker is the sole reaper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SessionOutcome {
    /// The session leader exited with this status code.
    Exited { code: i32 },
    /// The session leader was terminated by this signal number.
    Signaled { signal: i32 },
}

/// `halmasuit-session` broker → compositor (relay).
///
/// The session-lifecycle variants ([`Self::SessionOpened`],
/// [`Self::SessionEnded`]) are Amendment A5: they exist ONLY here,
/// never in [`CompositorToBroker`]. Trust is strictly one-way —
/// the privileged broker is the sole emitter; the unprivileged
/// compositor is a pure sink and has no frame that asserts session
/// lifecycle (structurally prevents a compromised greeter/client from
/// forging a force-logout or a "session ready" post-login phish).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerToCompositor {
    /// PAM is prompting for a response; relay to the greeter per
    /// `style`. The greeter MUST answer with
    /// [`CompositorToBroker::ConvResponse`] (or [`CompositorToBroker::Cancel`]).
    /// The broker advances to its `AwaitGreeterConvResponse` phase on
    /// this frame.
    ConvPrompt { style: PromptStyle, message: String },
    /// PAM emitted a display-only message (info banner, error text);
    /// the greeter MUST show it and MUST NOT respond on the broker
    /// wire. The compositor handles the greetd-side mandated
    /// `post_auth_message_response` and swallows it (R5). The broker
    /// does NOT advance phase on this frame — the worker has not
    /// blocked and is already processing the next conv message or PAM
    /// step.
    ///
    /// One-way semantics. Distinct from [`Self::ConvPrompt`] precisely
    /// so the type system rejects any code path that would forward a
    /// `ConvResponse` for a display message.
    ConvDisplay {
        style: DisplayStyle,
        message: String,
    },
    /// PAM completed. `username`/`uid`/`gid` are ONE atomic unit
    /// sourced from post-stack `pam_get_user` → pwent inside the
    /// broker (Epic R8). The compositor cannot and must not re-derive
    /// any of them from another; the shape gives it no opportunity.
    Success {
        username: String,
        uid: u32,
        gid: u32,
    },
    /// PAM rejected the attempt. `reason` is a human string for the
    /// greeter; it carries no identity.
    Failure { reason: String },
    /// The broker forked the session leader and it has `execve`'d
    /// (Amendment A5.2). This is the AUTHORIZATION half of the two-key
    /// flash-free swap: it names the session as live, but the
    /// compositor still gates the *visible* greeter→session swap on
    /// its own observation of the session client's first non-empty
    /// frame (A5.3 — swapping on this frame alone reintroduces the
    /// flash). Carries no pid (A5.4).
    SessionOpened,
    /// The session leader ended; the broker — its parent and the sole
    /// reaper — `waitpid`'d it before `pam_close_session` (Epic R7/R9,
    /// Amendment A5.4/A5.5). `outcome` distinguishes clean exit vs
    /// signal so the compositor can pick the revert UX. The compositor
    /// reverts to the greeter on this OR on the session client's
    /// Wayland disconnect, whichever is first.
    SessionEnded { outcome: SessionOutcome },
    /// Phase B v2: response to [`CompositorToBroker::RequestRootFd`].
    /// The broker's `/proc/self/root` fd is attached as SCM_RIGHTS
    /// ancillary data on the same frame; the compositor extracts it
    /// via `recvmsg` with a cmsg buffer.
    RootFd,
    /// Epic #47 R1: response to [`CompositorToBroker::SpawnGreeter`].
    /// `pid` names the greeter the broker forked + dropped + exec'd
    /// (so the compositor's introspection / logs can refer to it);
    /// the actual signaling capability — a pidfd to send `SIGKILL`
    /// at swap time — travels as SCM_RIGHTS ancillary data on the
    /// same frame. The compositor consumes the pidfd via `recvmsg`
    /// with a cmsg buffer, identical to how it consumes the leader
    /// pidfd in [`Self::SessionOpened`]'s post-Amendment-A5.6 path.
    /// Carrying a bare `pid` (no fd) is forbidden by the same
    /// "no raw leader pid" rule (CLAUDE.md hard rule); the pid is
    /// informational, the pidfd is authority.
    GreeterSpawned { pid: i32 },
}

/// Hard ceiling on a single framed message. Mirrors `halmasuit-greetd`'s
/// codec: the length prefix is rejected before any allocation.
pub const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;

const LENGTH_PREFIX_SIZE: usize = std::mem::size_of::<u32>();

/// Errors from the wire codec. Framing or JSON, never I/O.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The length prefix exceeded [`MAX_MESSAGE_SIZE`]. Rejected before
    /// any allocation.
    #[error("message length {0} exceeds MAX_MESSAGE_SIZE ({1})")]
    OversizedMessage(u32, u32),

    /// The body wasn't valid JSON, didn't deserialize to the expected
    /// type, or the encode side couldn't serialize the message.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encode a message to `[length_prefix:4][json_body:N]` wire bytes.
///
/// One logical message per `SOCK_SEQPACKET` datagram; the length prefix
/// is belt-and-suspenders against a partial read and keeps the codec
/// identical in shape to `halmasuit-greetd`.
///
/// # Errors
///
/// [`CodecError::OversizedMessage`] if the body exceeds
/// [`MAX_MESSAGE_SIZE`]; [`CodecError::Json`] if serialization fails.
pub fn encode<M: Serialize>(msg: &M) -> Result<Vec<u8>, CodecError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len())
        .map_err(|_| CodecError::OversizedMessage(u32::MAX, MAX_MESSAGE_SIZE))?;
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
/// - `Ok(Some((msg, consumed)))` — parsed; advance the read buffer by
///   `consumed`.
/// - `Ok(None)` — `buf` doesn't yet hold a complete message.
/// - `Err(_)` — framing (oversized prefix) or JSON error; the peer
///   connection should be torn down.
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

    // ── wire_format_*: pin the JSON shape ────────────────────────────
    //
    // These payloads ARE the contract. If a field is renamed/retyped or
    // a serde attribute changes, these break — that is the intended
    // drift mitigation now that the broker and compositor depend on a
    // frozen seam.

    #[test]
    fn wire_format_begin_auth() {
        let json = r#"{"type":"begin_auth","service":"halmasuit","username":"alice"}"#;
        let parsed: CompositorToBroker = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            CompositorToBroker::BeginAuth {
                service: "halmasuit".into(),
                username: "alice".into(),
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_conv_response() {
        let json = r#"{"type":"conv_response","response":"hunter2"}"#;
        let parsed: CompositorToBroker = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            CompositorToBroker::ConvResponse {
                response: Secret::new("hunter2".into()),
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_cancel() {
        let json = r#"{"type":"cancel"}"#;
        let parsed: CompositorToBroker = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, CompositorToBroker::Cancel);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_conv_prompt() {
        let json = r#"{"type":"conv_prompt","style":"secret","message":"Password: "}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "Password: ".into(),
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_success_is_atomic_identity() {
        // Epic R8: {username,uid,gid} is ONE atomic unit sourced from
        // post-stack pam_get_user. The parent must never re-derive
        // uid/gid from the name — the shape gives it no opportunity to.
        let json = r#"{"type":"success","username":"alice.canonical","uid":1001,"gid":1001}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            BrokerToCompositor::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_failure() {
        let json = r#"{"type":"failure","reason":"authentication failed"}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            BrokerToCompositor::Failure {
                reason: "authentication failed".into(),
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_prompt_style_all_variants() {
        // PromptStyle is deliberately narrowed to the two prompt-class
        // libpam codes (response REQUIRED). Display-only Info/Error
        // moved to DisplayStyle and travel on a distinct frame; see
        // `wire_format_display_style_all_variants` below.
        for (raw, variant) in [
            (r#""visible""#, PromptStyle::Visible),
            (r#""secret""#, PromptStyle::Secret),
        ] {
            let parsed: PromptStyle = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, variant);
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
        }
    }

    #[test]
    fn wire_format_display_style_all_variants() {
        // DisplayStyle covers libpam's two display-class codes:
        // PAM_TEXT_INFO and PAM_ERROR_MSG. The wire tags MUST match the
        // greetd `auth_message.type` strings so the compositor's
        // greetd-side translation is a 1:1 rename, not a remapping.
        for (raw, variant) in [
            (r#""info""#, DisplayStyle::Info),
            (r#""error""#, DisplayStyle::Error),
        ] {
            let parsed: DisplayStyle = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, variant);
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
        }
    }

    #[test]
    fn wire_format_conv_display() {
        // ConvDisplay frozen-seam test (mirrors wire_format_conv_prompt).
        // This frame is the one-way display-only counterpart of
        // ConvPrompt; the wire tag MUST be `conv_display` and the
        // payload shape MUST match — drift here breaks every greeter
        // that has been compiled against this contract.
        let json = r#"{"type":"conv_display","style":"info","message":"Please touch the device"}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            BrokerToCompositor::ConvDisplay {
                style: DisplayStyle::Info,
                message: "Please touch the device".into(),
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_conv_display_error() {
        // Error-class display message: same shape as info, different
        // tag. Pinned separately so future drift on either variant
        // gets caught.
        let json = r#"{"type":"conv_display","style":"error","message":"Authentication failure"}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            BrokerToCompositor::ConvDisplay {
                style: DisplayStyle::Error,
                message: "Authentication failure".into(),
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_start_session() {
        // The post-auth session-launch frame. cmd is argv (cmd[0] is
        // the program); env is explicit key/value pairs serialized as
        // JSON 2-arrays. This payload IS the contract.
        let json = r#"{"type":"start_session","cmd":["bash","-l"],"env":[["PATH","/usr/bin"],["XDG_SESSION_TYPE","wayland"]]}"#;
        let parsed: CompositorToBroker = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            CompositorToBroker::StartSession {
                cmd: vec!["bash".into(), "-l".into()],
                env: vec![
                    ("PATH".into(), "/usr/bin".into()),
                    ("XDG_SESSION_TYPE".into(), "wayland".into()),
                ],
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn start_session_empty_cmd_and_env_roundtrip() {
        // Degenerate but well-formed: the downstream session-leader
        // validator (not this crate) rejects an empty argv; the wire
        // codec must still round-trip it losslessly.
        let msg = CompositorToBroker::StartSession {
            cmd: vec![],
            env: vec![],
        };
        let bytes = encode(&msg).unwrap();
        let (decoded, consumed): (CompositorToBroker, usize) = try_decode(&bytes).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"start_session","cmd":[],"env":[]}"#
        );
    }

    #[test]
    fn start_session_tag_is_globally_disjoint() {
        // The untagged `ParentMessage` demux (worker.rs) and the
        // broker's frame routing depend on every wire tag being unique
        // across CompositorToBroker ∪ BrokerToCompositor ∪
        // WorkerOutcome. `start_session` collides with none: a
        // start_session datagram must NOT decode as any
        // BrokerToCompositor variant, and no broker/worker tag may
        // decode as a CompositorToBroker.
        let ss = encode(&CompositorToBroker::StartSession {
            cmd: vec!["x".into()],
            env: vec![],
        })
        .unwrap();
        let as_b2c: Result<Option<(BrokerToCompositor, usize)>, _> = try_decode(&ss);
        assert!(
            matches!(as_b2c, Err(CodecError::Json(_))),
            "start_session must not decode as BrokerToCompositor, got {as_b2c:?}"
        );

        for tag in [
            "conv_prompt",
            "success",
            "failure",
            "session_opened",
            "session_ended",
            "root_fd",
            "greeter_spawned",
            "worker_success",
            "worker_failure",
        ] {
            let body = format!(r#"{{"type":"{tag}"}}"#);
            let len = u32::try_from(body.len()).unwrap();
            let mut buf = len.to_ne_bytes().to_vec();
            buf.extend_from_slice(body.as_bytes());
            let as_c2b: Result<Option<(CompositorToBroker, usize)>, _> = try_decode(&buf);
            assert!(
                matches!(as_c2b, Err(CodecError::Json(_))),
                "tag {tag:?} must not decode as CompositorToBroker, got {as_c2b:?}"
            );
        }

        // Conversely: `request_root_fd` and `spawn_greeter` (C→B
        // tags) must NOT decode as any BrokerToCompositor variant.
        // The broker is the recipient of these requests, not the
        // emitter.
        for c2b in [
            CompositorToBroker::RequestRootFd,
            CompositorToBroker::SpawnGreeter,
        ] {
            let bytes = encode(&c2b).unwrap();
            let as_b2c: Result<Option<(BrokerToCompositor, usize)>, _> = try_decode(&bytes);
            assert!(
                matches!(as_b2c, Err(CodecError::Json(_))),
                "{c2b:?} must not decode as BrokerToCompositor, got {as_b2c:?}"
            );
        }
    }

    // ── Amendment A5: broker→compositor session-lifecycle frames ─────
    //
    // One-way: these live ONLY in BrokerToCompositor. The compositor is
    // a pure lifecycle sink (A5.1) — there is no CompositorToBroker
    // lifecycle variant and a session_* datagram must NOT decode as
    // CompositorToBroker. Outcome distinguishes clean exit vs signal
    // (A5.2, GDM SESSION_EXITED/SESSION_DIED); no pid is carried (A5.4
    // — the compositor never reaps/​signals the leader).

    #[test]
    fn wire_format_session_opened() {
        let json = r#"{"type":"session_opened"}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, BrokerToCompositor::SessionOpened);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_session_ended_exited() {
        let json = r#"{"type":"session_ended","outcome":{"result":"exited","code":0}}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Exited { code: 0 },
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_session_ended_signaled() {
        let json = r#"{"type":"session_ended","outcome":{"result":"signaled","signal":9}}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed,
            BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Signaled { signal: 9 },
            }
        );
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_request_root_fd() {
        // Phase B v2 cross-pivot per-process-root migration:
        // C→B side of the broker RootFd handoff. Tagless body, same
        // shape as Cancel.
        let json = r#"{"type":"request_root_fd"}"#;
        let parsed: CompositorToBroker = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, CompositorToBroker::RequestRootFd);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_root_fd() {
        // Phase B v2 cross-pivot per-process-root migration:
        // B→C reply carrying the broker's `/proc/self/root` as an
        // SCM_RIGHTS attachment. The frame body is empty (the fd
        // travels out-of-band on the SCM_RIGHTS control message);
        // the JSON shape is the discriminator only.
        let json = r#"{"type":"root_fd"}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, BrokerToCompositor::RootFd);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_spawn_greeter() {
        // Epic #47 R1: C→B request for the broker to fork-then-drop
        // the greeter. Empty body: the compositor MUST NOT assert
        // spawn policy (greeter_uid + command are read from broker's
        // own env). The tag is the entire payload.
        let json = r#"{"type":"spawn_greeter"}"#;
        let parsed: CompositorToBroker = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, CompositorToBroker::SpawnGreeter);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn wire_format_greeter_spawned() {
        // Epic #47 R1: B→C reply naming the spawned greeter pid. The
        // signaling capability (pidfd) travels as SCM_RIGHTS auxdata
        // on the same frame, identical to SessionOpened's A5.6 path.
        let json = r#"{"type":"greeter_spawned","pid":1234}"#;
        let parsed: BrokerToCompositor = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, BrokerToCompositor::GreeterSpawned { pid: 1234 });
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn broker_to_compositor_only_frames_do_not_cross_decode_as_compositor_to_broker() {
        // Structural anti-forge guarantee: frames the unprivileged
        // compositor must NEVER be able to forge MUST NOT decode as
        // any `CompositorToBroker` variant. Covers two distinct
        // invariants in one structural check, both held by the
        // tagged-enum discriminator:
        //
        //   - Session lifecycle (A5.1): `session_opened` /
        //     `session_ended{outcome=...}` are emitted by the broker
        //     ONLY; the compositor is a pure lifecycle sink. There
        //     is no `CompositorToBroker::Session*` variant.
        //
        //   - Cross-pivot process-root migration (Phase B v2):
        //     `root_fd` is the broker's SCM_RIGHTS-attached grant
        //     of its `/proc/self/root` fd. Only the broker emits
        //     it; the compositor receives + chroots. There is no
        //     `CompositorToBroker::RootFd*` variant.
        //
        // The two are different protocol concerns (session
        // lifecycle vs. fd grant) but the same anti-forge defense
        // (tag-disjointness), tested together.
        for frame in [
            BrokerToCompositor::SessionOpened,
            BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Exited { code: 0 },
            },
            BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Signaled { signal: 15 },
            },
            BrokerToCompositor::RootFd,
            BrokerToCompositor::GreeterSpawned { pid: 4242 },
        ] {
            let bytes = encode(&frame).unwrap();
            let as_c2b: Result<Option<(CompositorToBroker, usize)>, _> = try_decode(&bytes);
            assert!(
                matches!(as_c2b, Err(CodecError::Json(_))),
                "broker-to-compositor-only frame must not decode as \
                 CompositorToBroker, got {as_c2b:?}"
            );
        }
    }

    // ── codec roundtrips ─────────────────────────────────────────────

    #[test]
    fn codec_c2b_roundtrip_every_variant() {
        for msg in [
            CompositorToBroker::BeginAuth {
                service: "halmasuit".into(),
                username: "alice".into(),
            },
            CompositorToBroker::ConvResponse {
                response: Secret::new("hunter2".into()),
            },
            CompositorToBroker::StartSession {
                cmd: vec!["bash".into(), "-l".into()],
                env: vec![("HOME".into(), "/home/alice".into())],
            },
            CompositorToBroker::Cancel,
            CompositorToBroker::RequestRootFd,
            CompositorToBroker::SpawnGreeter,
        ] {
            let bytes = encode(&msg).unwrap();
            let (decoded, consumed): (CompositorToBroker, usize) =
                try_decode(&bytes).unwrap().unwrap();
            assert_eq!(decoded, msg);
            assert_eq!(consumed, bytes.len());
        }
    }

    #[test]
    fn codec_b2c_roundtrip_every_variant() {
        for msg in [
            BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "Password: ".into(),
            },
            BrokerToCompositor::ConvDisplay {
                style: DisplayStyle::Info,
                message: "Please touch the device".into(),
            },
            BrokerToCompositor::ConvDisplay {
                style: DisplayStyle::Error,
                message: "Authentication failure".into(),
            },
            BrokerToCompositor::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            },
            BrokerToCompositor::Failure {
                reason: "denied".into(),
            },
            BrokerToCompositor::SessionOpened,
            BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Exited { code: 0 },
            },
            BrokerToCompositor::SessionEnded {
                outcome: SessionOutcome::Signaled { signal: 9 },
            },
            BrokerToCompositor::RootFd,
            BrokerToCompositor::GreeterSpawned { pid: 9876 },
        ] {
            let bytes = encode(&msg).unwrap();
            let (decoded, consumed): (BrokerToCompositor, usize) =
                try_decode(&bytes).unwrap().unwrap();
            assert_eq!(decoded, msg);
            assert_eq!(consumed, bytes.len());
        }
    }

    #[test]
    fn codec_decode_returns_none_for_short_prefix() {
        let r: Result<Option<(CompositorToBroker, usize)>, CodecError> = try_decode(&[0u8, 0, 0]);
        assert!(matches!(r, Ok(None)));
    }

    #[test]
    fn codec_decode_returns_none_for_partial_body() {
        let bytes = encode(&CompositorToBroker::Cancel).unwrap();
        let truncated = &bytes[..bytes.len() - 1];
        let r: Result<Option<(CompositorToBroker, usize)>, CodecError> = try_decode(truncated);
        assert!(matches!(r, Ok(None)), "got: {r:?}");
    }

    #[test]
    fn codec_decode_consumes_one_message_at_a_time() {
        let a = encode(&CompositorToBroker::BeginAuth {
            service: "halmasuit".into(),
            username: "alice".into(),
        })
        .unwrap();
        let b = encode(&CompositorToBroker::Cancel).unwrap();
        let mut combined = Vec::new();
        combined.extend(&a);
        combined.extend(&b);

        let (first, consumed): (CompositorToBroker, usize) =
            try_decode(&combined).unwrap().unwrap();
        assert_eq!(
            first,
            CompositorToBroker::BeginAuth {
                service: "halmasuit".into(),
                username: "alice".into(),
            }
        );
        assert_eq!(consumed, a.len());

        let (second, consumed2): (CompositorToBroker, usize) =
            try_decode(&combined[consumed..]).unwrap().unwrap();
        assert_eq!(second, CompositorToBroker::Cancel);
        assert_eq!(consumed2, b.len());
    }

    #[test]
    fn codec_decode_rejects_oversized_prefix() {
        let oversized: u32 = MAX_MESSAGE_SIZE + 1;
        let mut buf = Vec::new();
        buf.extend_from_slice(&oversized.to_ne_bytes());
        let r: Result<Option<(CompositorToBroker, usize)>, CodecError> = try_decode(&buf);
        match r {
            Err(CodecError::OversizedMessage(got, max)) => {
                assert_eq!(got, oversized);
                assert_eq!(max, MAX_MESSAGE_SIZE);
            }
            other => panic!("expected OversizedMessage, got {other:?}"),
        }
    }

    #[test]
    fn codec_encode_rejects_oversized_message() {
        let huge = "x".repeat(MAX_MESSAGE_SIZE as usize + 1);
        let r = encode(&BrokerToCompositor::Failure { reason: huge });
        assert!(
            matches!(r, Err(CodecError::OversizedMessage(_, _))),
            "got: {r:?}"
        );
    }

    #[test]
    fn codec_decode_rejects_invalid_json() {
        let len: u32 = 5;
        let mut buf = Vec::new();
        buf.extend_from_slice(&len.to_ne_bytes());
        buf.extend_from_slice(b"xxxxx");
        let r: Result<Option<(BrokerToCompositor, usize)>, CodecError> = try_decode(&buf);
        assert!(matches!(r, Err(CodecError::Json(_))), "got: {r:?}");
    }

    #[test]
    fn codec_garbage_sequences_do_not_panic() {
        // Length prefix advertises a body; body is arbitrary bytes.
        for seed in 0u8..64 {
            let body: Vec<u8> = (0..seed)
                .map(|i| i.wrapping_mul(seed).wrapping_add(7))
                .collect();
            let len = u32::try_from(body.len()).unwrap();
            let mut buf = Vec::new();
            buf.extend_from_slice(&len.to_ne_bytes());
            buf.extend_from_slice(&body);
            // Must return a Result, never panic, for both directions.
            let _: Result<Option<(CompositorToBroker, usize)>, CodecError> = try_decode(&buf);
            let _: Result<Option<(BrokerToCompositor, usize)>, CodecError> = try_decode(&buf);
        }
    }

    // ── secret hygiene (Epic R3/R11) ─────────────────────────────────

    #[test]
    fn secret_debug_redacts_contents() {
        let sentinel = "S3NT1NEL_PLAINTEXT_PW";
        let s = Secret::new(sentinel.into());
        assert!(!format!("{s:?}").contains(sentinel));

        // …including when nested inside a relayed message (the shape
        // the broker/compositor actually log).
        let msg = CompositorToBroker::ConvResponse {
            response: Secret::new(sentinel.into()),
        };
        assert!(!format!("{msg:?}").contains(sentinel));
    }

    #[test]
    fn secret_exposes_its_value_for_the_conversation() {
        let s = Secret::new("hunter2".into());
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn secret_survives_a_wire_roundtrip_intact() {
        let msg = CompositorToBroker::ConvResponse {
            response: Secret::new("p@ss w0rd ☃".into()),
        };
        let bytes = encode(&msg).unwrap();
        let (decoded, _): (CompositorToBroker, usize) = try_decode(&bytes).unwrap().unwrap();
        match decoded {
            CompositorToBroker::ConvResponse { response } => {
                assert_eq!(response.expose(), "p@ss w0rd ☃");
            }
            other => panic!("expected ConvResponse, got {other:?}"),
        }
    }
}
