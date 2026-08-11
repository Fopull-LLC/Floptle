//! Gathering World state into renderer uniforms, once per render site: blob
//! materials, point lights, the Lighting node's shadow knobs, the proxy shadow
//! occluders harvested from collider shapes, the Skybox node, and the
//! PostProcess node. Pure read-the-World functions — no GPU types in here
//! beyond the plain uniform arrays they return.

use floptle_core::math::{DVec3, Mat3, Vec3};
use floptle_core::{Entity, Light, Material, Matter, World};
use floptle_render::MaterialParams;

/// Convert a core [`Material`] into the renderer's per-instance [`MaterialParams`].
///
/// The packing itself lives in the renderer ([`MaterialParams::from_material`]) so
/// the probes exercise the same code the editor draws with — including the tiling
/// lanes, which a **spritesheet** material rides to show one cell. Geometry-owned
/// paint offsets are still filled in by the caller (it knows the mesh).
pub(crate) fn material_params(m: &Material) -> MaterialParams {
    MaterialParams::from_material(m)
}

/// Everything a draw needs from a [`Material`] that only the RENDERER can
/// resolve: the group(1) texture set (base colour + the four surface maps) and
/// the surface-extras index (the PBR scalars and the retro flags).
///
/// Returns `(texture, params)` — a drop-in for the pair the gather already
/// carried, so the whole downstream path keeps treating a PBR material as one
/// `TexId` and one `MaterialParams`. Both halves are cached inside the renderer,
/// so calling this per node per frame costs two hash lookups.
///
/// `fallback` is the texture to use when the material names no base colour of
/// its own (a mesh's imported texture, a tilemap's page).
pub(crate) fn material_draw(
    raster: &mut floptle_render::Raster,
    gpu: &floptle_render::Gpu,
    m: &Material,
    registry: &std::collections::HashMap<String, floptle_render::TexId>,
    fallback: Option<floptle_render::TexId>,
) -> (Option<floptle_render::TexId>, MaterialParams) {
    let look = |p: Option<&String>| p.and_then(|p| registry.get(p.as_str()).copied());
    let base = look(m.texture.as_ref()).or(fallback);
    let mut params = material_params(m);
    params.ext_index =
        raster.push_surface_extras(floptle_render::SurfaceExtras::from_material(m));
    // A material with no maps gets its base texture back unchanged — no set is
    // built, nothing is cached, and the draw is byte-for-byte what it was.
    if !m.has_maps() {
        return (base, params);
    }
    let maps = m.maps().map(look);
    (Some(raster.material_set(gpu, base, maps)), params)
}

/// The default look for a Blob with no Material: neutral tint plus the subtle blue
/// rim the blob shipped with, so material-less blobs render exactly as before while a
/// blob that DOES carry a Material is fully driven by it.
pub(crate) fn blob_default_material() -> MaterialParams {
    let mut m = MaterialParams::flat([1.0, 1.0, 1.0]);
    m.rim = [0.5, 0.6, 0.8];
    m.rim_strength = 0.12;
    m
}

/// Pack up to 16 blobs' materials into the raymarch uniform arrays (tint, emissive,
/// specular, params=[shininess,rim,unlit,ambient], rim), mirroring `terrain_*`.
pub(crate) type BlobMatArrays =
    ([[f32; 4]; 16], [[f32; 4]; 16], [[f32; 4]; 16], [[f32; 4]; 16], [[f32; 4]; 16]);
pub(crate) fn blob_mat_arrays(set: &[(DVec3, f32, MaterialParams)]) -> BlobMatArrays {
    let mut tint = [[1.0f32, 1.0, 1.0, 0.0]; 16];
    let mut emissive = [[0.0f32; 4]; 16];
    let mut specular = [[1.0f32, 1.0, 1.0, 0.0]; 16];
    let mut params = [[16.0f32, 0.0, 0.0, 1.0]; 16];
    let mut rim = [[0.0f32; 4]; 16];
    for (i, (_, _, m)) in set.iter().take(16).enumerate() {
        tint[i] = [m.color[0], m.color[1], m.color[2], 0.0];
        emissive[i] = [m.emissive[0], m.emissive[1], m.emissive[2], m.emissive_strength];
        specular[i] = [m.specular[0], m.specular[1], m.specular[2], m.specular_strength];
        params[i] = [m.shininess, m.rim_strength, if m.unlit { 1.0 } else { 0.0 }, m.ambient];
        rim[i] = [m.rim[0], m.rim[1], m.rim[2], 0.0];
    }
    (tint, emissive, specular, params, rim)
}

/// One side of the light split: how many, where, what colour, what SURFACE each
/// one emits from, and — for the 2D side — which sorting layers it reaches and
/// how its falloff is shaped.
///
/// One shape for both sides is what stops the two from drifting: the 3D side
/// fills the 2D lanes and ignores them, and vice versa.
#[derive(Clone, Copy)]
pub(crate) struct LightSlots {
    pub count: usize,
    /// xyz = camera-relative position, w = range.
    pub pos: [[f32; 4]; 16],
    /// rgb = colour × intensity.
    pub color: [[f32; 4]; 16],
    /// 2D only: which sorting-layer ranks this light reaches.
    pub mask: [[u32; 4]; 16],
    /// 2D only: `[inner radius, exponent, casts-are-honoured, spare]`.
    pub falloff: [[f32; 4]; 16],
    /// The EMITTER: `[kind, a, b, flags]` — 0 point, 1 sphere (a = radius),
    /// 2 rect (a/b = half width/height, flag 1 = two-sided), 3 disk (a = radius,
    /// flag 1 = two-sided), 4 tube (a = half length, b = radius). Sizes are in
    /// world units, with the node's scale already folded in.
    pub shape: [[f32; 4]; 16],
    /// The emitter's world orientation (xyzw quaternion) — a rect faces the
    /// node's forward, a tube lies along its local X.
    pub rot: [[f32; 4]; 16],
}

impl Default for LightSlots {
    fn default() -> Self {
        Self {
            count: 0,
            pos: [[0.0; 4]; 16],
            color: [[0.0; 4]; 16],
            mask: [[0; 4]; 16],
            falloff: [[0.0; 4]; 16],
            shape: [[0.0; 4]; 16],
            rot: [[0.0, 0.0, 0.0, 1.0]; 16],
        }
    }
}

