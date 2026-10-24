//! Drawing scatter sources: seed → instances → one instanced
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

use floptle_core::math::{DVec3, Mat4, Vec3, Vec4};
use floptle_core::scatter::{self, Align, ChunkKey, Instance, ScatterSource};
use floptle_render::{instance_of_mat, InstanceRaw, MaterialParams, MeshId, TexId};

/// One chunk's instances, already dropped onto the surface.
pub(crate) struct ResolvedChunk {
    pub instances: Vec<Instance>,
    /// Props rolled for the chunk and not dropped onto the ground yet: settling
    /// is shared out across frames a prop at a time, so a chunk with hundreds
    /// of props fills in over a few frames instead of stopping one.
    pub pending: Vec<Instance>,
    /// The removal-set size the chunk was resolved at. A cut prop has to
    /// disappear this frame, not when the chunk next streams — so a change
    /// here invalidates the cache.
    pub removed_len: usize,
}

/// How many props may be dropped onto the ground in one frame, across every
/// source.
///
/// Settling is a raycast per prop, cached per chunk — so the first frame a
/// chunk comes into range pays for all of its props at once. A third-person
/// camera swings the eye several metres just from looking around, which crosses
/// chunk boundaries — uncapped, that drags thousands of fresh raycasts into a
/// single frame, and the game freezes as you look around.
///
/// The cost of the cap is that ground arriving all at once fills in over a few
/// frames, furthest last. The cost of not having it is a frame that stops.
const SETTLE_BUDGET: usize = 96;

/// …and the time the same frame may spend doing it. A prop costs a ray down
/// to the ground, and a ray over a terrain field is a march, so 512 of them
/// were most of a second on a planet: the count alone does not bound the frame.
const SETTLE_MS: f32 = 1.0;

/// How far past its range a source keeps the chunks it has settled, as a
/// multiple of the range. Past that a chunk is forgotten, and rolled and
/// settled again if the eye comes back.
const KEEP_RANGE: f64 = 1.5;

/// A frame starts a ray only when the rays so far, plus what a ray has been
/// costing lately, still fit [`SETTLE_MS`]. When one ray alone costs more
/// than the whole budget, the field still arrives, a ray every this many
/// frames, rather than a ray every frame each over budget.
const SLOW_RAY_EVERY: u64 = 8;

/// Which chunks one source is resident in, and where the eye was standing when
/// that was worked out.
struct Sweep {
    /// The chunk the eye was over, and its height band. The key set changes
    /// when these change, not when the frame advances. `None` = never swept.
    at: Option<(ChunkKey, i64)>,
    /// Nearest first, as `chunks_near` returns them.
    keys: Vec<ChunkKey>,
}

/// Where each prop landed, by `(source id, instance id)`, shared with the
/// scripts for `scatter.near`.
type Settled = std::rc::Rc<std::cell::RefCell<HashMap<(u32, u64), (DVec3, Vec3)>>>;

/// Per-source chunk caches, keyed by `(source id, chunk)`.
#[derive(Default)]
pub(crate) struct ScatterCache {
    chunks: HashMap<(u32, ChunkKey), ResolvedChunk>,
    /// The resident key list per source — swept when the eye crosses a chunk
    /// boundary, not once a frame.
    sweeps: HashMap<u32, Sweep>,
    /// Settle everything in range now, budget or no budget: a single picture
    /// (`floptle shot`) has no later frames for the field to fill in over.
    pub(crate) settle_all: bool,
    /// What one settling ray has been costing lately, in ms, decayed.
    ray_ms: f32,
    /// Frames this cache has built, for [`SLOW_RAY_EVERY`].
    frame: u64,
    /// Where to record each prop as it settles, for `scatter.near`.
    pub(crate) settled_out: Option<Settled>,
    /// What the last build spent rolling and settling, in ms: the
    /// `scatterSettle` part of the scatter bucket.
    pub(crate) settle_ms: f32,
    /// The points bending props away this frame (`scatter.setInteractors`).
    pub(crate) interactors: Vec<scatter::Interactor>,
    /// The game time this build is for, in seconds: how far a bent prop has
    /// eased back since the last one.
    pub(crate) time: f64,
    last_time: Option<f64>,
    /// The props leaning now, by `(source, instance)`: how far, and the build
    /// that last drew them.
    bent: HashMap<(u32, scatter::InstanceId), (Vec3, u64)>,
}

/// How quickly a bent prop stands back up once nothing presses on it, in
/// seconds: about 2% of the lean is left a second later. Pressing in is at
/// once; grass gives way to a boot, and straightens slowly behind it.
const SPRING_BACK_S: f32 = 0.25;

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
            // keyed in the source's own frame.
            let pl = src.frame.to_local(p);
            for key in scatter::chunks_near(src, pl, radius) {
                // What `scatter.near` was told stood here is no longer known:
                // the ground may be gone from under it.
                if let Some(old) = self.chunks.remove(&(src.id, key))
                    && let Some(landed) = &self.settled_out
                {
                    let mut l = landed.borrow_mut();
                    for i in &old.instances {
                        l.remove(&(src.id, i.id));
                    }
                }
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
/// A prop with no ground under it is dropped rather than left floating at the
/// region's nominal height — a tree hanging in the air over a canyon reads as a
/// bug, and a missing tree reads as a canyon.
/// The instance stays in the source's own frame; only the RAY goes out to the
/// world and only the normal comes back. That is what lets a
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
        // The real normal, not the region's idealised one: a tree on a hillside
        // should lean with the hill.
        inst.up = f.dir_to_local(normal);
    }
    Some(inst)
}

/// One drawable piece of a scatter prototype, ready to draw: a mesh, its
/// texture, where it sits within the prop, and its surface as its model file
/// describes it.
///
/// A `.glb` is one part at identity. A prefab is however many Mesh nodes it
/// holds, each at its authored place — which is what lets a plant be a trunk
/// and three fronds and still cost one instanced draw per piece rather than a
/// scene node per piece.
/// A prototype part as drawn: its mesh, texture, place in the prop and look,
/// and the surface-extras entries it wears while cross-fading between LOD
/// bands, dissolving out and dissolving in (see [`fade_extras`]).
pub(crate) type Part = (MeshId, Option<TexId>, Mat4, MaterialParams, [u32; 2]);

/// A prototype part as baked: everything about its look that the model file
/// fixed, kept as facts rather than as a [`MaterialParams`].
///
/// A drawn material carries an index into the renderer's surface store, and
/// that store is rebuilt when the project's artefacts change, so a material
/// baked once would point at the wrong entry afterwards. The facts are turned
/// into a material once per prototype per frame instead (see [`resolve_parts`]).
#[derive(Clone)]
pub(crate) struct BakedPart {
    pub mesh: MeshId,
    pub local: Mat4,
    /// The part's imported base-colour factor.
    pub base: [f32; 3],
    /// Its maps and glow, when its file names more than a base colour.
    pub imported: Option<crate::ImportedLook>,
    /// The prefab node's own Material, with its images, when it has one: it
    /// supersedes the model's look exactly as it does on a node, so a plant
    /// prefab's sway, backlight and colour reach every prop scattered from it.
    pub node_look: Option<NodeLook>,
}

/// A prefab node's Material and the images it names, resolved at bake time.
#[derive(Clone, Debug)]
pub(crate) struct NodeLook {
    pub material: floptle_core::Material,
    pub base: Option<TexId>,
    pub maps: Option<[Option<TexId>; 5]>,
}

/// A baked prototype's parts with their materials, for this frame — the same
/// look an ordinary Mesh node with no Material of its own draws: the imported
/// base colour, maps and glow, and the model's own vertex colours.
pub(crate) fn resolve_parts(
    raster: &mut floptle_render::Raster,
    gpu: &floptle_render::Gpu,
    baked: &[BakedPart],
) -> Vec<Part> {
    baked
        .iter()
        .map(|b| {
            let (tex, mut mp) = match (&b.node_look, &b.imported) {
                // A Material with no picture of its own draws untextured, on
                // purpose, rather than falling back to the model's picture it
                // is replacing — the rule a Mesh node follows.
                (Some(n), _) => {
                    let base = n.base.or_else(|| Some(raster.white_texture(gpu)));
                    crate::shading::material_draw_with(raster, gpu, &n.material, base, n.maps)
                }
                (None, Some(look)) => {
                    crate::shading::material_draw_with(raster, gpu, &look.material, look.base, Some(look.maps))
                }
                (None, None) => (None, MaterialParams::flat(b.base)),
            };
            // Imported COLOR_0 is a plain multiply (white is identity), as on
            // a Mesh node.
            mp.paint_base = raster.mesh_paint_base(b.mesh);
            let emissive_map = match (&b.node_look, &b.imported) {
                (Some(n), _) => n.maps.is_some_and(|m| m[4].is_some()),
                (None, Some(look)) => look.maps[4].is_some(),
                (None, None) => false,
            };
            glow_under_map(&mut mp, emissive_map);
            let fade = fade_extras(raster, mp.ext_index);
            (b.mesh, tex, b.local, mp, fade)
        })
        .collect()
}

