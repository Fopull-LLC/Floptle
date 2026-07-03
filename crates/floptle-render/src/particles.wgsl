// Billboard particles: camera-facing textured quads, instanced.
//
// Group 0 (per frame): camera globals — view·projection + the camera's right/up
// basis (the view matrix has no translation, ADR-0015: instance positions arrive
// camera-relative, so the camera is the origin). Group 1 (per batch): the track's
// texture + sampler — the SAME layout as the raster pass's material textures, so
// both passes share one registry.
//
// Vertex stream (buffer 0): the unit quad corner. Instance stream (buffer 1):
// position+spin, size, tint — written by the CPU sim each frame (and, later, by
// the GPU compute backend directly; this shader never knows which).
//
// Deliberately self-contained: when the shader system (ADR-0007) lands, a track's
// material IR compiles to a replacement fragment stage against these same inputs.

struct ParticleGlobals {
    view_proj: mat4x4<f32>,
    cam_right: vec4<f32>,
    cam_up: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: ParticleGlobals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsIn {
    // Unit quad corner in [-0.5, 0.5]².
    @location(0) corner: vec2<f32>,
    // Instance: camera-relative position (xyz) + spin angle in radians (w).
    @location(1) pos_rot: vec4<f32>,
    // Instance: billboard width/height in world units (xy; zw unused).
    @location(2) size: vec4<f32>,
    // Instance: tint × life-curve color, straight alpha.
    @location(3) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    // Spin the corner in the billboard plane, then span the camera basis.
    let ca = cos(in.pos_rot.w);
    let sa = sin(in.pos_rot.w);
    let c = vec2<f32>(
        in.corner.x * ca - in.corner.y * sa,
        in.corner.x * sa + in.corner.y * ca,
    );
    let world = in.pos_rot.xyz
        + g.cam_right.xyz * (c.x * in.size.x)
        + g.cam_up.xyz * (c.y * in.size.y);

    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(world, 1.0);
    // Un-spun corner maps the texture, so the image rotates with the particle.
    out.uv = vec2<f32>(in.corner.x + 0.5, 0.5 - in.corner.y);
    out.color = in.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(tex, samp, in.uv);
    let col = texel * in.color;
    // Fully transparent texels are discarded so depth-adjacent particles don't
    // fog each other's edges with invisible quads.
    if (col.a <= 0.001) {
        discard;
    }
    return col;
}
