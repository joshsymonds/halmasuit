//! halmasuit-pam — PAM FFI wrapped in a small safe surface.
//!
//! All `unsafe` related to libpam lives in this crate so
//! halmasuit-greetd can stay `#![forbid(unsafe_code)]`. The public
//! surface is:
//!
//! - [`PamThread`] — production worker-thread driver that owns a PAM
//!   transaction and implements `halmasuit_greetd::PamSession`. This
//!   is what halmasuit-greetd's `Connection` consumes via the
//!   `PamSessionFactory` trait.
//! - [`PamError`] — typed errors from the FFI surface.
//! - [`PromptChallenge`] and [`PamMessageStyle`] — value types
//!   describing one PAM conversation message.
//!
//! Lower-level building blocks (`Pam`, `ConvBridge`, `ConvDriver`,
//! `conv_pair`) are `pub(crate)`: the unsafe quarantine. Everything
//! external should go through `PamThread`.
//!
//! Internally, the conv callback ([`bridge_conv`]) marshals each
//! `pam_message` → [`PromptChallenge`] over a rendezvous channel,
//! waits for a [`Zeroizing<Vec<u8>>`] response, then `libc::calloc`s
//! a `pam_response` array and `libc::strdup`s each response into it
//! for PAM to free. All panicable paths are wrapped in
//! `catch_unwind` because panicking across an `extern "C"` boundary
//! is UB.

#![deny(unsafe_code)]

use halmasuit_greetd::{AuthMessageType, PamSession, PamStep};
use libc::{c_int, c_void};
use pam_sys::{
    PAM_BUF_ERR, PAM_CONV_ERR, PAM_ERROR_MSG, PAM_FAIL_DELAY, PAM_PROMPT_ECHO_OFF,
    PAM_PROMPT_ECHO_ON, PAM_RUSER, PAM_SUCCESS, PAM_TEXT_INFO, PAM_TTY, PAM_USER, pam_acct_mgmt,
    pam_authenticate, pam_conv, pam_end, pam_get_item, pam_handle_t, pam_message, pam_response,
    pam_set_item, pam_start,
};
use std::ffi::{CStr, CString, NulError};
use std::panic;
use std::ptr;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;
use thiserror::Error;
use zeroize::Zeroizing;

// ── PAM message types ───────────────────────────────────────────────────

/// The four message styles PAM's conv callback delivers, narrowed
/// from `pam_message::msg_style` (a raw `c_int` in the bindings).
///
/// Maps 1:1 to [`halmasuit_greetd::AuthMessageType`]; the translation
/// happens in [`translate_style`] inside the [`PamSession`] impl below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PamMessageStyle {
    /// `PAM_PROMPT_ECHO_ON` — visible input (e.g. username).
    PromptEchoOn,
    /// `PAM_PROMPT_ECHO_OFF` — hidden input (password).
    PromptEchoOff,
    /// `PAM_ERROR_MSG` — error banner, no response expected.
    ErrorMsg,
    /// `PAM_TEXT_INFO` — informational banner, no response expected.
    TextInfo,
}

impl PamMessageStyle {
    /// Narrow a raw `msg_style` value from libpam. Both
    /// `pam_message::msg_style` and the PAM_PROMPT_*/PAM_*_MSG/INFO
    /// constants are `c_int` in the generated bindings, so the match
    /// works directly.
    #[must_use]
    pub const fn from_raw(raw: c_int) -> Option<Self> {
        match raw {
            PAM_PROMPT_ECHO_ON => Some(Self::PromptEchoOn),
            PAM_PROMPT_ECHO_OFF => Some(Self::PromptEchoOff),
            PAM_ERROR_MSG => Some(Self::ErrorMsg),
            PAM_TEXT_INFO => Some(Self::TextInfo),
            _ => None,
        }
    }
}

/// One challenge from PAM to the greeter — the marshalled form of a
/// single `pam_message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptChallenge {
    pub style: PamMessageStyle,
    pub message: String,
}

// ── Conv-bridge channels ────────────────────────────────────────────────

/// PAM-side half of the conv channel pair. Owned by [`Pam`]; the
/// `bridge_conv` callback dereferences `pam_conv::appdata_ptr` (a
/// pointer to this) to push challenges and pull responses.
pub(crate) struct ConvBridge {
    challenge_tx: SyncSender<PromptChallenge>,
    response_rx: Receiver<Zeroizing<String>>,
}

/// Outside-the-thread half. [`PamThread`]'s `recv_next` reads
/// challenges from `challenge_rx`; its `step()` impl writes
/// responses to `response_tx`.
pub(crate) struct ConvDriver {
    pub(crate) challenge_rx: Receiver<PromptChallenge>,
    pub(crate) response_tx: SyncSender<Zeroizing<String>>,
}

/// Build a paired bridge + driver. Each channel is rendezvous
/// (capacity 0) so the conv callback and the driver synchronize
/// step-by-step.
#[must_use]
pub(crate) fn conv_pair() -> (ConvBridge, ConvDriver) {
    let (challenge_tx, challenge_rx) = sync_channel(0);
    let (response_tx, response_rx) = sync_channel(0);
    (
        ConvBridge {
            challenge_tx,
            response_rx,
        },
        ConvDriver {
            challenge_rx,
            response_tx,
        },
    )
}

/// Pure-Rust core of the conv: for each prompt, send it on the
/// challenge channel and immediately wait for its response. The
/// interleaved send/recv (vs. batch-send-then-batch-recv) is
/// load-bearing: the future PamSession driver returns one challenge
/// at a time and only sends a response after the state machine
/// processes it. A batched bridge would deadlock the moment
/// `num_msg > 1` because PAM would block sending prompt 2 while the
/// driver tried to send response 1.
///
/// Empty-string fallback if the driver side has hung up — PAM treats
/// an empty `pam_response` as "user provided no answer."
fn process_prompts(prompts: Vec<PromptChallenge>, bridge: &ConvBridge) -> Vec<Zeroizing<String>> {
    let n = prompts.len();
    let mut responses = Vec::with_capacity(n);
    for p in prompts {
        if bridge.challenge_tx.send(p).is_err() {
            while responses.len() < n {
                responses.push(Zeroizing::new(String::new()));
            }
            return responses;
        }
        let r = bridge
            .response_rx
            .recv()
            .unwrap_or_else(|_| Zeroizing::new(String::new()));
        responses.push(r);
    }
    responses
}

// ── Error type ──────────────────────────────────────────────────────────

/// Errors from PAM FFI.
#[derive(Debug, Error)]
pub enum PamError {
    /// `pam_start` returned a non-success status.
    #[error("pam_start failed: status {0}")]
    Start(c_int),
    /// `pam_set_item` returned a non-success status.
    #[error("pam_set_item failed: status {0}")]
    SetItem(c_int),
    /// `pam_authenticate` returned a non-success status. `PAM_AUTH_ERR`
    /// is the common case (bad password / denied).
    #[error("pam_authenticate failed: status {0}")]
    Authenticate(c_int),
    /// `pam_acct_mgmt` returned a non-success status.
    #[error("pam_acct_mgmt failed: status {0}")]
    AcctMgmt(c_int),
    /// `pam_get_item(PAM_USER)` returned a non-success status or
    /// produced a non-UTF-8 value.
    #[error("pam_get_item(PAM_USER) failed: status {0}")]
    GetUser(c_int),
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
            Self::Start(s)
            | Self::SetItem(s)
            | Self::Authenticate(s)
            | Self::AcctMgmt(s)
            | Self::GetUser(s) => Some(*s),
            Self::Nul(_) => None,
        }
    }
}

