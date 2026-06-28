// Phase 1 — the first geometry on screen. A single triangle whose three vertices
// are generated from the vertex index (no vertex buffers yet) with per-vertex
// color interpolated across the face. This is the seed of the forward raster path
// (it grows a camera-relative MVP + a textured quad next).

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 0.6),
        vec2<f32>(-0.6, -0.5),
        vec2<f32>(0.6, -0.5),
    );
    var colors = array<vec3<f32>, 3>(
        vec3<f32>(1.0, 0.25, 0.55),
        vec3<f32>(0.30, 0.85, 1.0),
        vec3<f32>(0.95, 0.9, 0.35),
    );
    var o: VOut;
    o.pos = vec4<f32>(positions[vi], 0.0, 1.0);
    o.color = colors[vi];
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
