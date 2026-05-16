// Fullscreen-triangle splash shader. One oversized triangle covers the
// whole clip volume; the fragment stage samples the splash texture
// stretched across the output (v1 = fill, no aspect preservation —
// Epic #1 anti-pattern forbids shader/scene complexity here).

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    // (-1,-1), (3,-1), (-1,3) — covers [-1,1]^2 with one triangle.
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let p = pos[idx];
    var out: VsOut;
    out.clip = vec4<f32>(p, 0.0, 1.0);
    // Map clip xy [-1,1] to uv [0,1] with v flipped (image origin is
    // top-left, clip-space y points up).
    out.uv = vec2<f32>(p.x * 0.5 + 0.5, 1.0 - (p.y * 0.5 + 0.5));
    return out;
}

@group(0) @binding(0) var splash_tex: texture_2d<f32>;
@group(0) @binding(1) var splash_smp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(splash_tex, splash_smp, in.uv);
}
