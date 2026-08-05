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

/// Collect up to 16 placeable point lights from the world into the camera-relative
/// uniform arrays (xyz pos + range; rgb = color×intensity) for the raster + raymarch
/// passes. Returns (count_vec4, positions, colors).
pub(crate) fn collect_point_lights(
    world: &World,
    cam_world: DVec3,
) -> ([f32; 4], [[f32; 4]; 16], [[f32; 4]; 16]) {
    let (n, pos, col, _) = split_point_lights(world, cam_world, &[], false).three_d;
    ([n as f32, 0.0, 0.0, 0.0], pos, col)
}

/// One side of the light split: how many, where, what colour, and — for the 2D
/// side — which sorting layers each one reaches.
pub(crate) type LightSlots = (usize, [[f32; 4]; 16], [[f32; 4]; 16], [[u32; 4]; 16]);

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
        let Matter::PointLight { color, intensity, range } = m else { continue };
        // A light turned off does not take a slot. Keeping N lights and parking
        // the spare ones at zero is the standard way to pool a capped resource —
        // and scripts cannot create a PointLight, so it is the ONLY way. A
        // parked light holding a slot would mean a pool exhausts the budget and
        // lights nothing (`floptle/0116`).
        if *intensity <= 0.0 || *range <= 0.0 {
            continue;
        }
        let lit = world.get::<floptle_core::Lighting2D>(e).cloned().unwrap_or_default();
        let (is_2d, _) = floptle_core::resolve_2d(lit.mode, facts);
        let wp = floptle_core::world_transform(world, e).translation;
        let c = (wp - cam_world).as_vec3();
        let side = if is_2d { &mut two } else { &mut three };
        side.push(Candidate {
            order: e.index(),
            score: contribution(c.length(), *range, *color, *intensity),
            pos: [c.x, c.y, c.z, range.max(0.0001)],
            color: [color[0] * intensity, color[1] * intensity, color[2] * intensity, 0.0],
            mask: layer_mask(&lit, sorting_names),
        });
    }
    SplitLights { three_d: fill(three), two_d: fill(two) }
}

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
    let mut out: LightSlots = (lights.len(), [[0.0; 4]; 16], [[0.0; 4]; 16], [[0; 4]; 16]);
    for (i, l) in lights.iter().enumerate() {
        out.1[i] = l.pos;
        out.2[i] = l.color;
        out.3[i] = l.mask;
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

/// The depth-fog uniforms for the Lighting node: `(fog_color, fog_params)` where
/// `fog_params = [start, end, on, dither_mode]` and the spare `fog_color.w` carries
/// the effective dither strength (0 = off). Fed to the raymarch/raster field globals
/// AND the particle globals so meshes, matter, terrain and particles fog together —
/// and band-break identically. Packing into the two already-spare `.w` lanes keeps
/// the uniform layout (and its byte-sync with the WGSL structs) unchanged.
/// Volumetric-fog uniform lanes (`vol_fog_a/b`): densities/heights straight off
/// the Lighting node, `time` drifting the noise, and the camera's WORLD height
/// so the shader can map camera-relative positions back to world y.
pub(crate) fn vol_fog_uniforms(l: &Light, time: f32, cam_y: f32) -> ([f32; 4], [f32; 4]) {
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

/// [`fog_uniforms`], overridden while the camera is under water.
///
/// The scene's own fog is REPLACED rather than added to: underwater is a
/// different medium, not the same air with more of it. Going through the one
/// fog channel every draw path already reads is what makes meshes, terrain, SDF
/// matter and particles go murky *together* — a separate underwater pass would
/// have had to be taught about each of them, and would have missed one.
pub(crate) fn fog_uniforms_at(
    l: &Light,
    world: &floptle_core::World,
    cam: floptle_core::math::DVec3,
) -> ([f32; 4], [f32; 4]) {
    match underwater_at(world, cam) {
        Some((tint, vis)) => (
            [tint[0], tint[1], tint[2], if l.fog_dither { l.fog_dither_strength.clamp(0.0, 1.0) } else { 0.0 }],
            // Start close to the eye: water attenuates from the first
            // centimetre, and a start distance would give you a crisp bubble of
            // clear water around the camera that moves with you.
            [vis * 0.05, vis, 1.0, 0.0],
        ),
        None => fog_uniforms(l),
    }
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
pub(crate) fn post_process_uniforms(world: &floptle_core::World) -> (floptle_render::PostSettings, [f32; 4]) {
    use floptle_core::AoMode;
    let off = floptle_render::PostSettings {
        bloom: false,
        bloom_threshold: 1.0,
        bloom_intensity: 0.7,
        vignette: false,
        vignette_strength: 0.5,
        vignette_radius: 0.7,
        ssao: false,
        ssao_strength: 0.7,
        ssao_radius: 0.5,
        posterize_bands: 0,
        posterize_dither: false,
        color_filter: 0,
        color_filter_strength: 1.0,
        simulate_deficiency: false,
    };
    for (e, m) in world.query::<Matter>() {
        // Same rule as the skybox above: a disabled chain is not the scene's
        // chain, so it is skipped rather than returning `off` — which would let
        // a base scene's sleeping node veto the layer that replaced it.
        if world.get::<floptle_core::Disabled>(e).is_some() {
            continue;
        }
        if let Matter::PostProcess {
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
                color_filter: 0,
                color_filter_strength: 1.0,
                simulate_deficiency: false,
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
        world.insert(e, Matter::PointLight { color: [1.0, 0.5, 0.25], intensity: 2.0, range: 8.0 });
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
        assert_eq!((s.two_d.0, s.three_d.0), (1, 1));
        // Perspective scene: both are 3D and nothing is on the 2D side.
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!((s.two_d.0, s.three_d.0), (0, 2));
    }

    /// A scene with no 2D lights in it must hand the 3D shader exactly what it
    /// always got — same count, same slots, same numbers.
    #[test]
    fn a_scene_with_no_2d_lights_shades_as_it_always_did() {
        let mut world = World::default();
        light_at(&mut world, 3.0, None);
        light_at(&mut world, -2.0, None);
        let (count, pos, col) = collect_point_lights(&world, DVec3::new(1.0, 0.0, 0.0));
        let s = split_point_lights(&world, DVec3::new(1.0, 0.0, 0.0), &layers(), false);
        assert_eq!(count[0] as usize, s.three_d.0);
        assert_eq!(pos, s.three_d.1);
        assert_eq!(col, s.three_d.2);
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
        light_at(&mut world, 0.0, Some(Lighting2D { mode: Lit2D::Yes, layers: vec![] }));
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.two_d.0, 1);
        assert_eq!(s.two_d.3[0], [!0u32; 4], "the mask must be all-ones, never zero");
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
            }),
        );
        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.two_d.3[0], [1 << 2, 0, 0, 0], "only Characters, which is rank 2");
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
            Some(Lighting2D { mode: Lit2D::Yes, layers: vec!["layer35".into()] }),
        );
        let s = split_point_lights(&world, DVec3::ZERO, &names, false);
        assert_eq!(s.two_d.3[0], [0, 1 << 3, 0, 0], "rank 35 is bit 3 of word 1");
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
            Matter::PointLight { color: [1.0; 3], intensity: 0.0, range: 8.0 },
        );
        let spent = light_at(&mut world, 1.0, None);
        world.insert(spent, Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 0.0 });
        light_at(&mut world, 2.0, None); // the only one actually lighting anything

        let s = split_point_lights(&world, DVec3::ZERO, &layers(), false);
        assert_eq!(s.three_d.0, 1, "a parked light took a slot");
        assert_eq!(s.three_d.1[0][0], 2.0, "…and the wrong one survived");
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
                (0..s.three_d.0).map(|i| s.three_d.1[i][0].round() as i32).collect();
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
        assert_eq!(s.two_d.0, 16, "the 2D side fills up");
        assert_eq!(s.three_d.0, 3, "…and the 3D side still gets all of its own");
    }
}