// ── PAM_FAIL_DELAY override ─────────────────────────────────────────────
//
// libpam's default `pam_fail_delay(3)` causes pam_unix (and many
// other modules) to call `usleep` for a random 1-3 seconds on
// authentication failure. With halmasuit's synchronous PamSession
// trait that runs on the calloop event-loop thread, that sleep
// freezes the entire compositor's event loop — including its ability
// to redraw, accept new connections, and respond to SIGTERM — for
// the duration. The pam_fail_delay(3) man page explicitly says
// event-driven applications "may wish to override" the default by
// registering their own delay callback via
// `pam_set_item(handle, PAM_FAIL_DELAY, callback)`. The callback we
// register does nothing — failed authentication returns immediately
// and the I/O layer can decide what (if anything) to do about
// throttling at a higher level.

#[expect(
    unsafe_code,
    reason = "extern \"C\" PAM fail-delay callback. No-op body; takes \
              raw pointer arguments but reads no memory through them \
              and cannot panic, so no catch_unwind needed."
)]
const unsafe extern "C" fn noop_fail_delay(
    _retval: c_int,
    _delay_usec: libc::c_uint,
    _appdata_ptr: *mut c_void,
) {
}

// ── Conv callback (the unsafe core) ─────────────────────────────────────

/// Free the first `count` `.resp` entries of a libc-calloc'd
/// `pam_response` array, then free the array itself. Used by
/// [`bridge_conv`] when it needs to abort partway through the
/// response-marshalling loop (interior-NUL response or strdup OOM).
fn rollback_resp_array(resp_array: *mut pam_response, count: usize) {
    // SAFETY: resp_array came from libc::calloc in bridge_conv; each
    // .resp entry up to `count` was set from libc::strdup. Both pair
    // with libc::free.
    #[expect(
        unsafe_code,
        reason = "rolling back libc allocations on partial-failure paths."
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

#[expect(
    unsafe_code,
    reason = "extern \"C\" PAM conv callback. The callback is invoked \
              by libpam on the same thread that called pam_authenticate. \
              All panicable paths run inside catch_unwind because \
              unwinding across an extern \"C\" boundary is UB. Pointer \
              safety arguments documented per unsafe block below."
)]
unsafe extern "C" fn bridge_conv(
    num_msg: c_int,
    msg: *mut *const pam_message,
    resp: *mut *mut pam_response,
    appdata_ptr: *mut c_void,
) -> c_int {
    let result = panic::catch_unwind(|| {
        if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
            return PAM_CONV_ERR as c_int;
        }
        // num_msg > 0 by the guard above; cast is non-negative.
        let n = usize::try_from(num_msg).unwrap_or(0);

        // SAFETY: appdata_ptr came from a Box<ConvBridge> we stored in
        // pam_conv::appdata_ptr (see Pam::start). The Box outlives the
        // Pam (and therefore the conv callback) because it's held by a
        // field that drops after pam_end.
        #[expect(
            unsafe_code,
            reason = "appdata_ptr was stashed by Pam::start from a Box \
                      that is kept alive in the Pam struct."
        )]
        let bridge: &ConvBridge = unsafe { &*(appdata_ptr.cast::<ConvBridge>()) };

        // Marshal: C array of *const pam_message → Vec<PromptChallenge>.
        let mut prompts = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: msg is a libpam-provided C array of num_msg
            // non-null pam_message pointers (guaranteed by the
            // protocol; we still null-check for defense).
            #[expect(
                unsafe_code,
                reason = "indexing libpam-provided pam_message array; \
                          bounds checked by the loop, contents checked \
                          for null below."
            )]
            let m_ptr = unsafe { *msg.add(i) };
            if m_ptr.is_null() {
                return PAM_CONV_ERR as c_int;
            }
            // SAFETY: m_ptr is non-null and points at a libpam-allocated
            // pam_message that's valid for the duration of this conv call.
            #[expect(
                unsafe_code,
                reason = "libpam guarantees pam_message lives until the \
                          conv callback returns."
            )]
            let m = unsafe { &*m_ptr };
            let Some(style) = PamMessageStyle::from_raw(m.msg_style) else {
                return PAM_CONV_ERR as c_int;
            };
            let text = if m.msg.is_null() {
                String::new()
            } else {
                // SAFETY: m.msg is a NUL-terminated C string owned by
                // libpam, valid for the conv call.
                #[expect(unsafe_code, reason = "libpam-provided NUL-terminated C string.")]
                let cstr = unsafe { CStr::from_ptr(m.msg) };
                cstr.to_string_lossy().into_owned()
            };
            prompts.push(PromptChallenge {
                style,
                message: text,
            });
        }

        // Drive the channels.
        let responses = process_prompts(prompts, bridge);

        // Marshal: Vec<Zeroizing<String>> → libc::calloc'd
        // pam_response array. PAM frees both the array and each
        // .resp via free(); we must use the matching libc allocator.
        let resp_size = std::mem::size_of::<pam_response>();
        // SAFETY: libc::calloc returns null on OOM or a valid block of
        // n * resp_size zeroed bytes otherwise.
        #[expect(
            unsafe_code,
            reason = "C allocator paired with libpam's free() (which \
                      libpam invokes on .resp and on the array itself)."
        )]
        let resp_array = unsafe { libc::calloc(n, resp_size) }.cast::<pam_response>();
        if resp_array.is_null() {
            return PAM_BUF_ERR as c_int;
        }

        for (i, r) in responses.into_iter().enumerate() {
            // Build a NUL-terminated buffer in a Zeroizing<Vec<u8>> so
            // every intermediate copy of the password is wiped on drop.
            // Going through CString would deallocate without zeroing
            // (std::ffi::CString::Drop just frees), leaving credential
            // residue accessible until the heap slot is reused.
            let mut bytes: Zeroizing<Vec<u8>> = Zeroizing::new(r.as_bytes().to_vec());
            // r (Zeroizing<String>) drops here, wiping the original buffer.
            drop(r);
            // Interior NULs are a hard error: silently stripping or
            // truncating would mutate the user's response under their
            // nose and could change the authentication outcome. libpam
            // can't carry NUL bytes in resp anyway — reject explicitly.
            if bytes.contains(&0) {
                rollback_resp_array(resp_array, i);
                return PAM_CONV_ERR as c_int;
            }
            // Append the trailing NUL so the buffer is a valid C string
            // for strdup. The buffer now contains exactly one NUL byte:
            // the trailing one we just pushed.
            bytes.push(0);
            // SAFETY: bytes contains a single trailing NUL (we just
            // pushed it; the interior-NUL check above guarantees no
            // interior NULs). libc::strdup copies into a new
            // libpam-owned allocation; bytes (Zeroizing<Vec<u8>>) drops
            // at the end of this iteration, wiping the source.
            #[expect(
                unsafe_code,
                reason = "strdup copies a NUL-terminated Zeroizing<Vec<u8>>; \
                          source wiped on drop at end of iteration."
            )]
            let dup = unsafe { libc::strdup(bytes.as_ptr().cast::<libc::c_char>()) };
            if dup.is_null() {
                // OOM partway through; free what we allocated already.
                rollback_resp_array(resp_array, i);
                return PAM_BUF_ERR as c_int;
            }
            // SAFETY: resp_array[i] is in-bounds (i < n) and points at
            // zeroed pam_response storage.
            #[expect(unsafe_code, reason = "writing to in-bounds pam_response slot.")]
            unsafe {
                (*resp_array.add(i)).resp = dup;
                (*resp_array.add(i)).resp_retcode = 0;
            }
        }

        // SAFETY: resp is the libpam-provided output pointer, valid for
        // the duration of this conv call.
        #[expect(unsafe_code, reason = "writing to libpam-provided out-pointer.")]
        unsafe {
            *resp = resp_array;
        }
        PAM_SUCCESS as c_int
    });
    result.unwrap_or(PAM_CONV_ERR as c_int)
}

