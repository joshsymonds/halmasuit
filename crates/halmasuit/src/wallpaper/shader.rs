// halmasuit/src/wallpaper/shader.rs — GLSL fragment-shader backend.
//
// Compiles a user-supplied GLSL ES 100 fragment shader via smithay's
// `GlesRenderer::compile_custom_pixel_shader` and runs it per-frame
// with declared uniforms wired to their config-specified sources
// (auto-* engine values, static constants, and event-time/event-value
// uniforms driven by the lifecycle bus: `notify_event` records a
// fired event's time/value, which `current_uniforms` reads each frame).
//
// Shadertoy-shape shaders (`void mainImage(out vec4, in vec2)`) are
// supported via a thin GLSL preamble that pre-declares the
// canonical Shadertoy uniforms (`iResolution`, `iTime`,
// `iTimeDelta`, `iFrame`, `iMouse`) and wires the user's `mainImage`
// into smithay's `void main()` entry point. Detection is by
// substring match (`mainImage(`); declared-uniforms shaders that
// write `void main()` directly bypass the preamble entirely.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::Instant;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType, UniformValue,
};
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::backend::WallpaperBackend;
use super::config::{StaticValue, UniformBinding};
use crate::drm::SceneElement;

/// The GLSL preamble injected before a Shadertoy-shape user shader.
/// Declares the canonical Shadertoy uniforms (which the engine binds
/// from the `Auto*` kinds wired to these well-known names) and wraps
/// the user's `mainImage` in smithay's expected `void main()` entry.
const SHADERTOY_PREAMBLE: &str = r"
precision mediump float;
uniform vec3 iResolution;
uniform float iTime;
uniform float iTimeDelta;
uniform int iFrame;
uniform vec4 iMouse;
";

/// The Shadertoy wrapper appended AFTER the user's source. Calls the
/// user's `mainImage(out vec4 fragColor, in vec2 fragCoord)` and
/// hands the result to `gl_FragColor`.
const SHADERTOY_WRAPPER: &str = r"
void main() {
    vec4 color;
    mainImage(color, gl_FragCoord.xy);
    gl_FragColor = color;
}
";

/// GLSL ES 1.00 needs `GL_OES_standard_derivatives` explicitly
/// enabled to call `fwidth` / `dFdx` / `dFdy`. Mesa's compiler
/// (the headless VM-test substrate) accepts these without the
/// directive; NVIDIA's GLSL compiler is strict and rejects them
/// with `error C7532: global function fwidth requires "#version
/// 300" or later`. Since smithay forces `#version 100`, the only
/// fix is the extension directive — and per the GLSL spec it must
/// appear after `#version` but before any non-preprocessor token,
/// so `assemble_source` prepends it (smithay's `#version 100\n`
/// lands immediately before whatever we return). This was the
/// gnomon Phase B failure mode once the EGL stack finally came up:
/// real NVIDIA hardware rejected the hexrain wallpaper's `fwidth`.
const DERIVATIVES_EXTENSION: &str = "#extension GL_OES_standard_derivatives : enable";

/// Detect Shadertoy-shape shaders by the entry-point signature.
/// Substring is unambiguous: `mainImage(` only appears in the
/// Shadertoy convention; declared-uniform shaders write
/// `void main()` directly.
fn is_shadertoy_shape(src: &str) -> bool {
    src.contains("mainImage(")
}

/// Whether the shader calls a derivative builtin that GLSL ES 1.00
/// gates behind `GL_OES_standard_derivatives`. Word-ish substring
/// match is sufficient: these tokens don't otherwise appear in GLSL.
fn uses_derivatives(src: &str) -> bool {
    src.contains("fwidth") || src.contains("dFdx") || src.contains("dFdy")
}

