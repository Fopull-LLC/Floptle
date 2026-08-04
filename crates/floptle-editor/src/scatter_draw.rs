//! Drawing scatter sources (`floptle/0036`): seed → instances → one instanced
//! draw per (mesh, LOD band).
//!
//! Nothing here creates a scene node, which is the entire point. The old answer
//! was `createNode` per trunk segment — 4–14 nodes a plant, ninety plants of
//! budget, a forest that was a bubble fifty-five metres across. These instances
//! cost a `mat4` each and never touch the ECS.
//!
//! ## Props sit on the ground, not at the region's nominal height
//!
//! A scatter source says *where in the world* things grow; the terrain says
//! *how high the ground is there*. Resolving that needs a query per instance,
//! so the answers are cached per chunk: a chunk is only re-dropped when it
//! first comes into range, and a chunk that has been resolved is a plain array
//! lookup from then on. Without the cache this is a raycast per prop per frame,
//! which is the difference between scatter being usable and being a
//! demonstration.

use std::collections::HashMap;

use floptle_core::math::{DVec3, Mat4, Vec3};
use floptle_core::scatter::{self, Align, ChunkKey, Instance, ScatterSource};
use floptle_render::{instance_of_mat, InstanceRaw, MaterialParams, MeshId, TexId};

/// One chunk's instances, already dropped onto the surface.
pub(crate) struct ResolvedChunk {
    pub instances: Vec<Instance>,
    /// The removal-set size the chunk was resolved at. A cut prop has to
    /// disappear THIS frame, not when the chunk next streams — so a change
    /// here invalidates the cache.
    pub removed_len: usize,
}

/// Per-source chunk caches, keyed by `(source id, chunk)`.
#[derive(Default)]
pub(crate) struct ScatterCache {
    chunks: HashMap<(u32, ChunkKey), ResolvedChunk>,
}

impl ScatterCache {
    /// Forget everything — a scene switch, or a terrain edit that moved the
    /// ground out from under a whole region.
    pub(crate) fn clear(&mut self) {
        self.chunks.clear();
    }

