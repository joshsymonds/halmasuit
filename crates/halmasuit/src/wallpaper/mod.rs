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

    /// Forward a fired system event to the active backend; returns the
    /// GLSL uniform names the backend updated (empty if no backend, or
    /// no binding matches `event_name`). See
    /// [`WallpaperBackend::notify_event`].
    pub(crate) fn notify_event(&mut self, event_name: &str, value: f32) -> Vec<String> {
        self.backend
            .as_mut()
            .map(|b| b.notify_event(event_name, value))
            .unwrap_or_default()
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
    /// Before rendering, asks the active backend whether it wants
    /// the engine to swap in a fallback ([`WallpaperBackend::requested_fallback`]).
    /// If yes, constructs the fallback and atomically replaces the
    /// active backend BEFORE the render call. The swap is bounded —
    /// a fallback that itself requests a fallback is logged but not
    /// recursed.
    ///
    /// # Errors
    ///
    /// Bubbles any backend-specific render error (e.g. shader compile
    /// fault on first call, video frame decode failure). Fallback-
    /// construction failures are logged but NOT propagated — the
    /// existing backend keeps rendering whatever it already had.
    pub fn render_element(
        &mut self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
    ) -> io::Result<Option<SceneElement>> {
        let _ = self.maybe_swap_fallback(renderer);
        self.backend.as_mut().map_or_else(
            || Ok(None),
            |b| b.render_element(renderer, output_size).map(Some),
        )
    }

    /// If the active backend has requested a fallback, construct it
    /// and atomically replace the active backend. Returns `true`
    /// when an actual swap happened (so timer-driven callers can
    /// queue an explicit render — without it, an idle render loop
    /// would leave the user staring at the last-decoded frame
    /// indefinitely instead of the configured fallback).
    /// Idempotent (the freshly-installed fallback's
    /// `requested_fallback` returns `None`); construction errors
    /// are logged and the existing backend stays.
    fn maybe_swap_fallback(&mut self, renderer: &mut GlesRenderer) -> bool {
        let request = self.backend.as_ref().and_then(|b| b.requested_fallback());
        let Some(kind) = request else {
            return false;
        };
        match kind {
            backend::FallbackKind::Image(path) => match ImageBackend::new(renderer, &path) {
                Ok(img) => {
                    tracing::info!(
                        path = %path.display(),
                        "wallpaper: swapping to fallback image (relay-dead)"
                    );
                    self.backend = Some(Box::new(img));
                    true
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        path = %path.display(),
                        "wallpaper: fallback ImageBackend construction failed; \
                         keeping current backend"
                    );
                    false
                }
            },
        }
    }

    /// Periodic background tick — dispatched by a calloop timer
    /// independent of the render path. Two responsibilities:
    /// 1. Delegate to the active backend's
    ///    [`WallpaperBackend::poll_pending`] (currently only
    ///    [`VideoBackend`] does useful work; drives
    ///    `DecoderRelay::poll_frames`).
    /// 2. Check for and execute a fallback swap if the active
    ///    backend has requested one. The render path's
    ///    [`Self::render_element`] does the same check, but after
    ///    the relay dies the render loop idles (no new content =
    ///    no vblank = no render); the timer is the only thing that
    ///    keeps firing.
    ///
    /// Returns `true` iff a fallback swap actually fired this
    /// tick. Callers use this to queue an explicit render so the
    /// newly-installed fallback reaches the screen instead of
    /// waiting for the next external render trigger (which, after
    /// a relay-death, may never come).
    pub fn tick(&mut self, renderer: &mut GlesRenderer) -> bool {
        if let Some(b) = self.backend.as_ref() {
            b.poll_pending();
        }
        self.maybe_swap_fallback(renderer)
    }

    /// Does the active backend want continuous renders driven by the
    /// wallpaper-tick timer, regardless of whether [`Self::tick`]
    /// requested a fallback swap?
    ///
    /// Static (image) backends return `false`: nothing per-frame
    /// changes, so re-rendering the same texture every tick wastes
    /// GLES draw calls without producing distinct frames. Animated
    /// (shader, video) backends return `true`: their on-screen
    /// content varies frame-to-frame, and the tick cadence is what
    /// drives the animation.
    ///
    /// `false` when no backend is configured (`has_backend() ==
    /// false`) — the SKIP-path test setup, the dev/no-DRM path.
    #[must_use]
    pub fn wants_continuous_render(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(|b| b.wants_continuous_render())
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