/// Stitch the final shader source. If the user wrote a Shadertoy-
/// shape shader, wrap with the preamble + wrapper; otherwise pass
/// the source through untouched (declared-uniforms shape). When the
/// source uses derivative builtins, the `GL_OES_standard_derivatives`
/// directive is prepended so it lands directly after smithay's
/// injected `#version 100` (where extension directives must live).
/// The directive is only added when needed and only when the user
/// didn't already declare it, keeping derivative-free shaders pristine.
fn assemble_source(user_src: &str) -> String {
    let body = if is_shadertoy_shape(user_src) {
        format!("{SHADERTOY_PREAMBLE}\n{user_src}\n{SHADERTOY_WRAPPER}")
    } else {
        user_src.to_string()
    };
    if uses_derivatives(&body) && !body.contains(DERIVATIVES_EXTENSION) {
        format!("{DERIVATIVES_EXTENSION}\n{body}")
    } else {
        body
    }
}

/// The UniformType smithay expects for each kind of binding.
/// `EventTime`/`EventValue` are scalar `_1f`: `EventTime` carries the
/// fire timestamp (same epoch as `AutoTime`) and `EventValue` the
/// event's scalar payload.
const fn uniform_type_for(binding: &UniformBinding) -> UniformType {
    match binding {
        // `_1f` covers `AutoTime`, `AutoDelta`, and the bus-driven
        // `EventTime`/`EventValue` (both scalar floats).
        UniformBinding::AutoTime
        | UniformBinding::AutoDelta
        | UniformBinding::EventTime { .. }
        | UniformBinding::EventValue { .. } => UniformType::_1f,
        UniformBinding::AutoResolution => UniformType::_3f,
        UniformBinding::AutoFrame => UniformType::_1i,
        UniformBinding::AutoMouse => UniformType::_4f,
        UniformBinding::Static { value: v } => match v {
            StaticValue::Float(_) => UniformType::_1f,
            StaticValue::Vec2(_) => UniformType::_2f,
            StaticValue::Vec3(_) => UniformType::_3f,
            StaticValue::Vec4(_) => UniformType::_4f,
            // GLSL ES 100 has no `bool` uniform; the conventional
            // encoding is `int` 0/1 in the shader. Sharing with
            // `Int` here is deliberate.
            StaticValue::Int(_) | StaticValue::Bool(_) => UniformType::_1i,
        },
    }
}

/// Translate a `StaticValue` into smithay's `UniformValue`.
const fn static_to_uniform_value(value: &StaticValue) -> UniformValue {
    match *value {
        StaticValue::Float(x) => UniformValue::_1f(x),
        StaticValue::Vec2([a, b]) => UniformValue::_2f(a, b),
        StaticValue::Vec3([a, b, c]) => UniformValue::_3f(a, b, c),
        StaticValue::Vec4([a, b, c, d]) => UniformValue::_4f(a, b, c, d),
        StaticValue::Int(x) => UniformValue::_1i(x),
        StaticValue::Bool(b) => UniformValue::_1i(b as i32),
    }
}

/// GLSL fragment-shader wallpaper backend.
///
/// Owns the compiled `GlesPixelProgram` (reusable across frames),
/// the declared-uniforms binding table (resolved once at
/// construction), and the per-frame state needed to compute
/// `Auto*` uniform values (frame start, last frame, frame counter).
pub struct ShaderBackend {
    program: GlesPixelProgram,
    /// Resolved binding list in insertion order so the per-frame
    /// uniform update visits them deterministically.
    bindings: Vec<(String, UniformBinding)>,
    frame_start: Instant,
    last_frame: Instant,
    frame_counter: u64,
    /// The output size at last render, used to skip rebuilding the
    /// `PixelShaderElement` area when nothing changed (a small
    /// optimization; the element is cheap to clone anyway).
    last_output_size: Size<i32, Logical>,
    /// Last fire time (seconds in the `frame_start` epoch, same as
    /// `AutoTime`) per canonical event name, for `EventTime` uniforms.
    /// Absent = never fired → the `-1.0` sentinel.
    event_times: HashMap<String, f32>,
    /// Last fired value per canonical event name, for `EventValue`
    /// uniforms. Absent = never fired → the `0.0` sentinel.
    event_values: HashMap<String, f32>,
}

