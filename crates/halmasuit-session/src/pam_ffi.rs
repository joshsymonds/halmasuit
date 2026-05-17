//! The libpam FFI shim — the crate's ONLY `unsafe` surface.
//!
//! Successor to `halmasuit-pam`'s FFI (Epic #1 R2/R14): the audited
//! unsafe marshalling techniques (`catch_unwind` across the C boundary,
//! null-guarded pointer walks, `libc::calloc`/`strdup` paired with
//! libpam's `free`, `Zeroizing` of credential buffers, partial-failure
//! rollback) are ported verbatim in spirit from
//! `crates/halmasuit-pam/src/lib.rs`. The difference is the driver
//! model: halmasuit-pam bridged a channel pair owned by an in-compositor
//! worker thread (the deleted C1 design); here the conv callback drives
//! a caller-supplied [`ConvResponder`] so the broker can relay prompts
//! over its SEQPACKET channel from an ephemeral privileged fork.
//!
//! Deliberate divergence from halmasuit-pam: NO `PAM_FAIL_DELAY`
//! override is installed. That existed only because halmasuit-pam ran
//! `pam_authenticate` on the compositor's calloop thread (a freeze
//! hazard). The broker runs it in a disposable fork (Epic R4); failure
//! cost stays delegated to the PAM stack (pam_faillock/pam_faildelay),
//! never reimplemented here (HANDOFF §0.4).
//!
//! No crate/module `#![forbid(unsafe_code)]`: this module needs FFI.
//! Every unsafe block carries `#[expect(unsafe_code, reason = "…")]`, so
//! the workspace `unsafe_code = "warn"` lint (denied under `clippy -D
//! warnings`) still flags any unjustified `unsafe` here or elsewhere.

use std::ffi::{CStr, CString, NulError, c_int, c_void};
use std::marker::PhantomData;
use std::{panic, ptr};

use halmasuit_session_ipc::{BrokerToCompositor, Secret};
use pam_sys::{
    PAM_BUF_ERR, PAM_CONV_ERR, PAM_DELETE_CRED, PAM_ESTABLISH_CRED, PAM_RUSER, PAM_SUCCESS,
    PAM_TTY, PAM_USER, pam_acct_mgmt, pam_authenticate, pam_close_session, pam_conv, pam_end,
    pam_get_item, pam_handle_t, pam_message, pam_open_session, pam_response, pam_set_item,
    pam_setcred, pam_start,
};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::conv;

/// Errors from the libpam FFI. Ported from `halmasuit-pam::PamError`
/// (Epic R14: this is the successor surface).
#[derive(Debug, Error)]
pub enum PamError {
    /// `pam_start` returned a non-success status.
    #[error("pam_start failed: status {0}")]
    Start(c_int),
    /// `pam_set_item` returned a non-success status.
    #[error("pam_set_item failed: status {0}")]
    SetItem(c_int),
    /// `pam_authenticate` returned a non-success status.
    #[error("pam_authenticate failed: status {0}")]
    Authenticate(c_int),
    /// `pam_acct_mgmt` returned a non-success status.
    #[error("pam_acct_mgmt failed: status {0}")]
    AcctMgmt(c_int),
    /// `pam_get_item(PAM_USER)` failed or returned non-UTF-8.
    #[error("pam_get_item(PAM_USER) failed: status {0}")]
    GetUser(c_int),
    /// `pam_setcred(PAM_ESTABLISH_CRED)` returned a non-success status.
    #[error("pam_setcred failed: status {0}")]
    SetCred(c_int),
    /// `pam_open_session` returned a non-success status.
    #[error("pam_open_session failed: status {0}")]
    OpenSession(c_int),
    /// `pam_close_session` returned a non-success status.
    #[error("pam_close_session failed: status {0}")]
    CloseSession(c_int),
    /// A string argument contained an interior NUL.
    #[error("argument contained NUL byte")]
    Nul(#[from] NulError),
}

/// The conversation responder declined or failed for a prompt; the
/// whole conv aborts with `PAM_CONV_ERR`.
#[derive(Debug, Error)]
#[error("conversation responder failed")]
pub struct ResponderError;

/// Produces the response for one PAM prompt.
///
/// Called by [`conv_trampoline`] ONLY for styles where
/// [`conv::style_expects_response`] is true (prompts) — never for
/// display-only `Info`/`Error`. The real broker impl relays the prompt
/// over its SEQPACKET channel; tests use a scripted impl.
pub trait ConvResponder {
    /// # Errors
    ///
    /// [`ResponderError`] aborts the conversation (`PAM_CONV_ERR`).
    fn respond(&mut self, prompt: &BrokerToCompositor) -> Result<Secret, ResponderError>;
}

/// Stable appdata behind `pam_conv::appdata_ptr`.
///
/// The caller owns this and MUST keep it alive for the whole PAM
/// transaction (until [`Pam`] is dropped) — libpam dereferences
/// `appdata_ptr` during the conv callback and during `pam_end`.
pub struct ConvCtx<'r> {
    pub responder: &'r mut dyn ConvResponder,
}

