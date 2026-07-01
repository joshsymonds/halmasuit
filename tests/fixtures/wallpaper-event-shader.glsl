// halmasuit wallpaper test fixture — event-reactive GLSL shader.
//
// Shadertoy-shape (void mainImage), so the preamble injects iTime and
// iResolution. Declares and USES a bus-driven EventTime uniform
// (uLoginTime) so GL keeps the uniform live (an unused uniform is
// optimized out, and the binding write would be meaningless). The
// visual-wallpaper-event VM gate binds uLoginTime to
// `halmasuit.session.opened`; on login the wallpaper-event consumer
// writes the login timestamp here and emits a wallpaper_uniform_applied
// marker the headless gate asserts.
//
// uLoginTime is -1.0 before the event ever fires (the "never fired"
// sentinel) and the login-time seconds (same epoch as iTime) after, so
// `iTime - uLoginTime` is a valid since-login elapsed time.

uniform float uLoginTime;

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord.xy / iResolution.xy;
    // Secondary signal that the time uniform is wired (mirrors the
    // other shader fixture's 60s sine).
    float hue = 0.5 + 0.25 * sin(iTime * 6.2831 / 60.0);
    // Gate the green channel on whether login has fired yet.
    float lit = uLoginTime >= 0.0 ? 1.0 : 0.0;
    vec3 col = vec3(uv.x, mix(hue, 1.0, lit), 1.0 - uv.x);
    fragColor = vec4(col, 1.0);
}