    /// Forget the chunks within `radius` of `p`: the ground there changed, so
    /// the props standing on it are at the old height. This is what "digging
    /// the ground out from under one drops or despawns it" is made of.
    pub(crate) fn invalidate_near(&mut self, sources: &[ScatterSource], p: DVec3, radius: f64) {
        for src in sources {
            for key in scatter::chunks_near(src, p, radius) {
                self.chunks.remove(&(src.id, key));
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.chunks.len()
    }
}

/// Drop an instance onto the real surface under it.
///
/// `ground` casts a ray and answers the distance to the first surface, if any.
/// A prop with no ground under it is DROPPED rather than left floating at the
/// region's nominal height — a tree hanging in the air over a canyon reads as a
/// bug, and a missing tree reads as a canyon.
fn settle(
    src: &ScatterSource,
    mut inst: Instance,
    ground: &mut impl FnMut(DVec3, Vec3, f32) -> Option<(f32, Vec3)>,
) -> Option<Instance> {
    // Start above and cast down along the region's own up — which on a planet
    // is radial, so this works at the equator and at the pole alike.
    const LIFT: f32 = 60.0;
    let up = inst.up;
    let from = inst.pos + up.as_dvec3() * LIFT as f64;
    let (dist, normal) = ground(from, -up, LIFT * 2.5)?;
    inst.pos = from - up.as_dvec3() * dist as f64;
    if src.align == Align::Surface {
        // The REAL normal, not the region's idealised one: a tree on a hillside
        // should lean with the hill.
        inst.up = normal;
    }
    Some(inst)
}

/// One drawable piece of a scatter prototype: a mesh, its texture, and where it
/// sits WITHIN the prop.
///
/// A `.glb` is one part at identity. A prefab is however many Mesh nodes it
/// holds, each at its authored place — which is what lets a plant be a trunk
/// and three fronds and still cost one instanced draw per piece rather than a
/// scene node per piece (`floptle/0065`).
pub(crate) type Part = (MeshId, Option<TexId>, Mat4);

/// Everything visible from `eye`, packed as instanced draws.
///
/// Returns `(mesh, texture, raw)` triples in the same shape `draw_scene` takes,
/// so scatter is drawn by the ordinary raster path and gets the ordinary
/// lighting, fog and shadows — including the underwater fog, which is how a
/// forest at a shoreline goes murky at the same rate as the ground it stands on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_instances(
    cache: &mut ScatterCache,
    sources: &[ScatterSource],
    eye: DVec3,
    mesh_of: &mut impl FnMut(&str) -> Option<Vec<Part>>,
    ground: &mut impl FnMut(DVec3, Vec3, f32) -> Option<(f32, Vec3)>,
    material: &MaterialParams,
    budget: usize,
    out: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) {
    for src in sources {
        let range = src.range();
        if range <= 0.0 || src.bands.is_empty() {
            continue;
        }
        for key in scatter::chunks_near(src, eye, range as f64) {
            let ck = (src.id, key);
            // Re-resolve when the chunk is new, or when something has been cut
            // since it was resolved.
            let stale = cache
                .chunks
                .get(&ck)
                .is_none_or(|c| c.removed_len != src.removed.len());
            if stale {
                let instances = scatter::chunk_instances(src, key)
                    .into_iter()
                    .filter_map(|i| settle(src, i, ground))
                    .collect();
                cache
                    .chunks
                    .insert(ck, ResolvedChunk { instances, removed_len: src.removed.len() });
            }
            let Some(chunk) = cache.chunks.get(&ck) else { continue };
            for inst in &chunk.instances {
                if out.len() >= budget {
                    return;
                }
                let d = (inst.pos - eye).length() as f32;
                let Some((band, blend)) = scatter::band_at(src, d) else { continue };
                let rot = inst.rotation(src.align);
                let model = Mat4::from_scale_rotation_translation(
                    Vec3::splat(inst.scale),
                    rot,
                    (inst.pos - eye).as_vec3(),
                );
                // Mid-fade draws BOTH bands, cross-dissolved. Drawing one and
                // switching is what a pop is; two half-opaque props for a few
                // metres of walking is what nobody notices.
                let mut push = |b: usize, alpha: f32| {
                    let Some(asset) = src.bands.get(b) else { return };
                    let Some(parts) = mesh_of(&asset.asset) else { return };
                    let mut mp = *material;
                    mp.alpha = alpha;
                    // The per-instance roll rides the albedo, so a game can
                    // give one species a spread of shades without a material
                    // per plant. Neutral at param 0.5.
                    let tintf = 0.85 + 0.3 * inst.param;
                    mp.color = [mp.color[0] * tintf, mp.color[1] * tintf, mp.color[2] * tintf];
                    // A prototype may be several parts (a prefab's nodes), each
                    // with its own mesh, texture and place within the prop. They
                    // are still ONE instanced draw each — the prefab is resolved
                    // to this list once, not instantiated per prop.
                    for &(mesh, tex, local) in &parts {
                        out.push((mesh, tex, instance_of_mat(model * local, &mp)));
                    }
                };
                // At the very ends of a fade window one half rounds to nothing.
                // Draw the OTHER half fully opaque rather than at 0.995: a
                // barely-transparent prop still pays for the blended pass —
                // depth-sorted, no depth write — for a difference no eye can
                // see, and every prop in a forest paying that is the cost.
                const FADE_EPS: f32 = 0.01;
                match blend {
                    b if b <= FADE_EPS => push(band, 1.0),
                    b if b >= 1.0 - FADE_EPS => push(band + 1, 1.0),
                    b => {
                        push(band, 1.0 - b);
                        push(band + 1, b);
                    }
                }
            }
        }
    }
}

impl crate::Editor {
    /// A scatter prototype resolved to its drawable parts, baked ONCE and kept.
    ///
    /// A mesh file is one part at identity — what scatter has always drawn. A
    /// **prefab** is each of its `Mesh` nodes at its authored place inside the
    /// prop, which is the point of `floptle/0065`: a game whose props are
    /// generated (Solar's plants are a trunk and a handful of fronds, assembled
    /// by a script) had nothing to hand scatter, because scatter took a file
    /// path and a plant is not a file.
    ///
    /// Baked at DECLARE time, not per instance: editing the prefab while the
    /// game runs does not re-bake it, exactly as editing a `.glb` mid-run does
    /// not re-import it.
    ///
    /// A prefab node the bake cannot use — anything that is not a Mesh with a
    /// model — is skipped, and a prototype that yields nothing at all says so
    /// once rather than drawing nothing in silence.
    pub(crate) fn bake_scatter_prototypes(&mut self) {
        let assets: Vec<String> = self
            .script_host
            .scatter_sources()
            .iter()
            .flat_map(|s| s.bands.iter().map(|b| b.asset.clone()))
            .collect();
        for a in assets {
            if !self.scatter_protos.contains_key(&a) {
                let _ = self.scatter_prototype(&a);
            }
        }
    }

