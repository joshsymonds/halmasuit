//! Env-gated broker wire-frame trace (Epic #42 R4).
//!
//! When `HALMASUIT_BROKER_TRACE_FRAMES=1` is in the broker's
//! environment at startup, every compositor↔broker frame is logged to
//! stderr (the unit's journal) at the broker's existing `tracing_log`
//! shape. When the env is absent or set to anything other than `"1"`,
//! [`emit`] is a no-op — zero cost when off.
//!
//! Secrets do not need explicit redaction here: the
//! [`halmasuit_session_ipc::Secret`] type already implements `Debug` as
//! `Secret(<redacted>)`. Formatting a `CompositorToBroker::ConvResponse`
//! via `{:?}` produces `ConvResponse { response: Secret(<redacted>) }`
//! — the plaintext never enters the log line. The
//! `secret_is_not_in_formatted_line` test pins this invariant.
//!
//! ## Posture
//!
//! Diagnostic-only. Wire is set on `halmasuit-session.service`'s
//! systemd `Environment=` for one diagnostic boot (gen-405), then
//! removed once Layer B analysis has captured what it needs. The Rust
//! code stays in place — off by default — so the lever is available
//! the next time we need it without re-introducing scaffolding.
#![forbid(unsafe_code)]

use std::fmt::Debug;
use std::sync::OnceLock;

/// Wire direction relative to the broker process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Broker → compositor.
    Send,
    /// Compositor → broker.
    Recv,
}

impl Direction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Recv => "recv",
        }
    }
}

/// Pure formatter — testable without env mutation.
///
/// Returns the trace line that would be emitted at this direction for
/// this frame. Secret-bearing frames are safe because
/// [`halmasuit_session_ipc::Secret`]'s `Debug` is `Secret(<redacted>)`.
pub fn format_line<T: Debug>(direction: Direction, frame: &T) -> String {
    format!(
        "wire_trace dir={dir} frame={frame:?}",
        dir = direction.as_str(),
    )
}

/// Pure decision + formatter. Returns `Some(line)` if `enabled`, else
/// `None`. Production calls [`emit`] which consults the env-derived
/// cached flag; tests can call this with a deterministic `enabled`.
pub fn maybe_format<T: Debug>(enabled: bool, direction: Direction, frame: &T) -> Option<String> {
    if enabled {
        Some(format_line(direction, frame))
    } else {
        None
    }
}

/// Lookup `HALMASUIT_BROKER_TRACE_FRAMES` via a caller-supplied env reader.
///
/// Returns `true` iff the value is literally `"1"` — any other value
/// (including `"true"`, `"yes"`, etc.) is `false` so the on/off
/// contract is unambiguous.
///
/// Production passes `|k| std::env::var(k).ok()`. Tests pass a
/// closure with deterministic values.
pub fn enabled_from<F>(lookup: F) -> bool
where
    F: FnOnce(&str) -> Option<String>,
{
    matches!(
        lookup("HALMASUIT_BROKER_TRACE_FRAMES").as_deref(),
        Some("1")
    )
}

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Cached env-derived enabled flag. First call reads
/// `HALMASUIT_BROKER_TRACE_FRAMES`; subsequent calls return the cached
/// value. This avoids per-frame env syscalls AND fixes the trace
/// posture for the lifetime of the broker process — exactly matching
/// the "set on systemd Environment= for one diagnostic boot" workflow.
fn enabled() -> bool {
    *ENABLED.get_or_init(|| enabled_from(|k| std::env::var(k).ok()))
}

