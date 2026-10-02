// Billboard particles: oriented textured quads, instanced.
//
// Each quad spans a PER-INSTANCE basis (`basis_right`/`basis_up`) the CPU packer
// picks per orientation mode — face-camera, upright, flat-on-ground, or stretched
// along velocity — so a track need not face the camera. Positions arrive
// camera-relative (the view matrix has no translation: the camera is the
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
// Deliberately self-contained: when the shader system lands, a track's
// material IR compiles to a replacement fragment stage against these same inputs.

struct ParticleGlobals {
    view_proj: mat4x4<f32>,
    cam_right: vec4<f32>,
    cam_up: vec4<f32>,
    fog_color: vec4<f32>,   // rgb = fog color
    fog_params: vec4<f32>,  // x start, y end, z on (0/1)
    proj_z: vec4<f32>,      // x P[2][2], y P[3][2], z 1 = orthographic
    // The key light for lit particles: xyz = the way TO it (w = 0), or its
    // camera-relative position (w = 1, a star).
    light_dir: vec4<f32>,
    light_color: vec4<f32>, // rgb, already scaled by intensity
    ambient: vec4<f32>,     // rgb
};

@group(0) @binding(0) var<uniform> g: ParticleGlobals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
// The scene's depth as drawn so far, for the soft edges. A float texture, not a
// depth one: GLSL cannot fetch a texel from a depth sampler.
@group(2) @binding(0) var scene_depth: texture_2d<f32>;

// The RGB a particle fades TOWARD in full fog — the blend mode's no-op identity, set
// per pipeline: 0 for alpha/additive/screen/premultiplied (fade to nothing), 1 for
// Multiply (fade to white = stop darkening). Alpha always fades to 0 alongside.
override fog_identity: f32 = 0.0;
// 1 when the blend mode weights the colour by alpha itself (alpha, additive), so
// a fade scales alpha alone; 0 when the weight lives in the colour and both fade.
override fades_by_alpha: f32 = 0.0;

// The particle at `keep` (1 = as authored, 0 = gone), faded the way its blend
// mode needs: toward the mode's identity in colour, or by alpha alone.
fn attenuate(col: vec4<f32>, keep: f32) -> vec4<f32> {
    if (fades_by_alpha > 0.5) {
        return vec4<f32>(col.rgb, col.a * keep);
    }
    return vec4<f32>(mix(vec3<f32>(fog_identity), col.rgb, keep), col.a * keep);
}

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
    // Instance: x = soft-edge distance in world units (0 = hard).
    @location(6) params: vec4<f32>,
    // Instance: xyz = the effect's up (the ground's, for a night-side test),
    // w = how lit the particle is (0 = unlit, as authored; 1 = fully lit).
    @location(7) light: vec4<f32>,
    // Instance: xy = the next flipbook frame's UV min, z = how far into it
    // (0 = this frame only), w = distortion strength (Distortion blend).
    @location(8) extra: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    // Camera-relative position, so the fragment can compute its own view distance.
    @location(2) view_pos: vec3<f32>,
    @location(3) @interpolate(flat) soft: f32,
    // The quad's corner in [-0.5, 0.5]², un-spun, for the rounded normal.
    @location(4) corner: vec2<f32>,
    @location(5) @interpolate(flat) right: vec3<f32>,
    @location(6) @interpolate(flat) up: vec3<f32>,
    @location(7) @interpolate(flat) light: vec4<f32>,
    // The next flipbook frame's UV, and how far into it.
    @location(8) uv_next: vec2<f32>,
    @location(9) @interpolate(flat) extra: vec2<f32>,
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
    out.uv_next = base_uv * rect.zw + in.extra.xy;
    out.extra = in.extra.zw;
    out.color = in.color;
    out.view_pos = world;
    out.soft = in.params.x;
    out.corner = in.corner;
    // The spun basis, so the rounded normal turns with the picture.
    out.right = normalize(in.basis_right.xyz) * ca + normalize(in.basis_up.xyz) * sa;
    out.up = normalize(in.basis_up.xyz) * ca - normalize(in.basis_right.xyz) * sa;
    out.light = in.light;
    return out;
}

// The particle's texel: this flipbook frame, crossfaded into the next one
// when the track blends its frames.
fn texel_of(in: VsOut) -> vec4<f32> {
    let here = textureSample(tex, samp, in.uv);
    let next = textureSample(tex, samp, in.uv_next);
    return mix(here, next, in.extra.x);
}