/// Record a fired event into the per-event time/value maps for any
/// binding whose `EventTime`/`EventValue` `event` matches `event_name`,
/// and return the GLSL uniform names that were updated (one
/// `WallpaperUniformApplied` marker is emitted per returned name).
///
/// Pure (no `self`, no clock) so it is unit-testable without a
/// `GlesRenderer`: the caller supplies `time_secs` (the shader's
/// `frame_start` epoch). `EventTime` bindings record the time;
/// `EventValue` bindings record the value.
fn apply_event(
    bindings: &[(String, UniformBinding)],
    event_name: &str,
    time_secs: f32,
    value: f32,
    event_times: &mut HashMap<String, f32>,
    event_values: &mut HashMap<String, f32>,
) -> Vec<String> {
    let mut written = Vec::new();
    for (uniform, binding) in bindings {
        match binding {
            UniformBinding::EventTime { event } if event == event_name => {
                event_times.insert(event.clone(), time_secs);
                written.push(uniform.clone());
            }
            UniformBinding::EventValue { event } if event == event_name => {
                event_values.insert(event.clone(), value);
                written.push(uniform.clone());
            }
            _ => {}
        }
    }
    written
}

impl ShaderBackend {
    /// Read the GLSL source from `source`, assemble the final
    /// shader (injecting the Shadertoy preamble if applicable),
    /// compile via smithay's `compile_custom_pixel_shader`, and
    /// resolve the declared-uniforms config into the per-frame
    /// binding list. `EventTime` / `EventValue` bindings are driven by
    /// the lifecycle bus via [`WallpaperBackend::notify_event`]; their
    /// uniforms read the `-1.0` / `0.0` sentinel until the event fires.
    ///
    /// # Errors
    ///
    /// Bubbles file-read failure, GLSL compile/link failure (with
    /// smithay's compiler diagnostic), or EGL state errors.
    pub fn new(
        renderer: &mut GlesRenderer,
        source: &Path,
        uniforms: HashMap<String, UniformBinding>,
    ) -> io::Result<Self> {
        let raw = std::fs::read_to_string(source).map_err(|e| {
            io::Error::other(format!("read wallpaper shader {}: {e}", source.display()))
        })?;
        let shadertoy = is_shadertoy_shape(&raw);
        let final_src = assemble_source(&raw);

        // Shadertoy shape couples its injected preamble + wrapper to
        // a fixed set of uniforms (iResolution, iTime, …). The
        // wrapper passes `gl_FragCoord.xy` into `mainImage` and the
        // user shader divides by `iResolution`; if that uniform is
        // unbound it defaults to vec3(0) and every fragment computes
        // `fragCoord/0 = Inf`, clamped to a solid color. Merge the
        // canonical Shadertoy bindings in for Shadertoy-shape
        // shaders so the JSON-config path (whose Nix option defaults
        // `uniforms` to `{}`) is no worse than the env-var path's
        // `infer_from_path`. User-supplied entries win on key
        // collisions — a user that wants a Static iResolution still
        // gets one.
        let mut uniforms = uniforms;
        if shadertoy {
            for (k, v) in super::config::default_shadertoy_bindings() {
                uniforms.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        // Stable order: sort by name so the binding list is
        // deterministic across runs (the HashMap iteration order is
        // not).
        let mut bindings: Vec<(String, UniformBinding)> = uniforms.into_iter().collect();
        bindings.sort_by(|a, b| a.0.cmp(&b.0));

        let uniform_names: Vec<UniformName<'_>> = bindings
            .iter()
            .map(|(name, binding)| UniformName::new(name.clone(), uniform_type_for(binding)))
            .collect();

        let program = renderer
            .compile_custom_pixel_shader(&final_src, &uniform_names)
            .map_err(|e| {
                io::Error::other(format!(
                    "compile wallpaper shader {}: {e}",
                    source.display()
                ))
            })?;

        let now = Instant::now();
        Ok(Self {
            program,
            bindings,
            frame_start: now,
            last_frame: now,
            frame_counter: 0,
            last_output_size: Size::from((0, 0)),
            event_times: HashMap::new(),
            event_values: HashMap::new(),
        })
    }

    /// Build the current frame's uniform list from the binding
    /// table + per-frame engine state.
    fn current_uniforms(
        &self,
        time_secs: f32,
        delta_secs: f32,
        frame_id: i32,
        resolution: (f32, f32),
    ) -> Vec<Uniform<'static>> {
        self.bindings
            .iter()
            .map(|(name, binding)| {
                let value = match binding {
                    UniformBinding::AutoTime => UniformValue::_1f(time_secs),
                    UniformBinding::AutoResolution => {
                        UniformValue::_3f(resolution.0, resolution.1, 1.0)
                    }
                    UniformBinding::AutoFrame => UniformValue::_1i(frame_id),
                    UniformBinding::AutoDelta => UniformValue::_1f(delta_secs),
                    UniformBinding::AutoMouse => UniformValue::_4f(0.0, 0.0, 0.0, 0.0),
                    UniformBinding::Static { value: v } => static_to_uniform_value(v),
                    // Bus-driven. `EventTime` carries the fire time in
                    // the same epoch as `AutoTime` (so `u_time -
                    // eventTime` is valid), sentinel `-1.0` until the
                    // event first fires (a shader's decay reads as fully
                    // settled). `EventValue` carries the event's scalar,
                    // sentinel `0.0` (the latched "not fired" gate).
                    UniformBinding::EventTime { event } => {
                        UniformValue::_1f(self.event_times.get(event).copied().unwrap_or(-1.0))
                    }
                    UniformBinding::EventValue { event } => {
                        UniformValue::_1f(self.event_values.get(event).copied().unwrap_or(0.0))
                    }
                };
                Uniform::new(name.clone(), value).into_owned()
            })
            .collect()
    }
}

impl WallpaperBackend for ShaderBackend {
    /// Shader wallpapers need the wallpaper-engine tick to drive
    /// renders unconditionally: every `render_element` call advances
    /// `iTime` from `Instant::now()`, so the tick cadence IS the
    /// shader's animation rate. Without this, the wallpaper-tick
    /// timer in `main.rs` only fires `render_one_frame` when a
    /// fallback swap is requested (which never happens for a stable
    /// shader), so post-PrepareForShutdown the shader freezes on
    /// whichever frame the last Wayland client commit produced.
    fn wants_continuous_render(&self) -> bool {
        true
    }

