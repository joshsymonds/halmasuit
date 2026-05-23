// halmasuit/src/wallpaper/video.rs — video wallpaper backend (STUB).
//
// Phase-A scaffold only. The struct + constructor signature pins the
// shape the follow-up task fills in. The intended implementation:
// `ffmpeg-the-third` MIT bindings + a minimal libavcodec build
// (`--disable-everything --enable-decoder=h264,av1 --enable-libdav1d
// --enable-small`, NO `--enable-gpl`). Decode runs on a non-render
// task; frames import as `AV_PIX_FMT_DRM_PRIME` dmabuf descriptors
// that smithay imports as `Dmabuf` → GLES texture via
// `EGL_EXT_image_dma_buf_import`. The first decoded frame is ready
// synchronously before halmasuit's first composite (frame-0
// readiness; epic anti-pattern "NO frame-0 readiness regression").
//
// Hardware decode acceleration (VA-API, Vulkan Video) is deferred to
// a follow-up epic — Mesa ANV/RADV vk_video coverage in 2026 is not
// uniform and Rust bindings are not production-ready (research
// finding, not preference).

use std::io;
use std::path::PathBuf;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Size};

use super::backend::WallpaperBackend;
use crate::drm::SceneElement;

/// Video wallpaper backend. Phase-A stub.
///
/// Fields present so the follow-up task can plumb them without
/// re-shaping the config layer; `#[allow(dead_code)]` only until
/// the live implementation lands.
#[allow(dead_code, reason = "Phase-A stub; follow-up task wires these")]
pub struct VideoBackend {
    source: PathBuf,
    loop_playback: bool,
}

impl VideoBackend {
    /// Construct a `VideoBackend` from its config. Phase-A: parses
    /// and stores the inputs but does not open a decoder; the live
    /// implementation is the next task's deliverable.
    ///
    /// # Errors
    ///
    /// Phase-A: returns "VideoBackend not yet wired" so the
    /// compositor fails closed when the operator picks
    /// `services.halmasuit.wallpaper = { type = "video"; ... }`
    /// before the live implementation lands.
    pub fn new(
        _renderer: &mut GlesRenderer,
        source: PathBuf,
        loop_playback: bool,
    ) -> io::Result<Self> {
        let _ = Self {
            source,
            loop_playback,
        };
        Err(io::Error::other(
            "VideoBackend not yet wired (Phase-A scaffold); see wallpaper-engine epic",
        ))
    }
}

impl WallpaperBackend for VideoBackend {
    fn render_element(
        &mut self,
        _renderer: &mut GlesRenderer,
        _output_size: Size<i32, Logical>,
    ) -> io::Result<SceneElement> {
        // Unreachable: `new` already fails closed in Phase-A.
        unreachable!("VideoBackend::new fails closed in Phase-A")
    }
}
