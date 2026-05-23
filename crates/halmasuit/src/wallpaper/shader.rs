// halmasuit/src/wallpaper/shader.rs — GLSL fragment-shader backend (STUB).
//
// Phase-A scaffold only. The struct + constructor signature pins the
// shape the follow-up task fills in. The intended implementation:
// compile the user-supplied GLSL ES 100 fragment shader via smithay's
// `GlesRenderer::compile_custom_pixel_shader`, render it to an
// offscreen texture on every frame with the declared uniforms wired
// to their config-specified sources (auto-* engine values, static
// constants, event-time/event-value bus markers — only auto and
// static fire in Phase-A; the event-* kinds parse but warn that no
// bus is connected).
//
// The Shadertoy convention (`iResolution`, `iTime`, `mainImage(out
// vec4, in vec2)`) is a thin GLSL preamble wrapped around the user
// shader; both Shadertoy-shape and declared-uniform shaders work via
// the same compile + per-frame uniform-update pipeline.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Size};

use super::backend::WallpaperBackend;
use super::config::UniformBinding;
use crate::drm::SceneElement;

/// GLSL fragment-shader wallpaper backend. Phase-A stub.
///
/// The fields are present so the follow-up task can read them
/// without re-plumbing the config layer; they are `#[allow(dead_code)]`
/// only until the live implementation lands.
#[allow(dead_code, reason = "Phase-A stub; follow-up task wires these")]
pub struct ShaderBackend {
    source: PathBuf,
    uniforms: HashMap<String, UniformBinding>,
}

impl ShaderBackend {
    /// Construct a `ShaderBackend` from its config. Phase-A: parses
    /// and stores the inputs but does not compile; the renderer
    /// construction is the follow-up task's deliverable.
    ///
    /// # Errors
    ///
    /// Phase-A: returns "ShaderBackend not yet wired" so the
    /// compositor fails closed when the operator picks
    /// `services.halmasuit.wallpaper = { type = "shader"; ... }`
    /// before the live implementation lands.
    pub fn new(
        _renderer: &mut GlesRenderer,
        source: PathBuf,
        uniforms: HashMap<String, UniformBinding>,
    ) -> io::Result<Self> {
        // The Phase-A stub deliberately fails at construction rather
        // than producing a broken backend. Constructed shapes are
        // typed correctly so callers can be written today, but the
        // live implementation is the next task's deliverable.
        let _ = Self { source, uniforms };
        Err(io::Error::other(
            "ShaderBackend not yet wired (Phase-A scaffold); see wallpaper-engine epic",
        ))
    }
}

impl WallpaperBackend for ShaderBackend {
    fn render_element(
        &mut self,
        _renderer: &mut GlesRenderer,
        _output_size: Size<i32, Logical>,
    ) -> io::Result<SceneElement> {
        // Unreachable: `new` already fails closed in Phase-A.
        unreachable!("ShaderBackend::new fails closed in Phase-A")
    }
}