    pub(crate) fn scatter_prototype(&mut self, asset: &str) -> Option<Vec<Part>> {
        if let Some(cached) = self.scatter_protos.get(asset) {
            return (!cached.is_empty()).then(|| cached.clone());
        }
        let parts = self.bake_scatter_prototype(asset);
        if parts.is_empty() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "scatter: `{asset}` has nothing to draw — a mesh file, or a prefab \
                     containing at least one Mesh node, is what a scatter prototype is"
                ),
                None,
            );
        }
        self.scatter_protos.insert(asset.to_string(), parts.clone());
        (!parts.is_empty()).then_some(parts)
    }

    fn bake_scatter_prototype(&mut self, asset: &str) -> Vec<Part> {
        // A prefab first, since a project may legitimately hold both names.
        if let Some(path) = self.resolve_prefab_request(asset)
            && let Ok(docs) = crate::prefab::load_prefab_docs(&path)
        {
            let mut out = Vec::new();
            for (i, d) in docs.iter().enumerate() {
                let floptle_scene::MatterDoc::Mesh { asset_path } = &d.matter else { continue };
                if !self.import_model(asset_path) {
                    continue;
                }
                let Some(a) = self.mesh_registry.get(asset_path) else { continue };
                let parts: Vec<MeshId> = a.parts.clone();
                let local = prefab_local(&docs, i);
                out.extend(parts.into_iter().map(|m| (m, None, local)));
            }
            return out;
        }
        if !self.import_model(asset) {
            return Vec::new();
        }
        self.mesh_registry
            .get(asset)
            .map(|a| a.parts.iter().map(|&m| (m, None, Mat4::IDENTITY)).collect())
            .unwrap_or_default()
    }
}