/// An emissive map with no colour behind it still says where a model glows:
/// white under it, at no strength, so the part stays dark until a source
/// turns the glow up and then glows in the map's own colours (see `push` in
/// [`build_instances`]).
fn glow_under_map(mp: &mut MaterialParams, emissive_map: bool) {
    if emissive_map && mp.emissive == [0.0; 3] {
        mp.emissive = [1.0; 3];
    }
}

/// The two surface-extras entries a part wears while its LOD bands cross:
/// its own, screen-door dithered, once with the pattern and once with its
/// complement. The band leaving draws the pixels the band arriving does not,
/// so a crown is whole at every point of the fade and both stay in the opaque
/// pass. Blending the two at half alpha drew every crown half see-through.
fn fade_extras(raster: &mut floptle_render::Raster, ext: u32) -> [u32; 2] {
    [raster.dithered_extras(ext, false), raster.dithered_extras(ext, true)]
}

/// Where each anchored source's node stands this frame, as `(source id,
/// frame)`.
///
/// A celestial body orbits at ~99 units/s, so a region pinned to the world
/// slides out from under its own props in about two seconds. Refreshing one
/// transform per source is the whole cost of following it: placement lives in
/// this frame, so nothing downstream is recomputed.
pub(crate) fn anchor_frames(
    script_host: &floptle_script::ScriptHost,
    world: &floptle_core::World,
) -> Vec<(u32, scatter::Frame)> {
    let mut out = Vec::new();
    // Each anchor is found through the scripts' name index, checked against
    // the world's own name: O(1) a source. Only a node renamed since the
    // scripts last looked costs a walk of the scene's names, once. A walk
    // every frame was the part of a source's cost that grew with the level.
    let anchored = script_host.anchored_scatter();
    let mut nodes: HashMap<&str, floptle_core::Entity> = HashMap::new();
    let mut missed = false;
    for (_, name) in &anchored {
        let hit = script_host
            .mirror_entity_named(name)
            .and_then(|i| world.entity_at(i))
            .filter(|&e| world.get::<floptle_core::Name>(e).is_some_and(|n| n.0 == *name));
        match hit {
            Some(e) => {
                nodes.insert(name.as_str(), e);
            }
            None => missed |= script_host.mirror_entity_named(name).is_some(),
        }
    }
    if missed {
        for (e, n) in world.query::<floptle_core::Name>() {
            if anchored.iter().any(|(_, a)| *a == n.0) {
                nodes.entry(n.0.as_str()).or_insert(e);
            }
        }
    }
    for (id, name) in &anchored {
        let node = nodes.get(name.as_str()).copied();
        let frame = node.map_or(scatter::Frame::IDENTITY, |e| {
            let wt = floptle_core::world_transform(world, e);
            scatter::Frame { origin: wt.translation, rot: wt.rotation.normalize() }
        });
        out.push((*id, frame));
    }
    out
}