// ── RAII PAM handle ─────────────────────────────────────────────────────

/// RAII handle for an in-flight PAM transaction.
///
/// Constructed by [`Pam::start`]. The transaction is closed via
/// `pam_end` on drop. `Pam` is `!Send` (inherited from the raw
/// `pam_handle_t` pointer); a worker-thread pattern that uses this
/// must construct the `Pam` on the worker thread itself.
///
/// # INVARIANT: drop order
///
/// `pam_end(self.handle, ...)` runs in our `Drop` impl. Rust drops
/// the struct's *fields* in declaration order **after** the `Drop`
/// impl returns. The fields `_conv` and `_bridge` are therefore
/// still valid throughout the `pam_end` call — libpam dereferences
/// the conv pointer and `appdata_ptr` during cleanup. **Do not
/// reorder these fields** and **do not move cleanup into a field's
/// own `Drop`** without re-thinking this invariant. A wrong order
/// would let libpam dereference freed memory.
pub(crate) struct Pam {
    handle: *mut pam_handle_t,
    // The pam_conv struct must outlive the handle: pam_start stores
    // a pointer to it (does not copy). Keep it pinned via Box.
    _conv: Box<pam_conv>,
    // The ConvBridge holds the channel ends the conv callback uses.
    // Boxed for a stable address; the conv's appdata_ptr aliases this.
    _bridge: Box<ConvBridge>,
    last_status: c_int,
}

impl Pam {
    /// Open a PAM transaction for `service_name` as `username`, wiring
    /// the given `bridge` into the conv callback.
    ///
    /// `service_name` selects which `/etc/pam.d/<name>` file PAM
    /// consults. `username` is the user being authenticated.
    ///
    /// # Errors
    ///
    /// [`PamError::Nul`] for interior NUL in either string argument,
    /// [`PamError::Start`] for a non-success libpam status.
    pub fn start(service_name: &str, username: &str, bridge: ConvBridge) -> Result<Self, PamError> {
        let service = CString::new(service_name)?;
        let user = CString::new(username)?;

        let mut boxed_bridge = Box::new(bridge);
        let appdata_ptr: *mut c_void =
            std::ptr::from_mut::<ConvBridge>(boxed_bridge.as_mut()).cast::<c_void>();

        let conv = Box::new(pam_conv {
            conv: Some(bridge_conv),
            appdata_ptr,
        });
        let mut handle: *mut pam_handle_t = ptr::null_mut();
        #[expect(
            unsafe_code,
            reason = "FFI: pam_start. service/user are NUL-terminated \
                      C strings owned by CStrings live for the call. \
                      conv's address is stable for self's lifetime \
                      (Box). handle is a valid out-pointer."
        )]
        let status = unsafe {
            pam_start(
                service.as_ptr(),
                user.as_ptr(),
                std::ptr::from_ref::<pam_conv>(conv.as_ref()),
                &raw mut handle,
            )
        };
        if status != PAM_SUCCESS as c_int {
            return Err(PamError::Start(status));
        }

        // Install the no-op fail-delay callback BEFORE returning so it's
        // in place for the eventual pam_authenticate call. libpam stores
        // the function pointer; since `noop_fail_delay` has static
        // lifetime, the pointer remains valid for the transaction.
        #[expect(
            unsafe_code,
            reason = "FFI: pam_set_item(PAM_FAIL_DELAY, fn_ptr). The \
                      function pointer has static lifetime; libpam \
                      stores it for the duration of the PAM transaction \
                      (until pam_end). The function-pointer-to-void-ptr \
                      cast is the documented libpam idiom for this item."
        )]
        let delay_status =
            unsafe { pam_set_item(handle, PAM_FAIL_DELAY, noop_fail_delay as *const c_void) };
        if delay_status != PAM_SUCCESS as c_int {
            // pam_set_item failed — we need to clean up the handle we
            // just allocated before returning the error.
            #[expect(
                unsafe_code,
                reason = "FFI: pam_end releases the handle we just \
                          allocated via pam_start before returning \
                          the error to the caller."
            )]
            unsafe {
                pam_end(handle, delay_status);
            }
            return Err(PamError::SetItem(delay_status));
        }

        Ok(Self {
            handle,
            _conv: conv,
            _bridge: boxed_bridge,
            last_status: status,
        })
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
            reason = "FFI: pam_set_item copies; cstr lifetime covers call."
        )]
        let status =
            unsafe { pam_set_item(self.handle, item_type, cstr.as_ptr().cast::<c_void>()) };
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::SetItem(status))
        }
    }

    /// Set `PAM_TTY`. PAM modules use this to scope rate limits and
    /// audit-log entries.
    ///
    /// # Errors
    /// As for [`Self::set_item_str`].
    pub fn set_tty(&mut self, value: &str) -> Result<(), PamError> {
        self.set_item_str(PAM_TTY, value)
    }

    /// Set `PAM_RUSER`. The "requesting user" — for a system compositor,
    /// typically the `compositor` or `greeter` system user, NOT the
    /// user being authenticated.
    ///
    /// # Errors
    /// As for [`Self::set_item_str`].
    pub fn set_ruser(&mut self, value: &str) -> Result<(), PamError> {
        self.set_item_str(PAM_RUSER, value)
    }

    /// Run the PAM authentication stack. Blocks until PAM either
    /// succeeds, fails, or runs the conv callback to exhaustion. The
    /// resulting status is stored so `pam_end` on drop sees the right
    /// value.
    ///
    /// # Errors
    /// [`PamError::Authenticate`] on any non-success status. `PAM_AUTH_ERR`
    /// (bad credentials) is the common case.
    pub fn authenticate(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_authenticate blocks on this thread and \
                      drives our bridge_conv callback. Handle is owned \
                      by self; flags=0 is the standard invocation."
        )]
        let status = unsafe { pam_authenticate(self.handle, 0) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::Authenticate(status))
        }
    }

    /// Run the PAM account-management stack. Catches "account expired",
    /// "must change password", "account disabled" — conditions PAM
    /// considers separately from authentication.
    ///
    /// # Errors
    /// [`PamError::AcctMgmt`] on any non-success status. `PAM_NEW_AUTHTOK_REQD`
    /// is the "must change password" case; the caller decides whether to
    /// surface that distinctly.
    pub fn acct_mgmt(&mut self) -> Result<(), PamError> {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_acct_mgmt. Handle owned by self; flags=0."
        )]
        let status = unsafe { pam_acct_mgmt(self.handle, 0) };
        self.last_status = status;
        if status == PAM_SUCCESS as c_int {
            Ok(())
        } else {
            Err(PamError::AcctMgmt(status))
        }
    }

    /// Read back `PAM_USER`. The username PAM has settled on — may
    /// differ from the username passed to [`Self::start`] if a module
    /// rewrote it (e.g. via `pam_username`).
    ///
    /// # Errors
    /// [`PamError::GetUser`] if pam_get_item returns non-success, the
    /// item pointer is null, or the value isn't valid UTF-8.
    pub fn get_user(&mut self) -> Result<String, PamError> {
        let mut raw: *const c_void = ptr::null();
        #[expect(
            unsafe_code,
            reason = "FFI: pam_get_item. Handle owned by self; raw is a \
                      valid out-pointer; the returned pointer (if any) \
                      is libpam-owned and valid until pam_end."
        )]
        let status = unsafe { pam_get_item(self.handle, PAM_USER, &raw mut raw) };
        if status != PAM_SUCCESS as c_int {
            return Err(PamError::GetUser(status));
        }
        if raw.is_null() {
            return Err(PamError::GetUser(PAM_CONV_ERR as c_int));
        }
        // SAFETY: PAM_USER's item, when present, is a NUL-terminated
        // C string owned by libpam.
        #[expect(
            unsafe_code,
            reason = "libpam-owned NUL-terminated C string for PAM_USER."
        )]
        let cstr = unsafe { CStr::from_ptr(raw.cast::<libc::c_char>()) };
        cstr.to_str()
            .map(str::to_owned)
            .map_err(|_| PamError::GetUser(PAM_CONV_ERR as c_int))
    }
}

