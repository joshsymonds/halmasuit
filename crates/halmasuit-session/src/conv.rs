//! Pure translation between libpam's conversation model and the
//! `halmasuit-session-ipc` frames.
//!
//! Data-only: this operates on libpam's `msg_style` integer codes and
//! `&str`, never on raw pointers, so the crate stays
//! `#![forbid(unsafe_code)]` and links no PAM here. The (later) unsafe
//! `extern "C"` conv callback is a thin shim over these functions —
//! keeping the mapping logic testable in isolation, off the FFI path.
//!
//! ## Contract: prompt vs. display is type-distinct
//!
//! libpam's four `msg_style` codes split cleanly along response policy:
//!
//! - `PAM_PROMPT_ECHO_ON`/`PAM_PROMPT_ECHO_OFF` are **prompts** —
//!   `pam_response_t.resp` MUST be filled, the conv MUST block until
//!   the application supplies it. These map to [`PromptStyle::Visible`]
//!   /[`PromptStyle::Secret`] and travel as
//!   [`BrokerToCompositor::ConvPrompt`].
//! - `PAM_TEXT_INFO`/`PAM_ERROR_MSG` are **display-only** —
//!   `pam_response_t.resp` MUST be `NULL`, the conv MUST NOT block,
//!   and conv still returns `PAM_SUCCESS`. These map to
//!   [`DisplayStyle::Info`]/[`DisplayStyle::Error`] and travel as
//!   [`BrokerToCompositor::ConvDisplay`].
//!
//! [`MessageKind`] is the exhaustive sum of these two cases. Calling
//! code MUST pattern-match it; there is no `_ =>` fallback. Adding a
//! hypothetical fifth `msg_style` (e.g. a future libpam variant) is a
//! compile error at every match site until each one is updated — the
//! whole point of the type split is that the bug-class behind the
//! gen-399 production failure ("info silently swallowed because the
//! style-guard helper was unused") cannot recur silently.
//!
//! Reference: `pam_conv(3)`; Linux-PAM Application Developers' Guide §6.2.
#![forbid(unsafe_code)]

use halmasuit_session_ipc::{DisplayStyle, PromptStyle};
use thiserror::Error;

// libpam message-style codes. Defined verbatim from Linux-PAM's
// `_pam_types.h`; hardcoded with this citation rather than pulling a
// PAM dependency into this pure slice. The (later) FFI task asserts
// these against `pam_sys` so a drift is caught there.
const PAM_PROMPT_ECHO_OFF: i32 = 1;
const PAM_PROMPT_ECHO_ON: i32 = 2;
const PAM_ERROR_MSG: i32 = 3;
const PAM_TEXT_INFO: i32 = 4;

/// Mapping failure between libpam's conversation model and the frames.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConvError {
    /// libpam handed a `msg_style` outside the four documented codes.
    /// Fail closed — never guess a UI for an unknown prompt kind.
    #[error("unknown PAM message style: {0}")]
    UnknownStyle(i32),
}

/// One libpam conversation message, classified by its response policy.
///
/// Returned by [`message_from_pam`]. The two variants are mutually
/// exclusive and exhaustive over libpam's four documented `msg_style`
/// codes — there is no third case. Callers pattern-match and dispatch:
///
/// - [`MessageKind::Prompt`] → block-and-wait for a response; build a
///   [`BrokerToCompositor::ConvPrompt`] frame.
/// - [`MessageKind::Display`] → fire-and-forget to the greeter; build
///   a [`BrokerToCompositor::ConvDisplay`] frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    /// libpam requires a response. Style names the input style
    /// (echoed vs. secret).
    Prompt { style: PromptStyle, message: String },
    /// libpam emitted a display-only message. No response is allowed
    /// or expected; the conv MUST set `resp = NULL` and return
    /// `PAM_SUCCESS`.
    Display {
        style: DisplayStyle,
        message: String,
    },
}

/// Classify a libpam `(msg_style, text)` into a [`MessageKind`].
///
/// Fails closed on any code outside the four documented `msg_style`
/// values: a future variant MUST be added explicitly, never silently
/// defaulted.
///
/// # Errors
///
/// [`ConvError::UnknownStyle`] for an `msg_style` code that is not one
/// of `PAM_PROMPT_ECHO_OFF`, `PAM_PROMPT_ECHO_ON`, `PAM_ERROR_MSG`,
/// `PAM_TEXT_INFO`.
pub fn message_from_pam(msg_style: i32, message: &str) -> Result<MessageKind, ConvError> {
    match msg_style {
        PAM_PROMPT_ECHO_OFF => Ok(MessageKind::Prompt {
            style: PromptStyle::Secret,
            message: message.to_owned(),
        }),
        PAM_PROMPT_ECHO_ON => Ok(MessageKind::Prompt {
            style: PromptStyle::Visible,
            message: message.to_owned(),
        }),
        PAM_ERROR_MSG => Ok(MessageKind::Display {
            style: DisplayStyle::Error,
            message: message.to_owned(),
        }),
        PAM_TEXT_INFO => Ok(MessageKind::Display {
            style: DisplayStyle::Info,
            message: message.to_owned(),
        }),
        other => Err(ConvError::UnknownStyle(other)),
    }
}

