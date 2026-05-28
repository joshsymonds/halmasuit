// halmasuit/src/wallpaper/config.rs — the wallpaper config schema.
//
// One discriminated union ([`WallpaperConfig`]) over the three
// backend shapes (image / shader / video), plus the
// declared-uniforms pipeline types ([`UniformBinding`],
// [`StaticValue`]) the shader backend consumes. Phase-A wires
// image + shader live; video is still a Phase-A stub (the
// follow-up task fills it in). The four `UniformBinding` kinds are
// all parseable from JSON; only `Auto*` and `Static` fire today —
// `Event*` kinds wait for the bus-event epic.
//
// Loaded from env in [`from_env`]. Two paths:
//
//   1. `HALMASUIT_WALLPAPER_CONFIG` — a JSON file matching
//      [`WallpaperConfig`]. The Nix module writes this whenever
//      `services.halmasuit.wallpaper` is set. Required for shader
//      wallpapers with declared (non-Shadertoy) uniform names.
//   2. `HALMASUIT_WALLPAPER_PATH` — a single path. The backend
//      variant is inferred from the file extension; shaders get
//      the default Shadertoy uniform bindings. Useful for quick
//      dev/test setups.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level wallpaper config. Discriminated union over the three
/// backend shapes. Deserialized from JSON the Nix module writes to
/// `$HALMASUIT_WALLPAPER_CONFIG`; the simpler `$HALMASUIT_WALLPAPER_PATH`
/// env-var-only path infers the variant from the file extension and
/// uses default uniform bindings.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WallpaperConfig {
    /// Static image (PNG/JPEG/WebP, via the `image` crate).
    Image {
        /// Absolute path to the image file.
        source: PathBuf,
    },
    /// GLSL ES 100 fragment shader with a declared-uniforms
    /// pipeline.
    Shader {
        /// Absolute path to the `.frag` / `.glsl` file.
        source: PathBuf,
        /// Named uniforms the shader declares and the engine binds.
        /// Phase-A: only `auto-*` and static-typed kinds fire; the
        /// `event-*` kinds parse cleanly and warn that no bus is
        /// connected (the bus-event epic wires them).
        #[serde(default)]
        uniforms: HashMap<String, UniformBinding>,
    },
    /// Video file (h264 or AV1; software decode via libavcodec
    /// configured minimal). Wired through
    /// [`crate::wallpaper::VideoBackend`] which forks
    /// [`halmasuit-decoder`](../../halmasuit-decoder) as a sandboxed
    /// subprocess.
    Video {
        /// Absolute path to the video file.
        source: PathBuf,
        /// Whether to loop the video. Defaults to `true` for
        /// wallpaper use.
        #[serde(default = "default_loop", rename = "loop")]
        loop_playback: bool,
        /// Absolute path to a static image to swap the wallpaper to
        /// when the decoder's restart budget exhausts (Epic #12 Req
        /// #4/#10). `None` keeps the last-good-frame / placeholder
        /// behavior. When set, an `ImageBackend` is constructed
        /// against this path the first time the relay reports
        /// [`crate::wallpaper::decoder_relay::DecoderRelay::is_dead`]
        /// and swapped in via [`crate::wallpaper::WallpaperEngine`].
        #[serde(default)]
        fallback: Option<PathBuf>,
    },
}

impl WallpaperConfig {
    /// `true` for backends whose runtime
    /// [`WallpaperBackend::wants_continuous_render`](crate::wallpaper::WallpaperBackend::wants_continuous_render)
    /// returns `true`. Read at config-time by `main.rs` to decide
    /// whether to register the wallpaper-engine 100 ms tick at all —
    /// static backends (image) skip the timer registration entirely
    /// so the compositor can stay deep-idle on battery-backed
    /// hardware. Animated backends (shader, video) need the tick to
    /// drive their per-frame state forward (iTime advance, decoder
    /// frame consumption) and so DO register the timer.
    ///
    /// The agreement test in `tests` below pins this to stay in
    /// sync with the trait's runtime decision: if a future
    /// `WallpaperConfig::Foo` variant lands without a matching
    /// `wants_continuous_render` override (or vice versa), the
    /// exhaustive `match` on this enum fails to compile / the test
    /// fires.
    #[must_use]
    pub const fn needs_tick(&self) -> bool {
        match self {
            Self::Image { .. } => false,
            Self::Shader { .. } | Self::Video { .. } => true,
        }
    }
}

/// Default `loop_playback` for video wallpapers — `true`, matching
/// the simple env-var path's inference.
const fn default_loop() -> bool {
    true
}