/// The scene's placeable lights, separated into the two systems that light with
/// them.
pub(crate) struct SplitLights {
    /// Lights that shade meshes the way they always have.
    pub three_d: LightSlots,
    /// Lights on the 2D path, read by the accumulation pass. `masks[i]` is a
    /// bitmask over SORTING-LAYER RANK spread across four words — bit `r` of
    /// word `r / 32` set means the light reaches rank `r`. All-ones = every
    /// layer, which is what a light that named none does.
    pub two_d: LightSlots,
    /// How many lights qualified but were ranked out of the sixteen, both sides
    /// together. Reported through `perf.counts().lightsDropped`: a cap nobody
    /// can see is the whole complaint in `floptle/0116`, and "my seventeenth
    /// torch does nothing" is not a thing anybody should have to guess.
    pub dropped: usize,
}

/// Split the scene's point lights by which lighting system owns them.
///
/// **A light belongs to exactly one side.** A 2D light that also lit meshes
/// would make a torch in a flat scene wash over any 3D prop that wandered into
/// it, and the whole point of the flag is that the two systems are separable.
/// The 3D array is what it always was for a scene with no 2D lights in it, so
/// nothing that exists today shades differently.
///
/// `sorting_names` is the project's layer order, so a light's named layers can
/// be turned into rank bits once here rather than per fragment. A name the
/// project no longer has contributes no bit — the light simply does not reach a
/// layer that does not exist.
///
/// **Sixteen slots a side, and the seventeenth is dropped on purpose.** A light
/// that is switched off does not take one, and when more than sixteen qualify
/// the survivors are the ones contributing most at the camera — see
/// [`contribution`]. Both matter for the same reason: the set has to hold still
/// between frames in which nothing about the lights changed, or a torch goes out
/// for no reason the room explains.
pub(crate) fn split_point_lights(
    world: &World,
    cam_world: DVec3,
    sorting_names: &[String],
    flat_camera: bool,
) -> SplitLights {
    let facts = floptle_core::Lit2DFacts { emits: true, flat_matter: false, flat_camera };
    let mut three: Vec<Candidate> = Vec::new();
    let mut two: Vec<Candidate> = Vec::new();
    for (e, m) in world.query::<Matter>() {
        let Matter::PointLight { color, intensity, range, shape, shadows } = m else { continue };
        // A light turned off does not take a slot. Keeping N lights and parking
        // the spare ones at zero is the standard way to pool a capped resource —
        // and scripts cannot create a PointLight, so it is the ONLY way. A
        // parked light holding a slot would mean a pool exhausts the budget and
        // lights nothing (`floptle/0116`).
        if *intensity <= 0.0 || *range <= 0.0 {
            continue;
        }
        // …and neither does a light on a node that is SWITCHED OFF, or under
        // one that is. `Disabled` takes a node out of physics and stops its
        // scripts, and a water volume beside this one already goes with it —
        // a lamp prefab you disabled still lighting the room is the reading
        // nobody expects. It costs a slot too, which is exactly the pool
        // exhaustion `floptle/0116` is about.
        if floptle_core::is_disabled(world, e) {
            continue;
        }
        let lit = world.get::<floptle_core::Lighting2D>(e).cloned().unwrap_or_default();
        let (is_2d, _) = floptle_core::resolve_2d(lit.mode, facts);
        let wt = floptle_core::world_transform(world, e);
        let c = (wt.translation - cam_world).as_vec3();
        let side = if is_2d { &mut two } else { &mut three };
        let q = wt.rotation;
        side.push(Candidate {
            order: e.index(),
            score: contribution(c.length(), *range, *color, *intensity),
            pos: [c.x, c.y, c.z, range.max(0.0001)],
            color: [color[0] * intensity, color[1] * intensity, color[2] * intensity, 0.0],
            mask: layer_mask(&lit, sorting_names),
            falloff: lit.falloff_lane(*range),
            shape: emitter_lane(*shape, wt.scale, *shadows),
            rot: [q.x, q.y, q.z, q.w],
        });
    }
    let dropped = three.len().saturating_sub(16) + two.len().saturating_sub(16);
    SplitLights { three_d: fill(three), two_d: fill(two), dropped }
}

/// A light's emitter packed for the shader: `[kind, a, b, flags]`.
///
/// The node's SCALE is folded in here rather than in the shader, because that is
/// what dragging a scale handle on a window light is expected to do — and doing
/// it once per light per frame beats doing it once per light per fragment.
fn emitter_lane(shape: floptle_core::LightShape, scale: Vec3, shadows: bool) -> [f32; 4] {
    use floptle_core::LightShape as S;
    // A uniform-ish scale for the shapes that only have a radius: the largest
    // axis, so scaling a bulb up never makes it smaller in some direction.
    let s = scale.x.abs().max(scale.y.abs()).max(scale.z.abs());
    // The `w` lane is a BITMASK — bit 0 two-sided, bit 1 casts shadows. It was a
    // plain 0/1 for two-sidedness until a second flag needed a home; see
    // `light_flag` in field.wgsl, where the matching read lives.
    let two = |b: bool| if b { LIGHT_TWO_SIDED } else { 0.0 };
    let sh = if shadows { LIGHT_SHADOWS } else { 0.0 };
    match shape {
        S::Point => [0.0, 0.0, 0.0, sh],
        S::Sphere { radius } => [1.0, (radius * s).max(0.0), 0.0, sh],
        S::Rect { width, height, two_sided } => [
            2.0,
            (width * 0.5 * scale.x.abs()).max(1e-4),
            (height * 0.5 * scale.y.abs()).max(1e-4),
            two(two_sided) + sh,
        ],
        S::Disk { radius, two_sided } => {
            [3.0, (radius * s).max(1e-4), 0.0, two(two_sided) + sh]
        }
        S::Tube { length, radius } => {
            [4.0, (length * 0.5 * scale.x.abs()).max(1e-4), (radius * s).max(1e-4), sh]
        }
    }
}

/// The emitter lane's flag bits, matching `LIGHT_TWO_SIDED` / `LIGHT_SHADOWS` in
/// field.wgsl. Spelled as floats because the lane is a `vec4<f32>`.
const LIGHT_TWO_SIDED: f32 = 1.0;
const LIGHT_SHADOWS: f32 = 2.0;

/// One light that qualified, before the sixteen are chosen.
struct Candidate {
    /// The node's own index, purely to break a tie between two lights that
    /// contribute exactly the same — so even that is not decided by whatever
    /// order the ECS happened to yield.
    order: u32,
    score: f32,
    pos: [f32; 4],
    color: [f32; 4],
    mask: [u32; 4],
    falloff: [f32; 4],
    shape: [f32; 4],
    rot: [f32; 4],
}

