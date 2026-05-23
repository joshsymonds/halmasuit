//! Wire contract for the compositor↔`halmasuit-decoder` video-decode
//! relay (Epic #12).
//!
//! This crate is **pure**: message types plus a length-prefixed codec.
//! It opens no socket, links no FFmpeg, and runs no process control. It
//! is the frozen seam between halmasuit (the compositor; owns DRM
//! master + GLES + composition) and `halmasuit-decoder` (a sandboxed
//! subprocess that decodes video files with rsmpeg and writes raw
//! frames back).
//!
//! Same clean-room posture as `halmasuit-session-ipc`: the shapes are
//! owned here and pinned by the `wire_format_*` drift tests, so an
//! accidental change to the contract fails CI.
//!
//! ## Wire topology
//!
//! `halmasuit` forks `halmasuit-decoder` and connects to it via a
//! single `SOCK_SEQPACKET` socketpair. Each `send`/`recv` pair is one
//! datagram. The decoder lives in a fresh namespace (no network, no
//! mount), under a seccomp-bpf allowlist, with bounded rlimits, and
//! retains only the IPC socket fd + the wallpaper file fd (passed in
//! via SCM_RIGHTS by `halmasuit` before sandbox setup).
//!
//! ## Frame plane vs. control plane
//!
//! Control messages ([`CompositorToDecoder`], [`DecoderToCompositor`]
//! variants other than `FrameHeader`) are small text payloads — JSON
//! with a `u32` length prefix, capped at [`MAX_CONTROL_MSG_BYTES`]
//! (4 KiB).
//!
//! A frame is one ATOMIC `SOCK_SEQPACKET` datagram laid out as
//! `[length_prefix:4][header_json:N][payload_bytes:M]` — the length
//! prefix covers only the JSON header (matching the control framing),
//! then `header.bytes_len` raw RGBA bytes follow IN THE SAME
//! datagram. Atomic-or-not on the wire eliminates the dropped-header
//! / orphan-payload class of bug entirely: the receiver either gets
//! the whole frame in one `recvmsg` or none of it. Atomicity also
//! lets the decoder treat `EAGAIN` on a frame send as "drop this
//! frame, continue" without corrupting the wire (the relay's Phase A
//! pacing model: decoder runs at max speed, compositor consume-and-
//! discard).
//!
//! Phase A sizes each frame to fit in one datagram (which requires
//! `setsockopt(SO_SNDBUF, …)` plus `net.core.wmem_max` sysctl to
//! match — see [`MAX_FRAME_BYTES`]). Phase B (later epic):
//! shared-memory pool for >1080p frames.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Protocol wire-format version. Sent in [`DecoderToCompositor::Ready`]
/// at decoder startup; the compositor verifies the value matches and
/// tears down the connection on mismatch.
///
/// A frame is one atomic `SOCK_SEQPACKET` datagram laid out as
/// `[length_prefix:4][header_json:N][payload_bytes:M]`.
pub const WIRE_VERSION: u8 = 2;

/// Hard ceiling on a single control-plane JSON message.
///
/// Mirrors `halmasuit-session-ipc`'s bounded-prefix posture: the
/// length prefix is rejected before any allocation. Control messages
/// are tiny (a few fields each); 4 KiB is generous.
pub const MAX_CONTROL_MSG_BYTES: u32 = 4 * 1024;

/// Hard ceiling on a single frame payload (the raw bytes following a
/// [`DecoderToCompositor::FrameHeader`]).
///
/// Phase A: ~1080p RGBA8 = 1920·1080·4 = 8.3 MiB. 16 MiB cap leaves
/// headroom for the JSON header datagram and allows up to ~2K RGBA on
/// hosts whose `SO_SNDBUF` permits it. Compositor-side validation MUST
/// reject any [`FrameHeader::bytes_len`] exceeding this constant
/// before reading the body datagram(s).
///
/// Phase B (deferred to a future epic): replace this single-datagram
/// model with a shared-memory pool indexed by slot, supporting 4K and
/// beyond without growing `SO_SNDBUF`.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Length-prefix size used by the codec.
const LENGTH_PREFIX_SIZE: usize = std::mem::size_of::<u32>();