/// Free the first `count` `.resp` entries of a libc-calloc'd
/// `pam_response` array, then the array itself. Used on partial-failure
/// paths in [`conv_trampoline`]. Ported from `halmasuit-pam`.
fn rollback_resp_array(resp_array: *mut pam_response, count: usize) {
    #[expect(
        unsafe_code,
        reason = "rolling back libc allocations on partial-failure paths; \
                  each freed ptr came from libc::calloc/strdup below."
    )]
    unsafe {
        for j in 0..count {
            let prev = (*resp_array.add(j)).resp;
            if !prev.is_null() {
                libc::free(prev.cast::<c_void>());
            }
        }
        libc::free(resp_array.cast::<c_void>());
    }
}

/// The libpam conversation callback. Ported from `halmasuit-pam`'s
/// `bridge_conv` (Epic R14) — same safety construction (`catch_unwind`
/// across the C boundary, null-guarded pointer walks, `libc::calloc`/
/// `strdup` paired with libpam's `free`, `Zeroizing` credential
/// buffers, interior-NUL rejection, partial-failure rollback). The
/// driver differs: a caller-supplied [`ConvResponder`] via
/// [`ConvCtx`], not an in-process channel bridge.
#[expect(
    unsafe_code,
    reason = "extern \"C\" PAM conv callback invoked by libpam on the \
              thread that called pam_authenticate. All panicable paths \
              run inside catch_unwind (unwinding across extern \"C\" is \
              UB). Pointer-safety argued per unsafe block."
)]
unsafe extern "C" fn conv_trampoline(
    num_msg: c_int,
    msg: *mut *const pam_message,
    resp: *mut *mut pam_response,
    appdata_ptr: *mut c_void,
) -> c_int {
    let result = panic::catch_unwind(|| {
        if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
            return PAM_CONV_ERR as c_int;
        }
        let n = usize::try_from(num_msg).unwrap_or(0);

        // SAFETY: appdata_ptr is the ConvCtx the caller stashed in
        // pam_conv::appdata_ptr (Pam::start); it outlives the handle
        // (documented ConvCtx invariant).
        #[expect(
            unsafe_code,
            reason = "appdata_ptr is a ConvCtx kept alive by the caller \
                      for the whole transaction."
        )]
        let ctx: &mut ConvCtx = unsafe { &mut *(appdata_ptr.cast::<ConvCtx>()) };

        let resp_size = std::mem::size_of::<pam_response>();
        // SAFETY: calloc → n zeroed pam_response, or null on OOM. Paired
        // with libpam's free() (it frees .resp and the array).
        #[expect(
            unsafe_code,
            reason = "C allocator paired with libpam's free() on .resp \
                      and the array itself."
        )]
        let resp_array = unsafe { libc::calloc(n, resp_size) }.cast::<pam_response>();
        if resp_array.is_null() {
            return PAM_BUF_ERR as c_int;
        }

        for i in 0..n {
            // SAFETY: msg is a libpam C array of num_msg pam_message
            // pointers; index in-bounds (i<n), null-checked below.
            #[expect(
                unsafe_code,
                reason = "indexing libpam-provided pam_message array; \
                          bounds checked by loop, null checked next."
            )]
            let m_ptr = unsafe { *msg.add(i) };
            if m_ptr.is_null() {
                rollback_resp_array(resp_array, i);
                return PAM_CONV_ERR as c_int;
            }
            // SAFETY: non-null libpam pam_message valid for this call.
            #[expect(
                unsafe_code,
                reason = "libpam guarantees pam_message lives until conv \
                          returns."
            )]
            let m = unsafe { &*m_ptr };
            let text = if m.msg.is_null() {
                String::new()
            } else {
                // SAFETY: NUL-terminated C string owned by libpam.
                #[expect(unsafe_code, reason = "libpam-provided NUL-terminated C string.")]
                let cstr = unsafe { CStr::from_ptr(m.msg) };
                cstr.to_string_lossy().into_owned()
            };

            let Ok(prompt) = conv::prompt_from_pam(m.msg_style, &text) else {
                rollback_resp_array(resp_array, i);
                return PAM_CONV_ERR as c_int;
            };
            let BrokerToCompositor::ConvPrompt { style, .. } = &prompt else {
                // conv::prompt_from_pam only ever yields ConvPrompt.
                rollback_resp_array(resp_array, i);
                return PAM_CONV_ERR as c_int;
            };
            if !conv::style_expects_response(*style) {
                // Display-only (Info/Error): leave the zeroed slot
                // (NULL resp). Never ask the responder.
                continue;
            }

            let Ok(secret) = ctx.responder.respond(&prompt) else {
                rollback_resp_array(resp_array, i);
                return PAM_CONV_ERR as c_int;
            };
            // Wipe every intermediate copy: a Zeroizing buffer, not
            // CString (whose Drop frees without zeroing).
            let mut bytes: Zeroizing<Vec<u8>> = Zeroizing::new(secret.expose().as_bytes().to_vec());
            drop(secret);
            if bytes.contains(&0) {
                // Interior NUL would silently truncate the response and
                // could change the auth outcome — hard error.
                rollback_resp_array(resp_array, i);
                return PAM_CONV_ERR as c_int;
            }
            bytes.push(0);
            // SAFETY: bytes is a single-trailing-NUL C string; strdup
            // copies into libpam-owned memory; bytes wiped on drop.
            #[expect(
                unsafe_code,
                reason = "strdup of a NUL-terminated Zeroizing buffer; \
                          source wiped on drop at iteration end."
            )]
            let dup = unsafe { libc::strdup(bytes.as_ptr().cast::<libc::c_char>()) };
            if dup.is_null() {
                rollback_resp_array(resp_array, i);
                return PAM_BUF_ERR as c_int;
            }
            // SAFETY: resp_array[i] in-bounds, zeroed pam_response.
            #[expect(unsafe_code, reason = "writing in-bounds pam_response slot.")]
            unsafe {
                (*resp_array.add(i)).resp = dup;
                (*resp_array.add(i)).resp_retcode = 0;
            }
        }

        // SAFETY: resp is the libpam out-pointer, valid for this call.
        #[expect(unsafe_code, reason = "writing libpam-provided out-pointer.")]
        unsafe {
            *resp = resp_array;
        }
        PAM_SUCCESS as c_int
    });
    result.unwrap_or(PAM_CONV_ERR as c_int)
}