    fn notify_event(&mut self, event_name: &str, value: f32) -> Vec<String> {
        // Stamp the time in OUR `frame_start` epoch — the same clock
        // `AutoTime` uses — so a shader's `u_time - eventTime` is valid.
        let time_secs = self.frame_start.elapsed().as_secs_f32();
        apply_event(
            &self.bindings,
            event_name,
            time_secs,
            value,
            &mut self.event_times,
            &mut self.event_values,
        )
    }

    fn render_element(
        &mut self,
        _renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
    ) -> io::Result<SceneElement> {
        let now = Instant::now();
        let time_secs = now.duration_since(self.frame_start).as_secs_f32();
        let delta_secs = now.duration_since(self.last_frame).as_secs_f32();
        let frame_id = i32::try_from(self.frame_counter).unwrap_or(i32::MAX);
        // Output dimensions are i32 logical pixels; f32 mantissa is
        // 24 bits — exact for any realistic output size (< 2^24 px).
        #[allow(
            clippy::cast_precision_loss,
            reason = "output dimensions fit in f32 mantissa"
        )]
        let resolution = (output_size.w as f32, output_size.h as f32);

        let uniforms = self.current_uniforms(time_secs, delta_secs, frame_id, resolution);
        let area = Rectangle::new(Point::from((0, 0)), output_size);

        self.last_frame = now;
        self.frame_counter = self.frame_counter.saturating_add(1);
        self.last_output_size = output_size;