/// The libpam `msg_style` code for a [`PromptStyle`]. Inverse of the
/// prompt half of [`message_from_pam`]; the round-trip is identity.
#[must_use]
pub const fn pam_code_of_prompt(style: PromptStyle) -> i32 {
    match style {
        PromptStyle::Secret => PAM_PROMPT_ECHO_OFF,
        PromptStyle::Visible => PAM_PROMPT_ECHO_ON,
    }
}

/// The libpam `msg_style` code for a [`DisplayStyle`]. Inverse of the
/// display half of [`message_from_pam`]; the round-trip is identity.
#[must_use]
pub const fn pam_code_of_display(style: DisplayStyle) -> i32 {
    match style {
        DisplayStyle::Error => PAM_ERROR_MSG,
        DisplayStyle::Info => PAM_TEXT_INFO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_session_ipc::{DisplayStyle, PromptStyle};

    // libpam style codes (pam_appl.h / _pam_types.h):
    const PAM_PROMPT_ECHO_OFF: i32 = 1;
    const PAM_PROMPT_ECHO_ON: i32 = 2;
    const PAM_ERROR_MSG: i32 = 3;
    const PAM_TEXT_INFO: i32 = 4;

    #[test]
    fn each_pam_code_maps_to_the_correct_kind() {
        // Prompt class: response expected, PromptStyle carries the
        // echo policy.
        assert_eq!(
            message_from_pam(PAM_PROMPT_ECHO_OFF, "Password: ").unwrap(),
            MessageKind::Prompt {
                style: PromptStyle::Secret,
                message: "Password: ".into(),
            }
        );
        assert_eq!(
            message_from_pam(PAM_PROMPT_ECHO_ON, "Login: ").unwrap(),
            MessageKind::Prompt {
                style: PromptStyle::Visible,
                message: "Login: ".into(),
            }
        );
        // Display class: NO response, DisplayStyle carries the
        // info-vs-error distinction.
        assert_eq!(
            message_from_pam(PAM_ERROR_MSG, "bad thing").unwrap(),
            MessageKind::Display {
                style: DisplayStyle::Error,
                message: "bad thing".into(),
            }
        );
        assert_eq!(
            message_from_pam(PAM_TEXT_INFO, "Please touch the device").unwrap(),
            MessageKind::Display {
                style: DisplayStyle::Info,
                message: "Please touch the device".into(),
            }
        );
    }

    #[test]
    fn unknown_pam_code_is_an_error_not_a_panic_or_default() {
        // Fail closed — a future libpam variant we don't know about
        // MUST surface as ConvError, never get silently defaulted to a
        // prompt or a display. This is the load-bearing guard against
        // the gen-399 bug class re-emerging via a new style.
        for bad in [0, 5, -1, i32::MAX, i32::MIN] {
            match message_from_pam(bad, "anything") {
                Err(ConvError::UnknownStyle(got)) => assert_eq!(got, bad),
                other => panic!("expected UnknownStyle({bad}), got {other:?}"),
            }
        }
    }

    #[test]
    fn prompt_code_round_trips_through_kind() {
        for code in [PAM_PROMPT_ECHO_OFF, PAM_PROMPT_ECHO_ON] {
            let kind = message_from_pam(code, "x").unwrap();
            let MessageKind::Prompt { style, .. } = kind else {
                panic!("expected Prompt for code {code}");
            };
            assert_eq!(pam_code_of_prompt(style), code);
        }
    }

    #[test]
    fn display_code_round_trips_through_kind() {
        for code in [PAM_ERROR_MSG, PAM_TEXT_INFO] {
            let kind = message_from_pam(code, "x").unwrap();
            let MessageKind::Display { style, .. } = kind else {
                panic!("expected Display for code {code}");
            };
            assert_eq!(pam_code_of_display(style), code);
        }
    }

    #[test]
    fn prompt_style_round_trips_through_pam_code() {
        for style in [PromptStyle::Visible, PromptStyle::Secret] {
            let MessageKind::Prompt {
                style: roundtripped,
                ..
            } = message_from_pam(pam_code_of_prompt(style), "").unwrap()
            else {
                panic!("prompt style {style:?} did not round-trip as Prompt");
            };
            assert_eq!(roundtripped, style);
        }
    }

    #[test]
    fn display_style_round_trips_through_pam_code() {
        for style in [DisplayStyle::Info, DisplayStyle::Error] {
            let MessageKind::Display {
                style: roundtripped,
                ..
            } = message_from_pam(pam_code_of_display(style), "").unwrap()
            else {
                panic!("display style {style:?} did not round-trip as Display");
            };
            assert_eq!(roundtripped, style);
        }
    }

    #[test]
    fn message_from_pam_accepts_empty_text() {
        // libpam permits `m.msg = NULL` (empty message text); the
        // trampoline turns that into `""` before reaching here. The
        // classification MUST still succeed.
        assert!(matches!(
            message_from_pam(PAM_TEXT_INFO, "").unwrap(),
            MessageKind::Display {
                style: DisplayStyle::Info,
                ..
            }
        ));
    }
}