/// How much a light can matter to this frame: how bright it is, against how far
/// its reach stops short of the eye.
///
/// A light whose sphere contains the camera scores its full brightness. Beyond
/// that it falls off with the gap, so a bright distant lamp can still outrank a
/// dim near one — which is what somebody looking at the screen would expect.
///
/// It depends only on the light and the camera. That is the whole point: the
/// same scene and the same camera choose the same sixteen every frame, so a
/// torch cannot go out for four frames because an enemy died somewhere else and
/// moved the ECS's iteration order (`floptle/0116`).
fn contribution(distance: f32, range: f32, color: [f32; 3], intensity: f32) -> f32 {
    let bright = (0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2]).max(0.0) * intensity;
    bright / (distance - range).max(1.0)
}

/// Take the best sixteen, in a stable order, into the slots the shader reads.
///
/// Ranking only happens when there ARE more than sixteen: under the cap every
/// light gets in whatever order it was found, which is exactly what this did
/// before and what nearly every scene sees.
fn fill(mut lights: Vec<Candidate>) -> LightSlots {
    if lights.len() > 16 {
        lights.sort_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.order.cmp(&b.order))
        });
        lights.truncate(16);
    }
    let mut out = LightSlots { count: lights.len(), ..LightSlots::default() };
    for (i, l) in lights.iter().enumerate() {
        out.pos[i] = l.pos;
        out.color[i] = l.color;
        out.mask[i] = l.mask;
        out.falloff[i] = l.falloff;
        out.shape[i] = l.shape;
        out.rot[i] = l.rot;
    }
    out
}

/// A light's named sorting layers as a bitmask over their RANKS, four words of
/// it — bit `r` of word `r / 32`.
///
/// Four words rather than one because a sorting layer's rank runs to 63
/// (`SORT_LAYER_STEP` is 1/64) and the shader has the space either way: a
/// single word would leave every layer past the 32nd unreachable by any light,
/// and unreachable *silently*, which is the failure shape this codebase keeps
/// paying for.
///
/// Naming no layers is every layer — all ones, not zero. A light that reached
/// nothing until a list was filled in would read as a broken light, and that
/// default has to survive the trip to the GPU as well as the trip to the
/// Inspector.
fn layer_mask(lit: &floptle_core::Lighting2D, sorting_names: &[String]) -> [u32; 4] {
    if lit.layers.is_empty() {
        return [!0; 4];
    }
    let mut mask = [0u32; 4];
    for (rank, name) in sorting_names.iter().enumerate().take(128) {
        if lit.reaches(name) {
            mask[rank / 32] |= 1 << (rank % 32);
        }
    }
    mask
}

/// The key light as the `light_dir` uniform vec4 for THIS camera. Directional:
/// xyz = the normalized direction, w = 0. Stars mode: xyz = the BRIGHTEST
/// star's camera-relative position, w = 1 — single-light consumers (atmosphere
/// daylight, sky glow) follow it; the full per-star loop is `key_light` in the
/// shaders.
pub(crate) fn sun_vec(world: &World, l: &Light, cam_world: DVec3) -> [f32; 4] {
    if l.stars {
        let (meta, pos, _) = star_uniforms(world, l, cam_world);
        if meta[0] > 0.0 {
            return [pos[0][0], pos[0][1], pos[0][2], 1.0];
        }
    }
    let d = Vec3::from(l.direction).normalize_or_zero();
    [d.x, d.y, d.z, 0.0]
}

/// The atmospheres near this camera (S8): up to 4 celestial bodies with
/// shells, deepest-immersion first, as the `atmo_*` raymarch-globals arrays.
/// Bodies are included even from SPACE — the shader draws their limb halo,
/// aerial haze and cloud decks from outside too.
pub(crate) type AtmoUniforms = ([f32; 4], [[f32; 4]; 4], [[f32; 4]; 4], [[f32; 4]; 4]);
pub(crate) fn atmo_uniforms(world: &World, cam_world: DVec3) -> AtmoUniforms {
    type AtmoItem = (f64, [f32; 4], [f32; 4], [f32; 4]);
    let mut items: Vec<AtmoItem> = Vec::new();
    for (e, cb) in world.query::<floptle_core::CelestialBody>() {
        if cb.atmo_height <= 0.0 || cb.atmo_density <= 0.0 {
            continue;
        }
        let wp = floptle_core::world_transform(world, e).translation;
        let rel = wp - cam_world;
        let frac = (rel.length() - cb.body_radius) / cb.atmo_height;
        let r = rel.as_vec3();
        items.push((
            frac,
            [
                cb.atmo_color[0],
                cb.atmo_color[1],
                cb.atmo_color[2],
                cb.atmo_density.clamp(0.0, 1.0),
            ],
            [r.x, r.y, r.z, cb.body_radius as f32],
            [cb.atmo_height as f32, cb.clouds.clamp(0.0, 1.0), 0.0, 0.0],
        ));
    }
    items.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut meta = [0.0f32; 4];
    let mut color = [[0.0f32; 4]; 4];
    let mut body = [[0.0f32, 0.0, 0.0, 1.0]; 4];
    let mut params = [[0.0f32; 4]; 4];
    for (i, it) in items.iter().take(4).enumerate() {
        color[i] = it.1;
        body[i] = it.2;
        params[i] = it.3;
        meta[0] = (i + 1) as f32;
    }
    (meta, color, body, params)
}

/// Stars mode: the luminous celestial bodies as the `star_*` uniform arrays,
/// brightest-at-camera first (irradiance = luminosity × 1e6 / d²). Zero count
/// when the Lighting node isn't in stars mode.
pub(crate) fn star_uniforms(
    world: &World,
    light: &Light,
    cam_world: DVec3,
) -> ([f32; 4], [[f32; 4]; 4], [[f32; 4]; 4]) {
    let mut meta = [0.0f32; 4];
    let mut pos = [[0.0f32; 4]; 4];
    let mut col = [[0.0f32; 4]; 4];
    if !light.stars {
        return (meta, pos, col);
    }
    type StarItem = (f64, [f32; 4], [f32; 4]);
    let mut items: Vec<StarItem> = Vec::new();
    for (e, cb) in world.query::<floptle_core::CelestialBody>() {
        if cb.luminosity <= 0.0 {
            continue;
        }
        let wp = floptle_core::world_transform(world, e).translation;
        let rel = wp - cam_world;
        let d2 = rel.length_squared().max(1.0);
        let k = cb.luminosity as f64 * 1.0e6;
        let r = rel.as_vec3();
        items.push((
            k / d2,
            [r.x, r.y, r.z, 0.0],
            [cb.star_color[0], cb.star_color[1], cb.star_color[2], k as f32],
        ));
    }
    items.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (i, it) in items.iter().take(4).enumerate() {
        pos[i] = it.1;
        col[i] = it.2;
        meta[0] = (i + 1) as f32;
    }
    (meta, pos, col)
}