// ============================================================================
// Frame format
// ============================================================================

/// Pixel layout of a decoded frame's payload bytes.
///
/// Phase A: RGBA8 only — the format the smithay `GlesRenderer` consumes
/// directly via `TextureBuffer`. Future variants (`Yuv420`, `Nv12`,
/// `Dmabuf`) belong to later epics and require corresponding compositor-
/// side upload paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameFormat {
    /// 8-bit-per-channel RGBA, tightly packed (`width * 4` bytes per
    /// row). No padding rows; `bytes_len` equals `width * height * 4`.
    Rgba8,
}

// ============================================================================
// Control plane (compositor → decoder)
// ============================================================================

/// Compositor → decoder control messages.
///
/// The wallpaper file fd is passed via `SCM_RIGHTS` on the same socket
/// (alongside, NOT inside, this message). The decoder receives the fd
/// in a separate ancillary message; this type carries only the load
/// policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompositorToDecoder {
    /// Begin decoding the file passed via SCM_RIGHTS. `loop_playback =
    /// true` seeks back to PTS 0 on EOF and continues; `false` emits
    /// [`DecoderToCompositor::EndOfFile`] and waits for further
    /// control.
    LoadFile { loop_playback: bool },

    /// Stop emitting frames; hold current decoder state. Frames in
    /// flight on the socket may still arrive at the relay; the relay
    /// drops them.
    Pause,

    /// Resume emitting frames after [`Self::Pause`].
    Resume,

    /// Seek to the given presentation timestamp (microseconds). On
    /// success, the next emitted frame's `pts_us` is >= the requested
    /// value (snapped to keyframe if necessary).
    Seek { pts_us: i64 },

    /// Clean exit. The decoder closes its file, flushes any pending
    /// frame, and exits with status 0. The compositor reaps via pidfd.
    Shutdown,
}

// ============================================================================
// Data plane (decoder → compositor)
// ============================================================================

/// Decoder → compositor messages.
///
/// Sent on the same `SOCK_SEQPACKET` socket as control. The compositor
/// distinguishes by serde tag.
///
/// A [`Self::FrameHeader`] is immediately followed by `bytes_len`
/// bytes of raw RGBA in the SAME datagram (single atomic recvmsg) —
/// see the crate-level "Frame plane vs. control plane" doc. The relay
/// reads one datagram, decodes the JSON header from the front, then
/// reads `bytes_len` payload bytes from the same buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecoderToCompositor {
    /// First message sent after the decoder finishes sandbox setup and
    /// before any frame. The compositor verifies `wire_version ==
    /// WIRE_VERSION` and tears down on mismatch.
    Ready { wire_version: u8 },

    /// Header for the frame whose raw bytes follow. The compositor
    /// MUST validate every field — particularly `bytes_len <=
    /// MAX_FRAME_BYTES` and `width * height * bytes_per_pixel ==
    /// bytes_len` — before reading the body.
    FrameHeader {
        /// Monotonically increasing per stream; starts at 0 after each
        /// `LoadFile`.
        frame_idx: u64,
        /// Presentation timestamp in microseconds.
        pts_us: i64,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Pixel layout.
        format: FrameFormat,
        /// Length of the payload bytes that follow this header on the
        /// wire. Bounded by [`MAX_FRAME_BYTES`].
        bytes_len: u32,
    },

    /// Stream reached EOF and `loop_playback = false`. The decoder
    /// halts emitting frames; the compositor may send another
    /// [`CompositorToDecoder::LoadFile`] or [`CompositorToDecoder::Shutdown`].
    EndOfFile,

    /// Decoder encountered a fatal error. The decoder will exit shortly
    /// after sending this; the compositor should reap and either
    /// restart (if retry budget remains) or fall back.
    DecoderError {
        code: DecoderErrorCode,
        message: String,
    },
}

