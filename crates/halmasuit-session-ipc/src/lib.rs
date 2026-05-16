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

/// PAM conversation message style — which UI the greeter should present.
/// Mirrors libpam's four `pam_message` styles (echo-on / echo-off /
/// text-info / error-msg).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStyle {
    /// Echoing prompt (PAM_PROMPT_ECHO_ON).
    Visible,
    /// Non-echoing prompt (PAM_PROMPT_ECHO_OFF) — passwords.
    Secret,
    /// Informational text (PAM_TEXT_INFO); no response collected.
    Info,
    /// Error text (PAM_ERROR_MSG); no response collected.
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
    /// Abort the in-flight auth. The broker SIGKILLs its auth fork and
    /// `pam_end`s the transaction (Epic R4/R5).
    Cancel,
}

/// `halmasuit-session` broker → compositor (relay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerToCompositor {
    /// PAM is prompting; relay to the greeter per `style`.
    ConvPrompt { style: PromptStyle, message: String },
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
        for (raw, variant) in [
            (r#""visible""#, PromptStyle::Visible),
            (r#""secret""#, PromptStyle::Secret),
            (r#""info""#, PromptStyle::Info),
            (r#""error""#, PromptStyle::Error),
        ] {
            let parsed: PromptStyle = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, variant);
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
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
            CompositorToBroker::Cancel,
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
                style: PromptStyle::Info,
                message: "one moment".into(),
            },
            BrokerToCompositor::Success {
                username: "alice.canonical".into(),
                uid: 1001,
                gid: 1001,
            },
            BrokerToCompositor::Failure {
                reason: "denied".into(),
            },
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
