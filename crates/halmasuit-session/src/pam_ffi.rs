//! The libpam FFI shim — the crate's ONLY `unsafe` surface.
//!
//! ## PAM conversation contract (Epic #24)
//!
//! [`conv_trampoline`] translates libpam's four `pam_message::msg_style`
//! codes into a [`crate::conv::MessageKind`] (a closed two-case sum:
//! `Prompt(PromptStyle, message)` vs. `Display(DisplayStyle, message)`)
//! and dispatches to the matching [`ConvResponder`] method:
//!
//! - **Prompts** (`PAM_PROMPT_ECHO_ON`/`PAM_PROMPT_ECHO_OFF`) → [`ConvResponder::respond`]
//!   blocks for the greeter's response, fills `resp_array[i].resp`.
//! - **Display-only** (`PAM_TEXT_INFO`/`PAM_ERROR_MSG`) → [`ConvResponder::display`]
//!   fires the frame and returns immediately; the `resp_array[i].resp`
//!   slot stays `NULL` (libpam's documented contract for these styles).
//!
//! Before Epic #24 the trampoline silently swallowed display messages
//! (`continue` past them without forwarding). That left the greeter
//! blind to PAM cues — `pam_u2f cue`'s "Please touch the device" never
//! reached DMS — and the greeter's resulting proactive `ConvResponse`
//! against an `AwaitWorker` broker phase caused the gen-399 production
//! failure (`unexpected frame for the current phase`).
//!
//! The asymmetric forward IS the bug fix; the rest of the FFI safety
//! construction (`catch_unwind` across `extern "C"`, null-guarded
//! pointer walks, `libc::calloc`/`strdup` paired with libpam's `free`,
//! `Zeroizing` of credential buffers, interior-NUL rejection,
//! partial-failure rollback) is preserved unchanged from
//! `halmasuit-pam`'s `bridge_conv` (Epic R14).
//!
//! Authoritative references for the contract: `pam_conv(3)`; Linux-PAM
//! Application Developers' Guide §6.2; OpenSSH `auth-pam.c` (the
//! architectural ancestor — the privileged-broker conv proxy was
//! modelled on its monitor); greetd `protocol.md` (the
//! compositor↔greeter wire that lives on the other side of the broker
//! boundary, which mandates `post_auth_message_response` for every
//! `auth_message` — the compositor swallows it for display-class
//! messages so the asymmetry survives end-to-end).
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

use crate::pam_sys::{
    PAM_BUF_ERR, PAM_CONV_ERR, PAM_DELETE_CRED, PAM_ESTABLISH_CRED, PAM_RUSER, PAM_SUCCESS,
    PAM_TTY, PAM_USER, pam_acct_mgmt, pam_authenticate, pam_close_session, pam_conv, pam_end,
    pam_get_item, pam_getenvlist, pam_handle_t, pam_message, pam_open_session, pam_putenv,
    pam_response, pam_set_item, pam_setcred, pam_start,
};
use halmasuit_session_ipc::{DisplayStyle, PromptStyle, Secret};
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
    /// `pam_putenv` returned a non-success status (Amendment A1.2:
    /// the StartSession env is pushed into the handle BEFORE
    /// `pam_open_session` so pam_systemd/logind register correct env).
    #[error("pam_putenv failed: status {0}")]
    PutEnv(c_int),
    /// `pam_getenvlist` returned NULL (libpam allocation failure).
    /// A successful empty environment is a non-NULL array whose
    /// first element is NULL — that is `Ok(vec![])`, not this error.
    #[error("pam_getenvlist returned NULL")]
    GetEnv,
    /// A string argument contained an interior NUL.
    #[error("argument contained NUL byte")]
    Nul(#[from] NulError),
}