/// Every scatter source's props for a camera at `eye`, pushed onto `out`;
/// returns how many. Anchored sources first follow their node to where it is
/// this frame.
///
/// The one place a gather draws scatter, shared by the window's gather and the
/// offscreen one (`camera.capture`, `floptle shot`, render targets), so a
/// picture of the world has the same plants and rocks the screen does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_scatter(
    script_host: &mut floptle_script::ScriptHost,
    world: &floptle_core::World,
    sim: Option<&floptle_physics::Sim>,
    raster: &mut floptle_render::Raster,
    gpu: &floptle_render::Gpu,
    protos: &HashMap<String, Vec<BakedPart>>,
    proto_radius: &HashMap<String, f32>,
    cache: &mut ScatterCache,
    eye: DVec3,
    frustum: &floptle_render::Frustum,
    out: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) -> usize {
    // Where each anchored source's node has got to this frame.
    for (id, frame) in anchor_frames(script_host, world) {
        script_host.set_scatter_frame(id, frame);
    }
    // Borrowed, not cloned: a source carries its density grid and every id it
    // has lost, and copying all of them was a per-source cost every frame.
    let sources = script_host.scatter_sources();
    if sources.is_empty() {
        if cache.len() > 0 {
            cache.clear();
        }
        return 0;
    }
    let before = out.len();
    let mut ground = |from: DVec3, dir: Vec3, max: f32| {
        let sim = sim?;
        let o = (from - sim.world.origin).as_vec3();
        // The ground's own answer: the same hit, but a terrain chunk is never
        // meshed for collision on a prop's account (see `ground_cast`).
        sim.world.raycast_ground(o, dir, max).map(|h| (h.distance, Vec3::from(h.normal)))
    };
    // Baked before the frame's GPU borrow (see
    // `bake_scatter_prototypes`), and given its materials once a frame
    // here: a prototype may be a prefab of several parts, and resolving
    // that per prop would re-walk it twenty thousand times.
    let mut looks: HashMap<&str, Vec<Part>> = HashMap::new();
    for src in sources.iter() {
        for band in &src.bands {
            if !looks.contains_key(band.asset.as_str())
                && let Some((name, baked)) = protos.get_key_value(&band.asset)
                && !baked.is_empty()
            {
                looks.insert(name.as_str(), resolve_parts(raster, gpu, baked));
            }
        }
    }
    let mut mesh_of = |asset: &str| looks.get(asset).cloned();
    // Measured at bake time from the same import bounds the mesh path
    // uses, so a field culls by direction as well as distance.
    let mut radius_of = |asset: &str| proto_radius.get(asset).copied();
    // A hard cap, logged nowhere and needing none: a source with a
    // silly density costs a frame-rate dip, never a frame that
    // never ends.
    const SCATTER_BUDGET: usize = 20_000;
    cache.settled_out = Some(script_host.scatter_settled());
    cache.interactors = script_host.scatter_interactors();
    cache.time = script_host.weather_time();
    build_instances(
        cache,
        &sources,
        eye,
        &mut mesh_of,
        &mut radius_of,
        &mut ground,
        frustum,
        SCATTER_BUDGET,
        out,
    );
    out.len().saturating_sub(before)
}

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
    frustum: &floptle_render::Frustum,
    budget: usize,
    out: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) {
    // Disjoint borrows: the sweep list and the resolved chunks are read and
    // written in the same loop.
    let ScatterCache {
        chunks: resolved,
        sweeps,
        settle_all,
        ray_ms,
        frame,
        settled_out,
        settle_ms: spent,
        interactors,
        time,
        last_time,
        bent,
    } = cache;
    let dt = last_time.map_or(0.0, |t| (*time - t).max(0.0)) as f32;
    *last_time = Some(*time);
    let spring = 1.0 - (-dt / SPRING_BACK_S).exp();
    *spent = 0.0;
    let mut landed = settled_out.as_ref().map(|s| s.borrow_mut());
    let settle_all = *settle_all;
    *frame += 1;
    // A prop not drawn in the last build has gone out of view, and stands
    // straight when it comes back.
    bent.retain(|_, (_, seen)| *seen + 1 >= *frame);
    // One ray over budget is allowed now and then when a ray alone is more
    // than the budget; otherwise a ray starts only if it is expected to fit.
    let slow_turn = *frame % SLOW_RAY_EVERY == 0;
    sweeps.retain(|id, _| sources.iter().any(|s| s.id == *id));
    // Shared across sources, because it is a frame budget. A source that eats
    // it all is fully resident afterwards and stops asking, so the next source
    // gets the next frame's — it converges rather than starving anyone.
    let mut settled = 0usize;
    // Time spent rolling and settling, and only that: the draw work around it
    // is the frame's anyway, and counting it spent the budget before a prop
    // settled.
    let mut settle_ms = 0.0f32;
    // Whether this frame has done any of that work yet. The first piece always
    // goes, so a field that is never cheap still arrives; every piece after it
    // waits for the budget.
    let mut worked = false;
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
        // the frame advances. Standing still, or walking within one chunk,
        // this is a hash lookup, not a square sweep allocated and thrown away
        // sixty times a second.
        // Everything below happens in the source'S own frame.
        // The eye comes to the region rather than the region going to the world,
        // so a body that orbits at 99 units/s changes exactly one number here —
        // and no id, no local position, no settled height and no cached chunk.
        let eye_local = src.frame.to_local(eye);
        let at = (scatter::eye_chunk(src, eye_local), scatter::eye_height_band(src, eye_local));
        let sweep = sweeps.entry(src.id).or_insert_with(|| Sweep { at: None, keys: Vec::new() });
        if sweep.at != Some(at) {
            sweep.at = Some(at);
            sweep.keys = scatter::chunks_near(src, eye_local, range as f64);
            // Chunks well behind the eye are forgotten, with where their props
            // landed. Kept, every chunk ever passed stayed resolved and every
            // prop stayed in the landed map, so a long flight grew both without
            // end, and each time the map doubled it moved every entry inside
            // one prop's settle: a frame of 11 to 21 ms, rarer and longer the
            // further you had flown. A margin past the range keeps a chunk you
            // turn back to from rolling again.
            let span = floptle_core::profile::Span::new();
            let keep = (range as f64 + 2.0 * src.chunk) * KEEP_RANGE;
            resolved.retain(|(id, key), chunk| {
                if *id != src.id || (scatter::chunk_center(src, *key) - eye_local).length() <= keep {
                    return true;
                }
                if let Some(l) = landed.as_mut() {
                    for i in &chunk.instances {
                        l.remove(&(src.id, i.id));
                    }
                }
                false
            });
            settle_ms += span.ms();
            *spent = settle_ms;
        }
        // Nearest first, so the draw budget below cuts the horizon rather than
        // your feet, and so streaming spends its first frames on what you can
        // actually see.
        // Each band's parts, looked up once per source rather than once per
        // prop drawn: a field of thousands of props has a handful of bands.
        let band_parts: Vec<Option<Vec<Part>>> = src.bands.iter().map(|b| mesh_of(&b.asset)).collect();
        for &key in &sweep.keys {
            // One lookup a chunk a frame. A field whose sources are sparse is
            // mostly chunks, and three lookups each made eight overlapping
            // sources cost two and a half times what one source did per prop.
            let chunk = match resolved.entry((src.id, key)) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let chunk = e.into_mut();
                    // Something was cut, or put back, since this chunk was
                    // resolved. A cut prop goes this frame — a prop that
                    // survives the swing that felled it is a bug — and that
                    // needs no ray: the props left standing are where they
                    // were. Re-settling the chunk here did, and since any cut
                    // anywhere in the source changes the count every chunk
                    // compares against, one harvest re-settled the whole field
                    // in one frame.
                    if chunk.removed_len != src.removed.len() {
                        let shrank = src.removed.len() < chunk.removed_len;
                        chunk.instances.retain(|i| !src.removed.contains(&i.id));
                        chunk.pending.retain(|i| !src.removed.contains(&i.id));
                        // A prop put back is rolled again and waits its turn
                        // like any arriving one.
                        if shrank {
                            let have: std::collections::HashSet<_> =
                                chunk.instances.iter().chain(&chunk.pending).map(|i| i.id).collect();
                            let back = scatter::chunk_instances(src, key).into_iter().filter(|i| !have.contains(&i.id));
                            chunk.pending.extend(back);
                        }
                        chunk.removed_len = src.removed.len();
                    }
                    chunk
                }
                // Arriving is the other case, and it is thousands of props at
                // once: a chunk is rolled within the frame's budget, and its
                // props wait their turn, a prop at a time, below. Rolling
                // every chunk a sweep turned up in the frame it did was most
                // of a walking frame's spike on a field of small chunks.
                std::collections::hash_map::Entry::Vacant(e) => {
                    if !(settle_all || settle_ms <= SETTLE_MS) {
                        continue;
                    }
                    let span = floptle_core::profile::Span::new();
                    let pending = scatter::chunk_instances(src, key);
                    settle_ms += span.ms();
                    *spent = settle_ms;
                    worked = true;
                    e.insert(ResolvedChunk { instances: Vec::new(), pending, removed_len: src.removed.len() })
                }
            };
            // Within the frame's budget, by count and by time, the first ray
            // included: a ray starts when it is expected to fit. The first
            // settle of a frame used to run whatever it cost, and one slow ray
            // was a frame over budget every frame a field was arriving.
            while !chunk.pending.is_empty() {
                let fits = settled < SETTLE_BUDGET && settle_ms + *ray_ms <= SETTLE_MS;
                let slow_ok = !worked && *ray_ms > SETTLE_MS && slow_turn;
                if !(settle_all || fits || slow_ok) {
                    break;
                }
                let span = floptle_core::profile::Span::new();
                let inst = chunk.pending.pop().expect("not empty");
                settled += 1;
                worked = true;
                let id = inst.id;
                match settle(src, inst, ground) {
                    Some(on) => {
                        if let Some(l) = landed.as_mut() {
                            l.insert((src.id, on.id), (on.pos, on.up));
                        }
                        chunk.instances.push(on);
                    }
                    // No ground under it: not drawn, so not standing anywhere
                    // a harvest could find it.
                    None => {
                        if let Some(l) = landed.as_mut() {
                            l.remove(&(src.id, id));
                        }
                    }
                }
                let ms = span.ms();
                settle_ms += ms;
                *spent = settle_ms;
                *ray_ms = *ray_ms * 0.8 + ms * 0.2;
            }
            for inst in &chunk.instances {
                if out.len() >= budget {
                    return;
                }
                // Distance is measured in the source's frame, which is a rigid
                // transform of the world's — so LOD bands are untouched by where
                // the body happens to be.
                let d = (inst.pos - eye_local).length() as f32;
                let Some((band, blend)) = scatter::band_at(src, d) else { continue };
                // …and only here does the world get involved: the instance's
                // place in its region, carried out to wherever that region is.
                let rot = src.frame.rot * inst.rotation(src.align);
                let centre = (src.frame.to_world(inst.pos) - eye).as_vec3();
                // …and only then, is it on screen? Distance was the only test a
                // field ever applied, so a full disc submitted everything behind
                // you. after the band test, which is the cheaper
                // one and also the one that rejects most.
                if let Some(pr) = prop_radius
                    && !frustum
                        .contains_sphere(centre, floptle_render::cull::scale_radius(pr, Vec3::splat(inst.scale)))
                {
                    continue;
                }
                let mut model =
                    Mat4::from_scale_rotation_translation(Vec3::splat(inst.scale), rot, centre);
                // Bent away from whatever is pushing through it: sheared about
                // its base, so the foot stays planted and the top goes over.
                // Only a source that gives way bends (`bend`); a rock does not.
                if src.bend > 0.0 && (!interactors.is_empty() || !bent.is_empty()) {
                    let up = rot * Vec3::Y;
                    let key = (src.id, inst.id);
                    let pressed = if interactors.is_empty() {
                        Vec3::ZERO
                    } else {
                        scatter::lean(interactors, src.frame.to_world(inst.pos), up) * src.bend
                    };
                    let was = bent.get(&key).map_or(Vec3::ZERO, |b| b.0);
                    // Pushed over at once, eased back up.
                    let l = if pressed.length_squared() >= was.length_squared() { pressed } else { was + (pressed - was) * spring };
                    if l.length_squared() > 1e-8 {
                        bent.insert(key, (l, *frame));
                    } else {
                        bent.remove(&key);
                    }
                    if l != Vec3::ZERO {
                        let shear = Mat4::from_cols(
                            (Vec3::X + l * up.x).extend(0.0),
                            (Vec3::Y + l * up.y).extend(0.0),
                            (Vec3::Z + l * up.z).extend(0.0),
                            Vec4::W,
                        );
                        model = Mat4::from_translation(centre) * shear * Mat4::from_translation(-centre) * model;
                    }
                }
                // Mid-fade draws both bands, cross-dissolved. Drawing one and
                // switching is what a pop is; two complementary screen doors
                // for a few metres of walking is what nobody notices.
                let mut push = |b: usize, alpha: f32, arriving: bool| {
                    let Some(Some(parts)) = band_parts.get(b) else { return };
                    // The per-instance roll rides the albedo, so a game can
                    // give one species a spread of shades without a material
                    // per plant. Neutral at param 0.5.
                    let tintf = 0.85 + 0.3 * inst.param;
                    let t = src.look.tint;
                    // A prototype may be several parts (a prefab's nodes), each
                    // with its own mesh, texture, look and place within the
                    // prop. They are still one instanced draw each — the prefab
                    // is resolved to this list once, not instantiated per prop.
                    for (mesh, tex, local, look, fade) in parts {
                        let mut mp = *look;
                        mp.alpha *= alpha;
                        // Mid-fade: dissolve rather than blend, the band
                        // leaving on the pattern and the band arriving on
                        // its complement (`fade_extras`).
                        if alpha < 1.0 {
                            mp.ext_index = fade[usize::from(arriving)];
                        }
                        // The source's tint multiplies the model's own colour,
                        // so a texture's palette is shaded rather than replaced.
                        mp.color = [mp.color[0] * t[0] * tintf, mp.color[1] * t[1] * tintf, mp.color[2] * t[2] * tintf];
                        // A source's glow strength supersedes the model's; a
                        // source with none leaves the model's own glow alone.
                        // Its colour is the source's `emissive` if it names
                        // one, else the model's own glow colour (and map), else
                        // the source's tint: a field told to glow and nothing
                        // else glows in its own colours, never white.
                        if src.look.emissive_strength > 0.0 {
                            mp.emissive = match src.look.emissive {
                                Some(c) => c,
                                None if mp.emissive != [0.0; 3] => mp.emissive,
                                None => t,
                            };
                            mp.emissive_strength = src.look.emissive_strength;
                        }
                        out.push((*mesh, *tex, instance_of_mat(model * *local, &mp)));
                    }
                };
                // At the very ends of a fade window one half rounds to nothing.
                // Draw the other half fully opaque rather than at 0.995: a
                // barely-transparent prop still pays for the blended pass —
                // depth-sorted, no depth write — for a difference no eye can
                // see, and every prop in a forest paying that is the cost.
                const FADE_EPS: f32 = 0.01;
                match blend {
                    b if b <= FADE_EPS => push(band, 1.0, false),
                    b if b >= 1.0 - FADE_EPS => push(band + 1, 1.0, false),
                    b => {
                        push(band, 1.0 - b, false);
                        push(band + 1, b, true);
                    }
                }
            }
        }
    }
}

