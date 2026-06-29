// Editor reference grid — camera-relative world-space lines on the y=0 plane,
// depth-tested (objects occlude it) but not depth-writing (it never occludes them),
// alpha-blended so it sits quietly under the scene.

struct G {
    view_proj: mat4x4<f32>,
    color: vec4<f32>,
};
@group(0) @binding(0) var<uniform> g: G;

@vertex
fn vs(@location(0) pos: vec3<f32>) -> @builtin(position) vec4<f32> {
    return g.view_proj * vec4<f32>(pos, 1.0);
}

@fragment
fn fs() -> @location(0) vec4<f32> {
    return g.color;
}