/// The Lighting node's shadow knobs as the raymarch-globals uniform vec4s
/// (`shadow_params` / `shadow_tint` / `shadow_extra`). Softness 0..1 maps to the
/// penumbra sharpness `k` on a log ramp (0 → 64 razor-hard, 1 → 2 dreamy-soft) so
/// the slider feels perceptually even.
pub(crate) fn shadow_uniforms(l: &Light) -> ([f32; 4], [f32; 4], [f32; 4]) {
    let k = 64.0 * (2.0f32 / 64.0).powf(l.shadow_softness.clamp(0.0, 1.0));
    (
        [
            if l.shadows { 1.0 } else { 0.0 },
            k,
            l.shadow_strength.clamp(0.0, 1.0),
            l.shadow_distance.max(1.0),
        ],
        [l.shadow_tint[0], l.shadow_tint[1], l.shadow_tint[2], l.shadow_quantize as f32],
        [if l.shadow_dither { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
    )
}

/// The contact-shadow lane. Reported OFF when the sun's shadows are off, because
/// a contact shadow is the same shadow: leaving it running under a scene whose
/// shadows are switched off would mean "shadows off" did not mean off.
pub(crate) fn contact_uniform(l: &Light) -> [f32; 4] {
    [
        if l.contact_shadows && l.shadows { 1.0 } else { 0.0 },
        l.contact_length.clamp(0.01, 20.0),
        l.contact_steps.clamp(2, 32) as f32,
        l.contact_strength.clamp(0.0, 1.0),
    ]
}

/// The screen-space-reflection lane, `[on, reach, steps, thickness]`.
///
/// **`primed` is what makes the first frame right.** With no stored picture the
/// march has nothing to sample, and reporting "on" would have every mirror in
/// the scene read a black texture and go dark for a frame — at a load, at a
/// scene switch, at every window resize. Off until there is something to
/// reflect, and the environment map covers the gap, which is the same answer
/// the effect gives for a ray that leaves the screen.
pub(crate) fn ssr_uniform(l: &Light, primed: bool) -> [f32; 4] {
    [
        if l.reflections && primed { 1.0 } else { 0.0 },
        l.reflection_distance.clamp(0.1, 500.0),
        l.reflection_steps.clamp(8, 64) as f32,
        l.reflection_thickness.clamp(0.01, 20.0),
    ]
}

/// The ceiling on one reflected bounce — see [`Light::reflection_clamp`]. Rides
/// the probe lane rather than [`ssr_uniform`] because that vector is full.
///
/// **0 means no ceiling**, and that reading is deliberate: it is the value a
/// globals block that never heard of this field already holds, and it has to
/// mean "reflect as before" rather than "clamp everything to nothing".
pub(crate) fn reflection_clamp(l: &Light) -> f32 {
    l.reflection_clamp.clamp(0.0, 10_000.0)
}

/// The depth-fog uniforms for the Lighting node: `(fog_color, fog_params)` where
/// `fog_params = [start, end, on, dither_mode]` and the spare `fog_color.w` carries
/// the effective dither strength (0 = off). Fed to the raymarch/raster field globals
/// AND the particle globals so meshes, matter, terrain and particles fog together —
/// and band-break identically. Packing into the two already-spare `.w` lanes keeps
/// the uniform layout (and its byte-sync with the WGSL structs) unchanged.
/// Volumetric-fog uniform lanes (`vol_fog_a/b`): densities/heights straight off
/// the Lighting node, `time` drifting the noise, and the camera's WORLD height
/// so the shader can map camera-relative positions back to world y.
/// The third lane carries the light injection: amount, phase anisotropy, march
/// steps, and whether each step marches the sun shadow (the shafts). `fog_shafts`
/// is reported as off when the amount is 0 — with no light to inject there is
/// nothing for a shadow to darken, and a scene left on the flat look should not
/// be paying for a shadow march per step to reach it.
pub(crate) fn vol_fog_uniforms(l: &Light, time: f32, cam_y: f32) -> ([f32; 4], [f32; 4], [f32; 4]) {
    let amount = l.fog_light.max(0.0);
    (
        [
            l.fog_density.max(0.0),
            l.fog_height,
            l.fog_falloff.max(1e-3),
            l.fog_noise.clamp(0.0, 1.0),
        ],
        [
            l.fog_noise_scale.max(1e-3),
            time,
            cam_y,
            if l.fog && l.fog_volumetric { 1.0 } else { 0.0 },
        ],
        [
            amount,
            l.fog_anisotropy.clamp(-0.95, 0.95),
            l.fog_steps.clamp(2, 64) as f32,
            if l.fog_shafts && amount > 0.0 { 1.0 } else { 0.0 },
        ],
    )
}

pub(crate) fn fog_uniforms(l: &Light) -> ([f32; 4], [f32; 4]) {
    let dither = if l.fog_dither { l.fog_dither_strength.clamp(0.0, 1.0) } else { 0.0 };
    (
        [l.fog_color[0], l.fog_color[1], l.fog_color[2], dither],
        [l.fog_start, l.fog_end.max(l.fog_start + 1e-3), if l.fog { 1.0 } else { 0.0 }, 0.0],
    )
}

/// The WaterVolume the point `p` is inside, if any: `(tint, visibility)`.
///
/// Deliberately computed from the ECS rather than from the physics sim, so it
/// answers in the editor viewport too — you can see what a sea looks like from
/// inside without entering Play, which is the only way tuning the tint is not
/// guesswork. Deepest volume wins, matching the physics rule exactly.
pub(crate) fn underwater_at(
    world: &floptle_core::World,
    p: floptle_core::math::DVec3,
) -> Option<([f32; 3], f32)> {
    use floptle_core::{Matter, WaterKind};
    let mut best: Option<(f64, [f32; 3], f32)> = None;
    for (e, m) in world.query::<Matter>() {
        let Matter::WaterVolume {
            kind, radius, half_extents, frozen, tint, visibility, ..
        } = m
        else {
            continue;
        };
        // A frozen sea has no inside to be in, and a switched-off node is off
        // for the look as well as for the physics.
        if *frozen || floptle_core::is_disabled(world, e) {
            continue;
        }
        let wt = floptle_core::world_transform(world, e);
        let depth = match kind {
            WaterKind::Sea => {
                let r = (*radius * wt.scale.max_element().max(1e-4)) as f64;
                r - (p - wt.translation).length()
            }
            WaterKind::Pool => {
                let half = floptle_core::math::Vec3::new(
                    half_extents[0] * wt.scale.x,
                    half_extents[1] * wt.scale.y,
                    half_extents[2] * wt.scale.z,
                )
                .abs()
                .as_dvec3();
                let local = wt.rotation.inverse().as_dquat() * (p - wt.translation);
                if local.x.abs() > half.x || local.z.abs() > half.z || local.y < -half.y {
                    continue;
                }
                half.y - local.y
            }
        };
        if depth > 0.0 && best.is_none_or(|(d, ..)| depth > d) {
            best = Some((depth, *tint, visibility.max(0.5)));
        }
    }
    best.map(|(_, tint, vis)| (tint, vis))
}

/// Mirror the scene's WaterVolume nodes for scripts, in WORLD coordinates.
///
/// The transform is folded in here rather than in Lua so the script answer and
/// the solver's come from the same geometry — a scaled or rotated tank is one
/// shape, not two that have to be kept in step.
pub(crate) fn water_infos(
    world: &floptle_core::World,
) -> Vec<floptle_script::water_api::WaterInfo> {
    use floptle_core::{Matter, WaterKind};
    let mut out = Vec::new();
    for (e, m) in world.query::<Matter>() {
        let Matter::WaterVolume { kind, radius, half_extents, density, frozen, .. } = m else {
            continue;
        };
        if floptle_core::is_disabled(world, e) {
            continue;
        }
        let wt = floptle_core::world_transform(world, e);
        let q = wt.rotation.as_dquat();
        out.push(floptle_script::water_api::WaterInfo {
            entity: e.index(),
            sea: *kind == WaterKind::Sea,
            center: wt.translation.to_array(),
            radius: (*radius * wt.scale.max_element().max(1e-4)) as f64,
            half: [
                (half_extents[0] * wt.scale.x).abs().max(1e-4) as f64,
                (half_extents[1] * wt.scale.y).abs().max(1e-4) as f64,
                (half_extents[2] * wt.scale.z).abs().max(1e-4) as f64,
            ],
            rot: [q.x, q.y, q.z, q.w],
            density: *density,
            frozen: *frozen,
        });
    }
    out
}

/// [`fog_uniforms`], overridden while the camera is under water — plus the
/// `fog_params` the PARTICLE pass should use.
///
/// The scene's own fog is REPLACED rather than added to: underwater is a
/// different medium, not the same air with more of it. Going through the one
/// fog channel every draw path already reads is what makes meshes, terrain, SDF
/// matter and particles go murky *together* — a separate underwater pass would
/// have had to be taught about each of them, and would have missed one.
///
/// The two sets of params differ in volumetric mode and only there. Particles
/// fade on a plain distance ramp — they don't march the media — so handing them the volumetric
/// lane's `y` would hand them the sky-ray *fence*, a number an artist raises for
/// distant sky quality and which would then silently stop particles fading at
/// all. Instead the ramp is derived from the density itself: `2.3/σ` is where
/// the marched layer reaches ~90% coverage, so a particle disappears roughly
/// where the fog behind it does.
pub(crate) fn fog_uniforms_and_particles_at(
    l: &Light,
    world: &floptle_core::World,
    cam: floptle_core::math::DVec3,
) -> (([f32; 4], [f32; 4]), [f32; 4]) {
    if let Some((tint, vis)) = underwater_at(world, cam) {
        let dither = if l.fog_dither { l.fog_dither_strength.clamp(0.0, 1.0) } else { 0.0 };
        // Start close to the eye: water attenuates from the first centimetre,
        // and a start distance would give you a crisp bubble of clear water
        // around the camera that moves with you.
        let params = [vis * 0.05, vis, 1.0, 0.0];
        return (([tint[0], tint[1], tint[2], dither], params), params);
    }
    let (color, params) = fog_uniforms(l);
    let particles = if l.fog && l.fog_volumetric {
        let full = 2.3 / l.fog_density.max(1e-4);
        [full * 0.15, full, 1.0, params[3]]
    } else {
        params
    };
    ((color, params), particles)
}

/// Harvest up to 32 proxy shadow occluders from the world's collider shapes —
/// how DYNAMIC raster meshes CAST sun shadows without being in the SDF field.
/// Mirrors the physics build: a RigidBody node casts its body shape; a Collidable
/// primitive casts the static shape `add_static_colliders` gives it (Cube →
/// 0.7·scale box, Sphere → 0.85·max-scale, Capsule → 0.5-sized). Static collider
/// MESHES don't proxy — they bake real shadow-only occluder volumes instead
/// (`refresh_mesh_occluders`), so a level casts with its true silhouette. Skips
/// hidden nodes and `CastShadow(false)` opt-outs; returns zeros when shadows are
/// off.
/// The proxy-occluder uniform block: `[count, 0, 0, 0]` plus the `prox_a` /
/// `prox_b` / `prox_rot` arrays the shadow march reads (see `field.wgsl`).
pub(crate) type ShadowProxies = ([f32; 4], [[f32; 4]; 32], [[f32; 4]; 32], [[f32; 4]; 32]);

pub(crate) fn collect_shadow_proxies(world: &World, cam_world: DVec3, enabled: bool) -> ShadowProxies {
    let mut a = [[0.0f32; 4]; 32];
    let mut b = [[0.0f32; 4]; 32];
    let mut r = [[0.0f32, 0.0, 0.0, 1.0]; 32];
    let mut n = 0usize;
    if !enabled {
        return ([0.0; 4], a, b, r);
    }
    let casts = |e: Entity| {
        world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true)
            && !matches!(world.get::<floptle_core::Visible>(e), Some(floptle_core::Visible(false)))
    };
    // Dynamic bodies first (the movers a shadow grounds most), then static
    // Collidable primitives. Blobs/terrain are already in the field itself.
    for (e, rb) in world.query::<floptle_core::RigidBody>() {
        if n >= floptle_render::MAX_SHADOW_PROXIES || !casts(e) {
            continue;
        }
        let wt = floptle_core::world_transform(world, e);
        let c = (wt.translation - cam_world).as_vec3();
        match rb.kind {
            floptle_core::BodyKind::Sphere => {
                a[n] = [c.x, c.y, c.z, rb.radius];
                b[n] = [0.0, 0.0, 0.0, 0.0];
            }
            floptle_core::BodyKind::Capsule => {
                let up = wt.rotation * Vec3::Y;
                let half = (0.5 * rb.height - rb.radius).max(0.0);
                let (pa, pb) = (c - up * half, c + up * half);
                a[n] = [pa.x, pa.y, pa.z, rb.radius];
                b[n] = [pb.x, pb.y, pb.z, 1.0];
            }
            floptle_core::BodyKind::Box => {
                let h = rb.half_extents;
                a[n] = [c.x, c.y, c.z, 0.0];
                b[n] = [h[0], h[1], h[2], 2.0];
                let q = wt.rotation;
                r[n] = [q.x, q.y, q.z, q.w];
            }
        }
        n += 1;
    }
    for (e, _) in world.query::<floptle_core::Collidable>() {
        if n >= floptle_render::MAX_SHADOW_PROXIES
            || !casts(e)
            || world.get::<floptle_core::RigidBody>(e).is_some()
        {
            continue;
        }
        let wt = floptle_core::world_transform(world, e);
        let c = (wt.translation - cam_world).as_vec3();
        let s = wt.scale;
        match world.get::<Matter>(e) {
            Some(Matter::Primitive { shape, .. }) => match shape {
                floptle_core::Shape::Cube => {
                    a[n] = [c.x, c.y, c.z, 0.0];
                    b[n] = [0.7 * s.x, 0.7 * s.y, 0.7 * s.z, 2.0];
                    let q = wt.rotation;
                    r[n] = [q.x, q.y, q.z, q.w];
                }
                floptle_core::Shape::Plane => {
                    // Flat in Z → a thin oriented box occluder (w = 2.0 = box).
                    a[n] = [c.x, c.y, c.z, 0.0];
                    b[n] = [0.7 * s.x, 0.7 * s.y, 0.02 * s.z.max(1.0), 2.0];
                    let q = wt.rotation;
                    r[n] = [q.x, q.y, q.z, q.w];
                }
                floptle_core::Shape::Sphere => {
                    a[n] = [c.x, c.y, c.z, 0.85 * s.max_element()];
                    b[n] = [0.0, 0.0, 0.0, 0.0];
                }
                floptle_core::Shape::Capsule => {
                    let up = wt.rotation * Vec3::Y;
                    let radius = 0.5 * s.x.max(s.z);
                    let half = (0.5 * s.y).max(0.0);
                    let (pa, pb) = (c - up * half, c + up * half);
                    a[n] = [pa.x, pa.y, pa.z, radius];
                    b[n] = [pb.x, pb.y, pb.z, 1.0];
                }
            },
            _ => continue, // trimesh colliders don't proxy (see doc comment)
        }
        n += 1;
    }
    ([n as f32, 0.0, 0.0, 0.0], a, b, r)
}

/// Cache key for a mesh shadow-occluder bake: the asset path + the node's world
/// rotation and scale quantized to 1e-3. Translation is deliberately absent —
/// the volume anchors on the node's f64 translation per frame, so MOVING a map
/// never rebakes; only re-orienting or rescaling it does.
pub(crate) type OccKey = (String, [i32; 4], [i32; 3]);

/// Resolve the scene's Skybox node into raymarch uniform fields:
/// `(sky_params [mode, size, _, _], sky_tint rgba, sky_rot 3 columns, solid_color rgb)`.
/// Falls back to the default dark background when there's no Skybox node.
pub(crate) fn skybox_uniforms(
    world: &floptle_core::World,
) -> ([f32; 4], [f32; 4], [[f32; 4]; 3], [f32; 3]) {
    // A DISABLED Skybox is not the scene's sky. That matters beyond the
    // Inspector's checkbox: it is how an additive layer loaded with
    // `{ environment = true }` takes the environment over — the base scene's
    // node steps aside rather than racing the layer's for this first match.
    let found = world.query::<Matter>().find_map(|(e, m)| match m {
        Matter::Skybox { color, size, texture, tint, .. }
            if world.get::<floptle_core::Disabled>(e).is_none() =>
        {
            Some((e, *color, *size, texture.is_some(), *tint))
        }
        _ => None,
    });
    match found {
        Some((e, color, size, textured, tint)) => {
            let rot = floptle_core::world_transform(world, e).rotation;
            let m = Mat3::from_quat(rot.inverse());
            let rot_cols = [
                [m.x_axis.x, m.x_axis.y, m.x_axis.z, 0.0],
                [m.y_axis.x, m.y_axis.y, m.y_axis.z, 0.0],
                [m.z_axis.x, m.z_axis.y, m.z_axis.z, 0.0],
            ];
            (
                [if textured { 1.0 } else { 0.0 }, size, 0.0, 0.0],
                [tint[0], tint[1], tint[2], 1.0],
                rot_cols,
                color,
            )
        }
        None => (
            [0.0; 4],
            [1.0; 4],
            [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]],
            [0.02, 0.02, 0.05],
        ),
    }
}

