// halmasuit wallpaper test fixture — fast-period animated GLSL.
//
// Shadertoy-shape entry (void mainImage). Same structure as
// `wallpaper-shader.glsl`, but with a 1-second period instead of
// 60 seconds. The faster animation makes phash variance detectable
// inside short observation windows (the ~700 ms shutdown window
// specifically — Epic #61 R3.5's video and shader matrix cells).
//
// Trade-off vs the 60s shader: faster animation introduces more
// frame-to-frame visual drift, which means an SSIMULACRA2 golden
// captured at one iTime is less likely to match at another. So this
// fixture is appropriate for tests that assert phash-progression
// (animation IS happening) rather than tests that assert
// SSIMULACRA2 against a golden (animation IS NOT happening between
// the golden moment and the capture moment).

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord.xy / iResolution.xy;
    // 1s period (vs 60s in wallpaper-shader.glsl) — visible hue
    // rotation across any 500 ms observation window.
    float hue = 0.5 + 0.25 * sin(iTime * 6.2831);
    vec3 col = vec3(hue, uv.x, 1.0 - uv.x);
    fragColor = vec4(col, 1.0);
}
