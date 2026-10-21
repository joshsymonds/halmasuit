// halmasuit wallpaper test fixture — animated shader with spatial motion.
//
// Shadertoy-shape entry (void mainImage); halmasuit's shader preamble
// injects iTime and iResolution. A bright vertical band sweeps left to
// right every 4 s over a dark vertical gradient.
//
// Built for phash_progression: the 8x8 average hash compares each
// block's mean against the frame mean, so it cannot see a uniform
// brightness change (wallpaper-shader.glsl's slow hue sine) but does see
// spatial motion. The band crosses one hash column every 0.5 s, so any
// window of a second or more yields several distinct phashes at any
// point in the cycle. Colours stay clear of the CLEAR_RGB sentinel.

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord.xy / iResolution.xy;
    float band = fract(iTime / 4.0);
    float lit = step(abs(uv.x - band), 0.06);
    vec3 base = vec3(0.12, 0.12 + 0.2 * uv.y, 0.3);
    fragColor = vec4(mix(base, vec3(1.0), lit), 1.0);
}
