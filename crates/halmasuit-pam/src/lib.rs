//! halmasuit-pam — PAM FFI wrapped in a small safe surface.
//!
//! All `unsafe` related to libpam lives in this crate so
//! halmasuit-greetd can stay `#![forbid(unsafe_code)]`. The public
//! surface is the [`Pam`] struct (RAII handle around `pam_handle_t`)
//! and [`PamError`].
//!
//! This crate exposes only what halmasuit-greetd's state machine
//! needs to drive PAM forward: handle lifecycle + `pam_set_item`. The
//! conversation callback bridge that connects the state machine's
//! step-by-step model to PAM's blocking `pam_authenticate` lands in
//! a follow-up task.

#![deny(unsafe_code)]

use libc::{c_int, c_void};
use pam_sys::{
    PAM_CONV_ERR, PAM_SUCCESS, pam_conv, pam_end, pam_handle_t, pam_message, pam_response,
    pam_set_item, pam_start,
};
use std::ffi::{CString, NulError};
use std::ptr;
use thiserror::Error;

/// Errors from PAM FFI.
#[derive(Debug, Error)]
pub enum PamError {
    /// `pam_start` returned a non-success status.
    #[error("pam_start failed: status {0}")]
    Start(c_int),
    /// `pam_set_item` returned a non-success status.
    #[error("pam_set_item failed: status {0}")]
    SetItem(c_int),
    /// An argument contained an interior NUL byte and can't be passed
    /// to libpam as a C string.
    #[error("argument contained NUL byte")]
    Nul(#[from] NulError),
}

impl PamError {
    /// The raw PAM status code, if applicable.
    #[must_use]
    pub const fn status(&self) -> Option<c_int> {
        match self {
            Self::Start(s) | Self::SetItem(s) => Some(*s),
            Self::Nul(_) => None,
        }
    }
}

/// RAII handle for an in-flight PAM transaction.
///
/// Constructed by [`Pam::start`]. The transaction is closed via
/// `pam_end` on drop. `Pam` is `!Send` (inherited from the raw
/// `pam_handle_t` pointer); a worker-thread pattern that uses this
/// must construct the `Pam` on the worker thread itself.
///
/// This struct does NOT yet carry a real conversation callback —
/// the conv field is a stub that returns `PAM_CONV_ERR` for every
/// message. Real conversation handling lands in the follow-up
/// "conv callback bridge" task.
pub struct Pam {
    handle: *mut pam_handle_t,
    // The pam_conv struct must outlive the handle: pam_start stores
    // a pointer to it (does not copy). Keep it pinned via Box so its
    // address is stable, and rely on Drop order (drop() runs first;
    // declared-field destructors run after) to ensure pam_end has
    // already released libpam's reference before _conv drops.
    _conv: Box<pam_conv>,
    last_status: c_int,
}

#[expect(
    unsafe_code,
    reason = "extern \"C\" PAM conv callback. Stub returns PAM_CONV_ERR \
              for every prompt; the real bridge to channels lands in \
              the next task. Safety: takes raw pointers but reads no \
              memory through them and writes nothing."
)]
const unsafe extern "C" fn stub_conv(
    _num_msg: c_int,
    _msg: *mut *const pam_message,
    _resp: *mut *mut pam_response,
    _appdata_ptr: *mut c_void,
) -> c_int {
    PAM_CONV_ERR as c_int
}

impl Pam {
    /// Open a PAM transaction for `service_name` as `username`.
    ///
    /// `service_name` selects which `/etc/pam.d/<name>` file PAM
    /// consults. `username` is the user being authenticated.
    ///
    /// # Errors
    ///
    /// Returns [`PamError::Nul`] if either argument contains an
    /// interior NUL byte, or [`PamError::Start`] if libpam returns a
    /// non-success status.
    pub fn start(service_name: &str, username: &str) -> Result<Self, PamError> {
        let service = CString::new(service_name)?;
        let user = CString::new(username)?;
        let conv = Box::new(pam_conv {
            conv: Some(stub_conv),
            appdata_ptr: ptr::null_mut(),
        });
        let mut handle: *mut pam_handle_t = ptr::null_mut();
        #[expect(
            unsafe_code,
            reason = "FFI: pam_start signature. service/user are valid \
                      NUL-terminated C strings owned by CStrings that \
                      live until this block returns. conv's address is \
                      stable for the lifetime of self (Box). handle is \
                      a valid out-pointer."
        )]
        let status = unsafe {
            pam_start(
                service.as_ptr(),
                user.as_ptr(),
                std::ptr::from_ref::<pam_conv>(conv.as_ref()),
                &raw mut handle,
            )
        };
        if status == PAM_SUCCESS as c_int {
            Ok(Self {
                handle,
                _conv: conv,
                last_status: status,
            })
        } else {
            Err(PamError::Start(status))
        }
    }

    /// Set a PAM item (e.g. `PAM_RUSER`, `PAM_TTY`, `PAM_XDISPLAY`).
    ///
    /// libpam copies the value internally, so the `CString` may drop
    /// after this call returns.
    ///
    /// # Errors
    ///
    /// [`PamError::Nul`] for interior NUL in `value`,
    /// [`PamError::SetItem`] for a non-success libpam status.
    pub fn set_item_str(&mut self, item_type: c_int, value: &str) -> Result<(), PamError> {
        let cstr = CString::new(value)?;
        #[expect(
            unsafe_code,
            reason = "FFI: pam_set_item copies the C string before \
                      returning, so cstr need only live for the call."
        )]
        let status =
            unsafe { pam_set_item(self.handle, item_type, cstr.as_ptr().cast::<c_void>()) };
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::SetItem(status))
        }
    }
}

impl Drop for Pam {
    fn drop(&mut self) {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_end releases libpam-owned state for this \
                      transaction. self.handle was set by a successful \
                      pam_start in Pam::start and is not aliased."
        )]
        unsafe {
            pam_end(self.handle, self.last_status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pam_sys::PAM_RUSER;

    /// `other` is the fallback PAM service that every conformant Linux
    /// PAM stack ships with (`/etc/pam.d/other`). pam_start succeeds
    /// against it regardless of whether the user exists — the failure
    /// point is later, at pam_authenticate, which this task doesn't
    /// touch.
    #[test]
    fn start_against_other_service_succeeds() {
        let pam = Pam::start("other", "nobody").expect("pam_start for 'other'");
        drop(pam);
    }

    #[test]
    fn set_ruser_succeeds() {
        let mut pam = Pam::start("other", "nobody").unwrap();
        pam.set_item_str(PAM_RUSER as c_int, "alice")
            .expect("set PAM_RUSER");
    }

    #[test]
    fn nul_byte_in_service_name_is_rejected() {
        let r = Pam::start("other\0bad", "nobody");
        assert!(matches!(r, Err(PamError::Nul(_))));
    }

    #[test]
    fn nul_byte_in_username_is_rejected() {
        let r = Pam::start("other", "no\0body");
        assert!(matches!(r, Err(PamError::Nul(_))));
    }

    #[test]
    fn nul_byte_in_set_item_value_is_rejected() {
        let mut pam = Pam::start("other", "nobody").unwrap();
        let r = pam.set_item_str(PAM_RUSER as c_int, "ali\0ce");
        assert!(matches!(r, Err(PamError::Nul(_))));
    }
}
