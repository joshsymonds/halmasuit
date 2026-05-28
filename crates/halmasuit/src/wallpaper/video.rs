//! Video wallpaper backend (Epic #12).
//!
//! Forks `halmasuit-decoder` as a sandboxed subprocess (the
//! decoder lives in a private user/net/mount namespace, under
//! rlimits, with PR_SET_NO_NEW_PRIVS set) and consumes decoded RGBA8
//! frames over a SOCK_SEQPACKET back-channel. Every frame is
//! validated (`bytes_len <= MAX_FRAME_BYTES` AND `width * height *
//! 4 == bytes_len`) before becoming a GLES texture.
//!
//! ## Lazy decoder spawn (Epic anti-pattern: "NO running the
//! decoder as root or with elevated capabilities")
//!
//! VideoBackend is constructed during `setup_drm_backend`, which
//! runs BEFORE halmasuit's in-process privilege drop
//! (`phase_entered:deprivileged`). Spawning the decoder eagerly
//! from `new()` would inherit the compositor's pre-drop uid (0 in
//! production deploys), making the sandbox's uid_map a 0→0
//! identity instead of `compositor_uid → compositor_uid`. The
//! sandbox primitives confine root-in-sandbox, but the explicit
//! epic anti-pattern requires the decoder NOT to run as root.
//!
//! Fix: `new()` only stages the source path + the placeholder
//! texture. The first call to [`Self::poll_pending`] performs the
//! spawn. The compositor's wallpaper-tick calloop timer (in
//! `main.rs`) is registered just before the privilege-drop block,
//! so its first dispatch — and therefore the lazy spawn — fires
//! from the post-`setresuid` main loop, at uid 998
//! (`halmasuit-compositor`). The decoder inherits that uid and
//! its user-namespace uid_map writes `998 998 1`, satisfying
//! the anti-pattern.
//!
//! ## Frame-0 readiness
//!
//! Wallpaper rendering is invariant: every frame must paint
//! SOMETHING (no black flash, ever — that's the project's purpose).
//! From [`Self::new`] through the first decoded frame, this backend
//! paints a 1×1 black placeholder texture stretched to the output.
//! The first valid frame replaces it; subsequent frames replace
//! whatever's current.
//!
//! ## Restart + fallback
//!
//! If the decoder process crashes or returns DecoderError too many
//! times in too short a window, the relay marks itself dead
//! ([`DecoderRelay::is_dead`]). At that point this backend keeps
//! rendering the last good frame (or the black placeholder if no
//! frame ever arrived). A higher-level wallpaper-engine fallback
//! (image / shader) requires the operator to configure one; not
//! Phase A.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};
use tracing::{debug, error, info, warn};

use super::backend::{FallbackKind, WallpaperBackend};
use super::decoder_relay::DecoderRelay;
use crate::drm::SceneElement;

/// Video wallpaper backend. Owns the relay (lazily spawned post-
/// privilege-drop; see module doc) and the currently-uploaded
/// frame's GLES texture.
pub struct VideoBackend {
    /// Wallpaper source path, retained for the lazy
    /// [`DecoderRelay::spawn`] call.
    source: PathBuf,
    /// Relay slot, populated on the first [`Self::poll_pending`]
    /// after halmasuit's privilege drop. `RefCell<Option<...>>`
    /// because lazy-spawn happens through a `&self` method and the
    /// slot starts empty. Once spawned (or once spawn fails fatally
    /// — see `spawn_attempted` below), the slot stays as-is.
    relay: RefCell<Option<DecoderRelay>>,
    /// True once [`Self::poll_pending`] has attempted to spawn. On
    /// spawn failure, we DO NOT retry — the wallpaper degrades to
    /// the placeholder/last-good-frame, but we don't burn CPU
    /// re-spawning every 100 ms (the relay's own restart-budget
    /// handles transient decoder crashes; failure to even fork-exec
    /// is treated as terminal).
    spawn_attempted: std::cell::Cell<bool>,
    /// Optional fallback image path to swap to when the decoder
    /// relay exhausts its restart budget (Epic #12 Req #4/#10).
    /// `None` keeps the last-good-frame / placeholder behavior;
    /// `Some(path)` makes [`Self::requested_fallback`] return
    /// `FallbackKind::Image(path)` once the relay is dead.
    fallback: Option<PathBuf>,
    /// Currently-uploaded GLES texture. Initially a 1×1 black
    /// placeholder so frame-0 has something to paint; replaced with
    /// each new decoded frame.
    current_buffer: TextureBuffer<GlesTexture>,
    /// Logical size of the current buffer (its texel extent). The
    /// `TextureRenderElement` needs an explicit `src` rect; passing
    /// `None` would default to the destination size and smear the
    /// edge texel.
    current_size: Size<i32, Logical>,
    /// Last-uploaded frame index, so we don't re-upload the same
    /// pixels twice when the decoder hasn't advanced.
    last_uploaded_idx: Option<u64>,
}

