//! Pure translation between libpam's conversation model and the
//! `halmasuit-session-ipc` frames.
//!
//! Data-only: this operates on libpam's `msg_style` integer codes and
//! `&str`, never on raw pointers, so the crate stays
//! `#![forbid(unsafe_code)]` and links no PAM here. The (later) unsafe
//! `extern "C"` conv callback is a thin shim over these functions —
//! keeping the mapping logic testable in isolation, off the FFI path.

use halmasuit_session_ipc::{BrokerToCompositor, PromptStyle};
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

/// The frame [`PromptStyle`] for a libpam `msg_style` code.
///
/// `ECHO_OFF`→`Secret`, `ECHO_ON`→`Visible`, `ERROR_MSG`→`Error`,
/// `TEXT_INFO`→`Info`. Any other code fails closed rather than
/// defaulting a prompt's echo behaviour.
///
/// # Errors
///
/// [`ConvError::UnknownStyle`] for a code outside the four documented
/// libpam styles.
pub const fn prompt_style_from_pam(msg_style: i32) -> Result<PromptStyle, ConvError> {
    match msg_style {
        PAM_PROMPT_ECHO_OFF => Ok(PromptStyle::Secret),
        PAM_PROMPT_ECHO_ON => Ok(PromptStyle::Visible),
        PAM_ERROR_MSG => Ok(PromptStyle::Error),
        PAM_TEXT_INFO => Ok(PromptStyle::Info),
        other => Err(ConvError::UnknownStyle(other)),
    }
}

/// The libpam `msg_style` code for a [`PromptStyle`]. Inverse of
/// [`prompt_style_from_pam`]; the round-trip is identity.
pub const fn pam_style_of(style: PromptStyle) -> i32 {
    match style {
        PromptStyle::Secret => PAM_PROMPT_ECHO_OFF,
        PromptStyle::Visible => PAM_PROMPT_ECHO_ON,
        PromptStyle::Error => PAM_ERROR_MSG,
        PromptStyle::Info => PAM_TEXT_INFO,
    }
}

/// Whether the greeter must collect a response for this prompt.
/// Prompts (`Visible`/`Secret`) do; display-only `Info`/`Error` do not
/// (mirrors greetd's `AuthMessageType` response policy).
pub const fn style_expects_response(style: PromptStyle) -> bool {
    matches!(style, PromptStyle::Visible | PromptStyle::Secret)
}

/// Build the [`BrokerToCompositor::ConvPrompt`] frame for a libpam
/// message.
///
/// # Errors
///
/// [`ConvError::UnknownStyle`] if `msg_style` is not a documented
/// libpam style (the frame is not built for an unknown prompt kind).
pub fn prompt_from_pam(msg_style: i32, msg: &str) -> Result<BrokerToCompositor, ConvError> {
    Ok(BrokerToCompositor::ConvPrompt {
        style: prompt_style_from_pam(msg_style)?,
        message: msg.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_session_ipc::{BrokerToCompositor, PromptStyle};

    // libpam style codes (pam_appl.h / _pam_types.h):
    const PAM_PROMPT_ECHO_OFF: i32 = 1;
    const PAM_PROMPT_ECHO_ON: i32 = 2;
    const PAM_ERROR_MSG: i32 = 3;
    const PAM_TEXT_INFO: i32 = 4;

    #[test]
    fn each_pam_code_maps_to_the_correct_style() {
        assert_eq!(
            prompt_style_from_pam(PAM_PROMPT_ECHO_OFF).unwrap(),
            PromptStyle::Secret
        );
        assert_eq!(
            prompt_style_from_pam(PAM_PROMPT_ECHO_ON).unwrap(),
            PromptStyle::Visible
        );
        assert_eq!(
            prompt_style_from_pam(PAM_ERROR_MSG).unwrap(),
            PromptStyle::Error
        );
        assert_eq!(
            prompt_style_from_pam(PAM_TEXT_INFO).unwrap(),
            PromptStyle::Info
        );
    }

    #[test]
    fn unknown_pam_code_is_an_error_not_a_panic_or_default() {
        for bad in [0, 5, -1, i32::MAX, i32::MIN] {
            match prompt_style_from_pam(bad) {
                Err(ConvError::UnknownStyle(got)) => assert_eq!(got, bad),
                other => panic!("expected UnknownStyle({bad}), got {other:?}"),
            }
        }
    }

    #[test]
    fn pam_code_round_trips_through_style() {
        for code in [
            PAM_PROMPT_ECHO_OFF,
            PAM_PROMPT_ECHO_ON,
            PAM_ERROR_MSG,
            PAM_TEXT_INFO,
        ] {
            let style = prompt_style_from_pam(code).unwrap();
            assert_eq!(pam_style_of(style), code);
        }
    }

    #[test]
    fn style_round_trips_through_pam_code() {
        for style in [
            PromptStyle::Visible,
            PromptStyle::Secret,
            PromptStyle::Info,
            PromptStyle::Error,
        ] {
            assert_eq!(prompt_style_from_pam(pam_style_of(style)).unwrap(), style);
        }
    }

    #[test]
    fn only_prompts_expect_a_response() {
        assert!(style_expects_response(PromptStyle::Visible));
        assert!(style_expects_response(PromptStyle::Secret));
        assert!(!style_expects_response(PromptStyle::Info));
        assert!(!style_expects_response(PromptStyle::Error));
    }

    #[test]
    fn prompt_from_pam_builds_the_expected_frame() {
        let got = prompt_from_pam(PAM_PROMPT_ECHO_OFF, "Password: ").unwrap();
        assert_eq!(
            got,
            BrokerToCompositor::ConvPrompt {
                style: PromptStyle::Secret,
                message: "Password: ".into(),
            }
        );
    }

    #[test]
    fn prompt_from_pam_rejects_unknown_style_without_panicking() {
        let r = prompt_from_pam(99, "whatever");
        assert!(matches!(r, Err(ConvError::UnknownStyle(99))), "got: {r:?}");
    }
}