/// What feeds a named uniform in a shader wallpaper.
///
/// Phase-A wires only `Auto*` and `Static`; `EventTime` /
/// `EventValue` parse cleanly and the engine logs a one-time
/// "wallpaper bus not yet connected" warning. The bus-event epic
/// connects them without changing this enum (that is the point of
/// typing all four kinds today).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    Static {
        /// The constant value.
        value: StaticValue,
    },
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
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
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
    /// `bool` (encoded as `_1i` in GLSL ES 100).
    Bool(bool),
}

/// Read the wallpaper config from a JSON file at `path`. The
/// schema mirrors [`WallpaperConfig`] with serde-tagged variants:
///
/// ```json
/// { "type": "image", "source": "/path/to/wallpaper.png" }
/// { "type": "shader", "source": "/path/to/wallpaper.frag",
///   "uniforms": { "iTime": {"kind": "auto_time"} } }
/// { "type": "video", "source": "/path/to/wallpaper.mp4", "loop": true }
/// ```
///
/// # Errors
///
/// Returns an error on file-read or JSON-parse failure.
pub fn from_json_file(path: &Path) -> io::Result<WallpaperConfig> {
    let bytes = std::fs::read(path)
        .map_err(|e| io::Error::other(format!("read wallpaper config {}: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("parse wallpaper config {}: {e}", path.display())))
}

/// Read the wallpaper config from environment variables.
///
/// Resolution order:
///
/// 1. `HALMASUIT_WALLPAPER_CONFIG` — path to a JSON file matching
///    [`WallpaperConfig`]. Used when richer config (named shader
///    uniforms, explicit video loop flag, etc.) is needed. The Nix
///    module writes this when `services.halmasuit.wallpaper` has
///    `uniforms` set or any non-default field.
/// 2. `HALMASUIT_WALLPAPER_PATH` — path-only fallback. Extension
///    inference picks the backend variant; uniforms default to the
///    canonical Shadertoy set for shader sources.
///
/// Returns `Ok(None)` when neither is set (the legacy clear-only
/// scene used by non-visual integration tests).
///
/// # Errors
///
/// Returns an error when `HALMASUIT_WALLPAPER_CONFIG` is set but
/// the file cannot be read or parsed as JSON.
pub fn from_env() -> io::Result<Option<WallpaperConfig>> {
    if let Some(raw) = std::env::var_os("HALMASUIT_WALLPAPER_CONFIG") {
        return Ok(Some(from_json_file(Path::new(&raw))?));
    }
    if let Some(raw) = std::env::var_os("HALMASUIT_WALLPAPER_PATH") {
        return Ok(Some(infer_from_path(PathBuf::from(raw))));
    }
    Ok(None)
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
            uniforms: default_shadertoy_bindings().clone(),
        },
        Some("mp4" | "webm" | "mkv") => WallpaperConfig::Video {
            source: path,
            loop_playback: true,
            fallback: None,
        },
        // Default: image. Covers .png/.jpg/.jpeg/.webp and anything
        // else the `image` crate can decode — the decode itself is
        // the source of truth for "is this an image."
        _ => WallpaperConfig::Image { source: path },
    }
}

/// The canonical Shadertoy uniform-binding set, auto-bound by name
/// to engine values. Two callers: the env-var path's
/// `infer_from_path` uses this as the initial set when a `.glsl`/
/// `.frag` is given via `HALMASUIT_WALLPAPER_PATH`; the shader
/// backend merges this in (user entries winning on collisions)
/// whenever the user shader is Shadertoy-shape — the injected
/// preamble + wrapper reference these uniforms, so for that shape
/// they MUST be bound regardless of whether the JSON config path
/// (which defaults `uniforms` to `{}`) supplied them.
///
/// Returns a `&'static` reference; the 5-entry map is built once
/// via `LazyLock` and shared across the (rare) callers. The owned
/// `HashMap` ownership the env-var path needs is recovered with
/// `.clone()` at that single site.
pub fn default_shadertoy_bindings() -> &'static HashMap<String, UniformBinding> {
    static MAP: std::sync::LazyLock<HashMap<String, UniformBinding>> =
        std::sync::LazyLock::new(|| {
            let mut m = HashMap::new();
            m.insert("iResolution".to_owned(), UniformBinding::AutoResolution);
            m.insert("iTime".to_owned(), UniformBinding::AutoTime);
            m.insert("iTimeDelta".to_owned(), UniformBinding::AutoDelta);
            m.insert("iFrame".to_owned(), UniformBinding::AutoFrame);
            m.insert("iMouse".to_owned(), UniformBinding::AutoMouse);
            m
        });
    &MAP
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins `WallpaperConfig::needs_tick` to stay in sync with each
    /// backend's runtime `WallpaperBackend::wants_continuous_render`
    /// decision. The `match` on every variant means a future
    /// `WallpaperConfig::Foo` addition fails to compile here AND
    /// forces a deliberate choice about whether Foo needs the tick
    /// (and a matching trait-method override on the corresponding
    /// backend impl). Defense against silent drift between the
    /// config-time and runtime views of "animated vs static."
    #[test]
    fn needs_tick_agrees_with_backend_runtime() {
        // Synthetic instances — only the variant shape matters for
        // `needs_tick`, the actual filesystem paths are not opened.
        let image = WallpaperConfig::Image {
            source: PathBuf::from("/dev/null"),
        };
        let shader = WallpaperConfig::Shader {
            source: PathBuf::from("/dev/null"),
            uniforms: HashMap::new(),
        };
        let video = WallpaperConfig::Video {
            source: PathBuf::from("/dev/null"),
            loop_playback: true,
            fallback: None,
        };
        // Image: static frame, no per-tick work — tick MUST NOT
        // register (deep-idle preservation on battery-backed boxes).
        assert!(!image.needs_tick());
        // Shader/Video: iTime / decoder frame consumption — tick
        // MUST register.
        assert!(shader.needs_tick());
        assert!(video.needs_tick());

        // Future-variant fail-closed: `WallpaperConfig::needs_tick`'s
        // exhaustive match (above in this file) fails to compile when
        // a new variant lands without a tick-policy decision, forcing
        // the author through the choice deliberately rather than
        // inheriting a silent default.
    }

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

    #[test]
    fn json_image_config_deserializes() {
        let json = r#"{"type":"image","source":"/path/to/wallpaper.png"}"#;
        let cfg: WallpaperConfig = serde_json::from_str(json).expect("image config parses");
        match cfg {
            WallpaperConfig::Image { source } => {
                assert_eq!(source, PathBuf::from("/path/to/wallpaper.png"));
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn json_shader_config_with_declared_uniforms_deserializes() {
        let json = r#"{
            "type": "shader",
            "source": "/path/to/wallpaper.frag",
            "uniforms": {
                "iTime": { "kind": "auto_time" },
                "iResolution": { "kind": "auto_resolution" },
                "theme_color": {
                    "kind": "static",
                    "value": { "type": "vec3", "value": [0.04, 0.0, 0.08] }
                },
                "login_event_time": {
                    "kind": "event_time",
                    "event": "halmasuit.session.opened"
                }
            }
        }"#;
        let cfg: WallpaperConfig = serde_json::from_str(json).expect("shader config parses");
        match cfg {
            WallpaperConfig::Shader { source, uniforms } => {
                assert_eq!(source, PathBuf::from("/path/to/wallpaper.frag"));
                assert!(matches!(
                    uniforms.get("iTime"),
                    Some(UniformBinding::AutoTime)
                ));
                assert!(matches!(
                    uniforms.get("iResolution"),
                    Some(UniformBinding::AutoResolution)
                ));
                assert!(matches!(
                    uniforms.get("theme_color"),
                    Some(UniformBinding::Static {
                        value: StaticValue::Vec3([_, _, _])
                    })
                ));
                assert!(matches!(
                    uniforms.get("login_event_time"),
                    Some(UniformBinding::EventTime { .. })
                ));
            }
            other => panic!("expected Shader, got {other:?}"),
        }
    }

    #[test]
    fn json_video_config_with_loop_deserializes() {
        let json = r#"{"type":"video","source":"/x.mp4","loop":false}"#;
        let cfg: WallpaperConfig = serde_json::from_str(json).expect("video config parses");
        match cfg {
            WallpaperConfig::Video {
                source,
                loop_playback,
                fallback,
            } => {
                assert_eq!(source, PathBuf::from("/x.mp4"));
                assert!(!loop_playback);
                assert!(fallback.is_none(), "fallback defaults to None");
            }
            other => panic!("expected Video, got {other:?}"),
        }
    }

    #[test]
    fn json_video_config_loop_defaults_to_true_when_omitted() {
        let json = r#"{"type":"video","source":"/x.mp4"}"#;
        let cfg: WallpaperConfig = serde_json::from_str(json).expect("video config parses");
        match cfg {
            WallpaperConfig::Video { loop_playback, .. } => {
                assert!(loop_playback, "loop defaults to true");
            }
            other => panic!("expected Video, got {other:?}"),
        }
    }

    #[test]
    fn json_video_config_with_fallback_deserializes() {
        let json = r#"{"type":"video","source":"/x.mp4","fallback":"/y.png"}"#;
        let cfg: WallpaperConfig = serde_json::from_str(json).expect("video config parses");
        match cfg {
            WallpaperConfig::Video { fallback, .. } => {
                assert_eq!(fallback, Some(PathBuf::from("/y.png")));
            }
            other => panic!("expected Video, got {other:?}"),
        }
    }
}