// How a lit particle is shaded: ambient plus the key light on a rounded
// normal (a puff reads as a ball, not a card), wrapped so the dark side keeps
// some light, and cut by the ground's own day and night: a particle whose
// ground faces away from the light is in that ground's shadow.
fn lighting(in: VsOut) -> vec3<f32> {
    let c = in.corner * 2.0;
    let toward_cam = normalize(cross(in.right, in.up)) * sign(dot(cross(in.right, in.up), -in.view_pos));
    let bulge = sqrt(max(1.0 - dot(c, c), 0.0));
    let n = normalize(in.right * c.x + in.up * c.y + toward_cam * (bulge + 0.35));
    var l = g.light_dir.xyz;
    if (g.light_dir.w > 0.5) {
        l = g.light_dir.xyz - in.view_pos;
    }
    l = normalize(l);
    let wrap = clamp(dot(n, l) * 0.6 + 0.4, 0.0, 1.0);
    var day = 1.0;
    if (dot(in.light.xyz, in.light.xyz) > 0.25) {
        day = smoothstep(-0.12, 0.2, dot(normalize(in.light.xyz), l));
    }
    return g.ambient.rgb + g.light_color.rgb * wrap * day;
}

// A depth-buffer value as a distance in front of the camera.
fn view_distance(ndc_z: f32) -> f32 {
    if (g.proj_z.z > 0.5) {
        return -(ndc_z - g.proj_z.y) / g.proj_z.x;
    }
    return g.proj_z.y / (ndc_z + g.proj_z.x);
}

// Dither thresholds (this module is standalone — it isn't concatenated with
// field.wgsl, so it carries its own copies). See field.wgsl for the rationale.
fn bayer4(pix: vec2<u32>) -> f32 {
    var m = array<u32, 16>(0u, 8u, 2u, 10u, 12u, 4u, 14u, 6u, 3u, 11u, 1u, 9u, 15u, 7u, 13u, 5u);
    return (f32(m[(pix.y % 4u) * 4u + (pix.x % 4u)]) + 0.5) / 16.0;
}
fn ign(pix: vec2<u32>) -> f32 {
    let p = vec2<f32>(f32(pix.x), f32(pix.y));
    return fract(52.9829189 * fract(dot(p, vec2<f32>(0.06711056, 0.00583715))));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let texel = texel_of(in);
    var col = texel * in.color;
    if (in.light.w > 0.0) {
        col = vec4<f32>(col.rgb * mix(vec3<f32>(1.0), lighting(in), in.light.w), col.a);
    }
    // Fully transparent texels are discarded so depth-adjacent particles don't
    // fog each other's edges with invisible quads.
    if (col.a <= 0.001) {
        discard;
    }
    // Soft edges: fade out over the last `soft` units before the surface behind,
    // so a sprite crossing a floor or a wall has no hard line through it.
    if (in.soft > 0.0) {
        let behind = textureLoad(scene_depth, vec2<i32>(in.clip.xy), 0).x;
        let gap = view_distance(behind) - view_distance(in.clip.z);
        col = attenuate(col, clamp(gap / in.soft, 0.0, 1.0));
    }
    // Depth fog: attenuate with distance rather than tint, so it is right for
    // every blend family — alpha particles vanish, additive light dims, Multiply
    // fades to white. `view_pos` is camera-relative, so length = view distance.
    if (g.fog_params.z > 0.5) {
        let denom = max(g.fog_params.y - g.fog_params.x, 1e-4);
        var f = clamp((length(in.view_pos) - g.fog_params.x) / denom, 0.0, 1.0);
        // Match the scene fog's optional dither (strength in fog_color.w, mode in
        // fog_params.w) so particles band-break identically to the meshes behind them.
        let amp = g.fog_color.w;
        if (amp > 0.0) {
            let pix = vec2<u32>(u32(in.clip.x), u32(in.clip.y));
            let d = select(bayer4(pix), ign(pix), g.fog_params.w > 0.5);
            f = clamp(f + (d - 0.5) * amp * 0.06, 0.0, 1.0);
        }
        col = attenuate(col, 1.0 - f);
    }
    return col;
}

// The scene as drawn before the particles, half resolution: what heat haze
// bends. Bound only for the Distortion pipeline.
@group(3) @binding(0) var scene_tex: texture_2d<f32>;
@group(3) @binding(1) var scene_samp: sampler;

// Heat haze: the scene behind the particle, pushed sideways by the texture's
// red and green (0.5 = no push) times the strength, faded by the particle's
// alpha. A plain white quad bends nothing but still shimmers with its alpha.
@fragment
fn fs_distort(in: VsOut) -> @location(0) vec4<f32> {
    let texel = texel_of(in);
    let a = texel.a * in.color.a;
    if (a <= 0.001) {
        discard;
    }
    let size = vec2<f32>(textureDimensions(scene_depth));
    let screen = in.clip.xy / size;
    let push = (texel.rg - vec2<f32>(0.5)) * 2.0 * in.extra.y * a;
    let behind = textureSampleLevel(scene_tex, scene_samp, screen + push, 0.0);
    var keep = 1.0;
    if (in.soft > 0.0) {
        let d = textureLoad(scene_depth, vec2<i32>(in.clip.xy), 0).x;
        keep = clamp((view_distance(d) - view_distance(in.clip.z)) / in.soft, 0.0, 1.0);
    }
    return vec4<f32>(behind.rgb, a * keep);
}