/// Resolve the scene's PostProcess node for the renderer: the PostStack settings
/// (bloom / vignette / SSAO) plus the raymarch SDF-AO params `[on, strength,
/// radius, _]`. A disabled chain — or a node deleted mid-session — turns
/// everything off (it self-heals back on the next scene load).
/// The camera history motion blur reprojects against: the previous frame's
/// view-projection, and where the camera was when it was taken.
pub(crate) type MotionHistory = (floptle_core::math::Mat4, DVec3);

/// Fill in the per-frame half of motion blur and return the history to keep.
///
/// The scene owns the shutter; the frame owns the two matrices and the streak
/// ceiling, because only the frame knows which camera is rendering and how many
/// pixels tall it is.
///
/// **The world is camera-relative** (ADR-0015), so the previous view-projection
/// cannot be used as it was taken: a point sitting still in the world has a
/// different relative position in each frame's coordinates. Shifting by how far
/// the camera itself moved is what turns "where was this pixel" into a question
/// about the scene rather than about the origin.
///
/// With no history — the first frame after a load, a scene switch, a camera cut
/// — the previous matrix IS the current one, so every pixel reports zero motion
/// and the frame is left sharp. That is the right answer for a cut, and the only
/// safe one: the alternative is one frame smeared by whatever the camera used to
/// be looking at.
pub(crate) fn motion_frame(
    s: &mut floptle_render::PostSettings,
    prev: Option<MotionHistory>,
    view_proj: floptle_core::math::Mat4,
    cam_world: DVec3,
    height: u32,
) -> MotionHistory {
    // The ceiling scales with the frame, because the same streak in uv is twice
    // as long on a 2160-tall picture as on a 1080-tall one — a fixed pixel cap
    // would be a different look at every window size.
    s.motion_max = (height.max(1) as f32 * 0.05).clamp(8.0, 96.0);
    s.motion_inv_view_proj = view_proj.inverse().to_cols_array_2d();
    s.motion_prev_view_proj = match prev {
        Some((vp, at)) => {
            let delta = (cam_world - at).as_vec3();
            (vp * floptle_core::math::Mat4::from_translation(delta)).to_cols_array_2d()
        }
        None => view_proj.to_cols_array_2d(),
    };
    (view_proj, cam_world)
}