impl Drop for Pam {
    fn drop(&mut self) {
        #[expect(
            unsafe_code,
            reason = "FFI: pam_end releases libpam-owned state. handle \
                      was set by a successful pam_start; not aliased."
        )]
        unsafe {
            pam_end(self.handle, self.last_status);
        }
    }
}

// ── PamThread: worker-thread driver implementing PamSession ─────────────
//
// PAM's C API is blocking: pam_authenticate runs the entire conv
// conversation on the calling thread before returning. The state
// machine in halmasuit-greetd needs a step-by-step interface
// (PamSession::step). PamThread bridges the two: it spawns a worker
// thread that calls pam_authenticate, while the outside thread
// invokes step() and pumps responses + receives challenges via the
// conv-channel pair built by `conv_pair`.

/// Terminal outcome of the PAM conversation, computed by the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PamOutcome {
    Success {
        username: String,
        uid: u32,
        gid: u32,
    },
    Failure {
        reason: String,
    },
}

/// PAM session driven by a worker thread.
///
/// Construct with [`PamThread::new`] (cheap — no thread yet). The
/// worker spawns on the first [`PamSession::step`] call, owns a `Pam`
/// (which is `!Send`), and runs `pam_authenticate` → `pam_acct_mgmt`
/// → `get_user` → pwent lookup. The outside thread drives the
/// conversation by sending responses and reading challenges through
/// the channels in `ThreadState::Running`.
///
/// Drop is safe at any time: dropping `PamThread` closes the
/// driver-side channels, which causes the conv callback to terminate
/// the conversation (empty responses, then pam_authenticate fails),
/// the worker drops its `Pam` (releasing libpam state via `pam_end`),
/// and tries to send the outcome (which fails silently — receiver is
/// gone). The worker handle is detached.
///
/// # Detached-worker accumulation under cancel + retry
///
/// Because the worker is detached and libpam offers no cancellation
/// point, a rapid greeter-side cancel/retry loop can briefly run
/// multiple workers in parallel — each carries its own PAM
/// transaction through to libpam's natural return (which may take
/// up to the `PAM_FAIL_DELAY` of the underlying module if we hadn't
/// overridden it, or up to a slow NSS / network-PAM module's
/// internal timeout otherwise). In normal use this window is
/// bounded by user typing speed; a buggy or malicious greeter
/// driving the loop in software could transiently keep N workers
/// alive. The compositor process itself remains responsive (the
/// calloop thread is freed as soon as the new worker hands off its
/// first challenge or terminal outcome).
pub struct PamThread {
    config: PamThreadConfig,
    state: ThreadState,
    /// Maximum time `PamSession::step` will block waiting for the
    /// next challenge or terminal outcome. If the worker is wedged
    /// inside libpam (e.g. a network PAM module like SSSD blocking
    /// on an unreachable LDAP server, or a broken NSS module),
    /// `step` returns `PamStep::Failure` after this duration instead
    /// of hanging the calloop thread indefinitely.
    ///
    /// The worker thread is NOT killed on timeout — libpam doesn't
    /// expose a cancellation point. It stays detached until libpam
    /// eventually returns.
    step_timeout: Duration,
}

#[derive(Debug, Clone, Default)]
struct PamThreadConfig {
    service: String,
    username: String,
    ruser: Option<String>,
    tty: Option<String>,
}

enum ThreadState {
    NotStarted,
    Running {
        challenge_rx: Receiver<PromptChallenge>,
        response_tx: SyncSender<Zeroizing<String>>,
        outcome_rx: Receiver<PamOutcome>,
        // Retained for potential future graceful-shutdown via .join();
        // detached today. We don't .join() on drop — the worker exits
        // on its own once the bridge channels close, and joining could
        // make Drop block on libpam (which doesn't expose a
        // cancellation point).
        _worker: JoinHandle<()>,
    },
    Done,
}

/// Default upper bound on [`PamSession::step`] recv duration.
///
/// 30 s comfortably covers `pam_unix` (typically tens of ms) plus
/// slow NSS or a sluggish network-PAM module while still keeping a
/// wedged libpam from hanging the calloop thread forever. Override
/// per-instance with [`PamThread::with_step_timeout`].
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30);

impl PamThread {
    /// Build a new PamThread for `service_name` as `username`. No
    /// worker thread is spawned yet. The per-step recv timeout
    /// defaults to [`DEFAULT_STEP_TIMEOUT`]; override with
    /// [`Self::with_step_timeout`] before the first `step()` call.
    #[must_use]
    pub fn new(service_name: &str, username: &str) -> Self {
        Self {
            config: PamThreadConfig {
                service: service_name.into(),
                username: username.into(),
                ruser: None,
                tty: None,
            },
            state: ThreadState::NotStarted,
            step_timeout: DEFAULT_STEP_TIMEOUT,
        }
    }