impl crate::Editor {
    /// A scatter prototype resolved to its drawable parts, baked once and kept.
    ///
    /// A mesh file is one part at identity — what scatter has always drawn. A
    /// **prefab** is each of its `Mesh` nodes at its authored place inside the
    /// prop, which is the point: a game whose props are
    /// generated (Solar's plants are a trunk and a handful of fronds, assembled
    /// by a script) had nothing to hand scatter, because scatter took a file
    /// path and a plant is not a file.
    ///
    /// Baked at declare time, not per instance: editing the prefab while the
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

    pub(crate) fn scatter_prototype(&mut self, asset: &str) -> Option<Vec<BakedPart>> {
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
        // A mesh a script built this pass is uploaded after it: not there yet
        // is not "nothing to draw", and is neither cached nor said.
        if floptle_script::mesh_api::is_built_mesh(asset) && !self.mesh_registry.contains_key(asset) {
            return None;
        }
        let parts = self.bake_scatter_prototype(asset);
        if parts.is_empty() {
            // **"Nothing to draw" has to be a fact about the asset.**
            //
            // A bake registers meshes on the GPU, so with no GPU it comes back
            // empty whatever the asset is — and `floptle run` has no GPU and
            // draws nothing at all. Printing this there said a project's props
            // were broken when they were fine, several times, in every single
            // headless run: exactly the kind of warning that teaches a reader to
            // skip the warning block.
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
                // Cached, but remembered as a non-answer: an editor that later
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
        floptle_vfs::exists(crate::project::resolve_asset_path(&self.project_root, asset))
    }

    fn bake_scatter_prototype(&mut self, asset: &str) -> Vec<BakedPart> {
        // A prefab first, since a project may legitimately hold both names.
        if let Some(path) = self.resolve_prefab_request(asset)
            && let Ok(docs) = crate::prefab::load_prefab_docs(&path)
        {
            let mut out = Vec::new();
            // The prop's bounding radius, accumulated as its pieces are found:
            // a frond three metres up the trunk reaches its own radius further
            // than the trunk does.
            let mut radius = 0.0f32;
            for (i, d) in docs.iter().enumerate() {
                let floptle_scene::MatterDoc::Mesh { asset_path } = &d.matter else { continue };
                if !self.import_model(asset_path) {
                    continue;
                }
                let Some(a) = self.mesh_registry.get(asset_path) else { continue };
                let local = prefab_local(&docs, i);
                let (scale, _, offset) = local.to_scale_rotation_translation();
                radius = radius.max(
                    offset.length()
                        + floptle_render::cull::radius_from_longest_edge(a.size, scale),
                );
                let parts = baked_parts(a, local);
                let look = d.material.as_ref().map(|m| {
                    let material = m.to_material();
                    let base = material.texture.clone().and_then(|t| self.ensure_texture(&t));
                    let maps = material.has_maps().then(|| {
                        material.maps().map(|p| p.cloned().and_then(|p| self.ensure_texture(&p)))
                    });
                    NodeLook { material, base, maps }
                });
                out.extend(parts.into_iter().map(|p| BakedPart { node_look: look.clone(), ..p }));
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
        let parts = baked_parts(a, Mat4::IDENTITY);
        if radius.is_finite() && radius > 0.0 {
            self.scatter_proto_radius.insert(asset.to_string(), radius);
        }
        parts
    }
}

/// A model's parts as a prototype bakes them, each with its imported look.
fn baked_parts(a: &crate::MeshAsset, local: Mat4) -> Vec<BakedPart> {
    a.parts
        .iter()
        .enumerate()
        .map(|(i, &mesh)| {
            let meta = a.part_meta.get(i);
            BakedPart {
                mesh,
                local,
                base: meta.map_or([1.0; 3], |m| m.base_color),
                imported: meta.and_then(|m| m.imported.clone()),
                node_look: None,
            }
        })
        .collect()
}

/// A prefab node's transform relative to the prefab's root — its own, composed
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

    /// A headless process must not report a project's props as broken.
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
        let _ = floptle_vfs::create_dir_all(dir.join("models"));
        // A file that exists — the bake still cannot register it without a GPU.
        floptle_vfs::write(dir.join("models/rock.glb"), b"not really a glb, but it is HERE").unwrap();

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
        // thousand times — but remembered as a non-answer, so an editor with a
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
            look: Default::default(),
            bend: 0.0,
        }
    }