/// Resolve the PostProcess node's **focus node** into a focus distance for one
/// camera, or `None` when the scene isn't using one.
///
/// Separate from [`post_process_uniforms`] because it is the one post setting
/// that depends on where the camera IS, and the editor renders the same scene
/// from more than one — the Scene view, the Game view, a camera preview. Folding
/// it into the settings would pick one of those cameras and be wrong in the
/// others: the Scene view would show the game camera's focus while you fly
/// around, which reads as the effect being broken.
///
/// A name that resolves to nothing returns `None`, so the authored `dof_focus`
/// stands. Renaming a node must not silently soften the whole frame.
pub(crate) fn dof_focus_distance(
    world: &floptle_core::World,
    cam_pos: floptle_core::math::DVec3,
) -> Option<f32> {
    let name = world.query::<Matter>().find_map(|(e, m)| match m {
        Matter::PostProcess { dof_focus_node, enabled, .. }
            if *enabled
                && !dof_focus_node.is_empty()
                && world.get::<floptle_core::Disabled>(e).is_none() =>
        {
            Some(dof_focus_node.clone())
        }
        _ => None,
    })?;
    let target = world
        .query::<floptle_core::Name>()
        .find(|(e, n)| n.0 == name && world.get::<floptle_core::Disabled>(*e).is_none())
        .map(|(e, _)| e)?;
    let p = floptle_core::world_transform(world, target).translation;
    Some((p - cam_pos).length() as f32)
}

