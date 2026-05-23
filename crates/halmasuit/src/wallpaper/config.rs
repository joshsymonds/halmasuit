// halmasuit/src/wallpaper/config.rs — the wallpaper config schema.
//
// One discriminated union ([`WallpaperConfig`]) over the three
// backend shapes (image / shader / video), plus the
// declared-uniforms pipeline types ([`UniformBinding`],
// [`StaticValue`]) the shader backend will consume. Phase-A:
// `WallpaperConfig::Image` is fully wired; the other variants and
// the four `UniformBinding` kinds are typed scaffolding the
// follow-up tasks fill in.
//
// Loaded from env in [`from_env`]: today's shape is a single
// `HALMASUIT_WALLPAPER_PATH` path with file-extension type
// inference. The richer TOML schema (named uniforms, per-monitor
// settings, etc.) lands with the shader-uniforms task that actually
// needs it — defining the Rust types now lets that task ship a
// config file shape without re-plumbing the parser.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Top-level wallpaper config. Discriminated union over the three
/// backend shapes.
#[derive(Debug, Clone)]
pub enum WallpaperConfig {
    /// Static image (PNG/JPEG/WebP, via the `image` crate).
    Image {
        /// Absolute path to the image file.
        source: PathBuf,
    },
    /// GLSL ES 100 fragment shader with a declared-uniforms pipeline.
    /// Phase-A: the variant is recognised but the backend is a stub —
    /// constructing this from a config will fail closed.
    Shader {
        /// Absolute path to the `.frag` / `.glsl` file.
        source: PathBuf,
        /// Named uniforms the shader declares and the engine binds.
        /// Phase-A: only `auto-*` and static-typed kinds fire; the
        /// `event-*` kinds parse cleanly and warn that no bus is
        /// connected (the bus-event epic wires them).
        uniforms: HashMap<String, UniformBinding>,
    },
    /// Video file (h264 or AV1; software decode via libavcodec
    /// configured minimal). Phase-A: stub; the live implementation
    /// is the video-backend task.
    Video {
        /// Absolute path to the video file.
        source: PathBuf,
        /// Whether to loop the video. Defaults to `true` for
        /// wallpaper use.
        loop_playback: bool,
    },
}

/// What feeds a named uniform in a shader wallpaper.
///
/// Phase-A wires only `Auto*` and `Static`; `EventTime` /
/// `EventValue` parse cleanly and the engine logs a one-time
/// "wallpaper bus not yet connected" warning. The bus-event epic
/// connects them without changing this enum (that is the point of
/// typing all four kinds today).
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "Phase-A scaffold: variants typed now so shader/bus epics ship without re-shaping the config enum"
)]
pub enum UniformBinding {
    /// Seconds since the wallpaper started, as `float`. Shadertoy's
    /// `iTime`.
    AutoTime,
    /// `(width, height, pixel_aspect)` as `vec3`. Shadertoy's
    /// `iResolution`.
    AutoResolution,
    /// Frame counter, as `int`. Shadertoy's `iFrame`.
    AutoFrame,
    /// Seconds since the previous frame, as `float`. Shadertoy's
    /// `iTimeDelta`.
    AutoDelta,
    /// Mouse position + click as `vec4 (x, y, click_x, click_y)`.
    /// Wallpaper has no mouse — Phase-A always writes zeros.
    /// Shadertoy's `iMouse`.
    AutoMouse,
    /// A typed constant from the config.
    Static(StaticValue),
    /// Phase-B: write `current_time` into this uniform when the
    /// named bus event fires. Phase-A: parses, warns "no bus
    /// connected", never writes.
    EventTime {
        /// The bus event name to listen for.
        event: String,
    },
    /// Phase-B: write the bus event's payload into this uniform.
    /// Phase-A: parses, warns "no bus connected", never writes.
    EventValue {
        /// The bus event name to listen for.
        event: String,
    },
}

/// A typed static value for a `Static` uniform binding.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "Phase-A scaffold: variants typed now so shader epic ships without re-shaping the value enum"
)]
pub enum StaticValue {
    /// `float`.
    Float(f32),
    /// `vec2`.
    Vec2([f32; 2]),
    /// `vec3`.
    Vec3([f32; 3]),
    /// `vec4`.
    Vec4([f32; 4]),
    /// `int`.
    Int(i32),
    /// `bool`.
    Bool(bool),
}

