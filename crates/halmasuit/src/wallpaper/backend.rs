// halmasuit/src/wallpaper/backend.rs — the WallpaperBackend trait.
//
// Three backends (image / shader / video) share one shape: each
// produces a [`SceneElement`] per frame and is opaque-by-default
// (the no-flash test fixture is opaque; production wallpapers'
// opacity is the user's choice). Each backend's constructor is
// required to synchronously commit its first renderable state
// BEFORE halmasuit's first composite — that is the frame-0 readiness
// invariant the no-flash test depends on (epic G1/R3/R6). For
// [`ImageBackend`](super::ImageBackend) that means decoding the PNG
// into a `TextureBuffer` during `new`; for `ShaderBackend` it will
// mean compiling and running the first pass synchronously; for
// `VideoBackend` it will mean decoding the first frame synchronously.

use std::io;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Size};

use crate::drm::SceneElement;

/// One pluggable wallpaper backend. See
/// [`super::ImageBackend`](super::ImageBackend) for the live
/// implementation; `ShaderBackend` and `VideoBackend` are typed
/// stubs that follow-up tasks fill in.
///
/// Implementations are `Send`-bound so a backend can be moved across
/// threads (the video backend will decode on a non-render task — epic
/// anti-pattern "NO video decoder on the compositor render thread").
pub trait WallpaperBackend: Send {
    /// Build the bottom-most render element for the current frame at
    /// the given output size. Called once per composite; ImageBackend
    /// rebuilds the cheap `TextureRenderElement` view of its
    /// already-loaded buffer; ShaderBackend will re-run its fragment
    /// pass with updated uniforms; VideoBackend will import the most
    /// recently decoded frame.
    ///
    /// # Errors
    ///
    /// Bubbles any backend-specific render-time failure with context.
    fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
    ) -> io::Result<SceneElement>;

    /// Optional background tick — called by a periodic calloop timer
    /// independent of the render path. Default no-op; only
    /// [`super::VideoBackend`] overrides this to drive
    /// `DecoderRelay::poll_frames`, which guarantees the relay
    /// observes decoder death (IPC EOF) and runs its respawn-budget
    /// machinery even when the render loop has stopped firing
    /// (e.g. wallpaper has reached a static visual state with no
    /// surface commits forcing new vblanks). The render path's
    /// `render_element` ALSO calls `poll_frames` when it runs; this
    /// hook is the keepalive that doesn't rely on the render loop.
    fn poll_pending(&self) {}
}