/// Categorized error codes from the decoder. Stable wire enum; new
/// variants must be added at the end and the version must bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderErrorCode {
    /// `LoadFile` could not open the passed fd (libavformat returned
    /// an error before parsing).
    OpenFailed,
    /// File opened but uses a codec the decoder cannot handle (e.g.
    /// h265 without a configured decoder).
    UnsupportedCodec,
    /// File opened but bitstream parsing or decoding failed.
    ParseError,
    /// Memory allocation failed inside the sandbox (RLIMIT_AS hit, or
    /// the decoder's frame allocator returned NULL).
    AllocationFailed,
    /// Decoder tripped a seccomp-bpf trap. Should be unreachable in
    /// steady state; if seen, it indicates the allowlist is too tight
    /// for the current rsmpeg path. Logged loudly by the compositor.
    SeccompTrap,
    /// Anything else.
    Internal,
}

// ============================================================================
// Codec — control-plane framing
// ============================================================================

/// Errors from the wire codec. Framing or JSON, never I/O.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The length prefix exceeded [`MAX_CONTROL_MSG_BYTES`]. Rejected
    /// before any allocation.
    #[error("control message length {0} exceeds MAX_CONTROL_MSG_BYTES ({1})")]
    OversizedControl(u32, u32),

    /// A [`DecoderToCompositor::FrameHeader`] declared a `bytes_len`
    /// exceeding [`MAX_FRAME_BYTES`]. Caller (the compositor's relay)
    /// MUST reject and tear down before reading the body.
    #[error("frame bytes_len {0} exceeds MAX_FRAME_BYTES ({1})")]
    OversizedFrame(u32, u32),

    /// A [`DecoderToCompositor::FrameHeader`] is internally
    /// inconsistent — either a zero dimension, or `bytes_len`
    /// doesn't match `width * height * bytes_per_pixel`.
    #[error("malformed frame header: {width}x{height} declares {declared_bytes} bytes")]
    MalformedFrame {
        width: u32,
        height: u32,
        declared_bytes: u32,
    },

    /// A SOCK_SEQPACKET datagram was shorter than its length prefix
    /// claims (truncated mid-message). Caller should tear down.
    #[error("truncated control message: prefix declared {declared} bytes, got {got}")]
    TruncatedControl { declared: u32, got: u32 },

    /// The decoder's `Ready` reported a wire version different from
    /// [`WIRE_VERSION`]. Caller MUST tear the connection down.
    #[error("wire-version mismatch: got {got}, expected {expected}")]
    WireVersionMismatch { got: u8, expected: u8 },

    /// The body wasn't valid JSON or didn't deserialize to the expected
    /// type, or the encode side couldn't serialize the message.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encode a control message to `[length_prefix:4][json_body:N]` wire
/// bytes. One logical message per `SOCK_SEQPACKET` datagram.
///
/// # Errors
///
/// [`CodecError::OversizedControl`] if the body exceeds
/// [`MAX_CONTROL_MSG_BYTES`]; [`CodecError::Json`] if serialization
/// fails.
pub fn encode_control<M: Serialize>(msg: &M) -> Result<Vec<u8>, CodecError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len())
        .map_err(|_| CodecError::OversizedControl(u32::MAX, MAX_CONTROL_MSG_BYTES))?;
    if len > MAX_CONTROL_MSG_BYTES {
        return Err(CodecError::OversizedControl(len, MAX_CONTROL_MSG_BYTES));
    }
    let mut out = Vec::with_capacity(LENGTH_PREFIX_SIZE + body.len());
    out.extend_from_slice(&len.to_ne_bytes());
    out.extend(body);
    Ok(out)
}