/// Read the wallpaper config from environment variables.
///
/// Phase-A: only `HALMASUIT_WALLPAPER_PATH` is read. Extension
/// inference picks the backend type:
///
/// | extension                  | backend           |
/// |----------------------------|-------------------|
/// | `.png` / `.jpg` / `.jpeg` / `.webp` | image     |
/// | `.frag` / `.glsl`          | shader (Phase-A stub) |
/// | `.mp4` / `.webm` / `.mkv`  | video (Phase-A stub)  |
///
/// Returns `None` when the env var is unset — the legacy
/// clear-only scene used by non-visual integration tests.
///
/// Future: when the shader-uniforms task lands, a
/// `HALMASUIT_WALLPAPER_CONFIG` env var pointing to a TOML file
/// will carry the richer shape (named uniforms, per-monitor, etc.)
/// and supersede the extension-inference path for shader/video.
#[must_use]
pub fn from_env() -> Option<WallpaperConfig> {
    let raw = std::env::var_os("HALMASUIT_WALLPAPER_PATH")?;
    let path = PathBuf::from(raw);
    Some(infer_from_path(path))
}

/// Pick the backend variant from the file extension. Public for
/// testability; `from_env` is the live caller.
#[must_use]
pub fn infer_from_path(path: PathBuf) -> WallpaperConfig {
    let ext = Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("frag" | "glsl") => WallpaperConfig::Shader {
            source: path,
            uniforms: HashMap::new(),
        },
        Some("mp4" | "webm" | "mkv") => WallpaperConfig::Video {
            source: path,
            loop_playback: true,
        },
        // Default: image. Covers .png/.jpg/.jpeg/.webp and anything
        // else the `image` crate can decode — the decode itself is
        // the source of truth for "is this an image."
        _ => WallpaperConfig::Image { source: path },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_inference_picks_image_for_png() {
        let cfg = infer_from_path(PathBuf::from("/path/to/wallpaper.png"));
        assert!(matches!(cfg, WallpaperConfig::Image { .. }));
    }

    #[test]
    fn extension_inference_picks_shader_for_frag() {
        let cfg = infer_from_path(PathBuf::from("/path/to/wallpaper.frag"));
        assert!(matches!(cfg, WallpaperConfig::Shader { .. }));
    }

    #[test]
    fn extension_inference_picks_shader_for_glsl() {
        let cfg = infer_from_path(PathBuf::from("/path/to/wallpaper.glsl"));
        assert!(matches!(cfg, WallpaperConfig::Shader { .. }));
    }

    #[test]
    fn extension_inference_picks_video_for_mp4() {
        let cfg = infer_from_path(PathBuf::from("/path/to/wallpaper.mp4"));
        assert!(matches!(cfg, WallpaperConfig::Video { .. }));
    }

    #[test]
    fn extension_inference_picks_video_for_webm() {
        let cfg = infer_from_path(PathBuf::from("/path/to/wallpaper.webm"));
        assert!(matches!(cfg, WallpaperConfig::Video { .. }));
    }

    #[test]
    fn extension_inference_is_case_insensitive() {
        assert!(matches!(
            infer_from_path(PathBuf::from("/x.PNG")),
            WallpaperConfig::Image { .. }
        ));
        assert!(matches!(
            infer_from_path(PathBuf::from("/x.MP4")),
            WallpaperConfig::Video { .. }
        ));
        assert!(matches!(
            infer_from_path(PathBuf::from("/x.FRAG")),
            WallpaperConfig::Shader { .. }
        ));
    }

    #[test]
    fn extension_inference_defaults_to_image_for_unknown() {
        // Anything that isn't a recognised shader/video extension
        // defaults to image — the `image` crate decode is the source
        // of truth for whether it's actually decodable.
        assert!(matches!(
            infer_from_path(PathBuf::from("/x")),
            WallpaperConfig::Image { .. }
        ));
        assert!(matches!(
            infer_from_path(PathBuf::from("/x.jpg")),
            WallpaperConfig::Image { .. }
        ));
    }

    #[test]
    fn video_default_loop_is_true() {
        match infer_from_path(PathBuf::from("/x.mp4")) {
            WallpaperConfig::Video { loop_playback, .. } => {
                assert!(loop_playback, "video wallpaper defaults to looping");
            }
            other => panic!("expected Video, got {other:?}"),
        }
    }
}