    /// **A source's tint and glow reach every prop it draws.** Scatter drew
    /// every prop flat white, so a game baked each palette into a texture of
    /// its own — two thousand model variants for one forest.
    #[test]
    fn a_sources_tint_and_glow_reach_its_props() {
        let s = ScatterSource {
            look: scatter::ScatterLook { tint: [1.0, 0.0, 0.0], emissive: Some([0.0, 1.0, 0.0]), emissive_strength: 2.0 },
            ..src()
        };
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut out = Vec::new();
        for _ in 0..50 {
            out.clear();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 20_000, &mut out,
            );
        }
        assert!(!out.is_empty(), "nothing drew");
        for (_, _, raw) in &out {
            assert!(raw.color[0] > 0.5 && raw.color[1] == 0.0 && raw.color[2] == 0.0, "untinted: {:?}", raw.color);
            assert_eq!(&raw.emissive, &[0.0, 1.0, 0.0, 2.0], "no glow");
        }
    }

    /// **A model's own colour is what a prop is tinted from.** Scatter drew
    /// every part from flat white, so a glTF's base-colour factor and its glow
    /// were thrown away, and a game baked each colour into a texture instead.
    #[test]
    fn a_props_own_colour_and_glow_survive_its_sources_tint() {
        let mut look = MaterialParams::flat([0.2, 0.4, 0.8]);
        look.emissive = [1.0, 0.5, 0.0];
        look.emissive_strength = 3.0;
        let asset = crate::MeshAsset {
            parts: vec![MeshId(4)],
            part_meta: vec![crate::PartMeta { material: "Leaf".into(), base_color: [0.2, 0.4, 0.8], textured: false, imported: None }],
            tex_filter: None,
            size: 1.0,
            rig: None,
        };
        let baked = baked_parts(&asset, Mat4::IDENTITY);
        assert_eq!(baked[0].base, [0.2, 0.4, 0.8], "the bake dropped the part's base colour");
        // A part whose file names maps or a glow keeps them through the bake.
        let mut mapped = asset.part_meta[0].clone();
        mapped.imported = Some(crate::ImportedLook { material: floptle_core::Material::default(), base: None, maps: [None; 5] });
        let with_maps = crate::MeshAsset { parts: vec![MeshId(4)], part_meta: vec![mapped], tex_filter: None, size: 1.0, rig: None };
        assert!(baked_parts(&with_maps, Mat4::IDENTITY)[0].imported.is_some(), "the bake dropped the part's imported maps");
        let s = ScatterSource { look: scatter::ScatterLook { tint: [1.0, 0.5, 1.0], ..Default::default() }, ..src() };
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(4), None, Mat4::IDENTITY, look, [0, 0])]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut out = Vec::new();
        for _ in 0..50 {
            out.clear();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 20_000, &mut out,
            );
        }
        assert!(!out.is_empty(), "nothing drew");
        for (_, _, raw) in &out {
            // Up to the per-prop shade roll, which scales all three alike.
            let k = raw.color[0] / 0.2;
            assert!((0.8..=1.2).contains(&k), "the part's colour was replaced: {:?}", raw.color);
            assert!((raw.color[1] - 0.4 * 0.5 * k).abs() < 1e-4, "{:?}", raw.color);
            assert!((raw.color[2] - 0.8 * k).abs() < 1e-4, "{:?}", raw.color);
            assert_eq!(&raw.emissive, &[1.0, 0.5, 0.0, 3.0], "the model's glow was dropped");
        }
    }

    /// **A field told to glow, and not what colour, glows in its own
    /// colours.** It glowed white. Now a prop takes its model's own glow
    /// colour (or emissive map), and a model with none glows in the source's
    /// tint.
    #[test]
    fn a_glow_with_no_colour_takes_the_models_own_or_the_tint() {
        let glow = |part: MaterialParams| {
            let s = ScatterSource {
                look: scatter::ScatterLook { tint: [1.0, 0.5, 0.2], emissive: None, emissive_strength: 2.0 },
                ..src()
            };
            let mut cache = ScatterCache { settle_all: true, ..Default::default() };
            let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, part, [0, 0])]);
            let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 20_000, &mut out,
            );
            assert!(!out.is_empty(), "nothing drew");
            out.iter().map(|(_, _, r)| r.emissive).collect::<Vec<_>>()
        };
        // A model with a glow colour of its own, at no strength: the source's
        // strength lights it in that colour.
        let mut own = MaterialParams::flat([1.0; 3]);
        own.emissive = [0.0, 0.8, 1.0];
        assert!(glow(own).iter().all(|e| *e == [0.0, 0.8, 1.0, 2.0]), "the model's own glow colour was replaced");
        // A model with none glows in the tint, not in white.
        let plain = glow(MaterialParams::flat([1.0; 3]));
        assert!(plain.iter().all(|e| *e == [1.0, 0.5, 0.2, 2.0]), "a model with no glow did not take the tint: {:?}", plain[0]);
        // A model with only an emissive map glows through it: white under the
        // map, and still dark until something turns it up.
        let mut mapped = MaterialParams::flat([1.0; 3]);
        glow_under_map(&mut mapped, true);
        assert_eq!((mapped.emissive, mapped.emissive_strength), ([1.0; 3], 0.0));
        assert!(glow(mapped).iter().all(|e| *e == [1.0, 1.0, 1.0, 2.0]), "the emissive map was given the tint");
        let mut unmapped = MaterialParams::flat([1.0; 3]);
        glow_under_map(&mut unmapped, false);
        assert_eq!(unmapped.emissive, [0.0; 3], "a part with no map was given a glow colour");
    }

    /// A prototype of several parts draws one instance per part, each at its
    /// place within the prop and all sharing the prop's transform.
    /// That is what lets a generated plant — a trunk and
    /// A field is culled by direction, not only by distance.
    ///
    /// `band_at` has always rejected props past the last LOD band, so a field was
    /// bounded — but never oriented. A full disc submitted its whole area
    /// including everything behind the camera, which is roughly half of it. This
    /// aims a real frustum at the field and then turns it round.
    #[test]
    fn a_scatter_field_submits_only_the_half_it_can_see() {
        let s = src();
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        // A measured prototype radius is what makes the cull possible at all;
        // without one the source is submitted whole, as it was before.
        let mut radius = |_: &str| Some(1.0f32);
        let mut count = |frustum: &floptle_render::Frustum,
                         radius: &mut dyn FnMut(&str) -> Option<f32>| {
            let mut out = Vec::new();
            // Settled whole: this is about culling, not about how fast a
            // field arrives.
            build_instances(
                &mut ScatterCache { settle_all: true, ..Default::default() },
                std::slice::from_ref(&s),
                DVec3::ZERO,
                &mut mesh,
                &mut { radius },
                &mut ground,
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
            Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0]), (MeshId(1), None, up, MaterialParams::flat([1.0; 3]), [0, 0])])
        };
        let mut one = Vec::new();
        build_instances(
            &mut ScatterCache::default(),
            std::slice::from_ref(&s),
            DVec3::ZERO,
            &mut |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]),
            &mut |_: &str| None,
            &mut ground,
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

    /// **A prop costs the same however many sources it shares the ground
    /// with.** A game measured about 1 µs a resident prop with one source and
    /// 4.3 µs with eight overlapping ones, the same props split eight ways.
    /// Asserted as a ratio, not a duration, so a slow machine cannot fail it.
    #[test]
    fn a_prop_costs_the_same_however_many_sources_overlap() {
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let field = |n: u32| -> Vec<ScatterSource> {
            (0..n)
                .map(|i| ScatterSource {
                    id: i + 1,
                    seed: 11 + i as u64,
                    // Two props a chunk between them, the shape a game's
                    // flora took: sparse fields are where chunk overhead shows.
                    per_chunk: 16 / n,
                    bands: vec![Band { asset: format!("p{i}"), distance: 60.0 }],
                    ..src()
                })
                .collect()
        };
        let mut per_prop = |sources: &[ScatterSource], ground: &mut dyn FnMut(DVec3, Vec3, f32) -> Option<(f32, Vec3)>| {
            let mut cache = ScatterCache { settle_all: true, ..Default::default() };
            let mut out = Vec::new();
            let mut g = |a: DVec3, b: Vec3, c: f32| ground(a, b, c);
            // Settled first: this is the cost of standing in a field, not of
            // arriving at one.
            for _ in 0..100 {
                out.clear();
                build_instances(&mut cache, sources, DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut g,
                    &floptle_render::Frustum::everything(), 100_000, &mut out);
            }
            let t = std::time::Instant::now();
            let frames = 300;
            for _ in 0..frames {
                out.clear();
                build_instances(&mut cache, sources, DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut g,
                    &floptle_render::Frustum::everything(), 100_000, &mut out);
            }
            let us = t.elapsed().as_secs_f64() * 1e6 / frames as f64;
            (us / out.len().max(1) as f64, out.len())
        };
        // Alternated, best of five each: the suite runs this beside a thousand
        // other tests, and a burst of load on one side must not decide it.
        let (one_f, eight_f) = (field(1), field(8));
        let (mut one, mut eight, mut n1, mut n8) = (f64::INFINITY, f64::INFINITY, 0, 0);
        for _ in 0..5 {
            let (t, n) = per_prop(&one_f, &mut ground);
            (one, n1) = (one.min(t), n);
            let (t, n) = per_prop(&eight_f, &mut ground);
            (eight, n8) = (eight.min(t), n);
        }
        eprintln!("one source: {one:.3} µs a prop ({n1}); eight: {eight:.3} µs a prop ({n8})");
        assert!(n1 > 400 && n8 > 400, "the fields are too small to measure: {n1}, {n8}");
        // About 1.6x on a quiet machine; the cost this guards read 4.3x, and
        // under a full suite's load the healthy ratio has read past 2.
        assert!(eight < one * 3.0, "eight overlapping sources cost {eight:.3} µs a prop against one source's {one:.3}");
    }

    /// A prop with no ground under it is dropped, not left floating at the
    /// region's nominal height. A tree hanging in the air over a canyon reads
    /// as a bug; a missing tree reads as a canyon.
    #[test]
    fn a_prop_with_no_ground_under_it_is_dropped() {
        let s = src();
        let inst = scatter::chunk_instances(&s, (0, 0))[0];
        let mut nothing = |_: DVec3, _: Vec3, _: f32| None;
        assert!(settle(&s, inst, &mut nothing).is_none());
    }

    /// …and one that does have ground lands on it, taking the ground's real
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

    /// Cutting a prop must show this frame, not when the chunk next streams —
    /// so the cache is keyed on the removal set having changed, not on time.
    #[test]
    fn cutting_a_prop_invalidates_the_cache_immediately() {
        // Settled whole first, so the second frame's count differs by the cut
        // and nothing else.
        let mut cache = ScatterCache { settle_all: true, ..Default::default() };
        let mut s = src();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
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
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
            &floptle_render::Frustum::everything(),
            10_000, &mut out,
        );
        assert_eq!(out.len(), before - 1, "the cut prop was still drawn");
    }

    /// The draw budget is a hard stop. A source with a silly density must cost
    /// a frame-rate dip, never a frame that never ends.
    #[test]
    fn the_instance_budget_is_never_exceeded() {
        let mut cache = ScatterCache::default();
        let mut s = src();
        s.per_chunk = 512;
        s.bands = vec![Band { asset: "a".into(), distance: 400.0 }];
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
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
    /// a third-person camera swinging the eye several metres does.
    #[test]
    fn arriving_ground_is_settled_over_several_frames_not_all_at_once() {
        let s = big_field();
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
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
                &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground,
            &floptle_render::Frustum::everything(),
                20_000, &mut out,
            );
            worst = worst.max(casts);
            frames += 1;
            if casts == 0 || frames > 5000 {
                break;
            }
        }
        // Never a chunk's worth over the cap: props settle one at a time.
        assert!(worst <= SETTLE_BUDGET, "one frame settled {worst} props — the cap is {SETTLE_BUDGET}");
        // …and standing still, a warmed field costs no ground work at all.
        let mut casts = 0usize;
        let mut ground = |_: DVec3, _: Vec3, _: f32| {
            casts += 1;
            Some((60.0f32, Vec3::Y))
        };
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground,
            &floptle_render::Frustum::everything(), 20_000,
            &mut out,
        );
        assert_eq!(casts, 0, "a stationary camera re-settled ground it had already settled");
        assert!(!out.is_empty(), "…while still drawing the field");
    }

    /// **A sweep's arriving chunks are rolled within the frame's budget too.**
    /// The first frame over a field of small chunks used to roll every chunk
    /// the sweep found — thousands — before a single prop settled, and that
    /// roll was the spike walking through a game's flora showed. A slow ray
    /// costs at most one over the budget, and a shot settles the lot at once.
    #[test]
    fn arriving_chunks_are_rolled_within_the_budget_and_a_shot_settles_all() {
        let s = big_field();
        let keys = scatter::chunks_near(&s, DVec3::ZERO, s.range() as f64).len();
        assert!(keys > 2000, "not a field of small chunks: {keys}");
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        // A ray that takes a quarter of a millisecond, like one over real ground.
        let slow = |casts: &mut usize| {
            *casts += 1;
            let t = std::time::Instant::now();
            while t.elapsed().as_secs_f64() < 0.000_25 {}
            Some((60.0f32, Vec3::Y))
        };
        let mut cache = ScatterCache::default();
        for _ in 0..5 {
            let mut casts = 0usize;
            let mut ground = |_: DVec3, _: Vec3, _: f32| slow(&mut casts);
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 20_000, &mut out,
            );
            assert!(casts <= (SETTLE_MS / 0.25) as usize + 1, "one frame cast {casts} slow rays");
        }
        assert!(cache.len() < keys / 2, "the first frames rolled {} of {keys} chunks", cache.len());
        // No bound on the frame's wall time: on a shared runner any one frame
        // can be paused. The ray count above is the budget at work.

        let mut shot = ScatterCache { settle_all: true, ..Default::default() };
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut out = Vec::new();
        build_instances(
            &mut shot, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
            &floptle_render::Frustum::everything(), 200_000, &mut out,
        );
        assert_eq!(shot.len(), keys, "a shot left chunks unrolled");
        assert!(shot.chunks.values().all(|c| c.pending.is_empty()), "a shot left props unsettled");
    }

    /// **Walking through a planet's flora at 6 m/s keeps scatter inside its
    /// budget.** A field of about 2,500 resident props on a generated planet,
    /// settled through the physics world's ground query as the game does, the
    /// eye walking the surface for five seconds. No terrain chunk is meshed
    /// for a prop, and the field arrives. Settling by the ordinary ray meshed a
    /// collision chunk at a time on this walk, tens of milliseconds each.
    #[test]
    fn walking_through_a_planets_flora_stays_in_budget() {
        let radius = 150.0;
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            cave_depth: radius * 0.35,
            core_r: 10.5,
            ..Default::default()
        });
        let mut world = floptle_physics::PhysicsWorld::default();
        world.add_collider(Box::new(floptle_physics::ChunkTerrain::new(field)));
        let s = ScatterSource {
            region: Region::Sphere { center: DVec3::ZERO, radius: radius as f64 },
            chunk: 18.0,
            per_chunk: 30,
            bands: vec![Band { asset: "plant".into(), distance: 70.0 }],
            ..src()
        };
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut drawn = 0;
        for f in 0..300 {
            // 6 m/s along a great circle, the eye two metres off the ground.
            let a = f as f64 / 60.0 * 6.0 / radius as f64;
            let eye = DVec3::new(a.sin(), 0.2, a.cos()).normalize() * (radius as f64 + 2.0);
            let mut ms = 0.0f64;
            let mut ground = |from: DVec3, dir: Vec3, max: f32| {
                let t = std::time::Instant::now();
                let hit = world.raycast_ground(from.as_vec3(), dir, max).map(|h| (h.distance, Vec3::from(h.normal)));
                ms += t.elapsed().as_secs_f64() * 1e3;
                hit
            };
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 20_000, &mut out,
            );
            // The part the profile is told about covers the ground queries.
            assert!(cache.settle_ms as f64 + 0.05 >= ms, "the scatterSettle part ({} ms) missed {ms:.3} ms of queries", cache.settle_ms);
            drawn = out.len();
        }
        let meshed = world.colliders[0].shape.chunk_terrain().map_or(0, |t| t.meshed_chunks());
        assert_eq!(meshed, 0, "settling meshed {meshed} terrain chunks for collision");
        assert!(drawn > 1000, "the field never arrived: {drawn} props after five seconds");
        // No bound on wall time: a shared runner can pause any frame. A chunk
        // meshed on a prop's account is what made the spikes, and the count
        // above catches that whatever the machine is doing.
    }

    /// **A long flight over a planet's flora keeps a bounded cache.** Every
    /// chunk the eye passed stayed resolved and every prop that landed stayed
    /// in the landed map, so both grew for as long as you flew, and each time
    /// the map doubled it rehashed inside a prop's settle: frames of 11 to
    /// 21 ms while flying low. Flying 25 m up at 40 m/s for 40 s over ground
    /// partly meshed for drawing, the map holds a fraction of the props ever
    /// landed, and no terrain chunk is meshed on a prop's account (three were,
    /// at the edge of the drawn ground). Counts, not times: a shared runner
    /// can pause any frame.
    #[test]
    fn flying_low_over_flora_keeps_a_bounded_cache() {
        let radius = 600.0;
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            cave_depth: radius * 0.35,
            core_r: 10.5,
            ..Default::default()
        });
        let mut world = floptle_physics::PhysicsWorld::default();
        let mut terrain = floptle_physics::ChunkTerrain::new(field.clone());
        // The ground near the start is meshed for drawing, as it is round the
        // camera in a game; the flight crosses out of it.
        let mut adopted = 0;
        for c in field.chunk_coords() {
            let centre = (Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) + 0.5) * field.voxel() * floptle_field::CHUNK as f32;
            let dir = centre.normalize_or_zero();
            if dir.z > 0.95 && dir.x.abs() < 0.2 {
                terrain.adopt_mesh(c, &floptle_physics::drawn_chunk(&field, c));
                adopted += 1;
            }
        }
        world.add_collider(Box::new(terrain));
        let s = ScatterSource {
            region: Region::Sphere { center: DVec3::ZERO, radius: radius as f64 },
            chunk: 18.0,
            per_chunk: 30,
            bands: vec![Band { asset: "plant".into(), distance: 120.0 }],
            ..src()
        };
        let landed: Settled = Default::default();
        let mut cache = ScatterCache { settled_out: Some(landed.clone()), ..Default::default() };
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut landed_max = 0;
        let mut ever: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let frames = 2400;
        for f in 0..frames {
            let a = f as f64 / 60.0 * 40.0 / radius as f64;
            let eye = DVec3::new(a.sin(), 0.2, a.cos()).normalize() * (radius as f64 + 25.0);
            let mut ground = |from: DVec3, dir: Vec3, max: f32| {
                world.raycast_ground(from.as_vec3(), dir, max).map(|h| (h.distance, Vec3::from(h.normal)))
            };
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 20_000, &mut out,
            );
            landed_max = landed_max.max(landed.borrow().len());
            ever.extend(landed.borrow().keys().map(|k| k.1));
        }
        // 1.6 km of flight lands several times what is in range at once; kept
        // forever, the map would hold all of it.
        assert!(ever.len() > 2000, "the field never arrived: {} props landed", ever.len());
        assert!(
            landed_max * 3 < ever.len(),
            "the landed map kept what the flight left behind: at most {landed_max} of the {} props ever landed",
            ever.len()
        );
        let meshed = world.colliders[0].shape.chunk_terrain().map_or(0, |t| t.meshed_chunks());
        assert_eq!(meshed, adopted, "settling meshed {} terrain chunks for collision", meshed - adopted);
    }

    /// **The first ray of a frame counts against the budget too.** Ground
    /// whose every ray costs 3 ms, more than the whole budget: a frame used to
    /// cast one regardless, so every frame a field was arriving went 3 ms
    /// over. Now a ray that is expected not to fit waits, and the field still
    /// arrives, one ray every few frames.
    #[test]
    fn a_ray_dearer_than_the_budget_does_not_run_every_frame() {
        let s = src();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut cache = ScatterCache::default();
        let mut casts = 0usize;
        let frames = 64;
        for _ in 0..frames {
            let mut ground = |_: DVec3, _: Vec3, _: f32| {
                casts += 1;
                let t = std::time::Instant::now();
                while t.elapsed().as_secs_f64() < 0.003 {}
                Some((60.0f32, Vec3::Y))
            };
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 10_000, &mut out,
            );
        }
        // A cheap first guess lets a couple through before the estimate learns.
        assert!(casts <= frames / SLOW_RAY_EVERY as usize + 3, "{casts} 3 ms rays in {frames} frames");
        assert!(casts >= frames / SLOW_RAY_EVERY as usize, "the field stopped arriving: {casts} rays");
    }

    /// **`scatter.near` answers where a prop stands, once it has landed.** It
    /// rolled the region afresh and gave the region's nominal surface, so a
    /// harvest on a hillside aimed metres off the plant. A prop the field has
    /// dropped onto the ground now answers with that place, `settled = true`;
    /// one not settled yet says so.
    #[test]
    fn scatter_near_answers_with_the_settled_place() {
        let dir = std::env::temp_dir().join(format!("floptle_near_settled_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("field.lua"),
            "function start() id = scatter.create{ asset = 'a', halfX = 20, halfZ = 20, perChunk = 8, align = 'world' } end\n\
             function update() local hits = scatter.near(id, vec3(0, 0, 0), 15)\n\
             \x20 y, landed = hits[1] and hits[1].pos.y, hits[1] and hits[1].settled end\n",
        )
        .unwrap();
        let mut world = floptle_core::World::new();
        let e = world.spawn();
        world.insert(e, floptle_core::Transform::IDENTITY);
        world.insert(e, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("field")]));
        let mut host = floptle_script::ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let read = |host: &floptle_script::ScriptHost| -> (f64, bool) {
            let env = host.instance_env(e.index(), "field").unwrap();
            (env.get("y").unwrap(), env.get("landed").unwrap())
        };
        assert_eq!(read(&host), (0.0, false), "before settling: the region's surface, and saying so");
        // The ground is 2.5 m below the region's surface: dropped from 60 m up.
        let mut cache = ScatterCache { settle_all: true, settled_out: Some(host.scatter_settled()), ..Default::default() };
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((62.5f32, Vec3::Y));
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut out = Vec::new();
        {
            let sources = host.scatter_sources();
            build_instances(
                &mut cache, &sources, DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 10_000, &mut out,
            );
        }
        assert!(!out.is_empty());
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        let (y, landed) = read(&host);
        assert!(landed && (y + 2.5).abs() < 1e-6, "near answered y = {y} (settled {landed}), not the ground at -2.5");

        // The ground is dug away: the chunks there are forgotten, and settle
        // again onto nothing. `near` must not report a plant standing there.
        cache.invalidate_near(&host.scatter_sources(), DVec3::ZERO, 20.0);
        let mut nothing = |_: DVec3, _: Vec3, _: f32| None;
        out.clear();
        {
            let sources = host.scatter_sources();
            build_instances(
                &mut cache, &sources, DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut nothing,
                &floptle_render::Frustum::everything(), 10_000, &mut out,
            );
        }
        host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
        assert_eq!(read(&host), (0.0, false), "a prop with no ground left is still reported standing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A harvest costs no rays.** Cutting one prop used to re-settle every
    /// resident chunk of its source in that frame, because each compares the
    /// source's whole removal count. Now the cut prop goes and the rest stay
    /// where they stood; putting it back settles that one prop again.
    #[test]
    fn a_cut_re_settles_nothing_and_a_restore_brings_the_prop_back() {
        let mut cache = ScatterCache::default();
        let mut s = src();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut frame = |cache: &mut ScatterCache, s: &ScatterSource| {
            let mut casts = 0usize;
            let mut ground = |_: DVec3, _: Vec3, _: f32| {
                casts += 1;
                Some((60.0f32, Vec3::Y))
            };
            let mut out = Vec::new();
            build_instances(
                cache, std::slice::from_ref(s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
                &floptle_render::Frustum::everything(), 10_000, &mut out,
            );
            (out.len(), casts)
        };
        let mut drawn = 0;
        for _ in 0..50 {
            let (n, casts) = frame(&mut cache, &s);
            drawn = n;
            if casts == 0 {
                break;
            }
        }
        assert!(drawn > 10, "nothing settled");
        let victim = scatter::chunk_instances(&s, (0, 0))[0].id;
        s.removed.insert(victim);
        assert_eq!(frame(&mut cache, &s), (drawn - 1, 0), "a cut re-settled ground");
        s.removed.remove(&victim);
        assert_eq!(frame(&mut cache, &s), (drawn, 1), "a restore should settle the one prop it put back");
    }

    /// **Following a planet costs a lookup a source, not a walk of the
    /// level.** Each frame every anchored source asks where its node is; that
    /// was a pass over every named node in the scene, so a field's cost grew
    /// with everything else in the level. Timed against a level a hundred
    /// times bigger, as a ratio, and the frame is checked to be the node's.
    #[test]
    fn finding_a_sources_anchor_does_not_walk_the_level() {
        fn level(nodes: usize) -> (floptle_script::ScriptHost, floptle_core::World) {
            let dir = std::env::temp_dir().join(format!("floptle_scatter_anchor_{}_{nodes}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::fs::write(
                dir.join("field.lua"),
                "function start() for i = 1, 8 do scatter.create{ asset = 'rock.glb', parent = 'Planet' } end end\n",
            )
            .unwrap();
            let mut world = floptle_core::World::new();
            for i in 0..nodes {
                let e = world.spawn();
                world.insert(e, floptle_core::Transform::IDENTITY);
                world.insert(e, floptle_core::Name(format!("Rock {i}")));
            }
            let planet = world.spawn();
            world.insert(planet, floptle_core::Transform::from_translation(DVec3::new(9.0, 0.0, 0.0)));
            world.insert(planet, floptle_core::Name("Planet".into()));
            let e = world.spawn();
            world.insert(e, floptle_core::Transform::IDENTITY);
            world.insert(e, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("field")]));
            let mut host = floptle_script::ScriptHost::new();
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
            assert!(host.errors().is_empty(), "{:?}", host.errors());
            let _ = std::fs::remove_dir_all(&dir);
            (host, world)
        }
        let time = |host: &floptle_script::ScriptHost, world: &floptle_core::World| {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                for _ in 0..50 {
                    std::hint::black_box(anchor_frames(host, world));
                }
                best = best.min(t.elapsed().as_secs_f64());
            }
            best
        };
        let (small_host, small) = level(100);
        let (big_host, big) = level(10_000);
        let frames = anchor_frames(&big_host, &big);
        assert_eq!(frames.len(), 8);
        assert!(frames.iter().all(|(_, f)| f.origin == DVec3::new(9.0, 0.0, 0.0)), "{frames:?}");
        let (mut a, mut b) = (f64::MAX, f64::MAX);
        for _ in 0..3 {
            a = a.min(time(&small_host, &small));
            b = b.min(time(&big_host, &big));
        }
        assert!(b < a * 5.0, "a level 100x bigger made finding anchors {:.1}x slower", b / a);
    }

    /// The draw budget cuts the horizon, not your feet. The sweep is ordered
    /// nearest first for exactly this reason: a budget spent on whichever
    /// chunks a nested loop reached first leaves holes in front of the player
    /// and props behind the fog.
    #[test]
    fn a_full_budget_drops_the_far_props_and_keeps_the_near_ones() {
        let s = big_field();
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let eye = DVec3::ZERO;

        // Warm, so the settle budget isn't what's limiting this.
        for _ in 0..40 {
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground,
            &floptle_render::Frustum::everything(),
                20_000, &mut out,
            );
        }
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), eye, &mut mesh, &mut |_: &str| None, &mut ground,
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

    /// A body that orbits carries its props.
    ///
    /// The reported symptom: *"the scattered props seem to just be being left
    /// behind by the planet traveling in orbit"*. A region was pinned to the
    /// world at declare time, and the spawn planet moves at 99 units/s — so a
    /// 240-unit field was entirely behind its own planet in 2.4 seconds.
    ///
    /// What must not happen while it follows is anything being recomputed: same
    /// ids, same local positions, same settled heights, same removals. A rock
    /// stays the same rock.
    #[test]
    fn a_field_rides_its_planet_without_re_rolling_a_single_prop() {
        let mut s = src();
        s.anchor = Some("Umunquo".into());
        let mut cache = ScatterCache::default();
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);

        // Frame one: at the origin.
        let casts = std::cell::Cell::new(0usize);
        let mut ground = |_: DVec3, _: Vec3, _: f32| {
            casts.set(casts.get() + 1);
            Some((60.0f32, Vec3::Y))
        };
        // Stream in fully first — arriving ground settles over several frames
        // on purpose, and that is not what is being measured.
        let mut before = Vec::new();
        for _ in 0..40 {
            before.clear();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh,
                &mut |_: &str| None, &mut ground,
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
            &mut cache, std::slice::from_ref(&s), moved, &mut mesh, &mut |_: &str| None, &mut ground,
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
        // Every prop is in the same place on the planet as before. Draw
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
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh, &mut |_: &str| None, &mut ground,
            &floptle_render::Frustum::everything(),
            20_000, &mut out,
        );
        assert!(!out.is_empty(), "an unanchored field stopped drawing");
    }

    /// Mid-fade draws both bands. Drawing one and switching is the pop.
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
        let mut out = Vec::new();
        {
            let mut mesh = |a: &str| {
                asked.push(a.to_string());
                Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [7, 8])])
            };
            // Stand ~35 m away: inside the first band's 10 m fade window.
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::new(0.0, 0.0, 0.0), &mut mesh,
                &mut |_: &str| None,
                &mut ground, &floptle_render::Frustum::everything(), 10_000, &mut out,
            );
        }
        let both = asked.iter().any(|a| a == "near") && asked.iter().any(|a| a == "far");
        assert!(both, "no boundary was cross-dissolved; asked for {asked:?}");
        // The two halves of a dissolve sum to one, so the pair is never brighter
        // or dimmer than the single prop it replaces. Everything not mid-fade is
        // a plain opaque draw, so the fractional alphas are exactly the halves.
        let mut fades: Vec<f32> =
            out.iter().map(|(_, _, r)| r.color[3]).filter(|a| *a < 0.999).collect();
        assert!(!fades.is_empty(), "nothing was mid-fade");
        assert_eq!(fades.len() % 2, 0, "a dissolve half with no partner");
        while let (Some(a), Some(b)) = (fades.pop(), fades.pop()) {
            assert!((a + b - 1.0).abs() < 0.02, "cross-fade sums to {}, not 1", a + b);
        }
        // **Dissolved, not blended.** Each half wears its screen door — the
        // band leaving on the pattern, the band arriving on its complement —
        // so a crown is whole at every point of the fade. Blended at half
        // alpha each, every crown drew half see-through.
        let ext: Vec<u32> =
            out.iter().filter(|(_, _, r)| r.color[3] < 0.999).map(|(_, _, r)| floptle_render::ext_index_of(r)).collect();
        let (leaving, arriving) = (ext.iter().filter(|&&e| e == 7).count(), ext.iter().filter(|&&e| e == 8).count());
        assert!(leaving > 0 && leaving == arriving && leaving + arriving == ext.len(), "fade doors {ext:?}");
        assert!(out.iter().filter(|(_, _, r)| r.color[3] >= 0.999).all(|(_, _, r)| floptle_render::ext_index_of(r) == 0));
    }
    /// **The horizon dissolves.** Past the last band there is nothing to fade
    /// into, and props there were cut at the edge in a single step. Now the
    /// last band's fade window dissolves them out on the leaving pattern, the
    /// same way a band boundary does.
    #[test]
    fn the_last_band_dissolves_out_instead_of_cutting_off() {
        let mut cache = ScatterCache { settle_all: true, ..Default::default() };
        let mut s = src();
        s.bands = vec![Band { asset: "only".into(), distance: 60.0 }];
        s.fade = 20.0;
        // The cast starts 60 m above the region, so a 60 m hit is level ground.
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [7, 8])]);
        let mut out = Vec::new();
        build_instances(
            &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh,
            &mut |_: &str| None,
            &mut ground, &floptle_render::Frustum::everything(), 100_000, &mut out,
        );
        let dist = |r: &floptle_render::InstanceRaw| Vec3::from_slice(&r.model[3][..3]).length();
        assert!(out.iter().all(|(_, _, r)| dist(r) < 60.0), "a prop past the last band was drawn");
        let fading: Vec<_> = out.iter().filter(|(_, _, r)| dist(r) > 41.0).collect();
        assert!(!fading.is_empty(), "the field has no props in the last fade window");
        // Every prop in the window is part-way gone, on the leaving pattern,
        // and fainter toward the edge.
        for (_, _, r) in &fading {
            let a = r.color[3];
            assert!(a > 0.0 && a < 1.0, "a prop {:.1} m out, inside the last fade, drew at alpha {a}", dist(r));
            assert_eq!(floptle_render::ext_index_of(r), 7, "a dissolving prop is not on the leaving pattern");
        }
        assert!(out.iter().filter(|(_, _, r)| dist(r) < 39.0).all(|(_, _, r)| r.color[3] >= 0.999), "a prop well inside the band faded");
    }

    /// **A prop beside a walker leans away from it; one 5 m off stays put.**
    /// Sway was one wind for the whole scene, so nothing could part the grass
    /// in front of a player.
    #[test]
    fn a_prop_beside_an_interactor_leans_away_and_a_far_one_does_not() {
        let s = ScatterSource { bend: 1.0, ..src() };
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut mesh = |_: &str| Some(vec![(MeshId(0), None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])]);
        let mut build = |points: Vec<scatter::Interactor>| {
            let mut cache = ScatterCache { settle_all: true, interactors: points, ..Default::default() };
            let mut out = Vec::new();
            build_instances(
                &mut cache, std::slice::from_ref(&s), DVec3::ZERO, &mut mesh,
                &mut |_: &str| None,
                &mut ground, &floptle_render::Frustum::everything(), 100_000, &mut out,
            );
            out.into_iter().map(|(_, _, r)| Mat4::from_cols_array_2d(&r.model)).collect::<Vec<_>>()
        };
        let upright = build(Vec::new());
        assert!(upright.len() > 20, "the field should have plenty of props");
        // A walker half a metre to the −x side of the first prop, standing a
        // metre up (a position at the hip, not the feet).
        let base = upright[0].w_axis.truncate();
        let walker = scatter::Interactor {
            pos: (base - Vec3::X * 0.5 + Vec3::Y).as_dvec3(),
            radius: 1.5,
            strength: 1.0,
        };
        let bent = build(vec![walker]);
        assert_eq!(bent.len(), upright.len());
        let top = |m: &Mat4| m.transform_point3(Vec3::Y);
        let moved = top(&bent[0]) - top(&upright[0]);
        assert!(moved.x > 0.3, "the prop beside the walker did not lean away: its top moved {moved}");
        assert!(
            (bent[0].w_axis.truncate() - base).length() < 1e-4,
            "the prop's foot left the ground instead of the prop bending over it"
        );
        let mut far = 0;
        for (a, b) in upright.iter().zip(&bent) {
            let d = (a.w_axis.truncate() - walker.pos.as_vec3()) * Vec3::new(1.0, 0.0, 1.0);
            if d.length() >= 5.0 {
                far += 1;
                assert!(a.abs_diff_eq(*b, 1e-6), "a prop {:.1} m from the walker leaned", d.length());
            }
        }
        assert!(far > 10, "no props far from the walker to compare");
    }

    /// **Grass gives way and springs back; a rock does not move.** Every
    /// source bent, rocks and trunks included, and a bent prop snapped
    /// upright the frame its walker left. Now only a source with `bend`
    /// gives way, and a prop eases back up over about a second.
    #[test]
    fn only_grass_bends_and_it_springs_back_after_the_walker_leaves() {
        // A small field: this builds it seventy times.
        let small = Region::Ground { center: DVec3::ZERO, half: [12.0, 12.0] };
        let rock = ScatterSource { id: 1, region: small, bands: vec![Band { asset: "rock".into(), distance: 50.0 }], ..src() };
        let grass = ScatterSource { id: 2, bend: 1.0, region: small, bands: vec![Band { asset: "grass".into(), distance: 50.0 }], ..src() };
        let sources = [rock, grass];
        let mut ground = |_: DVec3, _: Vec3, _: f32| Some((60.0f32, Vec3::Y));
        let mut mesh = |a: &str| {
            let id = if a == "rock" { MeshId(1) } else { MeshId(2) };
            Some(vec![(id, None, Mat4::IDENTITY, MaterialParams::flat([1.0; 3]), [0, 0])])
        };
        let mut cache = ScatterCache { settle_all: true, ..Default::default() };
        // The worst lean among one source's props: how far a prop's top is
        // off its foot, across the ground.
        let mut build = |cache: &mut ScatterCache, t: f64| {
            cache.time = t;
            let mut out = Vec::new();
            build_instances(
                cache, &sources, DVec3::ZERO, &mut mesh, &mut |_: &str| None,
                &mut ground, &floptle_render::Frustum::everything(), 100_000, &mut out,
            );
            let tilt = |id: u32| {
                out.iter()
                    .filter(|(m, _, _)| m.0 == id)
                    .map(|(_, _, r)| {
                        let m = Mat4::from_cols_array_2d(&r.model);
                        let top = m.transform_point3(Vec3::Y) - m.w_axis.truncate();
                        (top - Vec3::Y * top.y).length()
                    })
                    .fold(0.0f32, f32::max)
            };
            (tilt(1), tilt(2))
        };
        let (rock0, grass0) = build(&mut cache, 0.0);
        assert!(rock0 < 1e-5 && grass0 < 1e-5, "the field leaned with nothing pressing on it");
        // A walker standing in the middle of the field.
        cache.interactors = vec![scatter::Interactor { pos: DVec3::new(0.0, 1.0, 0.0), radius: 6.0, strength: 1.0 }];
        let (rock, grass) = build(&mut cache, 1.0 / 60.0);
        assert!(rock < 1e-5, "a rock leaned {rock} away from the walker");
        assert!(grass > 0.2, "the grass beside the walker barely leaned: {grass}");
        // The walker leaves; the grass straightens over the frames after.
        cache.interactors.clear();
        let mut t = 1.0 / 60.0;
        let mut after = |cache: &mut ScatterCache, until: f64| {
            let mut last = (0.0, 0.0);
            while t < until {
                t += 1.0 / 60.0;
                last = build(cache, t);
            }
            last
        };
        let (_, soon) = after(&mut cache, 0.1);
        assert!(soon > grass * 0.4, "the grass snapped upright: {soon} a tenth of a second after leaning {grass}");
        let (_, later) = after(&mut cache, 1.1);
        assert!(later < grass * 0.05, "a second after the walker left the grass still leans {later} (from {grass})");
    }
}