/// Emit a wire-trace line if enabled. No-op otherwise.
///
/// Writes to stderr (the unit's journal) via the broker's existing
/// `halmasuit-session: ...` line shape. No `tracing` crate dependency
/// (the broker stays dependency-light per `broker.rs`'s comment near
/// `tracing_log`).
pub fn emit<T: Debug>(direction: Direction, frame: &T) {
    if let Some(line) = maybe_format(enabled(), direction, frame) {
        eprintln!("halmasuit-session: {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_session_ipc::{
        BrokerToCompositor, CompositorToBroker, DisplayStyle, PromptStyle, Secret,
    };

    #[test]
    fn maybe_format_returns_none_when_disabled() {
        let frame = CompositorToBroker::Cancel;
        assert_eq!(maybe_format(false, Direction::Send, &frame), None);
        assert_eq!(maybe_format(false, Direction::Recv, &frame), None);
    }

    #[test]
    fn maybe_format_returns_some_when_enabled() {
        let frame = CompositorToBroker::Cancel;
        let line = maybe_format(true, Direction::Send, &frame).expect("Some when enabled");
        assert!(line.contains("dir=send"), "missing direction: {line}");
        assert!(line.contains("Cancel"), "missing frame tag: {line}");
    }

    #[test]
    fn format_line_includes_direction_send() {
        let line = format_line(Direction::Send, &CompositorToBroker::Cancel);
        assert!(line.contains("dir=send"), "got: {line}");
    }

    #[test]
    fn format_line_includes_direction_recv() {
        let line = format_line(Direction::Recv, &CompositorToBroker::Cancel);
        assert!(line.contains("dir=recv"), "got: {line}");
    }

    #[test]
    fn secret_is_not_in_formatted_line() {
        // The whole reason this trace is safe to leave on is that
        // Secret's Debug is `Secret(<redacted>)`. If a future refactor
        // changes Secret's Debug to print plaintext, this test catches
        // it — and the trace becomes unsafe overnight.
        let sentinel = "S3NT1NEL_PLAINTEXT_THAT_MUST_NEVER_LEAK";
        let frame = CompositorToBroker::ConvResponse {
            response: Secret::new(sentinel.into()),
        };
        let line = format_line(Direction::Recv, &frame);
        assert!(
            !line.contains(sentinel),
            "secret leaked into trace line: {line}",
        );
        assert!(
            line.contains("<redacted>"),
            "Secret debug shape changed: {line}"
        );
    }

    #[test]
    fn conv_prompt_message_is_in_formatted_line() {
        // For non-secret content (prompt text, display text) the trace
        // SHOULD include the bytes — that's the point of capturing.
        let frame = BrokerToCompositor::ConvPrompt {
            style: PromptStyle::Secret,
            message: "Please touch the device".into(),
        };
        let line = format_line(Direction::Send, &frame);
        assert!(line.contains("Please touch the device"), "got: {line}");
        assert!(line.contains("Secret"), "missing style: {line}");
    }

    #[test]
    fn conv_display_error_message_is_in_formatted_line() {
        let frame = BrokerToCompositor::ConvDisplay {
            style: DisplayStyle::Error,
            message: "Authentication failure".into(),
        };
        let line = format_line(Direction::Send, &frame);
        assert!(line.contains("Authentication failure"), "got: {line}");
        assert!(line.contains("Error"), "missing style: {line}");
    }

    #[test]
    fn enabled_from_true_only_for_literal_one() {
        assert!(enabled_from(|_| Some("1".into())), "1 enables");
        assert!(!enabled_from(|_| Some("0".into())), "0 disables");
        assert!(!enabled_from(|_| Some("true".into())), "true disables");
        assert!(!enabled_from(|_| Some("yes".into())), "yes disables");
        assert!(!enabled_from(|_| Some(String::new())), "empty disables");
        assert!(!enabled_from(|_| None), "missing disables");
    }

    #[test]
    fn enabled_from_passes_correct_var_name() {
        // Pin the env var name on a paired wiring test (the integration
        // arm of CLAUDE.md anti-pattern "NO env_lookup mocks without a
        // paired wiring test"). The VM test for halmasuit-session.service
        // asserts the systemd unit's Environment= contains this exact
        // key when the diagnostic trace is on.
        let mut seen: Option<String> = None;
        let _ = enabled_from(|k| {
            seen = Some(k.to_owned());
            None
        });
        assert_eq!(seen.as_deref(), Some("HALMASUIT_BROKER_TRACE_FRAMES"));
    }
}