    /// Override the per-step recv timeout. See [`Self`]'s field doc
    /// for the rationale. Mostly useful in tests; production should
    /// keep the default.
    #[must_use]
    pub const fn with_step_timeout(mut self, timeout: Duration) -> Self {
        self.step_timeout = timeout;
        self
    }

    /// Set `PAM_RUSER` (the requesting user) for the upcoming PAM
    /// transaction. No-op if step() has already started the worker.
    pub fn set_ruser(&mut self, value: &str) -> &mut Self {
        self.config.ruser = Some(value.into());
        self
    }

    /// Set `PAM_TTY` for the upcoming PAM transaction. No-op if step()
    /// has already started the worker.
    pub fn set_tty(&mut self, value: &str) -> &mut Self {
        self.config.tty = Some(value.into());
        self
    }

    fn spawn(&mut self) {
        let (bridge, driver) = conv_pair();
        let (outcome_tx, outcome_rx) = sync_channel(1);
        // Move config into the worker — the PamThread doesn't reference
        // it again after spawning. Saves four String allocations.
        let config = std::mem::take(&mut self.config);
        let worker = std::thread::spawn(move || run_pam(&config, bridge, &outcome_tx));
        self.state = ThreadState::Running {
            challenge_rx: driver.challenge_rx,
            response_tx: driver.response_tx,
            outcome_rx,
            _worker: worker,
        };
    }

    /// After a successful send-or-spawn, block on the next challenge,
    /// terminal outcome, or the configured timeout — whichever comes
    /// first.
    fn recv_next(&mut self) -> PamStep {
        let ThreadState::Running {
            challenge_rx,
            outcome_rx,
            ..
        } = &self.state
        else {
            unreachable!("recv_next called outside ThreadState::Running");
        };
        match await_next_step(challenge_rx, outcome_rx, self.step_timeout) {
            NextStep::Challenge(c) => PamStep::Challenge {
                kind: translate_style(c.style),
                prompt: c.message,
            },
            NextStep::Outcome(outcome) => {
                self.state = ThreadState::Done;
                match outcome {
                    PamOutcome::Success { username, uid, gid } => {
                        PamStep::Success { username, uid, gid }
                    }
                    PamOutcome::Failure { reason } => PamStep::Failure { reason },
                }
            }
            NextStep::Timeout => {
                self.state = ThreadState::Done;
                PamStep::Failure {
                    reason: format!(
                        "PAM auth exceeded the per-step timeout of {:?}",
                        self.step_timeout
                    ),
                }
            }
        }
    }
}

/// Outcome of one channel-await inside [`PamThread::recv_next`].
/// Extracted so the timeout / disconnect / challenge branching is
/// unit-testable without a real worker thread.
enum NextStep {
    Challenge(PromptChallenge),
    Outcome(PamOutcome),
    Timeout,
}

fn await_next_step(
    challenge_rx: &Receiver<PromptChallenge>,
    outcome_rx: &Receiver<PamOutcome>,
    timeout: Duration,
) -> NextStep {
    match challenge_rx.recv_timeout(timeout) {
        Ok(c) => NextStep::Challenge(c),
        Err(RecvTimeoutError::Timeout) => NextStep::Timeout,
        Err(RecvTimeoutError::Disconnected) => {
            // Worker dropped Pam → challenge channel closed. The
            // outcome is in the (1-slot buffered) outcome channel by
            // construction: the worker drops Pam THEN sends the
            // outcome.
            let outcome = outcome_rx.recv().unwrap_or_else(|_| PamOutcome::Failure {
                reason: "worker exited without an outcome".into(),
            });
            NextStep::Outcome(outcome)
        }
    }
}

impl PamSession for PamThread {
    fn step(&mut self, response: Option<String>) -> PamStep {
        match &self.state {
            ThreadState::NotStarted => {
                // First call: spawn the worker. Per the trait contract
                // the first response is None (no challenge has been
                // delivered yet), so we just spawn and wait for the
                // first challenge or terminal outcome.
                self.spawn();
            }
            ThreadState::Running { response_tx, .. } => {
                // Forward the response (or empty if None) through the
                // bridge. Send may fail if the worker has already
                // finished — that just means recv_next will see the
                // channel closed and read the outcome.
                let r = Zeroizing::new(response.unwrap_or_default());
                let _ = response_tx.send(r);
            }
            ThreadState::Done => {
                return PamStep::Failure {
                    reason: "PamSession::step called after PAM completion".into(),
                };
            }
        }
        self.recv_next()
    }
}

const fn translate_style(s: PamMessageStyle) -> AuthMessageType {
    match s {
        PamMessageStyle::PromptEchoOn => AuthMessageType::Visible,
        PamMessageStyle::PromptEchoOff => AuthMessageType::Secret,
        PamMessageStyle::ErrorMsg => AuthMessageType::Error,
        PamMessageStyle::TextInfo => AuthMessageType::Info,
    }
}

fn run_pam(config: &PamThreadConfig, bridge: ConvBridge, outcome_tx: &SyncSender<PamOutcome>) {
    // try_pam owns the Pam (and bridge). On scope exit (Ok or Err),
    // Pam drops → pam_end closes libpam state and the bridge's
    // challenge_tx drops → the driver-side challenge_rx returns Err.
    // ONLY THEN do we send the outcome on the 1-slot buffered
    // outcome channel. This ordering guarantees the driver reads
    // "channel closed" before reading the terminal outcome.
    let outcome = match try_pam(config, bridge) {
        Ok((username, uid, gid)) => PamOutcome::Success { username, uid, gid },
        Err(reason) => PamOutcome::Failure { reason },
    };
    let _ = outcome_tx.send(outcome);
}