pub(crate) fn post_process_uniforms(world: &floptle_core::World) -> (floptle_render::PostSettings, [f32; 4]) {
    use floptle_core::AoMode;
    // `PostSettings::default()` IS off, and is the one definition of what the
    // identity values are — half of them are 1.0, and writing them out a second
    // time here is how the two drift.
    let off = floptle_render::PostSettings::default();
    for (e, m) in world.query::<Matter>() {
        // Same rule as the skybox above: a disabled chain is not the scene's
        // chain, so it is skipped rather than returning `off` — which would let
        // a base scene's sleeping node veto the layer that replaced it.
        if world.get::<floptle_core::Disabled>(e).is_some() {
            continue;
        }
        if let Matter::PostProcess {
            tonemap,
            enabled,
            bloom,
            bloom_threshold,
            bloom_intensity,
            vignette,
            vignette_strength,
            vignette_radius,
            ao,
            ao_strength,
            ao_radius,
            posterize_bands,
            posterize_dither,
            posterize_chroma,
            exposure,
            contrast,
            saturation,
            temperature,
            tint,
            lift,
            grade_gamma,
            gain,
            aberration,
            distortion,
            sharpen,
            denoise,
            grain,
            grain_size,
            dof_focus,
            dof_range,
            dof_near_range,
            dof_max_blur,
            dof_blades,
            dof_blade_rotation,
            dof_highlight,
            dof_quality,
            motion_blur,
            motion_samples,
            dof_show_focus,
            // Resolved per VIEWPORT, against that viewport's own camera —
            // see `dof_focus_distance`. It cannot be folded in here: this
            // function does not know which eye is about to render.
            dof_focus_node: _,
            screen_shaders: _,
        } = m
        {
            if !enabled {
                return (off, [0.0; 4]);
            }
            let s = floptle_render::PostSettings {
                bloom: *bloom,
                bloom_threshold: *bloom_threshold,
                bloom_intensity: *bloom_intensity,
                vignette: *vignette,
                vignette_strength: *vignette_strength,
                vignette_radius: *vignette_radius,
                ssao: *ao == AoMode::ScreenSpace,
                ssao_strength: *ao_strength,
                ssao_radius: *ao_radius,
                posterize_bands: *posterize_bands,
                posterize_dither: *posterize_dither,
                posterize_chroma: *posterize_chroma,
                // A tonemap the scene never chose falls back to Clip, which is
                // what the pipeline did before there was a choice.
                tonemap: floptle_render::Tonemap::ALL
                    .get(*tonemap as usize)
                    .copied()
                    .unwrap_or_default(),
                exposure: *exposure,
                contrast: *contrast,
                saturation: *saturation,
                temperature: *temperature,
                tint: *tint,
                lift: *lift,
                grade_gamma: *grade_gamma,
                gain: *gain,
                aberration: *aberration,
                distortion: *distortion,
                sharpen: *sharpen,
                denoise: *denoise,
                grain: *grain,
                grain_size: *grain_size,
                dof_focus: *dof_focus,
                dof_range: *dof_range,
                dof_near_range: *dof_near_range,
                dof_max_blur: *dof_max_blur,
                dof_blades: *dof_blades,
                dof_blade_rotation: dof_blade_rotation.to_radians(),
                dof_highlight: *dof_highlight,
                dof_quality: *dof_quality,
                dof_show_focus: *dof_show_focus,
                // The shutter comes from the scene; the two matrices and the
                // streak ceiling come from the FRAME (and only the game view
                // fills them — see `motion_frame`).
                motion_blur: *motion_blur,
                motion_samples: *motion_samples,
                // The accessibility filter is a PREFERENCE, not a scene
                // setting — the caller folds it in after this (`floptle/0079`),
                // and `time` likewise comes from the frame, not the node.
                ..floptle_render::PostSettings::default()
            };
            let ao_p =
                if *ao == AoMode::Sdf { [1.0, *ao_strength, *ao_radius, 0.0] } else { [0.0; 4] };
            return (s, ao_p);
        }
    }
    (off, [0.0; 4])
}

#[cfg(test)]
mod light_split_tests {
    use super::*;
    use floptle_core::{Lighting2D, Lit2D, Matter, World};

    fn light_at(world: &mut World, x: f64, mode: Option<Lighting2D>) -> floptle_core::Entity {
        let e = world.spawn();
        world.insert(e, Matter::PointLight { color: [1.0, 0.5, 0.25], intensity: 2.0, range: 8.0, shape: Default::default() , shadows: false});
        world.insert(
            e,
            floptle_core::transform::Transform {
                translation: DVec3::new(x, 0.0, 0.0),
                ..Default::default()
            },
        );
        if let Some(l) = mode {
            world.insert(e, l);
        }
        e
    }

    fn layers() -> Vec<String> {
        vec!["Default".into(), "Terrain".into(), "Characters".into()]
    }

