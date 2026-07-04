// Billboard particles: oriented textured quads, instanced.
//
// Each quad spans a PER-INSTANCE basis (`basis_right`/`basis_up`) the CPU packer
// picks per orientation mode — face-camera, upright, flat-on-ground, or stretched
// along velocity — so a track need not face the camera. Positions arrive
// camera-relative (the view matrix has no translation, ADR-0015: the camera is the
// origin), so the basis vectors are camera-relative world directions too.
//
// Group 0 (per frame): camera globals — view·projection (+ a camera right/up basis
// the packer reads on the CPU for face-camera tracks). Group 1 (per batch): the
// track's texture + sampler — the SAME layout as the raster pass's material
// textures, so both passes share one registry.
//
// Vertex stream (buffer 0): the unit quad corner. Instance stream (buffer 1):
// position+spin, size, tint, basis — written by the CPU sim each frame (and, later,
// by the GPU compute backend directly; this shader never knows which).
//
// Deliberately self-contained: when the shader system (ADR-0007) lands, a track's
// material IR compiles to a replacement fragment stage against these same inputs.

struct ParticleGlobals {
    view_proj: mat4x4<f32>,
    cam_right: vec4<f32>,
    cam_up: vec4<f32>,
    fog_color: vec4<f32>,   // rgb = fog color
    fog_params: vec4<f32>,  // x start, y end, z on (0/1)
};

@group(0) @binding(0) var<uniform> g: ParticleGlobals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

// The RGB a particle fades TOWARD in full fog — the blend mode's no-op identity, set
// per pipeline: 0 for alpha/additive/screen/premultiplied (fade to nothing), 1 for
// Multiply (fade to white = stop darkening). Alpha always fades to 0 alongside.
override fog_identity: f32 = 0.0;

struct VsIn {
    // Unit quad corner in [-0.5, 0.5]².
    @location(0) corner: vec2<f32>,
    // Instance: camera-relative position (xyz) + spin angle in radians (w).
    @location(1) pos_rot: vec4<f32>,
    // Instance: billboard width/height in world units (xy; zw unused).
    @location(2) size: vec4<f32>,
    // Instance: tint × life-curve color, straight alpha.
    @location(3) color: vec4<f32>,
    // Instance: the quad's in-plane +X axis (xyz) in camera-relative world space.
    @location(4) basis_right: vec4<f32>,
    // Instance: the quad's in-plane +Y axis (xyz); its length carries stretch.
    @location(5) basis_up: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    // Camera-relative position, so the fragment can compute its own view distance.
    @location(2) view_pos: vec3<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
    // Spin the corner in the billboard plane, then span the per-instance basis (the
    // CPU packer picks it per orientation mode — camera-facing, upright, flat, or
    // velocity-stretched; `basis_up`'s length already carries any stretch).
    let ca = cos(in.pos_rot.w);
    let sa = sin(in.pos_rot.w);
    let c = vec2<f32>(
        in.corner.x * ca - in.corner.y * sa,
        in.corner.x * sa + in.corner.y * ca,
    );
    let world = in.pos_rot.xyz
        + in.basis_right.xyz * (c.x * in.size.x)
        + in.basis_up.xyz * (c.y * in.size.y);

    var out: VsOut;
    out.clip = g.view_proj * vec4<f32>(world, 1.0);
    // Un-spun corner maps the texture, so the image rotates with the particle. The
    // flipbook UV sub-rect [min_u, min_v, du, dv] rides the spare instance channels
    // (size.zw + the two basis .w's); a plain texture packs the full quad [0,0,1,1].
    let base_uv = vec2<f32>(in.corner.x + 0.5, 0.5 - in.corner.y);
    let rect = vec4<f32>(in.size.z, in.size.w, in.basis_right.w, in.basis_up.w);
    out.uv = base_uv * rect.zw + rect.xy;
    out.color = in.color;
    out.view_pos = world;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(tex, samp, in.uv);
    var col = texel * in.color;
    // Fully transparent texels are discarded so depth-adjacent particles don't
    // fog each other's edges with invisible quads.
    if (col.a <= 0.001) {
        discard;
    }
    // Depth fog: fade the contribution toward the blend mode's identity with distance
    // (attenuation, not a tint), so it's correct across every blend family — alpha
    // particles vanish, additive/screen light dims, premultiplied fades out, and
    // Multiply fades to white (no darkening) via the per-pipeline `fog_identity`
    // override — instead of adding fog-coloured light. `view_pos` is camera-relative,
    // so length = view distance.
    if (g.fog_params.z > 0.5) {
        let denom = max(g.fog_params.y - g.fog_params.x, 1e-4);
        let f = clamp((length(in.view_pos) - g.fog_params.x) / denom, 0.0, 1.0);
        col = vec4<f32>(mix(col.rgb, vec3<f32>(fog_identity), f), col.a * (1.0 - f));
    }
    return col;
}
