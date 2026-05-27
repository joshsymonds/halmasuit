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
use std::path::PathBuf;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Size};

use crate::drm::SceneElement;

/// What kind of fallback a backend is asking the engine to swap in
/// (Epic #12 Req #4/#10 — relay-dead fallback for the video backend).
/// Phase A: image only. Future expansions (solid color, shader) plug
/// in here without changing the engine's swap logic.
#[derive(Debug, Clone)]
pub enum FallbackKind {
    /// Construct an `ImageBackend` against this path and swap.
    Image(PathBuf),
}

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

    /// Has this backend hit a terminal failure and want the engine
    /// to swap in a fallback? Default `None` (no fallback requested).
    /// [`super::VideoBackend`] returns `Some(FallbackKind::Image(_))`
    /// once its `DecoderRelay` exhausts its restart budget AND the
    /// operator configured a fallback image (Epic #12 Req #4/#10).
    ///
    /// `WallpaperEngine` calls this on every `render_element` cycle
    /// and performs the swap before the render call. The swap is
    /// expected to be idempotent at the engine level — implementers
    /// may return `Some` repeatedly until swapped out.
    fn requested_fallback(&self) -> Option<FallbackKind> {
        None
    }

    /// Does this backend need the wallpaper-engine tick to keep
    /// driving render calls regardless of whether [`Self::requested_fallback`]
    /// has fired?
    ///
    /// Default `false` — suitable for static backends ([`super::ImageBackend`])
    /// where the kernel keeps scanning out the last-flipped framebuffer
    /// and no per-frame state advances.
    ///
    /// [`super::ShaderBackend`] and [`super::VideoBackend`] override this
    /// to `true`: a shader's `iTime` uniform advances every
    /// `render_element` call (so cadence == animation rate), and a video
    /// backend's decoder produces frames asynchronously that the render
    /// path must consume on every tick. Without this, the wallpaper-tick
    /// timer in `main.rs` would only call `render_one_frame` when a
    /// fallback swap fires, leaving shader/video frozen on the last
    /// frame whenever no Wayland client commits drive `frame_pending`
    /// — most visibly during the post-PrepareForShutdown window.
    fn wants_continuous_render(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::renderer::gles::GlesRenderer;
    use smithay::utils::{Logical, Size};

    /// Minimal backend that inherits every default. Verifies the
    /// trait's default `wants_continuous_render` is `false` — the
    /// behavior `ImageBackend` relies on without overriding.
    struct DefaultBackend;
    impl WallpaperBackend for DefaultBackend {
        fn render_element(
            &mut self,
            _renderer: &mut GlesRenderer,
            _output_size: Size<i32, Logical>,
        ) -> io::Result<SceneElement> {
            unreachable!("DefaultBackend is for trait-method tests only")
        }
    }

    /// Backend that overrides `wants_continuous_render` to `true`.
    /// Verifies the override path compiles and dispatches correctly —
    /// the contract `ShaderBackend` and `VideoBackend` both rely on.
    struct ContinuousBackend;
    impl WallpaperBackend for ContinuousBackend {
        fn render_element(
            &mut self,
            _renderer: &mut GlesRenderer,
            _output_size: Size<i32, Logical>,
        ) -> io::Result<SceneElement> {
            unreachable!("ContinuousBackend is for trait-method tests only")
        }
        fn wants_continuous_render(&self) -> bool {
            true
        }
    }

    #[test]
    fn wants_continuous_render_defaults_to_false() {
        assert!(!DefaultBackend.wants_continuous_render());
    }

    #[test]
    fn wants_continuous_render_can_be_overridden_to_true() {
        assert!(ContinuousBackend.wants_continuous_render());
    }

    /// Dynamic dispatch through a `Box<dyn WallpaperBackend>` reaches
    /// the override — this is what `WallpaperEngine` actually does
    /// (it stores `Option<Box<dyn WallpaperBackend>>`). Guards against
    /// a future regression where someone makes the method `Self`-bound.
    #[test]
    fn wants_continuous_render_dispatches_through_dyn_trait() {
        let static_b: Box<dyn WallpaperBackend> = Box::new(DefaultBackend);
        let continuous_b: Box<dyn WallpaperBackend> = Box::new(ContinuousBackend);
        assert!(!static_b.wants_continuous_render());
        assert!(continuous_b.wants_continuous_render());
    }
}
