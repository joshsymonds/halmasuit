// halmasuit/src/wallpaper — the wallpaper engine.
//
// Owns the bottom-most scene element of every composited frame. Hosts
// pluggable backends behind the [`WallpaperBackend`] trait: today only
// [`ImageBackend`] is wired ([`ShaderBackend`] and [`VideoBackend`]
// are typed stubs); the trait shape is the contract follow-up tasks
// fill in. The engine is internally swap-capable (private API; no
// public swap surface in this epic — see the wallpaper epic
// description).
//
// Why this lives in its own module rather than inline in `drm.rs`:
// the bottom-most plane is no longer "a static PNG hard-coded into
// the renderer." It is a content surface with three plug-in shapes
// (image / shader / video) and a uniform-pipeline contract that
// admits bus-event-driven values (Phase-B). Keeping every backend
// behind one trait, with one place that owns the engine, lets the
// renderer call site stay backend-agnostic.

pub mod backend;
pub mod config;
pub mod decoder_relay;
pub mod image;
pub mod shader;
pub mod video;

use std::io;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Size};

use crate::drm::SceneElement;

pub use backend::WallpaperBackend;
pub use config::WallpaperConfig;
pub use image::ImageBackend;
pub use shader::ShaderBackend;
pub use video::VideoBackend;

/// The wallpaper engine. Owns the active backend (or none — the
/// legacy clear-only path used by non-visual integration tests) and
/// builds the bottom-most scene element for the renderer to composite.
///
/// The active backend is private state; a swap API is deliberately
/// not exposed in Phase-A (the bus-event swap epic will add it
/// without changing the public surface).
pub struct WallpaperEngine {
    backend: Option<Box<dyn WallpaperBackend>>,
}

impl WallpaperEngine {
    /// Construct an engine with no backend — the legacy clear-only
    /// scene, used by non-visual integration tests where no wallpaper
    /// is configured.
    #[must_use]
    pub fn empty() -> Self {
        Self { backend: None }
    }

    /// Construct an engine wrapping the given backend.
    #[must_use]
    pub fn with_backend(backend: Box<dyn WallpaperBackend>) -> Self {
        Self {
            backend: Some(backend),
        }
    }

    /// `true` iff a backend is configured. Drives the frame-0 anchor
    /// emission in `main.rs` (the `ClientFirstFrame { Wallpaper }`
    /// event is suppressed when no backend is configured).
    #[must_use]
    pub fn has_backend(&self) -> bool {
        self.backend.is_some()
    }

    /// Build the bottom-most scene element for the current frame, or
    /// `None` if no backend is configured. Called once per frame from
    /// `DrmBackend::scene_elements`; the result is appended LAST to
    /// the front-to-back element list so every surface composites
    /// over the wallpaper (epic G1/R3/R6 — frame 0 must already be
    /// wallpaper-covered).
    ///
    /// # Errors
    ///
    /// Bubbles any backend-specific render error (e.g. shader compile
    /// fault on first call, video frame decode failure).
    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
    ) -> io::Result<Option<SceneElement>> {
        self.backend.as_mut().map_or_else(
            || Ok(None),
            |b| b.render_element(renderer, output_size).map(Some),
        )
    }

    /// Periodic background tick — dispatched by a calloop timer
    /// independent of the render path. Delegates to the active
    /// backend's [`WallpaperBackend::poll_pending`]. Today only
    /// [`VideoBackend`] makes meaningful use of this hook (it
    /// drives `DecoderRelay::poll_frames`); the default no-op
    /// keeps the call free for image/shader/empty configurations.
    pub fn poll_pending(&self) {
        if let Some(b) = self.backend.as_ref() {
            b.poll_pending();
        }
    }

    /// Private swap entry point — exists so future epics (bus-event
    /// driven swap, runtime config reload) can plug in without
    /// reshaping the engine. Phase-A: never called; intentionally
    /// `pub(crate)`, not `pub`, so the public surface stays read-only.
    /// The full no-flash-on-swap pre-render dance is the consumer
    /// epic's responsibility; this is just the seat for it.
    #[allow(dead_code, reason = "Phase-A: swap consumer lands in a follow-up epic")]
    pub(crate) fn swap(
        &mut self,
        new_backend: Box<dyn WallpaperBackend>,
    ) -> Option<Box<dyn WallpaperBackend>> {
        self.backend.replace(new_backend)
    }
}

/// The index the wallpaper occupies in the front-to-back element
/// list, or `None` when no backend is configured.
///
/// With `n_surfaces` surface elements already pushed, the wallpaper
/// goes at index `n_surfaces` — i.e. it is appended LAST, making it
/// the bottom-most element of the `n_surfaces + 1`-element scene.
/// Frame 0 with no surfaces is therefore exactly the wallpaper,
/// never a solid clear (epic G1/R3/R6). With no backend configured,
/// the scene is just the surfaces (the legacy clear-only path for
/// non-visual integration tests).
///
/// `scene_elements` in `drm.rs` branches on this; the contract is
/// unit-pinned by `tests::wallpaper_is_the_bottom_most_element`.
#[must_use]
pub fn wallpaper_slot(n_surfaces: usize, has_backend: bool) -> Option<usize> {
    has_backend.then_some(n_surfaces)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wallpaper is the bottom-most render element: appended LAST
    /// to the front-to-back element list (index 0 = topmost), so every
    /// wl_client composites over it and frame 0 is already the
    /// wallpaper — there is no pre-client solid phase (epic G1/R3/R6).
    /// `DrmBackend::scene_elements` branches on `wallpaper_slot`; this
    /// pins that contract without a GPU.
    #[test]
    fn wallpaper_is_the_bottom_most_element() {
        // Frame 0 / no surfaces, wallpaper configured: the scene is
        // exactly one element — the wallpaper — at index 0 (== last).
        assert_eq!(wallpaper_slot(0, true), Some(0));

        // With N surfaces the wallpaper slot is N: the last index of
        // an (N+1)-element list, i.e. bottom-most.
        for n in [1, 3, 7] {
            let slot = wallpaper_slot(n, true).expect("wallpaper configured");
            assert_eq!(slot, n, "wallpaper must be appended after every surface");
            let total_len = n + 1;
            assert_eq!(slot, total_len - 1, "wallpaper must be the LAST element");
        }

        // No wallpaper configured (non-visual integration paths): no
        // wallpaper slot — the legacy clear-only scene is unchanged.
        for n in [0, 1, 5] {
            assert_eq!(wallpaper_slot(n, false), None);
        }
    }
}