/// RAII libpam transaction handle.
///
/// Ported from `halmasuit-pam::Pam` (Epic R14). `pam_end` runs in
/// `Drop`; the `_conv` Box and the caller's [`ConvCtx`] must remain
/// valid through it (libpam dereferences both during cleanup) — do not
/// reorder fields or move cleanup into a field's Drop.
pub struct Pam<'c> {
    handle: *mut pam_handle_t,
    _conv: Box<pam_conv>,
    last_status: c_int,
    _ctx: PhantomData<&'c mut ConvCtx<'c>>,
}

impl<'c> Pam<'c> {
    /// Open a PAM transaction for `service` as `username`, wiring
    /// [`conv_trampoline`] to `ctx`. `ctx` MUST outlive the returned
    /// `Pam` (libpam holds `appdata_ptr` until `pam_end`).
    ///
    /// # Errors
    ///
    /// [`PamError::Nul`] for interior NUL in an argument;
    /// [`PamError::Start`] for a non-success libpam status.
    pub fn start(
        service: &str,
        username: &str,
        ctx: &'c mut ConvCtx<'c>,
    ) -> Result<Self, PamError> {
        let service_c = CString::new(service)?;
        let user_c = CString::new(username)?;
        let appdata_ptr: *mut c_void = ptr::from_mut::<ConvCtx>(ctx).cast::<c_void>();
        let conv = Box::new(pam_conv {
            conv: Some(conv_trampoline),
            appdata_ptr,
        });
        let mut handle: *mut pam_handle_t = ptr::null_mut();
        #[expect(
            unsafe_code,
            reason = "FFI: pam_start. service/user CStrings live for the \
                      call; conv's address is Box-stable for self's \
                      lifetime; handle is a valid out-pointer."
        )]
        let status = unsafe {
            pam_start(
                service_c.as_ptr(),
                user_c.as_ptr(),
                ptr::from_ref::<pam_conv>(conv.as_ref()),
                &raw mut handle,
            )
        };
        if status != PAM_SUCCESS as c_int {
            return Err(PamError::Start(status));
        }
        Ok(Self {
            handle,
            _conv: conv,
            last_status: status,
            _ctx: PhantomData,
        })
    }

    fn set_item_str(&mut self, item_type: c_int, value: &str) -> Result<(), PamError> {
        let cstr = CString::new(value)?;
        #[expect(
            unsafe_code,
            reason = "FFI: pam_set_item copies; cstr covers the call."
        )]
        let status =
            unsafe { pam_set_item(self.handle, item_type, cstr.as_ptr().cast::<c_void>()) };
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::SetItem(status))
        }
    }

    /// Set `PAM_TTY` (modules scope rate limits / audit by it).
    ///
    /// # Errors
    /// As [`Self::set_item_str`].
    pub fn set_tty(&mut self, value: &str) -> Result<(), PamError> {
        self.set_item_str(PAM_TTY, value)
    }

    /// Set `PAM_RUSER` (the requesting user — the greeter system user,
    /// never the authenticating user).
    ///
    /// # Errors
    /// As [`Self::set_item_str`].
    pub fn set_ruser(&mut self, value: &str) -> Result<(), PamError> {
        self.set_item_str(PAM_RUSER, value)
    }

    /// Run `pam_authenticate`. Blocks, driving [`conv_trampoline`].
    ///
    /// # Errors
    /// [`PamError::Authenticate`] on non-success (`PAM_AUTH_ERR` is the
    /// common bad-credentials case).
    pub fn authenticate(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_authenticate; handle owned by self, flags=0."
        )]
        let status = unsafe { pam_authenticate(self.handle, 0) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::Authenticate(status))
        }
    }

    /// Run `pam_acct_mgmt`.
    ///
    /// # Errors
    /// [`PamError::AcctMgmt`] on non-success (`PAM_NEW_AUTHTOK_REQD` =
    /// must-change-password).
    pub fn acct_mgmt(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_acct_mgmt; handle owned by self, flags=0."
        )]
        let status = unsafe { pam_acct_mgmt(self.handle, 0) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::AcctMgmt(status))
        }
    }

    /// Run `pam_setcred(PAM_ESTABLISH_CRED)` — establish credentials
    /// (kernel keyring keys, supplementary groups, etc.) on the SAME
    /// handle before `open_session` (Epic R1). The greetd-canonical
    /// ordering: authenticate → acct_mgmt → setcred(ESTABLISH) →
    /// open_session.
    ///
    /// # Errors
    /// [`PamError::SetCred`] on a non-success status.
    pub fn set_cred_established(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_setcred(PAM_ESTABLISH_CRED); handle owned by self."
        )]
        let status = unsafe { pam_setcred(self.handle, PAM_ESTABLISH_CRED as c_int) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::SetCred(status))
        }
    }

    /// Run `pam_open_session` on the SAME handle (Epic R1).
    ///
    /// Requires host-ns root (Epic R2/R6): pam_systemd creates
    /// `/run/user/$UID` and the logind session; pam_mount mounts
    /// `$HOME`. After this the broker `fork`s (NOT `execve` — Epic
    /// R1/R7) the privilege-dropped session leader; the handle stays
    /// in the broker parent to `close_session` at logout. First real
    /// exercise is the credential-passing VM gate (Epic R12 — never
    /// mocked).
    ///
    /// # Errors
    /// [`PamError::OpenSession`] on a non-success status.
    pub fn open_session(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_open_session; handle owned by self, flags=0."
        )]
        let status = unsafe { pam_open_session(self.handle, 0) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::OpenSession(status))
        }
    }

    /// Run `pam_close_session` on the SAME handle at logout (Epic R1),
    /// before `Drop` runs `pam_end`. Unmounts `$HOME` (pam_mount),
    /// tells logind the session ended (pam_systemd) — host-ns root.
    ///
    /// # Errors
    /// [`PamError::CloseSession`] on a non-success status.
    pub fn close_session(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_close_session; handle owned by self, flags=0."
        )]
        let status = unsafe { pam_close_session(self.handle, 0) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::CloseSession(status))
        }
    }

    /// Run `pam_setcred(PAM_DELETE_CRED)` on the SAME handle at logout
    /// (Epic R7): AFTER `close_session`, BEFORE `Drop`'s `pam_end`.
    /// Tears down the credentials `set_cred_established` set (keyring,
    /// supplementary groups). Reuses [`PamError::SetCred`].
    ///
    /// # Errors
    /// [`PamError::SetCred`] on a non-success status.
    pub fn set_cred_deleted(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_setcred(PAM_DELETE_CRED); handle owned by self."
        )]
        let status = unsafe { pam_setcred(self.handle, PAM_DELETE_CRED as c_int) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::SetCred(status))
        }
    }

    /// Read back `PAM_USER` — PAM's canonical post-stack username. Epic
    /// R8: the authoritative identity; the caller resolves uid/gid from
    /// THIS name, never from the pre-auth client string.
    ///
    /// # Errors
    /// [`PamError::GetUser`] on non-success or non-UTF-8.
    pub fn get_user(&mut self) -> Result<String, PamError> {
        let mut item: *const c_void = ptr::null();
        #[expect(
            unsafe_code,
            reason = "FFI: pam_get_item(PAM_USER) into a borrowed out-ptr."
        )]
        let status = unsafe { pam_get_item(self.handle, PAM_USER, &raw mut item) };
        if status != PAM_SUCCESS as c_int {
            return Err(PamError::GetUser(status));
        }
        if item.is_null() {
            return Err(PamError::GetUser(status));
        }
        // SAFETY: PAM_USER item is a NUL-terminated C string owned by
        // libpam, valid until pam_end.
        #[expect(unsafe_code, reason = "libpam-owned NUL-terminated PAM_USER string.")]
        let cstr = unsafe { CStr::from_ptr(item.cast::<libc::c_char>()) };
        cstr.to_str()
            .map(ToOwned::to_owned)
            .map_err(|_| PamError::GetUser(status))
    }
}

