//! Video wallpaper backend (Epic #12).
//!
//! Forks `halmasuit-decoder` as a sandboxed subprocess (the
//! decoder lives in a private user/net/mount namespace, under
//! rlimits, with PR_SET_NO_NEW_PRIVS set) and consumes decoded RGBA8
//! frames over a SOCK_SEQPACKET back-channel. Every frame is
//! validated (`bytes_len <= MAX_FRAME_BYTES` AND `width * height *
//! 4 == bytes_len`) before becoming a GLES texture.
//!
//! ## Frame-0 readiness
//!
//! Wallpaper rendering is invariant: every frame must paint
//! SOMETHING (no black flash, ever — that's the project's purpose).
//! Until the decoder hands us its first frame, this backend paints
//! a 1×1 black placeholder texture stretched to the output. The
//! first valid frame replaces it; subsequent frames replace
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

use std::io;
use std::path::Path;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};
use tracing::{debug, error};

use super::backend::WallpaperBackend;
use super::decoder_relay::DecoderRelay;
use crate::drm::SceneElement;

/// Video wallpaper backend. Owns the relay + the currently-uploaded
/// frame's GLES texture.
pub struct VideoBackend {
    relay: DecoderRelay,
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
    /// Construct the backend: spawn the decoder, allocate a 1×1
    /// black placeholder texture, send LoadFile with the wallpaper
    /// fd via SCM_RIGHTS.
    ///
    /// # Errors
    ///
    /// Propagates decoder-spawn or wallpaper-open failures from
    /// `DecoderRelay::spawn`. Texture-import failures are also
    /// propagated.
    pub fn new(
        renderer: &mut GlesRenderer,
        source: &Path,
        _loop_playback: bool,
    ) -> io::Result<Self> {
        // Phase A: always loop the wallpaper (decoder's loop_playback
        // = true sent unconditionally by DecoderRelay::spawn). The
        // _loop_playback parameter is reserved for the future
        // dynamic-config epic; for now we ignore it.
        let relay = DecoderRelay::spawn(source)
            .map_err(|e| io::Error::other(format!("decoder relay spawn: {e}")))?;

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
            relay,
            current_buffer: buffer,
            current_size: Size::from((1, 1)),
            last_uploaded_idx: None,
        })
    }
}

impl WallpaperBackend for VideoBackend {
    fn poll_pending(&self) {
        if !self.relay.is_dead() {
            self.relay.poll_frames();
        }
    }

    fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
    ) -> io::Result<SceneElement> {
        // 1. Drain any frames the decoder produced since last render.
        //    Once the relay's restart budget is exhausted, `is_dead`
        //    returns true and we stop polling — keeps rendering the
        //    last good frame (or placeholder).
        if !self.relay.is_dead() {
            self.relay.poll_frames();
        }

        // 2. If a new frame is available, re-upload as the current
        //    texture. We rebuild TextureBuffer (rather than mutating
        //    in-place) — smithay's TextureBuffer doesn't expose a
        //    raw-texel-update API, and the GPU driver makes the
        //    rebuild cheap for stable sizes.
        let new_frame_data: Option<(u64, u32, u32, Vec<u8>)> = {
            self.relay.latest_frame().and_then(|frame| {
                if Some(frame.frame_idx) == self.last_uploaded_idx {
                    None
                } else {
                    Some((
                        frame.frame_idx,
                        frame.width,
                        frame.height,
                        frame.bytes.clone(),
                    ))
                }
            })
        };
        if let Some((idx, w, h, bytes)) = new_frame_data {
            let w_i32 = i32::try_from(w)
                .map_err(|_| io::Error::other(format!("video frame width {w} > i32::MAX")))?;
            let h_i32 = i32::try_from(h)
                .map_err(|_| io::Error::other(format!("video frame height {h} > i32::MAX")))?;
            match TextureBuffer::from_memory(
                renderer,
                &bytes,
                Fourcc::Abgr8888,
                (w_i32, h_i32),
                false,
                1,
                Transform::Normal,
                None,
            ) {
                Ok(buffer) => {
                    self.current_buffer = buffer;
                    self.current_size = Size::from((w_i32, h_i32));
                    self.last_uploaded_idx = Some(idx);
                    debug!(
                        frame_idx = idx,
                        width = w,
                        height = h,
                        "video: uploaded frame"
                    );
                }
                Err(err) => {
                    // Log but keep the previous frame. A texture
                    // import failure shouldn't break the wallpaper.
                    error!(error = %err, "video: texture import failed; keeping last frame");
                }
            }
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