/// Attempt to decode one control message from the front of `buf`.
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
pub fn try_decode_control<T: serde::de::DeserializeOwned>(
    buf: &[u8],
) -> Result<Option<(T, usize)>, CodecError> {
    if buf.len() < LENGTH_PREFIX_SIZE {
        return Ok(None);
    }
    let mut len_bytes = [0u8; LENGTH_PREFIX_SIZE];
    len_bytes.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
    let len = u32::from_ne_bytes(len_bytes);
    if len > MAX_CONTROL_MSG_BYTES {
        return Err(CodecError::OversizedControl(len, MAX_CONTROL_MSG_BYTES));
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

/// Encode a frame datagram as one atomic `SOCK_SEQPACKET` payload.
///
/// Wire shape: `[length_prefix:4][header_json:N][payload:M]`. The
/// length prefix covers only the header JSON (matching
/// [`encode_control`]'s prefix semantics, so the receiver's
/// prefix-parsing path is shared); the payload's length is
/// encoded in the JSON header's `bytes_len` field.
///
/// # Errors
///
/// [`CodecError::OversizedFrame`] if `payload.len() > MAX_FRAME_BYTES`;
/// [`CodecError::Json`] if serialization fails;
/// [`CodecError::OversizedControl`] if the header JSON exceeds
/// [`MAX_CONTROL_MSG_BYTES`] (the FrameHeader variant fits easily,
/// but the bound is enforced).
pub fn encode_frame_datagram(
    header: &DecoderToCompositor,
    payload: &[u8],
) -> Result<Vec<u8>, CodecError> {
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| CodecError::OversizedFrame(u32::MAX, MAX_FRAME_BYTES))?;
    if payload_len > MAX_FRAME_BYTES {
        return Err(CodecError::OversizedFrame(payload_len, MAX_FRAME_BYTES));
    }
    let body = serde_json::to_vec(header)?;
    let len = u32::try_from(body.len())
        .map_err(|_| CodecError::OversizedControl(u32::MAX, MAX_CONTROL_MSG_BYTES))?;
    if len > MAX_CONTROL_MSG_BYTES {
        return Err(CodecError::OversizedControl(len, MAX_CONTROL_MSG_BYTES));
    }
    let mut out = Vec::with_capacity(LENGTH_PREFIX_SIZE + body.len() + payload.len());
    out.extend_from_slice(&len.to_ne_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Defensive validation a relay applies to a freshly-decoded
/// [`DecoderToCompositor::FrameHeader`] before reading the payload
/// bytes.
///
/// Checks:
/// - `bytes_len <= MAX_FRAME_BYTES` — over-cap → [`CodecError::OversizedFrame`]
/// - `width > 0` and `height > 0`, and
///   `bytes_len == width * height * bytes_per_pixel(format)` (no
///   trailing padding, no over-allocation) — malformed →
///   [`CodecError::MalformedFrame`]
///
/// # Errors
///
/// As above. The caller (compositor relay) should tear the
/// connection down on either.
pub fn validate_frame_header(
    width: u32,
    height: u32,
    format: FrameFormat,
    bytes_len: u32,
) -> Result<(), CodecError> {
    if bytes_len > MAX_FRAME_BYTES {
        return Err(CodecError::OversizedFrame(bytes_len, MAX_FRAME_BYTES));
    }
    let bpp = match format {
        FrameFormat::Rgba8 => 4u64,
    };
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|wh| wh.checked_mul(bpp))
        .unwrap_or(u64::MAX);
    if width == 0 || height == 0 || expected != u64::from(bytes_len) {
        return Err(CodecError::MalformedFrame {
            width,
            height,
            declared_bytes: bytes_len,
        });
    }
    Ok(())
}

// ============================================================================
// Tests — wire_format_*: pin the JSON shape; an accidental field rename
// or serde-attribute change fails CI.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T, expected_json: &str)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let encoded = encode_control(value).expect("encode");
        // Skip the 4-byte length prefix to inspect the body shape.
        let body = std::str::from_utf8(&encoded[LENGTH_PREFIX_SIZE..]).expect("utf-8");
        assert_eq!(body, expected_json, "wire format drift");
        let (decoded, consumed) = try_decode_control::<T>(&encoded)
            .expect("decode")
            .expect("complete");
        assert_eq!(&decoded, value);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn wire_format_load_file() {
        roundtrip(
            &CompositorToDecoder::LoadFile {
                loop_playback: true,
            },
            r#"{"type":"load_file","loop_playback":true}"#,
        );
    }

    #[test]
    fn wire_format_pause() {
        roundtrip(&CompositorToDecoder::Pause, r#"{"type":"pause"}"#);
    }

    #[test]
    fn wire_format_resume() {
        roundtrip(&CompositorToDecoder::Resume, r#"{"type":"resume"}"#);
    }

    #[test]
    fn wire_format_seek() {
        roundtrip(
            &CompositorToDecoder::Seek { pts_us: 1_500_000 },
            r#"{"type":"seek","pts_us":1500000}"#,
        );
    }

    #[test]
    fn wire_format_shutdown() {
        roundtrip(&CompositorToDecoder::Shutdown, r#"{"type":"shutdown"}"#);
    }

    #[test]
    fn wire_format_ready() {
        roundtrip(
            &DecoderToCompositor::Ready { wire_version: 2 },
            r#"{"type":"ready","wire_version":2}"#,
        );
    }

    #[test]
    fn wire_version_is_current_v2() {
        // Drift gate: keep this constant in sync with the crate-
        // level "Wire topology" doc and the `WIRE_VERSION`
        // constant's doc comment.
        assert_eq!(WIRE_VERSION, 2);
    }

    #[test]
    fn encode_frame_datagram_packs_header_then_payload() {
        let header = DecoderToCompositor::FrameHeader {
            frame_idx: 0,
            pts_us: 0,
            width: 2,
            height: 2,
            format: FrameFormat::Rgba8,
            bytes_len: 16,
        };
        // 2x2 RGBA = 16 bytes; payload distinguishable from JSON.
        let payload: Vec<u8> = (0u8..16u8).collect();
        let datagram = encode_frame_datagram(&header, &payload).expect("encode");
        // Parse the header from the front via the standard control
        // codec; consumed should equal LENGTH_PREFIX + json_len.
        let (decoded, consumed) = try_decode_control::<DecoderToCompositor>(&datagram)
            .expect("decode")
            .expect("complete");
        assert_eq!(decoded, header);
        // The payload bytes are at &datagram[consumed..].
        assert_eq!(&datagram[consumed..], &payload[..]);
    }

    #[test]
    fn encode_frame_datagram_rejects_oversized_payload() {
        let header = DecoderToCompositor::FrameHeader {
            frame_idx: 0,
            pts_us: 0,
            width: 1,
            height: 1,
            format: FrameFormat::Rgba8,
            bytes_len: MAX_FRAME_BYTES + 1,
        };
        let payload = vec![0u8; MAX_FRAME_BYTES as usize + 1];
        match encode_frame_datagram(&header, &payload) {
            Err(CodecError::OversizedFrame(_, _)) => {}
            other => panic!("expected OversizedFrame, got {other:?}"),
        }
    }

    #[test]
    fn wire_format_frame_header() {
        roundtrip(
            &DecoderToCompositor::FrameHeader {
                frame_idx: 42,
                pts_us: 16_667,
                width: 1920,
                height: 1080,
                format: FrameFormat::Rgba8,
                bytes_len: 1920 * 1080 * 4,
            },
            r#"{"type":"frame_header","frame_idx":42,"pts_us":16667,"width":1920,"height":1080,"format":"rgba8","bytes_len":8294400}"#,
        );
    }

    #[test]
    fn wire_format_end_of_file() {
        roundtrip(&DecoderToCompositor::EndOfFile, r#"{"type":"end_of_file"}"#);
    }

    #[test]
    fn wire_format_decoder_error() {
        roundtrip(
            &DecoderToCompositor::DecoderError {
                code: DecoderErrorCode::UnsupportedCodec,
                message: "hevc not enabled".into(),
            },
            r#"{"type":"decoder_error","code":"unsupported_codec","message":"hevc not enabled"}"#,
        );
    }

    #[test]
    fn wire_format_decoder_error_codes_all_variants() {
        // Each variant's wire name is part of the contract.
        for (code, expected) in [
            (DecoderErrorCode::OpenFailed, r#""open_failed""#),
            (DecoderErrorCode::UnsupportedCodec, r#""unsupported_codec""#),
            (DecoderErrorCode::ParseError, r#""parse_error""#),
            (DecoderErrorCode::AllocationFailed, r#""allocation_failed""#),
            (DecoderErrorCode::SeccompTrap, r#""seccomp_trap""#),
            (DecoderErrorCode::Internal, r#""internal""#),
        ] {
            assert_eq!(
                serde_json::to_string(&code).expect("encode"),
                expected,
                "DecoderErrorCode::{code:?} wire name drift"
            );
        }
    }

    #[test]
    fn wire_format_frame_format_all_variants() {
        // Phase A has only one variant; locking the name down now so a
        // future YUV/Dmabuf addition can't accidentally rename rgba8.
        assert_eq!(
            serde_json::to_string(&FrameFormat::Rgba8).expect("encode"),
            r#""rgba8""#,
        );
    }

    // ── Boundary defenses ──────────────────────────────────────────────

    #[test]
    fn encode_control_rejects_oversized() {
        // Constructing a real CompositorToDecoder variant >4KiB is
        // awkward; serialize a synthetic JSON string of the right size.
        let huge = "x".repeat(MAX_CONTROL_MSG_BYTES as usize + 1);
        let err = encode_control(&huge).expect_err("must reject oversize");
        match err {
            CodecError::OversizedControl(actual, max) => {
                assert!(actual > MAX_CONTROL_MSG_BYTES, "actual={actual}");
                assert_eq!(max, MAX_CONTROL_MSG_BYTES);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn try_decode_control_rejects_oversized_prefix() {
        let oversized: u32 = MAX_CONTROL_MSG_BYTES + 1;
        let mut buf = Vec::with_capacity(LENGTH_PREFIX_SIZE);
        buf.extend_from_slice(&oversized.to_ne_bytes());
        let err = try_decode_control::<CompositorToDecoder>(&buf)
            .expect_err("must reject oversize prefix");
        match err {
            CodecError::OversizedControl(actual, max) => {
                assert_eq!(actual, oversized);
                assert_eq!(max, MAX_CONTROL_MSG_BYTES);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn try_decode_control_returns_none_for_short_buffer() {
        // Length prefix not yet complete.
        let result =
            try_decode_control::<CompositorToDecoder>(&[0u8; 2]).expect("ok with short buf");
        assert!(result.is_none());

        // Length prefix complete, body not yet complete.
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_ne_bytes());
        buf.extend_from_slice(b"{partial");
        let result = try_decode_control::<CompositorToDecoder>(&buf).expect("ok with partial body");
        assert!(result.is_none());
    }

    // ── validate_frame_header ─────────────────────────────────────────

    #[test]
    fn validate_frame_header_accepts_well_formed_1080p() {
        assert!(validate_frame_header(1920, 1080, FrameFormat::Rgba8, 1920 * 1080 * 4).is_ok());
    }

    #[test]
    fn validate_frame_header_rejects_oversized() {
        let err = validate_frame_header(8192, 8192, FrameFormat::Rgba8, MAX_FRAME_BYTES + 1)
            .expect_err("oversized");
        assert!(
            matches!(err, CodecError::OversizedFrame(_, _)),
            "expected OversizedFrame, got {err:?}"
        );
    }

    #[test]
    fn validate_frame_header_rejects_size_mismatch() {
        // width * height * 4 != bytes_len
        let err =
            validate_frame_header(1920, 1080, FrameFormat::Rgba8, 1000).expect_err("mismatch");
        assert!(
            matches!(err, CodecError::MalformedFrame { .. }),
            "expected MalformedFrame, got {err:?}"
        );
    }

    #[test]
    fn validate_frame_header_rejects_zero_dimension() {
        let err = validate_frame_header(0, 1080, FrameFormat::Rgba8, 0).expect_err("zero width");
        assert!(
            matches!(err, CodecError::MalformedFrame { .. }),
            "expected MalformedFrame, got {err:?}"
        );

        let err = validate_frame_header(1920, 0, FrameFormat::Rgba8, 0).expect_err("zero height");
        assert!(
            matches!(err, CodecError::MalformedFrame { .. }),
            "expected MalformedFrame, got {err:?}"
        );
    }

    #[test]
    fn validate_frame_header_rejects_overflow_dimensions() {
        // u32::MAX * u32::MAX * 4 would overflow; checked_mul saturates
        // to u64::MAX, which mismatches bytes_len → MalformedFrame.
        let err = validate_frame_header(u32::MAX, u32::MAX, FrameFormat::Rgba8, MAX_FRAME_BYTES)
            .expect_err("overflow");
        assert!(
            matches!(err, CodecError::MalformedFrame { .. }),
            "expected MalformedFrame, got {err:?}"
        );
    }
}