/// A prefab node's transform relative to the prefab's ROOT — its own, composed
/// with every ancestor's, so a frond attached to a trunk lands on the trunk.
fn prefab_local(docs: &[floptle_scene::NodeDoc], i: usize) -> Mat4 {
    let mut m = Mat4::IDENTITY;
    let mut cur = Some(i);
    // Bounded by the list length: a malformed prefab with a parent cycle must
    // not hang the frame it is first drawn in.
    for _ in 0..=docs.len() {
        let Some(idx) = cur else { break };
        let Some(d) = docs.get(idx) else { break };
        let t = &d.transform;
        m = Mat4::from_scale_rotation_translation(
            Vec3::from(t.scale),
            floptle_core::math::Quat::from_array(t.rotation),
            Vec3::new(t.translation[0] as f32, t.translation[1] as f32, t.translation[2] as f32),
        ) * m;
        cur = d.parent.filter(|p| *p != idx);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::scatter::{Band, Region};

    fn src() -> ScatterSource {
        ScatterSource {
            id: 1,
            seed: 7,
            region: Region::Ground { center: DVec3::ZERO, half: [100.0, 100.0] },
            per_chunk: 8,
            chunk: 16.0,
            align: Align::Surface,
            scale: (1.0, 1.0),
            bands: vec![Band { asset: "a".into(), distance: 50.0 }],
            fade: 0.0,
            density: None,
            removed: Default::default(),
        }
    }

    /// A prototype of several parts draws one instance PER PART, each at its
    /// place within the prop and all sharing the prop's transform
    /// (`floptle/0065`). That is what lets a generated plant — a trunk and
    /// three fronds — be scattered without a scene node per frond.
    #[test]
    fn a_multi_part_prototype_draws_each_part_at_its_own_place() {
        let s = src();
        // Flat ground under everything, so props settle rather than being
        // dropped for having nothing to stand on.
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        // Two parts: one at the origin, one a metre up.
        let up = Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0));
        let mut mesh = |_: &str| {
            Some(vec![(MeshId(0), None, Mat4::IDENTITY), (MeshId(1), None, up)])
        };
        let mut one = Vec::new();
        build_instances(
            &mut ScatterCache::default(),
            std::slice::from_ref(&s),
            DVec3::ZERO,
            &mut |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]),
            &mut ground,
            &MaterialParams::flat([1.0; 3]),
            10_000,
            &mut one,
        );
        let mut two = Vec::new();
        build_instances(
            &mut ScatterCache::default(),
            &[s],
            DVec3::ZERO,
            &mut mesh,
            &mut ground,
            &MaterialParams::flat([1.0; 3]),
            10_000,
            &mut two,
        );
        assert!(!one.is_empty());
        assert_eq!(two.len(), one.len() * 2, "one draw per part per prop");
        assert!(
            two.iter().any(|(m, ..)| *m == MeshId(1)),
            "the second part is actually drawn"
        );
        // The parts of one prop are a metre apart, not on top of each other.
        let ys: Vec<f32> = two.iter().take(2).map(|(_, _, r)| r.model[3][1]).collect();
        assert!((ys[0] - ys[1]).abs() > 0.5, "the offset part moved: {ys:?}");
    }

    /// A prop with no ground under it is DROPPED, not left floating at the
    /// region's nominal height. A tree hanging in the air over a canyon reads
    /// as a bug; a missing tree reads as a canyon.
    #[test]
    fn a_prop_with_no_ground_under_it_is_dropped() {
        let s = src();
        let inst = scatter::chunk_instances(&s, (0, 0))[0];
        let mut nothing = |_: DVec3, _: Vec3, _: f32| None;
        assert!(settle(&s, inst, &mut nothing).is_none());
    }

    /// …and one that DOES have ground lands on it, taking the ground's real
    /// normal so a hillside's trees lean with the hill.
    #[test]
    fn a_settled_prop_takes_the_grounds_height_and_normal() {
        let s = src();
        let inst = scatter::chunk_instances(&s, (0, 0))[0];
        let slope = Vec3::new(0.0, 0.8, 0.6).normalize();
        // Ground 12 m below the cast start, i.e. 48 m below the nominal height.
        let mut hit = |_: DVec3, _: Vec3, _: f32| Some((12.0f32, slope));
        let out = settle(&s, inst, &mut hit).expect("there was ground");
        assert!((out.pos.y - (inst.pos.y + 60.0 - 12.0)).abs() < 1e-4, "landed at the wrong height");
        assert!(out.up.dot(slope) > 0.999, "did not take the ground's normal");
    }

    /// Cutting a prop must show THIS frame, not when the chunk next streams —
    /// so the cache is keyed on the removal set having changed, not on time.
    #[test]
    fn cutting_a_prop_invalidates_the_cache_immediately() {
        let mut cache = ScatterCache::default();
        let mut s = src();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut ground, &mat,
            10_000, &mut out,
        );
        let before = out.len();
        assert!(before > 0, "nothing was drawn");

        // Cut one that is definitely in range.
        let victim = scatter::chunk_instances(&s, (0, 0))[0].id;
        s.removed.insert(victim);
        out.clear();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut ground, &mat,
            10_000, &mut out,
        );
        assert_eq!(out.len(), before - 1, "the cut prop was still drawn");
    }

    /// The draw budget is a HARD stop. A source with a silly density must cost
    /// a frame-rate dip, never a frame that never ends.
    #[test]
    fn the_instance_budget_is_never_exceeded() {
        let mut cache = ScatterCache::default();
        let mut s = src();
        s.per_chunk = 512;
        s.bands = vec![Band { asset: "a".into(), distance: 400.0 }];
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut ground, &mat,
            64, &mut out,
        );
        assert_eq!(out.len(), 64);
    }

    /// Mid-fade draws BOTH bands. Drawing one and switching IS the pop.
    #[test]
    fn a_band_boundary_draws_both_bands_cross_dissolved() {
        let mut cache = ScatterCache::default();
        let mut s = src();
        s.per_chunk = 1;
        s.bands = vec![
            Band { asset: "near".into(), distance: 40.0 },
            Band { asset: "far".into(), distance: 200.0 },
        ];
        s.fade = 10.0;
        let mut asked: Vec<String> = Vec::new();
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        let mut out = Vec::new();
        {
            let mut mesh = |a: &str| {
                asked.push(a.to_string());
                Some(vec![(MeshId(0), None, Mat4::IDENTITY)])
            };
            // Stand ~35 m away: inside the first band's 10 m fade window.
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::new(0.0, 0.0, 0.0), &mut mesh,
                &mut ground, &mat, 10_000, &mut out,
            );
        }
        let both = asked.iter().any(|a| a == "near") && asked.iter().any(|a| a == "far");
        assert!(both, "no boundary was cross-dissolved; asked for {asked:?}");
        // The two halves of a dissolve sum to one, so the pair is never brighter
        // or dimmer than the single prop it replaces. Everything NOT mid-fade is
        // a plain opaque draw, so the fractional alphas are exactly the halves.
        let mut fades: Vec<f32> =
            out.iter().map(|(_, _, r)| r.color[3]).filter(|a| *a < 0.999).collect();
        assert!(!fades.is_empty(), "nothing was mid-fade");
        assert_eq!(fades.len() % 2, 0, "a dissolve half with no partner");
        while let (Some(a), Some(b)) = (fades.pop(), fades.pop()) {
            assert!((a + b - 1.0).abs() < 0.02, "cross-fade sums to {}, not 1", a + b);
        }
    }
}