/// Split a libpam `NAME=VALUE` entry (the `pam_getenvlist` form) into
/// its pair. The name is everything up to the FIRST `=`; the value is
/// the verbatim remainder (which may itself contain `=`). Returns
/// `None` if there is no `=` (libpam never emits that — fail closed).
fn split_env_pair(entry: &str) -> Option<(String, String)> {
    let eq = entry.find('=')?;
    Some((entry[..eq].to_owned(), entry[eq + 1..].to_owned()))
}

/// The conversation responder declined or failed for a prompt; the
/// whole conv aborts with `PAM_CONV_ERR`.
#[derive(Debug, Error)]
#[error("conversation responder failed")]
pub struct ResponderError;

/// Produces the response (or fire-and-forget for display) for ONE PAM
/// conversation message.
///
/// libpam's conv contract splits along response policy:
///
/// - **Prompt** (`PAM_PROMPT_ECHO_ON`/`PAM_PROMPT_ECHO_OFF`) →
///   [`Self::respond`]. Blocks until the greeter answers; the returned
///   [`Secret`] becomes the libpam `pam_response_t.resp` for this
///   message.
/// - **Display** (`PAM_TEXT_INFO`/`PAM_ERROR_MSG`) → [`Self::display`].
///   Fire-and-forget: the message goes to the greeter; the conv MUST
///   NOT block; libpam's `pam_response_t.resp` stays `NULL` for this
///   message (the trampoline leaves the slot zeroed).
///
/// The real broker impl relays each over its SEQPACKET channel; tests
/// use a scripted impl. Implementations MUST be sound against the
/// asymmetric semantics — never block in [`Self::display`], never
/// proceed without a real response in [`Self::respond`].
pub trait ConvResponder {
    /// Send a prompt and block for the greeter's response.
    ///
    /// # Errors
    ///
    /// [`ResponderError`] aborts the conversation (`PAM_CONV_ERR`).
    fn respond(&mut self, style: PromptStyle, message: &str) -> Result<Secret, ResponderError>;

    /// Send a display-only message. Returns as soon as the frame is on
    /// the wire — MUST NOT wait for the greeter to reply. (libpam's
    /// `PAM_TEXT_INFO`/`PAM_ERROR_MSG` carry no response data and the
    /// PAM worker is not blocked on the application here.)
    ///
    /// # Errors
    ///
    /// [`ResponderError`] aborts the conversation (`PAM_CONV_ERR`).
    fn display(&mut self, style: DisplayStyle, message: &str) -> Result<(), ResponderError>;
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