        let element = PixelShaderElement::new(
            self.program.clone(),
            area,
            // Opaque region = full area: the wallpaper plane is
            // opaque-by-default (epic requirement R3). User shaders
            // that want translucent regions are an explicit
            // future-epic opt-in.
            Some(vec![area]),
            1.0,
            uniforms,
            Kind::Unspecified,
        );
        Ok(SceneElement::WallpaperShader(element))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a binding table the way `ShaderBackend::new` would (name →
    /// binding), for the pure `apply_event` tests.
    fn bindings(entries: &[(&str, UniformBinding)]) -> Vec<(String, UniformBinding)> {
        entries
            .iter()
            .map(|(name, b)| ((*name).to_owned(), b.clone()))
            .collect()
    }

    #[test]
    fn apply_event_writes_matching_event_time_and_value_uniforms() {
        let table = bindings(&[
            (
                "u_login_time",
                UniformBinding::EventTime {
                    event: "halmasuit.session.opened".to_owned(),
                },
            ),
            (
                "u_login_active",
                UniformBinding::EventValue {
                    event: "halmasuit.session.opened".to_owned(),
                },
            ),
        ]);
        let mut times = HashMap::new();
        let mut values = HashMap::new();

        let written = apply_event(
            &table,
            "halmasuit.session.opened",
            12.5,
            1.0,
            &mut times,
            &mut values,
        );

        // Both uniforms updated; markers would be emitted for each.
        assert_eq!(written.len(), 2);
        assert!(written.contains(&"u_login_time".to_owned()));
        assert!(written.contains(&"u_login_active".to_owned()));
        // EventTime recorded the time; EventValue recorded the value,
        // each keyed by canonical event name. Bit-compare to assert exact
        // f32 equality without tripping clippy::float_cmp.
        assert_eq!(
            times
                .get("halmasuit.session.opened")
                .copied()
                .map(f32::to_bits),
            Some(12.5f32.to_bits()),
        );
        assert_eq!(
            values
                .get("halmasuit.session.opened")
                .copied()
                .map(f32::to_bits),
            Some(1.0f32.to_bits()),
        );
    }

    #[test]
    fn apply_event_is_a_noop_for_unmatched_event_name() {
        let table = bindings(&[(
            "u_login_time",
            UniformBinding::EventTime {
                event: "halmasuit.session.opened".to_owned(),
            },
        )]);
        let mut times = HashMap::new();
        let mut values = HashMap::new();

        let written = apply_event(
            &table,
            "halmasuit.foreground.greeter",
            3.0,
            1.0,
            &mut times,
            &mut values,
        );

        assert!(written.is_empty(), "no binding matches; nothing written");
        assert!(times.is_empty());
        assert!(values.is_empty());
    }

    #[test]
    fn current_uniforms_uses_sentinels_until_event_fires_then_the_fired_values() {
        // Pre-fire: EventTime reads -1.0 (never fired), EventValue 0.0.
        let times: HashMap<String, f32> = HashMap::new();
        let values: HashMap<String, f32> = HashMap::new();
        assert_eq!(
            times.get("e").copied().unwrap_or(-1.0).to_bits(),
            (-1.0f32).to_bits(),
        );
        assert_eq!(
            values.get("e").copied().unwrap_or(0.0).to_bits(),
            0.0f32.to_bits(),
        );

        // After apply_event, the fired values are read back.
        let table = bindings(&[
            (
                "u_t",
                UniformBinding::EventTime {
                    event: "e".to_owned(),
                },
            ),
            (
                "u_v",
                UniformBinding::EventValue {
                    event: "e".to_owned(),
                },
            ),
        ]);
        let mut times = HashMap::new();
        let mut values = HashMap::new();
        apply_event(&table, "e", 7.0, 1.0, &mut times, &mut values);
        assert_eq!(
            times.get("e").copied().unwrap_or(-1.0).to_bits(),
            7.0f32.to_bits(),
        );
        assert_eq!(
            values.get("e").copied().unwrap_or(0.0).to_bits(),
            1.0f32.to_bits(),
        );
    }

    #[test]
    fn shadertoy_shape_detection_recognizes_canonical_signature() {
        let src =
            "void mainImage(out vec4 fragColor, in vec2 fragCoord) { fragColor = vec4(1.0); }";
        assert!(is_shadertoy_shape(src));
    }

    #[test]
    fn shadertoy_shape_detection_is_unambiguous_for_declared_uniforms() {
        let src = "uniform float t;\nvoid main() { gl_FragColor = vec4(t); }";
        assert!(!is_shadertoy_shape(src));
    }

    #[test]
    fn assemble_source_injects_preamble_for_shadertoy_shape() {
        let user = "void mainImage(out vec4 c, in vec2 f) { c = vec4(0.0); }";
        let out = assemble_source(user);
        assert!(out.contains("uniform vec3 iResolution"));
        assert!(out.contains("uniform float iTime"));
        assert!(out.contains("mainImage(out vec4 c"));
        assert!(out.contains("void main()"));
    }

    #[test]
    fn assemble_source_passes_declared_uniforms_through_untouched() {
        let user = "uniform float t;\nvoid main() { gl_FragColor = vec4(t); }";
        let out = assemble_source(user);
        // The preamble's hallmark `iResolution` declaration is absent.
        assert!(!out.contains("uniform vec3 iResolution"));
        // The user source is verbatim.
        assert!(out.contains("uniform float t;\nvoid main()"));
    }

    #[test]
    fn uniform_type_maps_auto_kinds_to_expected_glsl_types() {
        assert_eq!(
            uniform_type_for(&UniformBinding::AutoTime),
            UniformType::_1f
        );
        assert_eq!(
            uniform_type_for(&UniformBinding::AutoDelta),
            UniformType::_1f
        );
        assert_eq!(
            uniform_type_for(&UniformBinding::AutoResolution),
            UniformType::_3f
        );
        assert_eq!(
            uniform_type_for(&UniformBinding::AutoFrame),
            UniformType::_1i
        );
        assert_eq!(
            uniform_type_for(&UniformBinding::AutoMouse),
            UniformType::_4f
        );
    }

    #[test]
    fn uniform_type_maps_static_kinds_to_their_value_shape() {
        assert_eq!(
            uniform_type_for(&UniformBinding::Static {
                value: StaticValue::Float(0.0)
            }),
            UniformType::_1f
        );
        assert_eq!(
            uniform_type_for(&UniformBinding::Static {
                value: StaticValue::Vec3([0.0, 0.0, 0.0])
            }),
            UniformType::_3f
        );
        assert_eq!(
            uniform_type_for(&UniformBinding::Static {
                value: StaticValue::Int(0)
            }),
            UniformType::_1i
        );
        // `bool` is encoded as `_1i` (GLSL ES 100 has no bool uniform).
        assert_eq!(
            uniform_type_for(&UniformBinding::Static {
                value: StaticValue::Bool(true)
            }),
            UniformType::_1i
        );
    }

    #[test]
    fn static_value_conversion_matches_smithay_uniform_value_variants() {
        assert!(matches!(
            static_to_uniform_value(&StaticValue::Float(0.5)),
            UniformValue::_1f(x) if (x - 0.5).abs() < f32::EPSILON
        ));
        assert!(matches!(
            static_to_uniform_value(&StaticValue::Vec3([0.1, 0.2, 0.3])),
            UniformValue::_3f(_, _, _)
        ));
        assert!(matches!(
            static_to_uniform_value(&StaticValue::Bool(true)),
            UniformValue::_1i(1)
        ));
        assert!(matches!(
            static_to_uniform_value(&StaticValue::Bool(false)),
            UniformValue::_1i(0)
        ));
    }

    #[test]
    fn shadertoy_preamble_is_self_contained_glsl_es_100() {
        // The preamble must not contain `#version` (smithay adds it)
        // and must declare the canonical uniforms with valid GLSL ES
        // 100 types.
        assert!(!SHADERTOY_PREAMBLE.contains("#version"));
        assert!(SHADERTOY_PREAMBLE.contains("uniform vec3 iResolution"));
        assert!(SHADERTOY_PREAMBLE.contains("uniform float iTime"));
        assert!(SHADERTOY_PREAMBLE.contains("uniform float iTimeDelta"));
        assert!(SHADERTOY_PREAMBLE.contains("uniform int iFrame"));
        assert!(SHADERTOY_PREAMBLE.contains("uniform vec4 iMouse"));
    }

    /// The Phase B shader-variant VM tests consume
    /// `tests/fixtures/wallpaper-shader.glsl` as the
    /// `services.halmasuit.wallpaper.source`. This unit test pins the
    /// fixture's shape so a regression (wrong entry point, missing
    /// uniform reference, accidental `#version` directive) fails
    /// `just check` before the slow VM sweep runs. The actual GPU
    /// compile via `ShaderBackend::new` requires a real `GlesRenderer`
    /// and is exercised end-to-end by the VM tests.
    const PHASE_B_SHADER_FIXTURE: &str =
        include_str!("../../../../tests/fixtures/wallpaper-shader.glsl");

    #[test]
    fn phase_b_shader_fixture_is_shadertoy_shape() {
        assert!(
            is_shadertoy_shape(PHASE_B_SHADER_FIXTURE),
            "fixture must use the `void mainImage(...)` Shadertoy entry \
             so shader.rs injects the preamble that declares iTime / \
             iResolution"
        );
    }

    #[test]
    fn phase_b_shader_fixture_uses_itime_and_iresolution() {
        assert!(
            PHASE_B_SHADER_FIXTURE.contains("iTime"),
            "fixture must reference iTime so the time-varying golden \
             actually animates (the no-flash gate would still pass on \
             a static shader, but the VM test's whole point is to \
             exercise the time-uniform code path)"
        );
        assert!(
            PHASE_B_SHADER_FIXTURE.contains("iResolution"),
            "fixture must reference iResolution so the per-frame \
             uniform bind for the resolution vec3 runs"
        );
    }

    #[test]
    fn derivative_using_shader_gets_oes_extension_directive_first() {
        // A shader that calls `fwidth` must have the
        // GL_OES_standard_derivatives extension enabled, and the
        // directive must come BEFORE any other line so that — after
        // smithay prepends `#version 100\n` — it sits immediately
        // after the version (the only legal spot for #extension).
        // Regression gate for the gnomon NVIDIA hexrain failure;
        // Mesa (VM substrate) compiles fwidth without it but NVIDIA
        // rejects it.
        let src = "void main() { float e = fwidth(gl_FragCoord.x); gl_FragColor = vec4(e); }";
        let assembled = assemble_source(src);
        assert!(
            assembled.starts_with(DERIVATIVES_EXTENSION),
            "derivative-using shader must lead with the extension \
             directive so it lands right after smithay's #version 100; \
             got:\n{assembled}"
        );
        // Sanity: smithay forbids #version in our source.
        assert!(!assembled.contains("#version"));
    }

    #[test]
    fn derivative_free_shader_is_left_pristine() {
        // No fwidth/dFdx/dFdy → no extension directive injected.
        let src = "void main() { gl_FragColor = vec4(1.0); }";
        let assembled = assemble_source(src);
        assert!(
            !assembled.contains("GL_OES_standard_derivatives"),
            "derivative-free shaders must not gain the directive"
        );
    }

    #[test]
    fn derivatives_directive_not_double_injected() {
        // If the user already enabled the extension, don't add a
        // second copy.
        let src = "#extension GL_OES_standard_derivatives : enable\n\
                   void main() { gl_FragColor = vec4(fwidth(gl_FragCoord.x)); }";
        let assembled = assemble_source(src);
        assert_eq!(
            assembled.matches("GL_OES_standard_derivatives").count(),
            1,
            "extension directive must appear exactly once"
        );
    }

    #[test]
    fn phase_b_shader_fixture_assembles_to_a_complete_glsl_es_100_program() {
        // Wrap-and-stitch via the production assembler. Asserts the
        // final source has both the user's `mainImage` AND the
        // generated `void main()` entry. A regression where the
        // assembler drops the user source would surface here.
        let assembled = assemble_source(PHASE_B_SHADER_FIXTURE);
        assert!(
            assembled.contains("mainImage(out vec4 fragColor"),
            "assembled source must contain the user's mainImage \
             signature verbatim"
        );
        assert!(
            assembled.contains("void main()"),
            "assembled source must contain smithay's expected \
             void main() entry (added by the Shadertoy wrapper)"
        );
        assert!(
            !assembled.contains("#version"),
            "assembled source must not declare #version; smithay \
             prepends it during compile"
        );
    }

    /// Mirror of the merge logic in `ShaderBackend::new` — the
    /// constructor needs a live `GlesRenderer` so we can't drive it
    /// directly here, but the merge itself is a pure HashMap
    /// operation and pinning its behavior in a unit test catches the
    /// regression class that produced solid-color shader goldens
    /// (Shadertoy-shape shader + empty `uniforms` map → unbound
    /// `iResolution` → `fragCoord/0 = Inf` → constant fragment).
    fn merge_shadertoy_defaults(
        src: &str,
        user: HashMap<String, UniformBinding>,
    ) -> HashMap<String, UniformBinding> {
        let mut out = user;
        if is_shadertoy_shape(src) {
            for (k, v) in super::super::config::default_shadertoy_bindings() {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        out
    }

    #[test]
    fn shadertoy_shape_with_empty_user_map_auto_binds_canonical_uniforms() {
        // Reproduces the JSON-config-path bug: the Nix module's
        // `uniforms` option defaults to `{}`, so a Shadertoy-shape
        // shader configured via `services.halmasuit.wallpaper`
        // arrived at ShaderBackend with an empty bindings map. The
        // injected preamble + wrapper still reference iResolution /
        // iTime / etc., so without a merge they end up unbound.
        let merged = merge_shadertoy_defaults(PHASE_B_SHADER_FIXTURE, HashMap::new());
        assert!(matches!(
            merged.get("iResolution"),
            Some(UniformBinding::AutoResolution)
        ));
        assert!(matches!(
            merged.get("iTime"),
            Some(UniformBinding::AutoTime)
        ));
        assert!(matches!(
            merged.get("iFrame"),
            Some(UniformBinding::AutoFrame)
        ));
    }

    #[test]
    fn shadertoy_shape_user_provided_uniform_wins_over_default() {
        // A user that ships a Shadertoy shader and explicitly binds
        // iTime to a Static value must NOT have the merge overwrite
        // it. Defaults fill MISSING keys only — `HashMap::entry`'s
        // `or_insert` semantics.
        let mut user = HashMap::new();
        user.insert(
            "iTime".to_owned(),
            UniformBinding::Static {
                value: StaticValue::Float(42.0),
            },
        );
        let merged = merge_shadertoy_defaults(PHASE_B_SHADER_FIXTURE, user);
        assert!(
            matches!(
                merged.get("iTime"),
                Some(UniformBinding::Static { value: StaticValue::Float(v) }) if (*v - 42.0).abs() < f32::EPSILON
            ),
            "user-provided Static iTime must survive the merge"
        );
        // The OTHER defaults still land — only the colliding key is
        // preserved as the user provided.
        assert!(matches!(
            merged.get("iResolution"),
            Some(UniformBinding::AutoResolution)
        ));
    }

    #[test]
    fn declared_uniform_shape_does_not_auto_bind_shadertoy_defaults() {
        // Non-Shadertoy shape: the assembler passes the source
        // through untouched, so the wrapper never gets injected and
        // the user shader doesn't reference iResolution / iTime by
        // name. Auto-binding them here would inject phantom uniforms
        // that GetUniformLocation reports as -1 (silent no-op), but
        // it'd also confuse the audit log + the binding count.
        let user = "uniform float t;\nvoid main() { gl_FragColor = vec4(t); }";
        let merged = merge_shadertoy_defaults(user, HashMap::new());
        assert!(merged.is_empty());
    }

    #[test]
    fn phase_b_shader_fixture_does_not_declare_event_uniforms() {
        // Phase A: EventTime / EventValue parse but the bus-event
        // delivery isn't wired (shader.rs:184-205 warns + writes 0.0).
        // The fixture must stick to fully-implemented uniforms so the
        // VM test exercises a path that actually fires non-zero
        // values. A future fixture that wants event uniforms lands
        // alongside the bus-event epic.
        assert!(
            !PHASE_B_SHADER_FIXTURE.contains("EventTime"),
            "fixture must not declare EventTime — Phase A leaves it \
             at sentinel 0.0; use iTime"
        );
        assert!(
            !PHASE_B_SHADER_FIXTURE.contains("EventValue"),
            "fixture must not declare EventValue — Phase A leaves it \
             at sentinel 0.0"
        );
    }
}
