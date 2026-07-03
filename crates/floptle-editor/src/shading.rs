//! Gathering World state into renderer uniforms, once per render site: blob
//! materials, point lights, the Lighting node's shadow knobs, the proxy shadow
//! occluders harvested from collider shapes, the Skybox node, and the
//! PostProcess node. Pure read-the-World functions — no GPU types in here
//! beyond the plain uniform arrays they return.

use floptle_core::math::{DVec3, Mat3, Vec3};
use floptle_core::{Entity, Light, Material, Matter, World};
use floptle_render::MaterialParams;

/// Convert a core [`Material`] into the renderer's per-instance [`MaterialParams`].
pub(crate) fn material_params(m: &Material) -> MaterialParams {
    MaterialParams {
        color: m.color,
        emissive: m.emissive,
        emissive_strength: m.emissive_strength,
        specular: m.specular,
        shininess: m.shininess,
        specular_strength: m.specular_strength,
        rim: m.rim,
        rim_strength: m.rim_strength,
        unlit: m.unlit,
        ambient: m.ambient,
        alpha: m.alpha,
    }
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
    let mut pos = [[0.0f32; 4]; 16];
    let mut col = [[0.0f32; 4]; 16];
    let mut n = 0usize;
    for (e, m) in world.query::<Matter>() {
        if let Matter::PointLight { color, intensity, range } = m {
            if n >= 16 {
                break;
            }
            let wp = floptle_core::world_transform(world, e).translation;
            let c = (wp - cam_world).as_vec3();
            pos[n] = [c.x, c.y, c.z, range.max(0.0001)];
            col[n] = [color[0] * intensity, color[1] * intensity, color[2] * intensity, 0.0];
            n += 1;
        }
    }
    ([n as f32, 0.0, 0.0, 0.0], pos, col)
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
    let found = world.query::<Matter>().find_map(|(e, m)| match m {
        Matter::Skybox { color, size, texture, tint } => {
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
    };
    for (_, m) in world.query::<Matter>() {
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
            };
            let ao_p =
                if *ao == AoMode::Sdf { [1.0, *ao_strength, *ao_radius, 0.0] } else { [0.0; 4] };
            return (s, ao_p);
        }
    }
    (off, [0.0; 4])
}