impl VideoBackend {
    /// Stage the backend: store the source path + allocate the
    /// placeholder texture. Does NOT spawn the decoder — that
    /// happens lazily on the first [`Self::poll_pending`], which
    /// fires from a calloop timer registered AFTER the compositor's
    /// `setresuid` (see module doc).
    ///
    /// # Errors
    ///
    /// Texture-import failures (placeholder allocation) are
    /// propagated. Decoder spawn failures are deferred and surface
    /// as a warning during the first `poll_pending`.
    pub fn new(
        renderer: &mut GlesRenderer,
        source: &Path,
        _loop_playback: bool,
        fallback: Option<PathBuf>,
    ) -> io::Result<Self> {
        // Phase A: always loop the wallpaper (decoder's loop_playback
        // = true sent unconditionally by DecoderRelay::spawn). The
        // _loop_playback parameter is reserved for the future
        // dynamic-config epic; for now we ignore it.

        // 1×1 black placeholder texture (RGBA). Stretched to the
        // output by `render_element`, this paints a black wallpaper
        // until the first decoded frame arrives.
        let placeholder = [0u8, 0u8, 0u8, 255u8];
        let buffer = TextureBuffer::from_memory(
            renderer,
            &placeholder,
            Fourcc::Abgr8888,
            (1, 1),
            false,
            1,
            Transform::Normal,
            None,
        )
        .map_err(|e| io::Error::other(format!("video placeholder texture: {e}")))?;

        Ok(Self {
            source: source.to_path_buf(),
            relay: RefCell::new(None),
            spawn_attempted: std::cell::Cell::new(false),
            fallback,
            current_buffer: buffer,
            current_size: Size::from((1, 1)),
            last_uploaded_idx: None,
        })
    }

    /// Lazy decoder spawn. Called on the first [`Self::poll_pending`]
    /// (timer-driven, post-`setresuid`). On failure we log and mark
    /// `spawn_attempted` so we don't retry forever.
    fn ensure_relay_spawned(&self) {
        if self.spawn_attempted.get() {
            return;
        }
        self.spawn_attempted.set(true);
        match DecoderRelay::spawn(&self.source) {
            Ok(relay) => {
                info!(
                    source = %self.source.display(),
                    "video wallpaper: decoder spawn deferred until post-deprivilege"
                );
                *self.relay.borrow_mut() = Some(relay);
            }
            Err(err) => {
                warn!(
                    error = %err,
                    source = %self.source.display(),
                    "video wallpaper: decoder spawn failed; keeping placeholder"
                );
            }
        }
    }
}

impl WallpaperBackend for VideoBackend {
    /// Video wallpapers need the wallpaper-engine tick to drive
    /// renders unconditionally: the decoder relay produces frames
    /// asynchronously, and `render_element` is what consumes them
    /// (via the `poll_frames()` call at the top + the GPU upload of
    /// the latest decoded frame). Without continuous renders, queued
    /// decoder frames pile up unused and the on-screen video freezes
    /// the moment the render path goes idle — most visibly during
    /// the post-PrepareForShutdown window when no Wayland client
    /// commits drive `frame_pending`.
    fn wants_continuous_render(&self) -> bool {
        true
    }