            let Ok(kind) = conv::message_from_pam(m.msg_style, &text) else {
                rollback_resp_array(resp_array, i);
                return PAM_CONV_ERR as c_int;
            };
            // Exhaustive over MessageKind: any future variant addition
            // is a compile error here — never a silent swallow. R1/R2/R4
            // from Epic #24.
            let secret = match kind {
                conv::MessageKind::Prompt { style, message } => {
                    let Ok(secret) = ctx.responder.respond(style, &message) else {
                        rollback_resp_array(resp_array, i);
                        return PAM_CONV_ERR as c_int;
                    };
                    secret
                }
                conv::MessageKind::Display { style, message } => {
                    // Display-only (Info/Error): forward to the greeter
                    // (R1 — MUST NOT swallow) but do NOT block (R2).
                    // The resp_array slot stays zeroed: libpam's
                    // contract for PAM_TEXT_INFO/PAM_ERROR_MSG is
                    // `resp = NULL`, and the conv still returns
                    // PAM_SUCCESS.
                    if ctx.responder.display(style, &message).is_err() {
                        rollback_resp_array(resp_array, i);
                        return PAM_CONV_ERR as c_int;
                    }
                    continue;
                }
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

    /// `pam_putenv` one `NAME=VALUE` entry into the handle's PAM
    /// environment (Amendment A1.2). Called for each StartSession env
    /// entry AFTER `set_cred_established` and STRICTLY BEFORE
    /// `open_session`, so pam_systemd/logind register the session
    /// against the correct environment (greetd `worker.rs` ordering).
    ///
    /// # Errors
    /// [`PamError::Nul`] for an interior NUL; [`PamError::PutEnv`] on a
    /// non-success libpam status.
    pub fn putenv(&mut self, name_value: &str) -> Result<(), PamError> {
        let cstr = CString::new(name_value)?;
        #[expect(
            unsafe_code,
            reason = "FFI: pam_putenv copies the NAME=VALUE string; cstr \
                      covers the call; handle owned by self."
        )]
        let status = unsafe { pam_putenv(self.handle, cstr.as_ptr()) };
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::PutEnv(status))
        }
    }

    /// Read the handle's PAM environment via `pam_getenvlist`
    /// (Amendment A1.3). This is the MERGE point: it returns the
    /// StartSession env this process `putenv`'d UNION whatever
    /// pam_env/pam_systemd/pam_mount added during
    /// `setcred`/`open_session`. The caller allowlist-filters it for
    /// the session leader's `execve` env — a blind "use the raw
    /// StartSession env" would clobber the module-added vars, the
    /// forbidden env analogue of a blind `initgroups`.
    ///
    /// libpam `malloc`s the array and each string; this frees both
    /// (the documented application responsibility).
    ///
    /// # Errors
    /// [`PamError::GetEnv`] if libpam returned NULL (allocation
    /// failure). An empty environment is `Ok(vec![])`.
    pub fn getenvlist(&self) -> Result<Vec<(String, String)>, PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_getenvlist; handle owned by self; returns a \
                      libpam-malloc'd NULL-terminated char** the app owns."
        )]
        let list = unsafe { pam_getenvlist(self.handle) };
        if list.is_null() {
            return Err(PamError::GetEnv);
        }
        let mut out = Vec::new();
        let mut i = 0usize;
        loop {
            // SAFETY: `list` is a libpam-malloc'd NULL-terminated array
            // of C strings; walk until the NULL sentinel, indexing
            // in-bounds by construction.
            #[expect(
                unsafe_code,
                reason = "walking libpam's NULL-terminated char** until the \
                          sentinel; each entry NUL-terminated."
            )]
            let entry = unsafe { *list.add(i) };
            if entry.is_null() {
                break;
            }
            // SAFETY: non-NULL libpam-owned NUL-terminated C string.
            #[expect(unsafe_code, reason = "libpam-owned NUL-terminated env string.")]
            let s = unsafe { CStr::from_ptr(entry) }
                .to_string_lossy()
                .into_owned();
            if let Some(pair) = split_env_pair(&s) {
                out.push(pair);
            }
            // SAFETY: free the libpam-malloc'd string (app owns it).
            #[expect(unsafe_code, reason = "free libpam-malloc'd env string (app-owned).")]
            unsafe {
                libc::free(entry.cast::<c_void>());
            }
            i += 1;
        }
        // SAFETY: free the libpam-malloc'd array itself (app owns it).
        #[expect(unsafe_code, reason = "free libpam-malloc'd env array (app-owned).")]
        unsafe {
            libc::free(list.cast::<c_void>());
        }
        Ok(out)
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
    use crate::pam_sys;
    use halmasuit_session_ipc::{BrokerToCompositor, DisplayStyle, PromptStyle};
    use std::ffi::{CString, c_void};

    /// A scripted [`ConvResponder`] for direct trampoline invocation —
    /// NO libpam involved (Epic R12: this mocks nothing; it exercises
    /// our own callback with synthetic inputs).
    ///
    /// Records every observed conversation message — prompt or display
    /// — so tests can assert on the exact dispatch the trampoline did.
    /// Epic #24 R1: the trampoline MUST forward display messages
    /// (info/error) to the responder, not silently drop them.
    struct ScriptedResponder {
        /// Reply texts handed to libpam, one per prompt-class message.
        replies: Vec<&'static str>,
        next: usize,
        /// Every prompt or display the trampoline asked us to handle,
        /// in arrival order.
        seen: Vec<BrokerToCompositor>,
        fail: bool,
    }
    impl ConvResponder for ScriptedResponder {
        fn respond(
            &mut self,
            style: PromptStyle,
            message: &str,
        ) -> Result<halmasuit_session_ipc::Secret, ResponderError> {
            self.seen.push(BrokerToCompositor::ConvPrompt {
                style,
                message: message.to_owned(),
            });
            if self.fail {
                return Err(ResponderError);
            }
            let r = self.replies[self.next];
            self.next += 1;
            Ok(halmasuit_session_ipc::Secret::new(r.to_owned()))
        }
        fn display(&mut self, style: DisplayStyle, message: &str) -> Result<(), ResponderError> {
            self.seen.push(BrokerToCompositor::ConvDisplay {
                style,
                message: message.to_owned(),
            });
            if self.fail {
                return Err(ResponderError);
            }
            Ok(())
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
        // Pins the hardcoded libpam codes in `conv.rs` against the
        // `pam_sys` FFI declarations. If a future Linux-PAM bump
        // renumbers a code, this fails before the bug reaches the
        // privileged path.
        //
        // Post Epic #24: PromptStyle has been narrowed to 2 variants
        // and DisplayStyle was added; both go through `message_from_pam`
        // (which also fails closed on unknown codes — see
        // `conv::tests::unknown_pam_code_is_an_error_not_a_panic_or_default`).
        assert_eq!(
            conv::pam_code_of_prompt(PromptStyle::Secret),
            pam_sys::PAM_PROMPT_ECHO_OFF
        );
        assert_eq!(
            conv::pam_code_of_prompt(PromptStyle::Visible),
            pam_sys::PAM_PROMPT_ECHO_ON
        );
        assert_eq!(
            conv::pam_code_of_display(DisplayStyle::Error),
            pam_sys::PAM_ERROR_MSG
        );
        assert_eq!(
            conv::pam_code_of_display(DisplayStyle::Info),
            pam_sys::PAM_TEXT_INFO
        );
        // Reverse direction: a PAM_TEXT_INFO classifies into the
        // Display arm with Info style. Drift-guards the trampoline's
        // dispatch.
        let kind = conv::message_from_pam(pam_sys::PAM_TEXT_INFO, "").unwrap();
        match kind {
            conv::MessageKind::Display {
                style: DisplayStyle::Info,
                ..
            } => {}
            other => panic!("expected Display{{Info}}, got {other:?}"),
        }
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
    fn info_and_error_are_forwarded_to_display_with_null_resp_slots() {
        // Epic #24 R1: the trampoline MUST forward PAM_TEXT_INFO and
        // PAM_ERROR_MSG to the responder (via `display()`), not silently
        // drop them. Pre-Epic-#24 this test asserted the OPPOSITE
        // ("responder must not be asked") — that was the gen-399 bug.
        //
        // Epic #24 R2: even though display IS called, the conv's
        // `resp_array[i].resp` slot MUST stay NULL — libpam contract
        // for `PAM_TEXT_INFO`/`PAM_ERROR_MSG`.
        let mut r = ScriptedResponder {
            replies: vec![],
            next: 0,
            seen: vec![],
            fail: false,
        };
        let (rc, resp) = run(
            &[
                (pam_sys::PAM_TEXT_INFO, "Please touch the device"),
                (pam_sys::PAM_ERROR_MSG, "Authentication failure"),
            ],
            &mut r,
        );
        assert_eq!(rc, pam_sys::PAM_SUCCESS);
        assert!(!resp.is_null());
        #[expect(
            unsafe_code,
            reason = "asserting NULL resp slots for display-only messages \
                      (libpam contract)."
        )]
        unsafe {
            assert!((*resp.add(0)).resp.is_null());
            assert!((*resp.add(1)).resp.is_null());
        }
        free_like_pam(resp, 2);
        assert_eq!(
            r.seen,
            vec![
                BrokerToCompositor::ConvDisplay {
                    style: DisplayStyle::Info,
                    message: "Please touch the device".into(),
                },
                BrokerToCompositor::ConvDisplay {
                    style: DisplayStyle::Error,
                    message: "Authentication failure".into(),
                },
            ],
            "trampoline MUST forward display-only messages to the \
             responder via display() so the greeter can show cues like \
             `pam_u2f cue`'s 'touch the device'"
        );
    }

    #[test]
    fn display_responder_error_is_conv_err() {
        // If display() fails (e.g. broker channel closed mid-conv) the
        // trampoline MUST surface PAM_CONV_ERR with no resp array
        // published, exactly like the prompt-side error path.
        let mut r = ScriptedResponder {
            replies: vec![],
            next: 0,
            seen: vec![],
            fail: true,
        };
        let (rc, resp) = run(&[(pam_sys::PAM_TEXT_INFO, "Please touch")], &mut r);
        assert_eq!(rc, pam_sys::PAM_CONV_ERR);
        assert!(resp.is_null(), "no array published on the error path");
    }

    #[test]
    fn batched_info_then_prompt_dispatches_correctly() {
        // libpam permits num_msg > 1 with mixed styles in one conv
        // call. The trampoline iterates per-message; an Info followed
        // by a Secret prompt in the SAME call must produce: one
        // display() with the info text + one respond() with the
        // password. The resp_array slot for the info stays NULL; the
        // slot for the prompt holds the strdup'd response.
        //
        // This shape mirrors the production gen-399 conv shape
        // (`pam_u2f cue + pam_unix try_first_pass` can deliver both
        // messages in one conv call when batched).
        let mut r = ScriptedResponder {
            replies: vec!["sekrit"],
            next: 0,
            seen: vec![],
            fail: false,
        };
        let (rc, resp) = run(
            &[
                (pam_sys::PAM_TEXT_INFO, "Please touch the device"),
                (pam_sys::PAM_PROMPT_ECHO_OFF, "Password: "),
            ],
            &mut r,
        );
        assert_eq!(rc, pam_sys::PAM_SUCCESS);
        assert!(!resp.is_null());
        #[expect(
            unsafe_code,
            reason = "reading back trampoline's response array in-test."
        )]
        unsafe {
            assert!(
                (*resp.add(0)).resp.is_null(),
                "info slot must be NULL (display-only)"
            );
            let pw = std::ffi::CStr::from_ptr((*resp.add(1)).resp);
            assert_eq!(pw.to_str().unwrap(), "sekrit");
        }
        free_like_pam(resp, 2);
        assert_eq!(
            r.seen,
            vec![
                BrokerToCompositor::ConvDisplay {
                    style: DisplayStyle::Info,
                    message: "Please touch the device".into(),
                },
                BrokerToCompositor::ConvPrompt {
                    style: PromptStyle::Secret,
                    message: "Password: ".into(),
                },
            ],
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
    fn split_env_pair_splits_on_first_eq_only() {
        // pam_getenvlist yields `NAME=VALUE`; the value may contain
        // further `=` (e.g. base64, DBUS addresses). Name is up to the
        // FIRST `=`, value is the verbatim remainder.
        assert_eq!(
            split_env_pair("PATH=/usr/bin:/bin"),
            Some(("PATH".to_owned(), "/usr/bin:/bin".to_owned()))
        );
        assert_eq!(
            split_env_pair("DBUS=unix:abstract=/tmp/x,guid=ab"),
            Some(("DBUS".to_owned(), "unix:abstract=/tmp/x,guid=ab".to_owned()))
        );
        // Empty value is well-formed (VAR=).
        assert_eq!(
            split_env_pair("EMPTY="),
            Some(("EMPTY".to_owned(), String::new()))
        );
        // No `=` at all: libpam never emits this; fail closed (None).
        assert_eq!(split_env_pair("BOGUS"), None);
        assert_eq!(split_env_pair(""), None);
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