fn try_pam(config: &PamThreadConfig, bridge: ConvBridge) -> Result<(String, u32, u32), String> {
    let mut pam =
        Pam::start(&config.service, &config.username, bridge).map_err(|e| e.to_string())?;
    if let Some(ruser) = &config.ruser {
        pam.set_ruser(ruser).map_err(|e| e.to_string())?;
    }
    if let Some(tty) = &config.tty {
        pam.set_tty(tty).map_err(|e| e.to_string())?;
    }
    pam.authenticate().map_err(|e| e.to_string())?;
    pam.acct_mgmt().map_err(|e| e.to_string())?;
    let resolved = pam.get_user().map_err(|e| e.to_string())?;
    // Genericized failure reason to avoid account enumeration: an
    // attacker who reaches the post-auth pwent branch already knows
    // the credentials they submitted are correct (pam_authenticate
    // succeeded). Distinguishing "no pwent for known user" from
    // "pwent IO error" reveals whether the user exists in the local
    // passwd database vs only in the PAM stack (e.g. LDAP without
    // local nss-ldap caching). Collapse both into the same message;
    // the verbose form belongs in a debug log once one exists.
    let pw = nix::unistd::User::from_name(&resolved)
        .map_err(|_e| "post-auth account lookup failed".to_string())?
        .ok_or_else(|| "post-auth account lookup failed".to_string())?;
    // Return the canonical name alongside the ids it was resolved
    // from. Downstream `initgroups(3)` must use this name, not the
    // pre-auth client string, or a username-rewriting PAM stack could
    // pair this uid with a different identity's supplementary groups.
    Ok((resolved, pw.uid.as_raw(), pw.gid.as_raw()))
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // ── handle lifecycle (carried over from the previous task) ──────────

    #[test]
    fn start_against_other_service_succeeds() {
        let (bridge, _driver) = conv_pair();
        let pam = Pam::start("other", "nobody", bridge).expect("pam_start for 'other'");
        drop(pam);
    }

    #[test]
    fn set_ruser_succeeds() {
        let (bridge, _driver) = conv_pair();
        let mut pam = Pam::start("other", "nobody", bridge).unwrap();
        pam.set_item_str(PAM_RUSER, "alice").expect("set PAM_RUSER");
    }

    #[test]
    fn nul_byte_in_service_name_is_rejected() {
        let (bridge, _driver) = conv_pair();
        let r = Pam::start("other\0bad", "nobody", bridge);
        assert!(matches!(r, Err(PamError::Nul(_))));
    }

    #[test]
    fn nul_byte_in_username_is_rejected() {
        let (bridge, _driver) = conv_pair();
        let r = Pam::start("other", "no\0body", bridge);
        assert!(matches!(r, Err(PamError::Nul(_))));
    }

    #[test]
    fn nul_byte_in_set_item_value_is_rejected() {
        let (bridge, _driver) = conv_pair();
        let mut pam = Pam::start("other", "nobody", bridge).unwrap();
        let r = pam.set_item_str(PAM_RUSER, "ali\0ce");
        assert!(matches!(r, Err(PamError::Nul(_))));
    }

    // ── authenticate / acct_mgmt / get_user ─────────────────────────────

    #[test]
    fn authenticate_against_other_service_fails() {
        // /etc/pam.d/other uses pam_deny.so on every conformant Linux
        // PAM stack — pam_authenticate returns PAM_AUTH_ERR (or similar
        // non-success) immediately without invoking conv.
        let (bridge, _driver) = conv_pair();
        let mut pam = Pam::start("other", "nobody", bridge).unwrap();
        let r = pam.authenticate();
        assert!(matches!(r, Err(PamError::Authenticate(_))), "got: {r:?}");
    }

    #[test]
    fn get_user_round_trips_start_username() {
        let (bridge, _driver) = conv_pair();
        let mut pam = Pam::start("other", "alice", bridge).unwrap();
        let user = pam.get_user().expect("get_user");
        assert_eq!(user, "alice");
    }

    #[test]
    fn set_tty_and_set_ruser_succeed() {
        let (bridge, _driver) = conv_pair();
        let mut pam = Pam::start("other", "alice", bridge).unwrap();
        pam.set_tty("/dev/tty1").expect("set_tty");
        pam.set_ruser("compositor").expect("set_ruser");
    }

    // ── PamThread + PamSession impl ─────────────────────────────────────

    #[test]
    fn pam_thread_against_other_service_yields_failure() {
        use halmasuit_greetd::PamSession as _;
        // pam_deny.so denies pam_authenticate immediately without ever
        // invoking the conv callback — so the worker finishes before
        // delivering any challenge, and the very first step() returns
        // Failure.
        let mut pt = PamThread::new("other", "nobody");
        let step = pt.step(None);
        match step {
            PamStep::Failure { reason } => {
                assert!(!reason.is_empty(), "Failure reason should not be empty");
                assert!(
                    reason.contains("pam_authenticate") || reason.contains("authenticate"),
                    "Failure reason should mention the failing call: {reason}",
                );
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn pam_thread_set_builders_record_values() {
        let mut pt = PamThread::new("other", "nobody");
        pt.set_ruser("compositor").set_tty("/dev/tty1");
        assert_eq!(pt.config.ruser.as_deref(), Some("compositor"));
        assert_eq!(pt.config.tty.as_deref(), Some("/dev/tty1"));
        // service / username are also recorded from new()
        assert_eq!(pt.config.service, "other");
        assert_eq!(pt.config.username, "nobody");
    }

    #[test]
    fn pam_thread_step_after_failure_returns_failure() {
        use halmasuit_greetd::PamSession as _;
        let mut pt = PamThread::new("other", "nobody");
        let _ = pt.step(None);
        // After completion, further step() calls fall into the Done arm.
        let second = pt.step(Some("anything".into()));
        match second {
            PamStep::Failure { reason } => {
                assert!(reason.contains("after PAM completion"), "got: {reason}");
            }
            other => panic!("expected Failure on Done arm, got {other:?}"),
        }
    }

    #[test]
    fn pam_thread_is_send() {
        // Compile-time assertion: PamThread must be Send so the state
        // machine driver (running on the calloop thread) can own it.
        fn assert_send<T: Send>() {}
        assert_send::<PamThread>();
    }

    // ── await_next_step (the recv-timeout primitive) ────────────────────

    #[test]
    fn await_next_step_returns_challenge_when_one_arrives() {
        let (challenge_tx, challenge_rx) = sync_channel::<PromptChallenge>(1);
        let (_outcome_tx, outcome_rx) = sync_channel::<PamOutcome>(1);

        challenge_tx
            .send(PromptChallenge {
                style: PamMessageStyle::PromptEchoOff,
                message: "password:".into(),
            })
            .unwrap();

        let r = await_next_step(&challenge_rx, &outcome_rx, Duration::from_secs(5));
        match r {
            NextStep::Challenge(c) => {
                assert_eq!(c.style, PamMessageStyle::PromptEchoOff);
                assert_eq!(c.message, "password:");
            }
            other => panic!("expected Challenge, got {other:?}"),
        }
    }

    #[test]
    fn await_next_step_returns_timeout_when_no_message_in_window() {
        // Hold both senders so the channels stay connected; never send.
        // recv_timeout fires Timeout before Disconnected.
        let (_challenge_tx_held, challenge_rx) = sync_channel::<PromptChallenge>(1);
        let (_outcome_tx_held, outcome_rx) = sync_channel::<PamOutcome>(1);

        let start = std::time::Instant::now();
        let r = await_next_step(&challenge_rx, &outcome_rx, Duration::from_millis(50));
        let elapsed = start.elapsed();

        assert!(
            matches!(r, NextStep::Timeout),
            "expected Timeout, got {r:?}"
        );
        // Sanity: actually waited at least the requested window.
        assert!(
            elapsed >= Duration::from_millis(50),
            "elapsed {elapsed:?} < 50ms"
        );
        // And not absurdly longer (catches accidental no-op timeout).
        assert!(elapsed < Duration::from_secs(1), "elapsed {elapsed:?} > 1s");
    }

    #[test]
    fn await_next_step_returns_outcome_when_channels_disconnect() {
        let (challenge_tx, challenge_rx) = sync_channel::<PromptChallenge>(1);
        let (outcome_tx, outcome_rx) = sync_channel::<PamOutcome>(1);

        // Simulate the worker's order: send outcome, then drop the
        // challenge tx (which closes the channel).
        outcome_tx
            .send(PamOutcome::Failure {
                reason: "bad password".into(),
            })
            .unwrap();
        drop(challenge_tx);
        drop(outcome_tx);

        let r = await_next_step(&challenge_rx, &outcome_rx, Duration::from_secs(5));
        match r {
            NextStep::Outcome(PamOutcome::Failure { reason }) => {
                assert_eq!(reason, "bad password");
            }
            other => panic!("expected Outcome::Failure, got {other:?}"),
        }
    }

    impl std::fmt::Debug for NextStep {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Challenge(c) => write!(f, "Challenge({c:?})"),
                Self::Outcome(o) => write!(f, "Outcome({o:?})"),
                Self::Timeout => write!(f, "Timeout"),
            }
        }
    }

    #[test]
    fn pam_thread_with_step_timeout_overrides_default() {
        let pt = PamThread::new("other", "nobody").with_step_timeout(Duration::from_millis(1));
        assert_eq!(pt.step_timeout, Duration::from_millis(1));
    }

    // ── process_prompts (the safe core) ─────────────────────────────────

    #[test]
    fn process_prompts_forwards_in_order() {
        let (bridge, driver) = conv_pair();
        let responder = thread::spawn(move || {
            // Interleaved: recv, send, recv, send. Matches the
            // interleaved process_prompts implementation.
            let mut got: Vec<PromptChallenge> = Vec::new();
            got.push(driver.challenge_rx.recv().unwrap());
            driver
                .response_tx
                .send(Zeroizing::new("alice".into()))
                .unwrap();
            got.push(driver.challenge_rx.recv().unwrap());
            driver
                .response_tx
                .send(Zeroizing::new("hunter2".into()))
                .unwrap();
            got
        });

        let prompts = vec![
            PromptChallenge {
                style: PamMessageStyle::PromptEchoOn,
                message: "login:".into(),
            },
            PromptChallenge {
                style: PamMessageStyle::PromptEchoOff,
                message: "password:".into(),
            },
        ];
        let responses = process_prompts(prompts.clone(), &bridge);
        let got = responder.join().unwrap();

        assert_eq!(got, prompts);
        assert_eq!(responses.len(), 2);
        assert_eq!(&**responses[0], "alice");
        assert_eq!(&**responses[1], "hunter2");
    }

    #[test]
    fn process_prompts_returns_empty_strings_when_driver_dropped() {
        let (bridge, driver) = conv_pair();
        drop(driver);
        let prompts = vec![PromptChallenge {
            style: PamMessageStyle::PromptEchoOff,
            message: "password:".into(),
        }];
        let responses = process_prompts(prompts, &bridge);
        assert_eq!(responses.len(), 1);
        assert_eq!(&**responses[0], "");
    }

    // ── PamMessageStyle ─────────────────────────────────────────────────

    #[test]
    fn pam_message_style_from_raw_covers_all_variants() {
        assert_eq!(
            PamMessageStyle::from_raw(PAM_PROMPT_ECHO_ON),
            Some(PamMessageStyle::PromptEchoOn)
        );
        assert_eq!(
            PamMessageStyle::from_raw(PAM_PROMPT_ECHO_OFF),
            Some(PamMessageStyle::PromptEchoOff)
        );
        assert_eq!(
            PamMessageStyle::from_raw(PAM_ERROR_MSG),
            Some(PamMessageStyle::ErrorMsg)
        );
        assert_eq!(
            PamMessageStyle::from_raw(PAM_TEXT_INFO),
            Some(PamMessageStyle::TextInfo)
        );
        assert_eq!(PamMessageStyle::from_raw(9999), None);
    }

    // ── bridge_conv via synthetic C input ───────────────────────────────
    //
    // Calls the extern "C" callback directly with a hand-crafted
    // pam_message array. Verifies that the strdup'd response array
    // carries the right strings, then frees with libc::free in the
    // shape PAM would.

    fn call_bridge_conv(
        prompts: &[(c_int, &str)],
        responder: impl FnOnce(&ConvDriver) + Send + 'static,
    ) -> Vec<String> {
        let (bridge, driver) = conv_pair();
        let mut boxed_bridge = Box::new(bridge);

        let handle = thread::spawn(move || responder(&driver));

        // Build pam_message structs + a C array of pointers to them.
        let cstrings: Vec<CString> = prompts
            .iter()
            .map(|(_, m)| CString::new(*m).unwrap())
            .collect();
        let messages: Vec<pam_message> = prompts
            .iter()
            .zip(&cstrings)
            .map(|((style, _), cs)| pam_message {
                msg_style: *style,
                msg: cs.as_ptr().cast_mut(),
            })
            .collect();
        let mut msg_ptrs: Vec<*const pam_message> =
            messages.iter().map(std::ptr::from_ref).collect();
        // Suppress unused_mut: msg_ptrs is borrowed as *mut below.
        let _ = &mut msg_ptrs;

        let mut resp_out: *mut pam_response = ptr::null_mut();
        let appdata: *mut c_void =
            std::ptr::from_mut::<ConvBridge>(boxed_bridge.as_mut()).cast::<c_void>();

        let num_msg = c_int::try_from(prompts.len()).expect("prompts.len() fits in c_int");
        let status = {
            #[expect(
                unsafe_code,
                reason = "test harness: invoke the extern \"C\" callback \
                          directly to exercise its FFI path."
            )]
            unsafe {
                bridge_conv(num_msg, msg_ptrs.as_mut_ptr(), &raw mut resp_out, appdata)
            }
        };
        assert_eq!(status, PAM_SUCCESS as c_int, "bridge_conv non-success");

        // Read back the response array, then free it the way libpam would.
        let mut got = Vec::with_capacity(prompts.len());
        #[expect(
            unsafe_code,
            reason = "reading and then freeing the libc::calloc'd resp \
                      array that bridge_conv returned; same allocator \
                      pair that PAM would use."
        )]
        unsafe {
            for i in 0..prompts.len() {
                let r_ptr = (*resp_out.add(i)).resp;
                let s = if r_ptr.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(r_ptr).to_string_lossy().into_owned()
                };
                got.push(s);
                if !r_ptr.is_null() {
                    libc::free(r_ptr.cast::<c_void>());
                }
            }
            libc::free(resp_out.cast::<c_void>());
        }

        handle.join().unwrap();
        got
    }

    #[test]
    fn bridge_conv_round_trips_single_prompt() {
        let got = call_bridge_conv(&[(PAM_PROMPT_ECHO_OFF, "password:")], |driver| {
            let c = driver.challenge_rx.recv().unwrap();
            assert_eq!(c.style, PamMessageStyle::PromptEchoOff);
            assert_eq!(c.message, "password:");
            driver
                .response_tx
                .send(Zeroizing::new("hunter2".into()))
                .unwrap();
        });
        assert_eq!(got, vec!["hunter2".to_string()]);
    }

    #[test]
    fn bridge_conv_round_trips_multiple_prompts() {
        // Interleaved recv/send — mirrors how the PamSession driver
        // will consume one challenge at a time. A batched responder
        // (recv all, then send all) would deadlock against the
        // interleaved process_prompts.
        let got = call_bridge_conv(
            &[
                (PAM_PROMPT_ECHO_ON, "login:"),
                (PAM_PROMPT_ECHO_OFF, "password:"),
            ],
            |driver| {
                let _ = driver.challenge_rx.recv().unwrap();
                driver
                    .response_tx
                    .send(Zeroizing::new("alice".into()))
                    .unwrap();
                let _ = driver.challenge_rx.recv().unwrap();
                driver
                    .response_tx
                    .send(Zeroizing::new("hunter2".into()))
                    .unwrap();
            },
        );
        assert_eq!(got, vec!["alice".to_string(), "hunter2".to_string()]);
    }

    #[test]
    fn bridge_conv_rejects_interior_nul_response() {
        // Drive bridge_conv directly because call_bridge_conv asserts
        // PAM_SUCCESS; we expect PAM_CONV_ERR here.
        let (bridge, driver) = conv_pair();
        let mut boxed_bridge = Box::new(bridge);
        let appdata: *mut c_void =
            std::ptr::from_mut::<ConvBridge>(boxed_bridge.as_mut()).cast::<c_void>();

        let prompt_cstr = CString::new("password:").unwrap();
        let message = pam_message {
            msg_style: PAM_PROMPT_ECHO_OFF,
            msg: prompt_cstr.as_ptr().cast_mut(),
        };
        let msg_ptr: *const pam_message = &raw const message;
        let mut resp_out: *mut pam_response = ptr::null_mut();

        // Send a response with an interior NUL via the driver.
        let driver_thread = std::thread::spawn(move || {
            let _ = driver.challenge_rx.recv().unwrap();
            driver
                .response_tx
                .send(Zeroizing::new("hu\0nter".into()))
                .unwrap();
        });

        let status = {
            #[expect(unsafe_code, reason = "test the interior-NUL refusal path")]
            unsafe {
                bridge_conv(
                    1,
                    std::ptr::from_ref::<*const pam_message>(&msg_ptr).cast_mut(),
                    &raw mut resp_out,
                    appdata,
                )
            }
        };
        driver_thread.join().unwrap();
        assert_eq!(status, PAM_CONV_ERR as c_int);
        assert!(
            resp_out.is_null(),
            "rejected response array should remain null"
        );
    }

    // ── bridge_conv error paths ─────────────────────────────────────────
    //
    // The guard `num_msg <= 0 || msg.is_null() || resp.is_null() ||
    // appdata_ptr.is_null()` short-circuits. To prove each branch is
    // load-bearing, each test below leaves *only* the named branch
    // invalid; the other three are valid pointers / a valid num_msg.

    /// Build a single valid `pam_message` (PAM_TEXT_INFO; libpam will
    /// not try to write a meaningful response, but the conv callback
    /// has to handle it anyway). The CString backing `msg.msg` is
    /// returned alongside so the caller can keep it alive for the
    /// duration of the bridge_conv call.
    fn one_valid_message() -> (CString, pam_message) {
        let cs = CString::new("hello").unwrap();
        let m = pam_message {
            msg_style: PAM_TEXT_INFO,
            msg: cs.as_ptr().cast_mut(),
        };
        (cs, m)
    }

    #[test]
    fn bridge_conv_rejects_null_appdata_in_isolation() {
        let (cs, message) = one_valid_message();
        let msg_ptr: *const pam_message = &raw const message;
        let mut resp_out: *mut pam_response = ptr::null_mut();
        let status = {
            #[expect(unsafe_code, reason = "isolated null-appdata guard test")]
            unsafe {
                bridge_conv(
                    1,
                    std::ptr::from_ref::<*const pam_message>(&msg_ptr).cast_mut(),
                    &raw mut resp_out,
                    ptr::null_mut(), // only null thing
                )
            }
        };
        assert_eq!(status, PAM_CONV_ERR as c_int);
        drop(cs); // keep cs alive through the call above
    }

    #[test]
    fn bridge_conv_rejects_negative_num_msg_in_isolation() {
        let (bridge, _driver) = conv_pair();
        let mut boxed_bridge = Box::new(bridge);
        let appdata: *mut c_void =
            std::ptr::from_mut::<ConvBridge>(boxed_bridge.as_mut()).cast::<c_void>();
        let (cs, message) = one_valid_message();
        let msg_ptr: *const pam_message = &raw const message;
        let mut resp_out: *mut pam_response = ptr::null_mut();
        let status = {
            #[expect(unsafe_code, reason = "isolated negative-num_msg guard test")]
            unsafe {
                bridge_conv(
                    -1, // only invalid input
                    std::ptr::from_ref::<*const pam_message>(&msg_ptr).cast_mut(),
                    &raw mut resp_out,
                    appdata,
                )
            }
        };
        assert_eq!(status, PAM_CONV_ERR as c_int);
        drop(cs);
    }

    #[test]
    fn bridge_conv_rejects_null_msg_in_isolation() {
        let (bridge, _driver) = conv_pair();
        let mut boxed_bridge = Box::new(bridge);
        let appdata: *mut c_void =
            std::ptr::from_mut::<ConvBridge>(boxed_bridge.as_mut()).cast::<c_void>();
        let mut resp_out: *mut pam_response = ptr::null_mut();
        let status = {
            #[expect(unsafe_code, reason = "isolated null-msg guard test")]
            unsafe {
                bridge_conv(
                    1,
                    ptr::null_mut(), // only null thing
                    &raw mut resp_out,
                    appdata,
                )
            }
        };
        assert_eq!(status, PAM_CONV_ERR as c_int);
    }

    #[test]
    fn bridge_conv_rejects_null_resp_in_isolation() {
        let (bridge, _driver) = conv_pair();
        let mut boxed_bridge = Box::new(bridge);
        let appdata: *mut c_void =
            std::ptr::from_mut::<ConvBridge>(boxed_bridge.as_mut()).cast::<c_void>();
        let (cs, message) = one_valid_message();
        let msg_ptr: *const pam_message = &raw const message;
        let status = {
            #[expect(unsafe_code, reason = "isolated null-resp guard test")]
            unsafe {
                bridge_conv(
                    1,
                    std::ptr::from_ref::<*const pam_message>(&msg_ptr).cast_mut(),
                    ptr::null_mut(), // only null thing
                    appdata,
                )
            }
        };
        assert_eq!(status, PAM_CONV_ERR as c_int);
        drop(cs);
    }

    #[test]
    fn bridge_conv_rejects_unknown_msg_style() {
        let (bridge, _driver) = conv_pair();
        let mut boxed_bridge = Box::new(bridge);
        let appdata: *mut c_void =
            std::ptr::from_mut::<ConvBridge>(boxed_bridge.as_mut()).cast::<c_void>();

        let cs = CString::new("?:").unwrap();
        let message = pam_message {
            msg_style: 9999,
            msg: cs.as_ptr().cast_mut(),
        };
        let msg_ptr: *const pam_message = &raw const message;
        let mut resp_out: *mut pam_response = ptr::null_mut();
        let status = {
            #[expect(unsafe_code, reason = "test the unknown-msg-style refusal path")]
            unsafe {
                bridge_conv(
                    1,
                    std::ptr::from_ref::<*const pam_message>(&msg_ptr).cast_mut(),
                    &raw mut resp_out,
                    appdata,
                )
            }
        };
        assert_eq!(status, PAM_CONV_ERR as c_int);
    }
}
