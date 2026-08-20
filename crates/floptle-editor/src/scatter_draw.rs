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

/// How many props may be dropped onto the ground in one frame, across every
/// source (`floptle/0071`).
///
/// Settling is a raycast per prop, cached per chunk — so the FIRST frame a
/// chunk comes into range pays for all of its props at once. A third-person
/// camera swings the eye several metres just from looking around, which crosses
/// chunk boundaries, which used to drag thousands of fresh raycasts into a
/// single frame. The report was "it freezes more as I'm looking around", and
/// that is exactly what that was.
///
/// The cost of the cap is that ground arriving all at once fills in over a few
/// frames, furthest last. The cost of not having it is a frame that stops.
const SETTLE_BUDGET: usize = 512;

/// Which chunks one source is resident in, and where the eye was standing when
/// that was worked out.
struct Sweep {
    /// The chunk the eye was in. The key set changes when THIS changes, not
    /// when the frame advances. `None` = never swept.
    at: Option<ChunkKey>,
    /// Nearest first, as `chunks_near` returns them.
    keys: Vec<ChunkKey>,
}

/// Per-source chunk caches, keyed by `(source id, chunk)`.
#[derive(Default)]
pub(crate) struct ScatterCache {
    chunks: HashMap<(u32, ChunkKey), ResolvedChunk>,
    /// The resident key list per source — swept when the eye crosses a chunk
    /// boundary, not once a frame (`floptle/0071`).
    sweeps: HashMap<u32, Sweep>,
}

impl ScatterCache {
    /// Forget everything — a scene switch, or a terrain edit that moved the
    /// ground out from under a whole region.
    pub(crate) fn clear(&mut self) {
        self.chunks.clear();
        self.sweeps.clear();
    }