impl Drop for Pam<'_> {
    fn drop(&mut self) {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_end closes the transaction. _conv and the \
                      caller's ConvCtx are still valid here (fields drop \
                      after Drop returns; ConvCtx outlives Pam by its \
                      documented invariant)."
        )]
        unsafe {
            pam_end(self.handle, self.last_status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halmasuit_session_ipc::{BrokerToCompositor, PromptStyle};
    use std::ffi::{CString, c_void};

    /// A scripted [`ConvResponder`] for direct trampoline invocation —
    /// NO libpam involved (Epic R12: this mocks nothing; it exercises
    /// our own callback with synthetic inputs).
    struct ScriptedResponder {
        replies: Vec<&'static str>,
        next: usize,
        seen: Vec<BrokerToCompositor>,
        fail: bool,
    }
    impl ConvResponder for ScriptedResponder {
        fn respond(
            &mut self,
            prompt: &BrokerToCompositor,
        ) -> Result<halmasuit_session_ipc::Secret, ResponderError> {
            self.seen.push(prompt.clone());
            if self.fail {
                return Err(ResponderError);
            }
            let r = self.replies[self.next];
            self.next += 1;
            Ok(halmasuit_session_ipc::Secret::new(r.to_owned()))
        }
    }

    /// Build a libpam-style `*mut *const pam_message` array. The
    /// returned `CString`s must outlive the trampoline call.
    fn msgs(
        specs: &[(i32, &str)],
    ) -> (
        Vec<CString>,
        Vec<*const pam_sys::pam_message>,
        Vec<pam_sys::pam_message>,
    ) {
        let cstrings: Vec<CString> = specs
            .iter()
            .map(|(_, t)| CString::new(*t).unwrap())
            .collect();
        let messages: Vec<pam_sys::pam_message> = specs
            .iter()
            .zip(&cstrings)
            .map(|((style, _), cs)| pam_sys::pam_message {
                msg_style: *style,
                msg: cs.as_ptr(),
            })
            .collect();
        let ptrs: Vec<*const pam_sys::pam_message> =
            messages.iter().map(std::ptr::from_ref).collect();
        (cstrings, ptrs, messages)
    }

    /// Free a libpam-style response array the way libpam would, so the
    /// test does not leak what the trampoline allocated.
    fn free_like_pam(resp: *mut pam_sys::pam_response, n: usize) {
        if resp.is_null() {
            return;
        }
        #[expect(
            unsafe_code,
            reason = "test teardown: free what the trampoline calloc/strdup'd, exactly as libpam would."
        )]
        unsafe {
            for i in 0..n {
                let p = (*resp.add(i)).resp;
                if !p.is_null() {
                    libc::free(p.cast::<c_void>());
                }
            }
            libc::free(resp.cast::<c_void>());
        }
    }

    fn run(
        specs: &[(i32, &str)],
        responder: &mut dyn ConvResponder,
    ) -> (i32, *mut pam_sys::pam_response) {
        let (_keep, ptrs, _msgs) = msgs(specs);
        let mut ctx = ConvCtx { responder };
        let mut resp: *mut pam_sys::pam_response = std::ptr::null_mut();
        let n = i32::try_from(specs.len()).unwrap();
        #[expect(
            unsafe_code,
            reason = "directly invoking our own extern \"C\" trampoline with synthetic libpam-shaped inputs; no libpam linked into this test."
        )]
        let rc = unsafe {
            conv_trampoline(
                n,
                ptrs.as_ptr()
                    .cast::<*const pam_sys::pam_message>()
                    .cast_mut(),
                &raw mut resp,
                (&raw mut ctx).cast::<c_void>(),
            )
        };
        (rc, resp)
    }

    #[test]
    fn drift_conv_constants_match_pam_sys() {
        // Pins task #4's hardcoded libpam codes against pam_sys.
        assert_eq!(
            conv::pam_style_of(PromptStyle::Secret),
            pam_sys::PAM_PROMPT_ECHO_OFF
        );
        assert_eq!(
            conv::pam_style_of(PromptStyle::Visible),
            pam_sys::PAM_PROMPT_ECHO_ON
        );
        assert_eq!(
            conv::pam_style_of(PromptStyle::Error),
            pam_sys::PAM_ERROR_MSG
        );
        assert_eq!(
            conv::pam_style_of(PromptStyle::Info),
            pam_sys::PAM_TEXT_INFO
        );
        assert_eq!(
            conv::prompt_style_from_pam(pam_sys::PAM_TEXT_INFO).unwrap(),
            PromptStyle::Info
        );
    }

    #[test]
    fn yields_responses_for_prompt_styles() {
        let mut r = ScriptedResponder {
            replies: vec!["pw", "alice"],
            next: 0,
            seen: vec![],
            fail: false,
        };
        let (rc, resp) = run(
            &[
                (pam_sys::PAM_PROMPT_ECHO_OFF, "Password: "),
                (pam_sys::PAM_PROMPT_ECHO_ON, "Login: "),
            ],
            &mut r,
        );
        assert_eq!(rc, pam_sys::PAM_SUCCESS);
        assert!(!resp.is_null());
        #[expect(
            unsafe_code,
            reason = "reading back the trampoline's response array in-test."
        )]
        unsafe {
            let r0 = std::ffi::CStr::from_ptr((*resp.add(0)).resp);
            let r1 = std::ffi::CStr::from_ptr((*resp.add(1)).resp);
            assert_eq!(r0.to_str().unwrap(), "pw");
            assert_eq!(r1.to_str().unwrap(), "alice");
        }
        free_like_pam(resp, 2);
        assert_eq!(r.seen.len(), 2);
    }

    #[test]
    fn no_response_collected_for_info_and_error() {
        let mut r = ScriptedResponder {
            replies: vec![],
            next: 0,
            seen: vec![],
            fail: false,
        };
        let (rc, resp) = run(
            &[
                (pam_sys::PAM_TEXT_INFO, "one moment"),
                (pam_sys::PAM_ERROR_MSG, "bad thing"),
            ],
            &mut r,
        );
        assert_eq!(rc, pam_sys::PAM_SUCCESS);
        assert!(!resp.is_null());
        #[expect(
            unsafe_code,
            reason = "asserting NULL resp slots for display-only messages."
        )]
        unsafe {
            assert!((*resp.add(0)).resp.is_null());
            assert!((*resp.add(1)).resp.is_null());
        }
        free_like_pam(resp, 2);
        assert!(
            r.seen.is_empty(),
            "responder must not be asked for Info/Error"
        );
    }

    #[test]
    fn unknown_style_is_conv_err() {
        let mut r = ScriptedResponder {
            replies: vec![],
            next: 0,
            seen: vec![],
            fail: false,
        };
        let (rc, resp) = run(&[(99, "weird")], &mut r);
        assert_eq!(rc, pam_sys::PAM_CONV_ERR);
        assert!(resp.is_null(), "no array published on the error path");
        assert!(r.seen.is_empty());
    }

    #[test]
    fn responder_error_is_conv_err() {
        let mut r = ScriptedResponder {
            replies: vec![],
            next: 0,
            seen: vec![],
            fail: true,
        };
        let (rc, resp) = run(&[(pam_sys::PAM_PROMPT_ECHO_OFF, "Password: ")], &mut r);
        assert_eq!(rc, pam_sys::PAM_CONV_ERR);
        assert!(resp.is_null());
    }

    #[test]
    fn null_and_nonpositive_guards_return_conv_err() {
        let mut r = ScriptedResponder {
            replies: vec![],
            next: 0,
            seen: vec![],
            fail: false,
        };
        let mut ctx = ConvCtx { responder: &mut r };
        let mut resp: *mut pam_sys::pam_response = std::ptr::null_mut();
        #[expect(
            unsafe_code,
            reason = "exercising the trampoline's defensive null/<=0 guards."
        )]
        unsafe {
            assert_eq!(
                conv_trampoline(
                    0,
                    std::ptr::null_mut(),
                    &raw mut resp,
                    (&raw mut ctx).cast::<c_void>()
                ),
                pam_sys::PAM_CONV_ERR
            );
            assert_eq!(
                conv_trampoline(
                    1,
                    std::ptr::null_mut(),
                    &raw mut resp,
                    (&raw mut ctx).cast::<c_void>()
                ),
                pam_sys::PAM_CONV_ERR
            );
            assert_eq!(
                conv_trampoline(
                    1,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    (&raw mut ctx).cast::<c_void>()
                ),
                pam_sys::PAM_CONV_ERR
            );
        }
    }

    #[test]
    fn channel_responder_drives_trampoline_end_to_end() {
        // ChannelResponder, behind ConvCtx, drives one synthetic prompt
        // through the real conv_trampoline; the responder really talks
        // over a socketpair to a peer thread (no libpam linked).
        use crate::transport::SeqpacketChannel;
        use halmasuit_session_ipc::{BrokerToCompositor, CompositorToBroker, PromptStyle, Secret};
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
        use std::thread;

        let (sock_a, sock_b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");
        let broker = SeqpacketChannel::new(sock_a);
        let peer = SeqpacketChannel::new(sock_b);

        let peer_thread = thread::spawn(move || {
            let got: BrokerToCompositor = peer.recv().expect("peer recv");
            assert_eq!(
                got,
                BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "PW: ".into(),
                }
            );
            peer.send(&CompositorToBroker::ConvResponse {
                response: Secret::new("s3cr3t".into()),
            })
            .expect("peer send");
        });

        let mut responder = crate::responder::ChannelResponder::new(&broker);
        let mut ctx = ConvCtx {
            responder: &mut responder,
        };
        let text = CString::new("PW: ").unwrap();
        let message = pam_sys::pam_message {
            msg_style: pam_sys::PAM_PROMPT_ECHO_OFF,
            msg: text.as_ptr(),
        };
        let mut mptr: *const pam_sys::pam_message = std::ptr::from_ref(&message);
        let mut resp: *mut pam_sys::pam_response = std::ptr::null_mut();
        #[expect(
            unsafe_code,
            reason = "end-to-end: our own trampoline with one synthetic \
                      prompt; the responder really talks over the \
                      socketpair (no libpam linked into this test)."
        )]
        let rc = unsafe {
            conv_trampoline(
                1,
                &raw mut mptr,
                &raw mut resp,
                (&raw mut ctx).cast::<c_void>(),
            )
        };
        assert_eq!(rc, pam_sys::PAM_SUCCESS);
        #[expect(
            unsafe_code,
            reason = "read back then free the trampoline's response array, as libpam would."
        )]
        unsafe {
            let got = std::ffi::CStr::from_ptr((*resp).resp);
            assert_eq!(got.to_str().unwrap(), "s3cr3t");
            libc::free((*resp).resp.cast::<c_void>());
            libc::free(resp.cast::<c_void>());
        }
        peer_thread.join().unwrap();
    }

    #[test]
    fn session_phase_pam_errors_display_status_not_credentials() {
        // The session-phase variants exist and Display the call name +
        // status only — never anything credential-ish. Their real
        // libpam exercise (host-ns root) is the R7/credential-passing
        // VM gate (Epic R12 forbids mocking PAM here).
        for (e, needle) in [
            (PamError::SetCred(7), "pam_setcred"),
            (PamError::OpenSession(9), "pam_open_session"),
            (PamError::CloseSession(3), "pam_close_session"),
        ] {
            let shown = e.to_string();
            assert!(shown.contains(needle), "got {shown:?}");
            assert!(
                !shown.to_lowercase().contains("password")
                    && !shown.to_lowercase().contains("secret"),
                "error must not leak credentials: {shown:?}"
            );
        }
    }
}