    fn requested_fallback(&self) -> Option<FallbackKind> {
        // Only request a fallback when (a) the relay has been spawned
        // (otherwise there's nothing to fail), (b) the relay is dead
        // (budget exhausted), AND (c) the operator configured one.
        // Without (c) the engine keeps the last-good-frame /
        // placeholder behavior as documented in the module header.
        let relay = self.relay.borrow();
        let relay_dead = relay.as_ref().is_some_and(DecoderRelay::is_dead);
        if relay_dead {
            self.fallback.clone().map(FallbackKind::Image)
        } else {
            None
        }
    }

    fn poll_pending(&self) {
        // Lazy-spawn on the first call. The wallpaper-tick calloop
        // timer in main.rs is registered before the privilege-drop
        // block but only DISPATCHES from the main loop, which runs
        // AFTER setresuid — so this call site executes at the
        // configured compositor uid, not at the inherited root uid
        // of WallpaperBackend::new's call site.
        self.ensure_relay_spawned();
        if let Some(r) = self.relay.borrow().as_ref()
            && !r.is_dead()
        {
            r.poll_frames();
        }
    }

    fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
    ) -> io::Result<SceneElement> {
        // Drain any frames produced since the last render. The 100 ms
        // wallpaper-tick calloop timer in main.rs ALSO polls (and is
        // the source of polling when the render path has idled); the
        // double-call here is intentional — under active rendering it
        // cuts the frame-arrival latency from up to 100 ms (timer
        // cadence) down to one render tick. `poll_frames()` is a
        // cheap no-op when nothing is pending.
        if let Some(r) = self.relay.borrow().as_ref()
            && !r.is_dead()
        {
            r.poll_frames();
        }

        // If a new frame is available, re-upload as the current
        // texture. The upload happens INSIDE the relay's
        // `latest_frame()` borrow so smithay's `from_memory` reads
        // the bytes directly out of the relay's buffer — avoids the
        // per-frame `Vec::clone()` (8 MiB at 1080p) the prior shape
        // forced. We rebuild TextureBuffer (rather than mutating in
        // place) — smithay's TextureBuffer doesn't expose a raw-
        // texel-update API, and the GPU driver makes the rebuild
        // cheap for stable sizes.
        let upload: Option<(u64, i32, i32, TextureBuffer<GlesTexture>)> =
            self.relay.borrow().as_ref().and_then(|r| {
                r.latest_frame().and_then(|frame| {
                    if Some(frame.frame_idx) == self.last_uploaded_idx {
                        return None;
                    }
                    let Ok(w_i32) = i32::try_from(frame.width) else {
                        error!(width = frame.width, "video: frame width > i32::MAX");
                        return None;
                    };
                    let Ok(h_i32) = i32::try_from(frame.height) else {
                        error!(height = frame.height, "video: frame height > i32::MAX");
                        return None;
                    };
                    match TextureBuffer::from_memory(
                        renderer,
                        &frame.bytes,
                        Fourcc::Abgr8888,
                        (w_i32, h_i32),
                        false,
                        1,
                        Transform::Normal,
                        None,
                    ) {
                        Ok(buffer) => Some((frame.frame_idx, w_i32, h_i32, buffer)),
                        Err(err) => {
                            // Log but keep the previous frame. A
                            // texture import failure shouldn't break
                            // the wallpaper.
                            error!(
                                error = %err,
                                "video: texture import failed; keeping last frame"
                            );
                            None
                        }
                    }
                })
            });
        if let Some((idx, w_i32, h_i32, buffer)) = upload {
            self.current_buffer = buffer;
            self.current_size = Size::from((w_i32, h_i32));
            self.last_uploaded_idx = Some(idx);
            debug!(
                frame_idx = idx,
                width = w_i32,
                height = h_i32,
                "video: uploaded frame"
            );
        }

        // 3. Render whatever's current (placeholder until first frame
        //    arrives; last good frame thereafter). Source = the
        //    texture's own extent (NOT None — same gotcha as
        //    ImageBackend), so smithay scales it to the full output.
        let src = Rectangle::<f64, Logical>::from_size(self.current_size.to_f64());
        Ok(SceneElement::Wallpaper(
            TextureRenderElement::from_texture_buffer(
                Point::<f64, smithay::utils::Physical>::from((0.0, 0.0)),
                &self.current_buffer,
                None,
                Some(src),
                Some(output_size),
                Kind::Unspecified,
            ),
        ))
    }
}
