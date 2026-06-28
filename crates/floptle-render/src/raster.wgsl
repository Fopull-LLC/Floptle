// Forward raster: a textured quad drawn with a camera-relative MVP.
//
// `cam.view_proj` is the camera's view·projection (the view has NO translation —
// the camera is the render-space origin, ADR-0015). `cam.model` is the object's
// camera-relative model matrix (`Transform::render_matrix`), so the GPU only ever
// sees small coordinates. One object for now; `model` becomes per-instance when
// meshes/instancing land.

struct Camera {
    view_proj: mat4x4<f32>,
    model: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> cam: Camera;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@location(0) pos: vec3<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.clip = cam.view_proj * cam.model * vec4<f32>(pos, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