    /// Forget the chunks within `radius` of `p`: the ground there changed, so
    /// the props standing on it are at the old height. This is what "digging
    /// the ground out from under one drops or despawns it" is made of.
    pub(crate) fn invalidate_near(&mut self, sources: &[ScatterSource], p: DVec3, radius: f64) {
        for src in sources {
            // `p` is where the ground changed, in the world. The chunks are
            // keyed in the source's own frame (`floptle/0073`).
            let pl = src.frame.to_local(p);
            for key in scatter::chunks_near(src, pl, radius) {
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
/// The instance stays in the source's own frame; only the RAY goes out to the
/// world and only the normal comes back (`floptle/0073`). That is what lets a
/// settled chunk survive its planet moving — the cached answer never mentioned
/// the world, so the world moving cannot invalidate it.
fn settle(
    src: &ScatterSource,
    mut inst: Instance,
    ground: &mut impl FnMut(DVec3, Vec3, f32) -> Option<(f32, Vec3)>,
) -> Option<Instance> {
    // Start above and cast down along the region's own up — which on a planet
    // is radial, so this works at the equator and at the pole alike.
    const LIFT: f32 = 60.0;
    let f = &src.frame;
    let up = inst.up;
    let from = inst.pos + up.as_dvec3() * LIFT as f64;
    let (dist, normal) =
        ground(f.to_world(from), f.dir_to_world(-up), LIFT * 2.5)?;
    inst.pos = from - up.as_dvec3() * dist as f64;
    if src.align == Align::Surface {
        // The REAL normal, not the region's idealised one: a tree on a hillside
        // should lean with the hill.
        inst.up = f.dir_to_local(normal);
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
    radius_of: &mut impl FnMut(&str) -> Option<f32>,
    ground: &mut impl FnMut(DVec3, Vec3, f32) -> Option<(f32, Vec3)>,
    material: &MaterialParams,
    frustum: &floptle_render::Frustum,
    budget: usize,
    out: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) {
    // Disjoint borrows: the sweep list and the resolved chunks are read and
    // written in the same loop.
    let ScatterCache { chunks: resolved, sweeps } = cache;
    sweeps.retain(|id, _| sources.iter().any(|s| s.id == *id));
    // Shared across sources, because it is a FRAME budget. A source that eats
    // it all is fully resident afterwards and stops asking, so the next source
    // gets the next frame's — it converges rather than starving anyone.
    let mut settled = 0usize;
    for src in sources {
        let range = src.range();
        if range <= 0.0 || src.bands.is_empty() {
            continue;
        }
        // The largest prop this source can draw, at scale 1 — the whole source's
        // radius, taken once rather than per band per prop. Conservative on
        // purpose: an LOD band whose stand-in is smaller than the near mesh must
        // not be culled by the near mesh's own tighter sphere.
        //
        // `None` — a prototype whose bounds nothing measured — means this source
        // is never direction-culled. Its props still cull by distance, as they
        // always did.
        let prop_radius = src
            .bands
            .iter()
            .map(|b| radius_of(&b.asset))
            .try_fold(0.0f32, |acc, r| r.map(|r| acc.max(r)))
            .filter(|r| r.is_finite() && *r > 0.0);
        // The key set changes when the eye crosses a chunk boundary, not when
        // the frame advances (`floptle/0071`). Standing still, or walking
        // within one chunk, this is a hash lookup — it used to be a square
        // sweep, allocated and thrown away sixty times a second.
        // Everything below happens in the SOURCE'S OWN FRAME (`floptle/0073`).
        // The eye comes to the region rather than the region going to the world,
        // so a body that orbits at 99 units/s changes exactly one number here —
        // and no id, no local position, no settled height and no cached chunk.
        let eye_local = src.frame.to_local(eye);
        let at = scatter::eye_chunk(src, eye_local);
        let sweep = sweeps.entry(src.id).or_insert_with(|| Sweep { at: None, keys: Vec::new() });
        if sweep.at != Some(at) {
            sweep.at = Some(at);
            sweep.keys = scatter::chunks_near(src, eye_local, range as f64);
        }
        // Nearest first, so the draw budget below cuts the horizon rather than
        // your feet, and so streaming spends its first frames on what you can
        // actually see.
        for &key in &sweep.keys {
            let ck = (src.id, key);
            let known = resolved.get(&ck);
            // Something has been cut since this chunk was resolved: redo it NOW,
            // budget or no budget. That is one chunk, it is the player's own
            // doing, and a prop that survives the swing that felled it is a bug.
            let cut = known.is_some_and(|c| c.removed_len != src.removed.len());
            if known.is_none() || cut {
                // Arriving is the other case, and it is thousands of props at
                // once. Let them come over the next few frames instead.
                if known.is_none() && settled >= SETTLE_BUDGET {
                    continue;
                }
                let rolled = scatter::chunk_instances(src, key);
                settled += rolled.len();
                let instances =
                    rolled.into_iter().filter_map(|i| settle(src, i, ground)).collect();
                resolved.insert(ck, ResolvedChunk { instances, removed_len: src.removed.len() });
            }
            let Some(chunk) = resolved.get(&ck) else { continue };
            for inst in &chunk.instances {
                if out.len() >= budget {
                    return;
                }
                // Distance is measured in the source's frame, which is a rigid
                // transform of the world's — so LOD bands are untouched by where
                // the body happens to be.
                let d = (inst.pos - eye_local).length() as f32;
                let Some((band, blend)) = scatter::band_at(src, d) else { continue };
                // …and only HERE does the world get involved: the instance's
                // place in its region, carried out to wherever that region is.
                let rot = src.frame.rot * inst.rotation(src.align);
                let centre = (src.frame.to_world(inst.pos) - eye).as_vec3();
                // …and only then, is it on screen? Distance was the only test a
                // field ever applied, so a full disc submitted everything behind
                // you (`floptle/0075`). AFTER the band test, which is the cheaper
                // one and also the one that rejects most.
                if let Some(pr) = prop_radius
                    && !frustum
                        .contains_sphere(centre, floptle_render::cull::scale_radius(pr, Vec3::splat(inst.scale)))
                {
                    continue;
                }
                let model =
                    Mat4::from_scale_rotation_translation(Vec3::splat(inst.scale), rot, centre);
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
        // A "nothing baked" answer taken without a GPU is not about the asset,
        // so it stops being the answer the moment there is one to bake on.
        if self.gpu.is_some() && !self.scatter_protos_gpuless.is_empty() {
            for a in std::mem::take(&mut self.scatter_protos_gpuless) {
                self.scatter_protos.remove(&a);
            }
        }
        if let Some(cached) = self.scatter_protos.get(asset) {
            return (!cached.is_empty()).then(|| cached.clone());
        }
        let parts = self.bake_scatter_prototype(asset);
        if parts.is_empty() {
            // **"Nothing to draw" has to be a fact about the ASSET.**
            //
            // A bake registers meshes on the GPU, so with no GPU it comes back
            // empty whatever the asset is — and `floptle run` has no GPU and
            // draws nothing at all. Printing this there said a project's props
            // were broken when they were fine, several times, in every single
            // headless run: exactly the kind of warning that teaches a reader to
            // skip the warning block (`floptle/0157`).
            //
            // A missing file is still worth saying and is knowable either way,
            // so that half is unconditional and the rest waits for a process
            // that could actually have drawn it.
            if !self.scatter_asset_exists(asset) {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "scatter: `{asset}` has nothing to draw — no prefab and no model \
                         file of that name. A scatter prototype is a mesh file, or a prefab \
                         containing at least one Mesh node."
                    ),
                    None,
                );
            } else if self.gpu.is_some() {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "scatter: `{asset}` has nothing to draw — a mesh file, or a prefab \
                         containing at least one Mesh node, is what a scatter prototype is"
                    ),
                    None,
                );
            } else {
                // Cached, but remembered as a NON-answer: an editor that later
                // gets a GPU re-bakes it (see `scatter_protos_gpuless`). Not
                // caching at all was the obvious move and the wrong one — a
                // headless `floptle run` bakes inside every one of its thousands
                // of steps, so an un-cached miss re-reads and re-parses the
                // prefab thousands of times over.
                self.scatter_protos_gpuless.insert(asset.to_string());
                self.scatter_protos.insert(asset.to_string(), parts);
                return None;
            }
        }
        self.scatter_protos.insert(asset.to_string(), parts.clone());
        (!parts.is_empty()).then_some(parts)
    }

    /// Is there anything on disk this scatter asset could name — a prefab, or a
    /// model file? The half of "nothing to draw" that is true with or without a
    /// GPU.
    fn scatter_asset_exists(&mut self, asset: &str) -> bool {
        if self.resolve_prefab_request(asset).is_some() {
            return true;
        }
        crate::project::resolve_asset_path(&self.project_root, asset).exists()
    }

    fn bake_scatter_prototype(&mut self, asset: &str) -> Vec<Part> {
        // A prefab first, since a project may legitimately hold both names.
        if let Some(path) = self.resolve_prefab_request(asset)
            && let Ok(docs) = crate::prefab::load_prefab_docs(&path)
        {
            let mut out = Vec::new();
            // The prop's bounding radius, accumulated as its pieces are found:
            // a frond three metres up the trunk reaches its own radius further
            // than the trunk does (`floptle/0075`).
            let mut radius = 0.0f32;
            for (i, d) in docs.iter().enumerate() {
                let floptle_scene::MatterDoc::Mesh { asset_path } = &d.matter else { continue };
                if !self.import_model(asset_path) {
                    continue;
                }
                let Some(a) = self.mesh_registry.get(asset_path) else { continue };
                let parts: Vec<MeshId> = a.parts.clone();
                let local = prefab_local(&docs, i);
                let (scale, _, offset) = local.to_scale_rotation_translation();
                radius = radius.max(
                    offset.length()
                        + floptle_render::cull::radius_from_longest_edge(a.size, scale),
                );
                out.extend(parts.into_iter().map(|m| (m, None, local)));
            }
            if radius.is_finite() && radius > 0.0 {
                self.scatter_proto_radius.insert(asset.to_string(), radius);
            }
            return out;
        }
        if !self.import_model(asset) {
            return Vec::new();
        }
        let Some(a) = self.mesh_registry.get(asset) else { return Vec::new() };
        let radius = floptle_render::cull::radius_from_longest_edge(a.size, Vec3::ONE);
        let parts: Vec<Part> = a.parts.iter().map(|&m| (m, None, Mat4::IDENTITY)).collect();
        if radius.is_finite() && radius > 0.0 {
            self.scatter_proto_radius.insert(asset.to_string(), radius);
        }
        parts
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

    /// A headless process must not report a project's props as broken
    /// (`floptle/0157`).
    ///
    /// The bake registers meshes on the GPU, so with no GPU it comes back empty
    /// for every asset alike — and `floptle run` has no GPU. It printed
    /// "`grass.glb` has nothing to draw" for each prototype, in every run, about
    /// models that were perfectly fine. A warning that is always there and never
    /// true is worse than no warning: it teaches whoever is reading to skip the
    /// block that also holds the real ones.
    #[test]
    fn a_gpuless_bake_does_not_call_a_perfectly_good_model_undrawable() {
        let dir = std::env::temp_dir().join(format!("floptle-scatter-warn-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("models"));
        // A file that exists — the bake still cannot register it without a GPU.
        std::fs::write(dir.join("models/rock.glb"), b"not really a glb, but it is HERE").unwrap();

        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        assert!(ed.gpu.is_none(), "this test is about the no-GPU path");

        assert!(ed.scatter_prototype("models/rock.glb").is_none(), "nothing bakes without a GPU");
        assert!(
            ed.console.entries.is_empty(),
            "a model that is right there must not be called undrawable by a process that \
             could not have drawn anything: {:?}",
            ed.console.entries.iter().map(|e| &e.msg).collect::<Vec<_>>()
        );
        // …and it is not cached as an answer, so the same editor with a GPU
        // would still bake it.
        // Cached so a thousand-step headless run does not re-parse it a
        // thousand times — but remembered as a non-answer, so an editor WITH a
        // GPU bakes it properly.
        assert!(ed.scatter_protos.contains_key("models/rock.glb"), "cached, so it is asked once");
        assert!(ed.scatter_protos_gpuless.contains("models/rock.glb"), "…and known to be provisional");

        // An asset that names nothing on disk is a real mistake, and saying so
        // needs no GPU at all.
        assert!(ed.scatter_prototype("models/nope.glb").is_none());
        assert_eq!(ed.console.entries.len(), 1, "the missing one is still reported");
        assert!(
            ed.console.entries[0].msg.contains("no prefab and no model file"),
            "{}",
            ed.console.entries[0].msg
        );
    }

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
            anchor: None,
            frame: Default::default(),
        }
    }

    /// A prototype of several parts draws one instance PER PART, each at its
    /// place within the prop and all sharing the prop's transform
    /// (`floptle/0065`). That is what lets a generated plant — a trunk and
    /// A field is culled by DIRECTION, not only by distance (`floptle/0075`).
    ///
    /// `band_at` has always rejected props past the last LOD band, so a field was
    /// bounded — but never oriented. A full disc submitted its whole area
    /// including everything behind the camera, which is roughly half of it. This
    /// aims a real frustum at the field and then turns it round.
    #[test]
    fn a_scatter_field_submits_only_the_half_it_can_see() {
        let s = src();
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]);
        // A measured prototype radius is what makes the cull possible at all;
        // without one the source is submitted whole, as it was before.
        let mut radius = |_: &str| Some(1.0f32);
        let mat = MaterialParams::flat([1.0; 3]);
        let mut count = |frustum: &floptle_render::Frustum,
                         radius: &mut dyn FnMut(&str) -> Option<f32>| {
            let mut out = Vec::new();
            build_instances(
                &mut ScatterCache::default(),
                std::slice::from_ref(&s),
                DVec3::ZERO,
                &mut mesh,
                &mut { radius },
                &mut ground,
                &mat,
                frustum,
                100_000,
                &mut out,
            );
            out.len()
        };
        let proj = Mat4::perspective_rh(60f32.to_radians(), 16.0 / 9.0, 0.1, 1000.0);
        let everything = count(&floptle_render::Frustum::everything(), &mut radius);
        assert!(everything > 20, "the field should have plenty of props: {everything}");
        // Looking down −Z: only the props on that side survive.
        let forward = count(&floptle_render::Frustum::from_view_proj(proj), &mut radius);
        assert!(
            forward < everything,
            "aiming the camera at half the field submitted all of it ({forward} of {everything})"
        );
        assert!(forward > 0, "…but the half in front of the camera has to still be there");
        // Turned round: the same field, none of it visible.
        let behind = count(
            &floptle_render::Frustum::from_view_proj(proj * Mat4::from_rotation_y(std::f32::consts::PI)),
            &mut radius,
        );
        assert!(behind < forward, "turning round should submit less, not the same");
        // A prototype nothing measured is never direction-culled — an
        // unmeasurable asset must cost a frame, not disappear.
        let unmeasured = count(&floptle_render::Frustum::from_view_proj(proj), &mut |_: &str| None);
        assert_eq!(
            unmeasured, everything,
            "a source with no measured prop radius must submit whole rather than vanish"
        );
    }

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
            &mut |_: &str| None,
            &mut ground,
            &MaterialParams::flat([1.0; 3]),
            &floptle_render::Frustum::everything(),
            10_000,
            &mut one,
        );
        let mut two = Vec::new();
        build_instances(
            &mut ScatterCache::default(),
            &[s],
            DVec3::ZERO,
            &mut mesh,
            &mut |_: &str| None,
            &mut ground,
            &MaterialParams::flat([1.0; 3]),
            &floptle_render::Frustum::everything(),
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
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(),
            10_000, &mut out,
        );
        let before = out.len();
        assert!(before > 0, "nothing was drawn");

        // Cut one that is definitely in range.
        let victim = scatter::chunk_instances(&s, (0, 0))[0].id;
        s.removed.insert(victim);
        out.clear();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(),
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
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(),
            64, &mut out,
        );
        assert_eq!(out.len(), 64);
    }

    /// A field of the shape that froze a game: a long view distance against a
    /// small chunk, over a region big enough that the distance is what decides.
    fn big_field() -> ScatterSource {
        ScatterSource {
            region: Region::Ground { center: DVec3::ZERO, half: [5_000.0, 5_000.0] },
            chunk: 22.0,
            per_chunk: 26,
            bands: vec![Band { asset: "rock.glb".into(), distance: 700.0 }],
            ..src()
        }
    }

    /// Settling is a raycast per prop, and a chunk arriving pays for all of its
    /// props at once. Crossing a chunk boundary used to drag thousands of them
    /// into one frame — "it freezes more as I'm looking around", which is what
    /// a third-person camera swinging the eye several metres does
    /// (`floptle/0071`).
    #[test]
    fn arriving_ground_is_settled_over_several_frames_not_all_at_once() {
        let s = big_field();
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]);
        let mat = MaterialParams::flat([1.0; 3]);
        let eye = DVec3::ZERO;

        // Left alone this configuration is ninety thousand props, every one of
        // them a raycast on the frame its chunk arrives.
        assert!(scatter::cost(&s).props > 80_000, "not the pathological case");

        let mut worst = 0usize;
        let mut frames = 0;
        loop {
            let mut casts = 0usize;
            let mut ground = |_: DVec3, _: Vec3, _: f32| {
                casts += 1;
                Some((60.0f32, Vec3::Y))
            };
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(),
                20_000, &mut out,
            );
            worst = worst.max(casts);
            frames += 1;
            if casts == 0 || frames > 400 {
                break;
            }
        }
        assert!(
            worst <= SETTLE_BUDGET + s.per_chunk as usize,
            "one frame settled {worst} props — the cap is {SETTLE_BUDGET}"
        );
        // …and standing still, a warmed field costs NO ground work at all.
        let mut casts = 0usize;
        let mut ground = |_: DVec3, _: Vec3, _: f32| {
            casts += 1;
            Some((60.0f32, Vec3::Y))
        };
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(), 20_000,
            &mut out,
        );
        assert_eq!(casts, 0, "a stationary camera re-settled ground it had already settled");
        assert!(!out.is_empty(), "…while still drawing the field");
    }

    /// The draw budget cuts the HORIZON, not your feet. The sweep is ordered
    /// nearest first for exactly this reason: a budget spent on whichever
    /// chunks a nested loop reached first leaves holes in front of the player
    /// and props behind the fog.
    #[test]
    fn a_full_budget_drops_the_far_props_and_keeps_the_near_ones() {
        let s = big_field();
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mat = MaterialParams::flat([1.0; 3]);
        let eye = DVec3::ZERO;

        // Warm, so the settle budget isn't what's limiting this.
        for _ in 0..40 {
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(),
                20_000, &mut out,
            );
        }
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(), 300,
            &mut out,
        );
        assert_eq!(out.len(), 300, "the budget is a hard stop");
        // Every prop drawn is one of the nearest — nothing at the far edge of
        // the range got in ahead of something underfoot. The model's
        // translation is already camera-relative.
        let far = out
            .iter()
            .map(|(_, _, r)| {
                let m = r.model;
                (m[3][0] * m[3][0] + m[3][1] * m[3][1] + m[3][2] * m[3][2]).sqrt()
            })
            .fold(0.0f32, f32::max);
        assert!(far < 120.0, "a budgeted draw reached {far:.0} m out — the order is wrong");
    }

    /// A body that orbits carries its props (`floptle/0073`).
    ///
    /// The reported symptom: *"the scattered props seem to just be being left
    /// behind by the planet traveling in orbit"*. A region was pinned to the
    /// world at declare time, and the spawn planet moves at 99 units/s — so a
    /// 240-unit field was entirely behind its own planet in 2.4 seconds.
    ///
    /// What must NOT happen while it follows is anything being recomputed: same
    /// ids, same local positions, same settled heights, same removals. A rock
    /// stays the same rock.
    #[test]
    fn a_field_rides_its_planet_without_re_rolling_a_single_prop() {
        let mut s = src();
        s.anchor = Some("Umunquo".into());
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]);
        let mat = MaterialParams::flat([1.0; 3]);

        // Frame one: at the origin.
        let casts = std::cell::Cell::new(0usize);
        let mut ground = |_: DVec3, _: Vec3, _: f32| {
            casts.set(casts.get() + 1);
            Some((60.0f32, Vec3::Y))
        };
        // Stream in fully first — arriving ground settles over several frames
        // on purpose (`floptle/0071`), and that is not what is being measured.
        let mut before = Vec::new();
        for _ in 0..40 {
            before.clear();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh,
                &mut |_: &str| None, &mut ground, &mat,
                &floptle_render::Frustum::everything(), 20_000, &mut before,
            );
        }
        assert!(!before.is_empty(), "nothing was drawn to begin with");
        assert!(casts.get() > 0, "nothing settled");

        // …and now the planet has moved 240 units — further than the whole
        // field reaches — and turned. The eye rides with it.
        let moved = DVec3::new(240.0, 0.0, 0.0);
        s.frame = scatter::Frame {
            origin: moved,
            rot: floptle_core::math::Quat::from_rotation_y(0.7),
        };
        casts.set(0);
        let mut after = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), moved, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(), 20_000,
            &mut after,
        );

        assert_eq!(
            after.len(),
            before.len(),
            "the field changed size when its planet moved — props were left behind"
        );
        assert_eq!(
            casts.get(), 0,
            "{casts:?} props were re-settled onto the ground because their planet moved; \
             a settled height is expressed in the body's own frame and cannot go stale"
        );
        // Every prop is in the same place ON THE PLANET as before. Draw
        // positions are camera-relative, so read them back into the body's own
        // frame — the planet turned as well as moved, and a prop that rode it
        // correctly turned with it.
        let on_body = |v: &Vec<(MeshId, Option<TexId>, InstanceRaw)>,
                       eye: DVec3,
                       f: &scatter::Frame| -> Vec<DVec3> {
            v.iter()
                .map(|(_, _, r)| {
                    let rel =
                        DVec3::new(r.model[3][0] as f64, r.model[3][1] as f64, r.model[3][2] as f64);
                    f.to_local(rel + eye)
                })
                .collect()
        };
        let a = on_body(&before, DVec3::ZERO, &scatter::Frame::IDENTITY);
        let b = on_body(&after, moved, &s.frame);
        let worst =
            a.iter().zip(&b).map(|(p, q)| (*p - *q).length()).fold(0.0f64, f64::max);
        assert!(
            worst < 0.01,
            "a prop moved {worst:.3} units across the surface of its own planet when \
             that planet orbited — the field is not riding the body"
        );
    }

    /// …and an un-anchored source is untouched by any of it. Most scatter is on
    /// ground that never moves, and it must not pay for the planet case.
    #[test]
    fn a_field_with_no_anchor_still_sits_where_the_world_says() {
        let s = src();
        assert!(s.anchor.is_none() && s.frame.is_identity());
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY)]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mat = MaterialParams::flat([1.0; 3]);
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground, &mat,
            &floptle_render::Frustum::everything(),
            20_000, &mut out,
        );
        assert!(!out.is_empty(), "an unanchored field stopped drawing");
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
                &mut |_: &str| None,
                &mut ground, &mat, &floptle_render::Frustum::everything(), 10_000, &mut out,
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
