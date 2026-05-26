// halmasuit wallpaper test fixture — animated GLSL fragment shader.
//
// Shadertoy-shape entry (void mainImage), so the preamble in
// crates/halmasuit/src/wallpaper/shader.rs injects iTime and
// iResolution declarations. Consumed by the Phase B shader-variant
// VM tests to exercise the real shader rendering path (GLSL compile
// then per-frame uniform update then PixelShaderElement).
//
// Design choices for golden stability: a 60 second sine period
// keeps a 100 ms frame-timing drift inside the SSIMULACRA2
// EXACT_IMAGE_THRESHOLD that the visual-tests harness uses; the
// horizontal uv.x gradient is the load-bearing visual feature
// (covers the full framebuffer) and the time-modulated R channel
// is the secondary signal proving the time uniform is wired.

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord.xy / iResolution.xy;
    float hue = 0.5 + 0.25 * sin(iTime * 6.2831 / 60.0);
    vec3 col = vec3(hue, uv.x, 1.0 - uv.x);
    fragColor = vec4(col, 1.0);
}