    /// A light belongs to exactly ONE system. A 2D torch that also lit meshes
    /// would wash over any 3D prop that wandered into a flat scene, and the
    /// whole point of the flag is that the two are separable.
    #[test]
    fn a_light_lands_on_one_side_and_only_one() {
        let mut world = World::default();
        light_at(&mut world, 0.0, None); // auto
        light_at(&mut world, 1.0, Some(Lighting2D { mode: Lit2D::No, ..Default::default() }));

        // Flat scene: auto is 2D, the stated 3D one is not.
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), true);
        assert_eq!((s.two_d.count, s.three_d.count), (1, 1));
        // Perspective scene: both are 3D and nothing is on the 2D side.
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!((s.two_d.count, s.three_d.count), (0, 2));
    }

    /// A scene with no 2D lights in it must hand the 3D shader exactly what it
    /// always got — same count, same slots, same numbers.
    #[test]
    fn a_scene_with_no_2d_lights_shades_as_it_always_did() {
        let mut world = World::default();
        light_at(&mut world, 3.0, None);
        light_at(&mut world, -2.0, None);
        let s = split_point_lights(&world, DVec3::new(1.0, 0.0, 0.0), &layers(), false);
        let (count, pos, col) = (s.three_d.count, s.three_d.pos, s.three_d.color);
        assert_eq!(count, 2, "both lights are 3D in a perspective scene");
        assert_eq!(pos[0], [2.0, 0.0, 0.0, 8.0], "camera-relative, range in w");
        assert_eq!(col[0], [2.0, 1.0, 0.5, 0.0], "colour times intensity");
    }

    /// Naming no layers is EVERY layer, all the way to the GPU. A default that
    /// arrived as a zero mask would light nothing, which is the same bug as a
    /// light that lit nothing until a list was filled in — just further away
    /// from where anybody would look for it.
    #[test]
    fn a_light_that_names_no_layers_arrives_reaching_all_of_them() {
        let mut world = World::default();
        light_at(&mut world, 0.0, Some(Lighting2D { mode: Lit2D::Yes, ..Default::default() }));
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.two_d.count, 1);
        assert_eq!(s.two_d.mask[0], [!0u32; 4], "the mask must be all-ones, never zero");
    }

    /// Named layers become RANK bits, so the shader compares a number rather
    /// than a string. A name the project no longer has contributes no bit — the
    /// light does not reach a layer that does not exist.
    #[test]
    fn named_layers_become_rank_bits() {
        let mut world = World::default();
        light_at(
            &mut world,
            0.0,
            Some(Lighting2D {
                mode: Lit2D::Yes,
                layers: vec!["Characters".into(), "Gone".into()],
                ..Default::default()
            }),
        );
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.two_d.mask[0], [1 << 2, 0, 0, 0], "only Characters, which is rank 2");
    }

    /// A rank past the 32nd has to land in a later WORD, not fall off the end of
    /// the first one. A project with that many sorting layers would otherwise
    /// find every layer past the 32nd unlit by every light, with nothing said.
    #[test]
    fn a_layer_past_the_thirty_second_still_gets_a_bit() {
        let mut world = World::default();
        let names: Vec<String> = (0..40).map(|i| format!("layer{i}")).collect();
        light_at(
            &mut world,
            0.0,
            Some(Lighting2D { mode: Lit2D::Yes, layers: vec!["layer35".into()], ..Default::default() }),
        );
        let s = split_point_lights(&world, DVec3::ZERO, &names, false);
        assert_eq!(s.two_d.mask[0], [0, 1 << 3, 0, 0], "rank 35 is bit 3 of word 1");
    }

    /// A light turned off must not hold a slot. Scripts cannot create a
    /// `PointLight`, so authoring N and parking the spare ones at zero is the
    /// only pool available — and a parked light that consumed the budget would
    /// mean the pool exhausts all sixteen and lights nothing.
    #[test]
    fn a_light_switched_off_does_not_hold_a_slot() {
        let mut world = World::default();
        let dark = light_at(&mut world, 0.0, None);
        world.insert(
            dark,
            Matter::PointLight { color: [1.0; 3], intensity: 0.0, range: 8.0, shape: Default::default() , shadows: false},
        );
        let spent = light_at(&mut world, 1.0, None);
        world.insert(spent, Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 0.0, shape: Default::default() , shadows: false});
        light_at(&mut world, 2.0, None); // the only one actually lighting anything

        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.three_d.count, 1, "a parked light took a slot");
        assert_eq!(s.three_d.pos[0][0], 2.0, "…and the wrong one survived");
    }

    /// A node you switched OFF is off. `Disabled` already takes a node out of
    /// physics, stops its scripts and stops it drawing — a lamp prefab you
    /// disabled that still lit the room is the one reading nobody expects, and
    /// it spent a slot doing it.
    ///
    /// The subtree counts, because that is what `Disabled` means everywhere
    /// else: disabling the lamp disables the bulb hanging off it.
    #[test]
    fn a_switched_off_node_does_not_light_the_scene() {
        let mut world = World::default();
        let off = light_at(&mut world, 0.0, None);
        world.insert(off, floptle_core::Disabled);
        // A child of a disabled node, which is how a prefab actually carries
        // its light.
        let child = light_at(&mut world, 1.0, None);
        world.insert(child, floptle_core::Parent(off));
        light_at(&mut world, 2.0, None); // the one still switched on

        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.three_d.count, 1, "a disabled node's light still lit the scene");
        assert_eq!(s.three_d.pos[0][0], 2.0, "…and it was the wrong one that survived");
        assert_eq!(s.dropped, 0, "nothing was dropped — they never qualified");
    }

    /// Which sixteen survive must depend on the lights and the camera, and on
    /// nothing else. The bug this guards is the nastiest kind: ECS iteration
    /// order moves as things spawn and despawn, so an order-dependent choice
    /// would drop a different light from frame to frame with nothing in the
    /// scene having changed — a torch that goes out when an enemy dies
    /// somewhere else, which a player reads as a tell.
    #[test]
    fn the_chosen_sixteen_do_not_depend_on_iteration_order() {
        let build = |near_first: bool| {
            let mut world = World::default();
            let xs: Vec<i32> = if near_first { (0..24).collect() } else { (0..24).rev().collect() };
            for x in xs {
                light_at(&mut world, x as f64 * 10.0, None);
            }
            let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
            let mut got: Vec<i32> =
                (0..s.three_d.count).map(|i| s.three_d.pos[i][0].round() as i32).collect();
            got.sort_unstable();
            got
        };
        let near_first = build(true);
        assert_eq!(near_first.len(), 16, "the cap still caps");
        assert_eq!(near_first, build(false), "the same scene chose a different sixteen");
        // …and it kept the ones nearest the camera, not the first it happened
        // to walk over.
        assert_eq!(near_first, (0..16).map(|i| i * 10).collect::<Vec<_>>());
    }

    /// Sixteen slots per side, and running out of one must not spill into the
    /// other or drop the side that had room.
    #[test]
    fn each_side_fills_its_own_sixteen() {
        let mut world = World::default();
        for i in 0..20 {
            light_at(&mut world, i as f64, Some(Lighting2D { mode: Lit2D::Yes, ..Default::default() }));
        }
        for i in 0..3 {
            light_at(&mut world, i as f64, Some(Lighting2D { mode: Lit2D::No, ..Default::default() }));
        }
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.two_d.count, 16, "the 2D side fills up");
        assert_eq!(s.three_d.count, 3, "…and the 3D side still gets all of its own");
    }
}
