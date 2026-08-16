//! Baking a scene's collision geometry into a navmesh.
//!
//! The bake itself lives in `floptle-nav`, which knows nothing about scenes:
//! triangles in, polygons out. This is the half that knows what a scene is —
//! which nodes count as ground, where they are in the world, and where the
//! result goes.
//!
//! # What counts
//!
//! **Anything a character would collide with.** A node is baked when it carries
//! the collidable switch (or a static rigidbody, or is terrain), which is the
//! rule that stays right as a level changes: a wall built today blocks a path
//! today, without anybody remembering to tag it.
//!
//! Three things take a node back out, in this order:
//!
//! 1. [`NavMeshExclude`](floptle_core::NavMeshExclude) — this one, never, for
//!    any reason. A glass floor collides and is not ground.
//! 2. The node's layer is not in the volume's `layers` filter (empty = all).
//! 3. The node is switched off. An invisible wall standing where a disabled
//!    node used to be is the bug people spend an evening on, and it is just as
//!    bad in a navmesh as in the physics sim.
//!
//! # Where it goes
//!
//! `<project>/nav/<scene>.<id>.fnav`, beside the terrain fields and the light
//! bake, for the same reason: it is a build artefact measured in hundreds of
//! kilobytes and a `.ron` is a thing people read. The `id` is the node's, not
//! its entity index, because entity indices die on undo and reload.

use floptle_core::math::{DVec3, Mat4, Vec3};
use floptle_core::{Entity, Matter, World};
use floptle_nav::{NavMesh, NavSettings, OffLink, Tri};

/// The scene's navmesh node, if it has one.
///
/// Like the light-probe volume, one per scene is the shape that makes sense —
/// but unlike it, several are meaningful (a level can want two agent sizes), so
/// this answers with the first and the editor bakes the selected one.
pub(crate) fn nav_node(world: &World) -> Option<(Entity, Matter)> {
    world
        .query::<Matter>()
        .find(|(_, m)| matches!(m, Matter::NavMesh { .. }))
        .map(|(e, m)| (e, m.clone()))
}

/// What the Inspector shows about a bake without holding the world open.
///
/// A snapshot rather than a borrow, for the same reason the GI one is: the
/// Inspector already holds the world mutably while it draws the node's knobs,
/// and "how big is this bake" is a question about the whole editor.
#[derive(Clone, Default)]
pub(crate) struct NavStatus {
    /// Polygons in the bake currently in hand (0 = none yet).
    pub polys: usize,
    /// Walkable square metres in it.
    pub area: f32,
    /// How many disconnected islands. More than one is worth seeing: it is
    /// usually a door the character does not fit through.
    pub regions: usize,
    /// How long the last bake took.
    pub seconds: f32,
    /// The bake no longer matches the node's settings.
    pub stale: bool,
    /// How many nodes the filter currently selects, and their triangles. Shown
    /// BEFORE baking, because "0 nodes" and "a bake that came back empty" are
    /// the same screen otherwise, and only one of them is about the filter.
    pub sources: usize,
    pub triangles: usize,
    /// Settings that will quietly do something other than what they say.
    pub advice: Option<String>,
    /// The file this bake came off disk from, shown as a plain relative path.
    ///
    /// "My bake vanished when I reopened the project" is a report nobody can act
    /// on — not the person making it and not whoever reads it — until the panel
    /// says whether a file was found and which one. It is one line and it turns
    /// a mystery into a fact.
    pub file: Option<String>,
    /// A bake is running on another thread right now.
    pub baking: bool,
    /// The last bake's box left part of the level out — see
    /// [`coverage_warning`]. Carried to the Inspector as well as the Console
    /// because it is a fact about the bake you are looking at, and the panel is
    /// where somebody looks when a character will not walk somewhere.
    pub coverage: Option<String>,
}

/// What the Inspector should say about the navmesh this frame.
///
/// Runs every frame, so it counts the nodes a bake WOULD see without building
/// any of their triangles — gathering geometry means importing models off disk,
/// and doing that per frame to fill in a label would be a stall nobody could
/// account for. The triangle count comes from the last bake instead.
pub(crate) fn nav_status(
    world: &World,
    node: Option<&Matter>,
    held: NavHeld<'_>,
    project_root: &std::path::Path,
) -> NavStatus {
    let NavHeld { mesh: baked, seconds, triangles, file, baking, coverage } = held;
    let mut st =
        NavStatus { seconds, triangles, baking, coverage: coverage.cloned(), ..Default::default() };
    st.file = file.map(|p| {
        p.strip_prefix(project_root).unwrap_or(p).to_string_lossy().replace('\\', "/")
    });
    let Some(m) = node else { return st };
    let Matter::NavMesh { layers, .. } = m else { return st };

    st.sources = world.query::<Matter>().filter(|(e, _)| counts(world, *e, layers)).count();
    if let Some(s) = settings_of(m) {
        st.advice = floptle_nav::cell_size_advice(&s);
        if let Some(b) = baked {
            // The settings the bake was made with, against the ones on the node
            // now. This is the whole of "stale": a moved node or a moved wall
            // cannot be seen from here, and claiming otherwise would be worse
            // than saying nothing.
            st.stale = b.settings != s;
        }
    }
    if let Some(b) = baked {
        st.polys = b.polys.len();
        st.area = b.area();
        st.regions = {
            let mut seen: Vec<u32> = b.polys.iter().map(|p| p.region).collect();
            seen.sort_unstable();
            seen.dedup();
            seen.len()
        };
    }
    st
}

/// What the editor is holding, as opposed to what the world says.
///
/// One argument rather than five, because they travel together and always will:
/// they are all facts about the bake in hand.
pub(crate) struct NavHeld<'a> {
    pub mesh: Option<&'a NavMesh>,
    pub seconds: f32,
    pub triangles: usize,
    /// Where it came from on disk, if it was loaded or saved.
    pub file: Option<&'a std::path::Path>,
    /// One is running on another thread right now.
    pub baking: bool,
    /// What the last bake's box left out, if anything.
    pub coverage: Option<&'a String>,
}

/// Everything a bake needs from the scene, gathered before anything is built.
pub(crate) struct Gathered {
    pub tris: Vec<Tri>,
    pub sources: usize,
}

/// Why a bake is running, which is the whole of what it says when it finishes.
///
/// Three different silences are wanted here, so this is an enum rather than a
/// `quiet` flag: a bake somebody asked for reports what it made, a bake the
/// watcher started says nothing at all, and a bake that replaces a file this
/// engine could not read says so once — because that one happened without
/// anybody asking, and unexplained work is indistinguishable from a bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BakeReason {
    /// Somebody pressed Bake.
    Asked,
    /// The level changed and stopped changing, with `auto_rebake` on.
    Watched,
    /// The `.fnav` beside the scene could not be read, so it is being made
    /// again from the level it describes.
    Reread,
}

/// A bake running on another thread, and everything needed to put it in when it
/// lands.
///
/// The measurements are carried rather than re-read, because the world moves on
/// while a bake runs: applying the box that *this* bake measured, to the node it
/// was measured for, is the only version of this that stays true.
pub(crate) struct NavJob {
    rx: std::sync::mpsc::Receiver<(Option<NavMesh>, f32)>,
    entity: Entity,
    id: u32,
    triangles: usize,
    half: Vec3,
    shift: Option<Vec3>,
    areas: usize,
    settings: NavSettings,
    /// What the level hashed to when this started — see
    /// [`crate::Editor::nav_inputs_stamp`].
    stamp: u64,
    reason: BakeReason,
}

/// Whether `e` is level geometry for this volume's purposes.
fn counts(world: &World, e: Entity, layers: &[String]) -> bool {
    if world.get::<floptle_core::NavMeshExclude>(e).is_some() {
        return false;
    }
    if floptle_core::is_disabled(world, e) {
        return false;
    }
    if !layers.is_empty() {
        let name = world.get::<floptle_core::Layer>(e).map(|l| l.0.clone()).unwrap_or_default();
        // An unnamed node is on Default, which is the same rule physics applies.
        let name = if name.is_empty() { "Default".to_string() } else { name };
        if !layers.contains(&name) {
            return false;
        }
    }
    // Terrain is ground whether or not anybody ticked a box on it — it is the
    // one node type that exists to be stood on.
    if matches!(world.get::<Matter>(e), Some(Matter::Terrain { .. })) {
        return true;
    }
    let collidable = world.get::<floptle_core::Collidable>(e).is_some()
        || world.get::<floptle_core::MeshCollider>(e).is_some();
    let static_body = world
        .get::<floptle_core::RigidBody>(e)
        .is_some_and(|rb| rb.mode == floptle_core::BodyMode::Static);
    collidable || static_body
}

/// Add a box's twelve triangles, in world space.
fn push_box(out: &mut Vec<Tri>, centre: Vec3, half: Vec3, rot: floptle_core::math::Quat) {
    let c = |sx: f32, sy: f32, sz: f32| {
        centre + rot * Vec3::new(sx * half.x, sy * half.y, sz * half.z)
    };
    let v = [
        c(-1.0, -1.0, -1.0),
        c(1.0, -1.0, -1.0),
        c(1.0, -1.0, 1.0),
        c(-1.0, -1.0, 1.0),
        c(-1.0, 1.0, -1.0),
        c(1.0, 1.0, -1.0),
        c(1.0, 1.0, 1.0),
        c(-1.0, 1.0, 1.0),
    ];
    // Slope is judged on |normal.y|, so winding cannot cost a floor: a face that
    // points the wrong way is still a floor. It is worth getting right anyway —
    // the baker fills the inside of a solid by reading which way its top and
    // bottom look, so a box wound outward is a box a character cannot walk
    // through, and one wound inward is merely a box with an untidy middle.
    const FACES: [[usize; 4]; 6] = [
        [0, 1, 2, 3], // -y, looking down
        [4, 7, 6, 5], // +y, looking up
        [0, 1, 5, 4],
        [2, 3, 7, 6],
        [1, 2, 6, 5],
        [3, 0, 4, 7],
    ];
    for f in FACES {
        out.push(Tri::new(v[f[0]].into(), v[f[1]].into(), v[f[2]].into()));
        out.push(Tri::new(v[f[0]].into(), v[f[2]].into(), v[f[3]].into()));
    }
}

/// A sphere as a coarse latitude/longitude mesh.
///
/// Coarse on purpose: nothing about a sphere's navmesh needs resolving finely,
/// and a hundred triangles per rock is a bake that takes longer than the level
/// it is in.
fn push_sphere(out: &mut Vec<Tri>, centre: Vec3, r: f32) {
    const RINGS: usize = 6;
    const SEGS: usize = 10;
    let at = |i: usize, j: usize| {
        let phi = std::f32::consts::PI * i as f32 / RINGS as f32;
        let th = std::f32::consts::TAU * j as f32 / SEGS as f32;
        centre + Vec3::new(phi.sin() * th.cos(), phi.cos(), phi.sin() * th.sin()) * r
    };
    for i in 0..RINGS {
        for j in 0..SEGS {
            let (a, b, c, d) = (at(i, j), at(i, j + 1), at(i + 1, j + 1), at(i + 1, j));
            out.push(Tri::new(a.into(), b.into(), c.into()));
            out.push(Tri::new(a.into(), c.into(), d.into()));
        }
    }
}

/// Gather the triangles a bake should see.
///
/// `origin` is the world position everything is measured from — the navmesh
/// node's own translation. Geometry is baked RELATIVE to it in `f32`, the same
/// trade the physics sim makes (ADR-0015): residuals stay small and exact no
/// matter how far out the level sits.
pub(crate) fn gather(
    world: &World,
    origin: DVec3,
    layers: &[String],
    maps: &crate::map_edit::MapStore,
    terrains: &std::collections::HashMap<Entity, crate::terrain_edit::EditorTerrain>,
) -> Gathered {
    let mut tris: Vec<Tri> = Vec::new();
    let mut sources = 0usize;
    let ents: Vec<Entity> = world.query::<Matter>().map(|(e, _)| e).collect();
    for e in ents {
        if !counts(world, e, layers) {
            continue;
        }
        let t = floptle_core::world_transform(world, e);
        let local = (t.translation - origin).as_vec3();
        let s = t.scale;
        let m = Mat4::from_scale_rotation_translation(s, t.rotation, local);
        let before = tris.len();
        match world.get::<Matter>(e) {
            Some(Matter::Mesh { asset_path }) => {
                let path = asset_path.clone();
                let Ok(model) = floptle_assets::gltf_import::import(std::path::Path::new(&path))
                else {
                    continue;
                };
                for part in &model.parts {
                    let v = &part.mesh.vertices;
                    for i in part.mesh.indices.chunks_exact(3) {
                        let p = |k: usize| {
                            m.transform_point3(Vec3::from(v[i[k] as usize].pos)).into()
                        };
                        tris.push(Tri::new(p(0), p(1), p(2)));
                    }
                }
            }
            Some(Matter::MapMesh { id }) => {
                let Some(mesh) = maps.meshes.get(id) else { continue };
                for sm in floptle_map::triangulate(mesh) {
                    for i in sm.indices.chunks_exact(3) {
                        let p = |k: usize| {
                            m.transform_point3(Vec3::from(sm.positions[i[k] as usize])).into()
                        };
                        tris.push(Tri::new(p(0), p(1), p(2)));
                    }
                }
            }
            // Terrain is meshed by the same surface-nets pass the renderer uses,
            // so the navmesh sits on exactly the ground that is drawn.
            // Terrain is keyed by ENTITY in the editor's store, not by the
            // `id` on the component — the id keys the file on disk.
            Some(Matter::Terrain { .. }) => {
                let Some(terrain) = terrains.get(&e) else { continue };
                for (_, cm) in floptle_field::mesher::mesh_field(&terrain.field, 1) {
                    // A chunk mesh's positions are relative to its own origin.
                    let off = Vec3::from(cm.origin);
                    for i in cm.indices.chunks_exact(3) {
                        let p = |k: usize| {
                            m.transform_point3(Vec3::from(cm.positions[i[k] as usize]) + off)
                                .into()
                        };
                        tris.push(Tri::new(p(0), p(1), p(2)));
                    }
                }
            }
            // The same sizes the static colliders use, so what you path on is
            // what you bump into.
            Some(Matter::Primitive { shape, .. }) => match shape {
                floptle_core::Shape::Cube => {
                    push_box(&mut tris, local, Vec3::new(0.7 * s.x, 0.7 * s.y, 0.7 * s.z), t.rotation);
                }
                floptle_core::Shape::Plane => {
                    push_box(
                        &mut tris,
                        local,
                        Vec3::new(0.7 * s.x, 0.7 * s.y, 0.02 * s.z.max(1.0)),
                        t.rotation,
                    );
                }
                floptle_core::Shape::Sphere => {
                    push_sphere(&mut tris, local, 0.85 * s.max_element());
                }
                // A capsule as the box it stands in. Its round ends are not
                // walkable at any slope worth walking, so the difference never
                // reaches the result — and a capsule is usually a character
                // rather than the floor under one.
                floptle_core::Shape::Capsule => {
                    let r = 0.5 * s.x.max(s.z);
                    push_box(&mut tris, local, Vec3::new(r, 0.5 * s.y + r, r), t.rotation);
                }
            },
            _ => continue,
        }
        if tris.len() > before {
            sources += 1;
        }
    }
    Gathered { tris, sources }
}

/// The settings on a navmesh node, as the baker's own.
pub(crate) fn settings_of(m: &Matter) -> Option<NavSettings> {
    let Matter::NavMesh {
        agent_radius,
        agent_height,
        max_slope,
        step_height,
        cell_size,
        ..
    } = m
    else {
        return None;
    };
    Some(NavSettings {
        agent_radius: *agent_radius,
        agent_height: *agent_height,
        max_slope: *max_slope,
        step_height: *step_height,
        cell_size: *cell_size,
    })
}

/// The level's area volumes, and the names they gave the ground.
///
/// Gathered **after** the bake's anchor is known, because a volume has to be
/// measured in the same space as the triangles it paints — and auto bounds move
/// that space out from under anything gathered earlier.
///
/// Area 0 is always plain walkable ground. Names are matched case-insensitively
/// and share an id, so two volumes both called "water" are one kind of ground
/// however they were typed, and a script that says `"Water"` finds it.
pub(crate) fn gather_areas(
    world: &World,
    anchor: DVec3,
) -> (Vec<floptle_nav::AreaVolume>, Vec<floptle_nav::Area>, Vec<String>) {
    let mut areas = vec![floptle_nav::Area::walkable()];
    let mut volumes = Vec::new();
    let mut warnings = Vec::new();
    let ents: Vec<Entity> = world.query::<Matter>().map(|(e, _)| e).collect();
    for e in ents {
        let Some(Matter::NavArea { half_extents, area, cost, blocks, enabled }) =
            world.get::<Matter>(e).cloned()
        else {
            continue;
        };
        if !enabled || floptle_core::is_disabled(world, e) {
            continue;
        }
        let t = floptle_core::world_transform(world, e);
        let half = Vec3::from(half_extents) * t.scale;
        if half.min_element() <= 0.0 {
            continue;
        }
        let local = (t.translation - anchor).as_vec3();
        // World → the box's own frame, where the box is the cube from −1 to 1:
        // undo the translation, undo the rotation, then divide by the extents.
        let m = Mat4::from_scale_rotation_translation(half, t.rotation, local).inverse();
        let id = if blocks {
            floptle_nav::WALKABLE
        } else {
            // TRIMMED at registration, because everything that matches against
            // this list trims too — a stray space must not mint a second
            // "water " that every filter then misses.
            let name = {
                let n = area.trim();
                if n.is_empty() { "area".to_string() } else { n.to_string() }
            };
            match register_area(&mut areas, &name, cost) {
                Ok(i) => {
                    // First name in wins the cost — worth saying when a
                    // same-named volume disagrees, or the Inspector shows a
                    // number the bake is not using.
                    if (areas[i as usize].cost - cost).abs() > 1e-4 {
                        warnings.push(format!(
                            "two \"{name}\" volumes name different costs ({} and {cost}) — \
                             an area has ONE cost, and the first volume's won",
                            areas[i as usize].cost
                        ));
                    }
                    i
                }
                Err(()) => {
                    // More kinds of ground than a filter can name (32). The
                    // volume paints NOTHING rather than aliasing another
                    // area's id — and the bake says so, because silently
                    // merging it would look exactly like a volume in the
                    // wrong place.
                    warnings.push(format!(
                        "area \"{name}\" is one kind of ground too many (the bake holds {}) — \
                         this volume painted nothing; reuse a name or remove one",
                        floptle_nav::MAX_AREAS
                    ));
                    continue;
                }
            }
        };
        volumes.push(floptle_nav::AreaVolume {
            inverse: m.to_cols_array(),
            area: id,
            blocks,
        });
    }
    (volumes, areas, warnings)
}

/// The one place an area NAME becomes an id: find it case-insensitively, or
/// register it (`Err` = the bake is out of area slots). Volumes and links both
/// go through here, so a link can name an area no volume painted and a filter
/// still finds it.
fn register_area(areas: &mut Vec<floptle_nav::Area>, name: &str, cost: f32) -> Result<u8, ()> {
    if let Some(i) = areas.iter().position(|a| a.name.eq_ignore_ascii_case(name)) {
        return Ok(i as u8);
    }
    if areas.len() >= floptle_nav::MAX_AREAS {
        return Err(());
    }
    areas.push(floptle_nav::Area::new(name.to_string(), cost));
    Ok((areas.len() - 1) as u8)
}

/// The level's hand-placed links, in the bake's own space.
///
/// A link's name is its node's name, because that is what somebody would type
/// into `nav.link("front door", false)` — and a name that has to be kept in step
/// with a second field is a name that goes stale.
pub(crate) fn gather_links(
    world: &World,
    anchor: DVec3,
    areas: &mut Vec<floptle_nav::Area>,
) -> (Vec<OffLink>, Vec<String>) {
    let mut out: Vec<OffLink> = Vec::new();
    let mut warnings = Vec::new();
    let ents: Vec<Entity> = world.query::<Matter>().map(|(e, _)| e).collect();
    for e in ents {
        let Some(Matter::NavLink { id, to, bidirectional, cost, area, duration, enabled }) =
            world.get::<Matter>(e).cloned()
        else {
            continue;
        };
        if floptle_core::is_disabled(world, e) {
            continue;
        }
        let t = floptle_core::world_transform(world, e);
        let far = t.mul_transform(&floptle_core::Transform::from_translation(DVec3::new(
            to[0] as f64,
            to[1] as f64,
            to[2] as f64,
        )));
        let rel = |p: DVec3| {
            let d = p - anchor;
            [d.x as f32, d.y as f32, d.z as f32]
        };
        let name = world
            .get::<floptle_core::Name>(e)
            .map(|n| n.0.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("link {id}"));
        let mut link = OffLink::new(id, name, rel(t.translation), rel(far.translation));
        link.bidirectional = bidirectional;
        link.cost = cost;
        link.duration = duration;
        link.enabled = enabled;
        // A link's area name REGISTERS the area (default cost) rather than
        // silently resolving to plain ground when no volume happens to share
        // the name — "tag twenty links `jump` and exclude them all" has to
        // work without also painting a jump-coloured box somewhere.
        let area_name = area.trim();
        link.area = if area_name.is_empty() {
            floptle_nav::WALKABLE
        } else {
            match register_area(areas, area_name, 1.0) {
                Ok(i) => i,
                Err(()) => {
                    warnings.push(format!(
                        "link \"{}\": area \"{area_name}\" is one kind of ground too many — \
                         the link counts as plain ground",
                        link.name
                    ));
                    floptle_nav::WALKABLE
                }
            }
        };
        out.push(link);
    }
    // By id, so a bake is the same bake twice running whatever order the world
    // happened to hand its nodes back in.
    out.sort_by_key(|l| l.id);
    // Two links sharing an id are two links a script cannot tell apart —
    // `nav.link(id)` and a rebake's identity both stop meaning one thing.
    // (Editor copies mint fresh ids; this catches hand-written scene files.)
    for pair in out.windows(2) {
        if pair[0].id == pair[1].id {
            warnings.push(format!(
                "nav links \"{}\" and \"{}\" share id {} — scripts and route crossings can \
                 only ever reach the first; give each link its own id",
                pair[0].name, pair[1].name, pair[0].id
            ));
        }
    }
    (out, warnings)
}

/// Cut the gathered triangles down to the volume's box.
///
/// A triangle is kept when its own bounds OVERLAP the box — not when one of its
/// corners is inside it. That distinction is the whole of this function, and
/// getting it wrong ate a level: a floor is often two enormous triangles whose
/// corners are far outside any box you would draw around a room, and testing
/// corners threw the floor away while keeping every small object standing on
/// it. What came back was the tops of the furniture.
///
/// Overlapping bounds keeps a few triangles that only come near the box. That
/// costs a little grid and nothing else. The opposite mistake is silent, and
/// looks like a level that is somehow not walkable.
///
/// Nothing is actually cut: the baker's own bounds grow to whatever it is
/// given, so a triangle that straddles the edge keeps its far half.
pub(crate) fn clip(tris: Vec<Tri>, half: Vec3) -> Vec<Tri> {
    tris.into_iter()
        .filter(|t| {
            let (lo, hi) = t.bounds();
            lo[0] <= half.x
                && hi[0] >= -half.x
                && lo[1] <= half.y
                && hi[1] >= -half.y
                && lo[2] <= half.z
                && hi[2] >= -half.z
        })
        .collect()
}

/// What a hand-sized box leaves out of the level it was pointed at, if enough
/// to matter.
///
/// **A navmesh that covers one corner of the map looks exactly like a navmesh.**
/// It bakes cleanly, reports a healthy polygon count, draws a convincing
/// overlay, and characters walk on it — right up to the invisible edge, where
/// they stop, because off the bake there is nowhere to go and nothing anywhere
/// says why. That is the shape of failure this engine keeps meeting: not a
/// crash, a plausible result that answers a smaller question than the one asked.
///
/// So the bake measures what it was given, against the box it was told to use,
/// and says the two numbers out loud. Only when the gap is real: a level always
/// spills a little past a box drawn round it, and a warning that cries at 2%
/// gets turned off in a week.
///
/// `None` when the box covers the level, and when there is nothing to compare.
pub(crate) fn coverage_warning(tris: &[Tri], half: Vec3) -> Option<String> {
    if tris.is_empty() {
        return None;
    }
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for t in tris {
        for v in [t.a, t.b, t.c] {
            for i in 0..3 {
                lo[i] = lo[i].min(v[i]);
                hi[i] = hi[i].max(v[i]);
            }
        }
    }
    // Triangles arrive in the volume's own space, so the box is centred on the
    // origin and its corners are ±half.
    let outside = tris
        .iter()
        .filter(|t| {
            let (l, h) = t.bounds();
            !(l[0] <= half.x
                && h[0] >= -half.x
                && l[1] <= half.y
                && h[1] >= -half.y
                && l[2] <= half.z
                && h[2] >= -half.z)
        })
        .count();
    if outside == 0 {
        return None;
    }
    // The floor plan is what people mean by "the level" — a box that is short is
    // a different mistake with a different fix (its own advice, below), and
    // mixing them into one warning makes both easier to ignore.
    let (span_x, span_z) = (hi[0] - lo[0], hi[2] - lo[2]);
    let (box_x, box_z) = (half.x * 2.0, half.z * 2.0);
    let short = span_x > box_x * 1.1 || span_z > box_z * 1.1;
    let mostly = outside * 4 >= tris.len(); // a quarter of the level or more
    if !short && !mostly {
        return None;
    }
    let tall = hi[1] - lo[1] > half.y * 2.0 * 1.1;
    Some(format!(
        "the volume covers {box_x:.0} × {box_z:.0} m of a level that spans {span_x:.0} × \
         {span_z:.0} m{}, so {outside} of {} triangles were left out of it. Characters cannot path \
         where there is no bake — they walk to the edge of it and stop. Tick “fit the box to what \
         it finds” on the Nav Mesh node, or size the box to cover the ground you want walkable.",
        if tall { " and stands taller than the box" } else { "" },
        tris.len(),
    ))
}

/// Bake, and say how long it took — with the volumes that paint or carve the
/// ground, the links that join it up, and the names for the areas the volumes
/// used.
pub(crate) fn bake_with(
    tris: &[Tri],
    settings: &NavSettings,
    volumes: &[floptle_nav::AreaVolume],
    links: Vec<OffLink>,
    areas: &[floptle_nav::Area],
) -> (Option<NavMesh>, f32) {
    let t = std::time::Instant::now();
    let mesh = floptle_nav::bake_with(tris, settings, volumes, links)
        .map(|m| if areas.is_empty() { m } else { m.with_areas(areas.to_vec()) });
    (mesh, t.elapsed().as_secs_f32())
}

/// What a `.fnav` starts with, so a file can say what it is before it says
/// anything else.
const MAGIC: &[u8; 4] = b"FNAV";

/// The format this engine writes.
///
/// **Postcard is not self-describing**, so `#[serde(default)]` buys nothing
/// here: adding one field to [`NavMesh`] changes the byte layout, and every file
/// written before it becomes unreadable. That is not hypothetical — v0.60 added
/// areas and links, and every bake made before it stopped loading. Silently,
/// because the reader swallowed the error and answered "there is no bake", which
/// is indistinguishable from never having baked at all. A whole level's bake
/// disappearing on reopen with the editor reporting nothing is the worst failure
/// this file has.
///
/// So a bake carries its version, and a reader that does not recognise one
/// **says so, by name, with what to do about it**. Bump this whenever
/// `NavMesh`'s serialized shape changes.
const VERSION: u32 = 2;

/// Why a bake could not be read. Every variant is something somebody can act on,
/// which is the whole reason this is not an `Option`.
#[derive(Debug)]
pub(crate) enum LoadError {
    /// No file. The ordinary state of a scene nobody has baked.
    Missing,
    /// Written by a different version of the engine.
    Version { found: Option<u32> },
    /// There, recognised, and damaged.
    Corrupt(String),
    Io(String),
}

impl LoadError {
    /// What to tell somebody who was expecting their bake to be there — or
    /// `None` when there is genuinely nothing to say.
    pub(crate) fn message(&self, path: &std::path::Path) -> Option<String> {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match self {
            // Not an event: a scene with no navmesh is most scenes.
            LoadError::Missing => None,
            LoadError::Version { found } => Some(format!(
                "navmesh: {name} was baked by {} and cannot be read by this one. Nothing is wrong \
                 with your level — press Bake on the Nav Mesh node to make it again.",
                match found {
                    Some(v) => format!("format {v} of the engine"),
                    None => "an older version of the engine".into(),
                }
            )),
            LoadError::Corrupt(why) => Some(format!(
                "navmesh: {name} is damaged and could not be read ({why}). Press Bake to make it \
                 again."
            )),
            LoadError::Io(why) => Some(format!("navmesh: could not read {name}: {why}")),
        }
    }
}

/// Write a bake, version first.
pub(crate) fn save(path: &std::path::Path, mesh: &NavMesh) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = Vec::with_capacity(1 << 16);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    let body = postcard::to_stdvec(mesh)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    bytes.extend_from_slice(&body);
    std::fs::write(path, bytes)
}

/// Read a bake back, or say why not.
pub(crate) fn load(path: &std::path::Path) -> Result<NavMesh, LoadError> {
    if !path.exists() {
        return Err(LoadError::Missing);
    }
    let bytes = std::fs::read(path).map_err(|e| LoadError::Io(e.to_string()))?;
    // No header at all is a bake from before this file had one. There is
    // nothing to migrate — the fields it lacks were never written — so the only
    // honest thing to do is say which file and why.
    let Some(rest) = bytes.strip_prefix(MAGIC.as_slice()) else {
        return Err(LoadError::Version { found: None });
    };
    if rest.len() < 4 {
        return Err(LoadError::Corrupt("the file ends in its own header".into()));
    }
    let (version, body) = rest.split_at(4);
    let version = u32::from_le_bytes(version.try_into().unwrap_or_default());
    if version != VERSION {
        return Err(LoadError::Version { found: Some(version) });
    }
    postcard::from_bytes(body).map_err(|e| LoadError::Corrupt(e.to_string()))
}

impl crate::Editor {
    /// Where this navmesh's bake is saved.
    ///
    /// Keyed off the scene's real relative path, not its stem — two scenes
    /// called `main.ron` in different folders are two scenes, and keying on the
    /// stem is how the terrain store once had them overwrite each other
    /// (`floptle/0111`). The node's `id` follows, so a scene can hold more than
    /// one navmesh without them fighting over a file.
    pub(crate) fn nav_path(&self, id: u32) -> std::path::PathBuf {
        let mut p = self.scene_path();
        p.set_extension(format!("{id}.fnav"));
        p
    }

    /// Hand the baked navmesh to everything that reads one.
    ///
    /// **One call site per reader is how the two drift.** There are two readers
    /// — the running game's scripts and the editor's packages — and every place
    /// that changes the bake has to reach both. Every place reaches this
    /// instead, so adding a third reader is one line here rather than a search
    /// for the five that were easy to find and the one that was not.
    pub(crate) fn publish_nav_mesh(&mut self) {
        self.script_host.set_nav_mesh(self.nav_baked.clone());
        self.ext.set_nav_mesh(self.nav_baked.clone());
    }

    /// Load this scene's navmesh, if it has one baked.
    pub(crate) fn load_nav(&mut self) {
        self.nav_baked = None;
        self.nav_overlay = None;
        self.nav_seconds = 0.0;
        self.nav_triangles = 0;
        self.nav_coverage = None;
        let Some((_, Matter::NavMesh { id, .. })) = nav_node(&self.world) else {
            self.publish_nav_mesh();
            return;
        };
        let path = self.nav_path(id);
        // Silence here looks exactly like "the AI is broken": no outline, nil
        // paths, units standing still. Every way this can fail names the file
        // and what fixes it.
        match load(&path) {
            Ok(mesh) => {
                self.nav_loaded_from = Some(path);
                self.nav_baked = Some(mesh);
            }
            Err(err) => {
                self.nav_loaded_from = None;
                if let Some(msg) = err.message(&path) {
                    self.console.push(floptle_script::LogLevel::Warn, msg, None);
                }
                // **A bake this engine cannot read is not lost work — it is work
                // it can do again.** The level that produced it is open right
                // here, and baking is a function of that level. Telling somebody
                // to press a button to recompute something the editor could
                // recompute itself is the difference between a format change
                // costing an evening and costing nothing.
                //
                // Not for `Missing`: a scene nobody has ever baked must stay
                // unbaked. That is a choice, and making it for people would put
                // a bake in a project that never asked for one.
                self.nav_heal = matches!(err, LoadError::Version { .. } | LoadError::Corrupt(_));
            }
        }
        self.nav_overlay = None;
        self.publish_nav_mesh();
    }

    /// What the level currently looks like to a bake, as one number.
    ///
    /// Everything that would change the result and nothing that would not: the
    /// character's settings, and every node the filter selects with the pose it
    /// is in. Moving a wall changes it; moving the camera does not.
    ///
    /// It is a hash rather than a dirty flag because the question being asked is
    /// "does the bake in hand still describe this level", and a flag can only
    /// answer "something happened". Undo, a scene reload and a nudge that ends
    /// where it started all leave a flag set and this number unchanged.
    pub(crate) fn nav_inputs_stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let Some((_, matter)) = nav_node(&self.world) else { return 0 };
        let Matter::NavMesh { layers, .. } = &matter else { return 0 };
        if let Some(s) = settings_of(&matter) {
            for f in [s.agent_radius, s.agent_height, s.max_slope, s.step_height, s.cell_size] {
                f.to_bits().hash(&mut h);
            }
        }
        let mut count = 0u64;
        for (e, m) in self.world.query::<Matter>() {
            // A link or an area volume is an input too, and neither is
            // collidable — a door moved two metres has to be noticed.
            let is_nav_extra = matches!(m, Matter::NavLink { .. } | Matter::NavArea { .. });
            if !is_nav_extra && !counts(&self.world, e, layers) {
                continue;
            }
            count += 1;
            e.index().hash(&mut h);
            let t = floptle_core::world_transform(&self.world, e);
            for f in [t.translation.x, t.translation.y, t.translation.z] {
                f.to_bits().hash(&mut h);
            }
            for f in [
                t.rotation.x, t.rotation.y, t.rotation.z, t.rotation.w, t.scale.x, t.scale.y,
                t.scale.z,
            ] {
                f.to_bits().hash(&mut h);
            }
            crate::matter_catalog::matter_kind_label(m).hash(&mut h);
            match m {
                Matter::NavLink { to, bidirectional, cost, area, duration, enabled, .. } => {
                    for f in to {
                        f.to_bits().hash(&mut h);
                    }
                    (*bidirectional, *enabled, area).hash(&mut h);
                    (cost.to_bits(), duration.to_bits()).hash(&mut h);
                }
                Matter::NavArea { half_extents, area, cost, blocks, enabled } => {
                    for f in half_extents {
                        f.to_bits().hash(&mut h);
                    }
                    (area, *blocks, *enabled, cost.to_bits()).hash(&mut h);
                }
                _ => {}
            }
        }
        count.hash(&mut h);
        h.finish()
    }

    /// Start a bake if the level has changed and stopped changing.
    ///
    /// Called every frame while a Nav Mesh node has `auto_rebake` on. The wait
    /// is the point: dragging a wall across a room would otherwise start a bake
    /// on every frame of the drag, and every one of them would be wrong by the
    /// time it finished.
    pub(crate) fn tick_nav_autobake(&mut self, dt: f32) {
        // A file that would not read, remade. This runs a frame after the load
        // rather than during it because a bake gathers geometry — models come
        // off disk, terrain is meshed — and the scene has to have finished
        // arriving before any of that answers correctly.
        if self.nav_heal {
            self.nav_heal = false;
            if self.nav_job.is_none() {
                self.start_nav_bake(BakeReason::Reread);
            }
        }
        let auto = matches!(
            nav_node(&self.world),
            Some((_, Matter::NavMesh { auto_rebake: true, enabled: true, .. }))
        );
        if !auto || self.nav_job.is_some() {
            return;
        }
        let now = self.nav_inputs_stamp();
        if now != self.nav_watch_stamp {
            self.nav_watch_stamp = now;
            self.nav_watch_settled = 0.0;
            return;
        }
        if now == self.nav_baked_stamp {
            return; // the bake in hand already describes this level
        }
        self.nav_watch_settled += dt;
        // Long enough that a drag is one bake, short enough that it feels like
        // the editor noticed.
        if self.nav_watch_settled >= 0.4 {
            self.start_nav_bake(BakeReason::Watched);
        }
    }

    /// Take a finished background bake and put it in.
    ///
    /// Called every frame. The apply half runs here rather than on the worker
    /// because it writes the scene — the node's measured box, the file, the
    /// script host's copy — and none of that belongs on another thread.
    pub(crate) fn poll_nav_bake(&mut self) {
        let Some(job) = self.nav_job.as_ref() else { return };
        let Ok((mesh, seconds)) = job.rx.try_recv() else { return };
        let job = self.nav_job.take().expect("checked above");
        self.finish_nav_bake(job, mesh, seconds);
    }

    /// Bake the scene's navmesh, reporting what happened either way.
    pub(crate) fn bake_nav(&mut self) {
        self.start_nav_bake(BakeReason::Asked);
    }

    /// Gather everything a bake needs, then hand it to a worker thread.
    ///
    /// The gather stays here: it reads the world and imports models off disk,
    /// and neither of those can leave the main thread. What goes over the wall
    /// is the part that costs — voxelising, eroding and cutting a level into
    /// polygons — so the editor keeps drawing and a running game keeps its
    /// frame rate while its level is re-measured underneath it.
    ///
    /// See [`BakeReason`] for what each kind of bake says when it lands.
    fn start_nav_bake(&mut self, reason: BakeReason) {
        use floptle_script::LogLevel;
        let quiet = reason != BakeReason::Asked;
        if self.nav_job.is_some() {
            if !quiet {
                self.console.push(
                    LogLevel::Debug,
                    "navmesh: a bake is already running".into(),
                    None,
                );
            }
            return;
        }
        let stamp = self.nav_inputs_stamp();
        let Some((e, matter)) = nav_node(&self.world) else {
            if !quiet {
                self.console.push(
                    LogLevel::Warn,
                    "nothing to bake: the scene has no Nav Mesh node".into(),
                    None,
                );
            }
            return;
        };
        let Matter::NavMesh { id, auto_bounds, layers, half_extents, .. } = matter.clone() else {
            return;
        };
        let Some(settings) = settings_of(&matter) else { return };

        let origin = floptle_core::world_transform(&self.world, e).translation;
        let g = gather(&self.world, origin, &layers, &self.maps, &self.terrains);
        if g.tris.is_empty() {
            self.console.push(
                LogLevel::Warn,
                if g.sources == 0 {
                    "navmesh: nothing to bake. A navmesh bakes what a character would collide \
                     with — level geometry needs the collidable switch on it, and the layer \
                     filter has to include it."
                        .into()
                } else {
                    format!(
                        "navmesh: {} object(s) matched but produced no geometry to bake",
                        g.sources
                    )
                },
                None,
            );
            return;
        }
        let triangles = g.tris.len();

        // Auto bounds: measure what was found, put the node in the middle of it
        // and size the box to fit. Moving the node is deliberate — the box IS
        // the node, and a volume that claims to fit the level while sitting
        // somewhere else would be lying about the one thing it is for.
        let (tris, half, shift) = if auto_bounds {
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for t in &g.tris {
                for v in [t.a, t.b, t.c] {
                    for i in 0..3 {
                        lo[i] = lo[i].min(v[i]);
                        hi[i] = hi[i].max(v[i]);
                    }
                }
            }
            let centre = Vec3::new(
                (lo[0] + hi[0]) * 0.5,
                (lo[1] + hi[1]) * 0.5,
                (lo[2] + hi[2]) * 0.5,
            );
            // A hair of margin so geometry sitting exactly on the boundary is
            // inside the box rather than on its edge.
            let half = Vec3::new(hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]) * 0.5
                + Vec3::splat(settings.cell_size);
            let moved = g
                .tris
                .into_iter()
                .map(|t| {
                    let s = |p: [f32; 3]| [p[0] - centre.x, p[1] - centre.y, p[2] - centre.z];
                    Tri::new(s(t.a), s(t.b), s(t.c))
                })
                .collect();
            self.nav_coverage = None;
            (moved, half, Some(centre))
        } else {
            let half = Vec3::from(half_extents);
            // Measured BEFORE the cut, because afterwards there is nothing left
            // to compare against — and "the box is smaller than the level" is
            // invisible from the result. What comes back is a perfectly good
            // navmesh of one corner of the map.
            self.nav_coverage = coverage_warning(&g.tris, half);
            if let Some(msg) = self.nav_coverage.clone() {
                self.console.push(LogLevel::Warn, format!("navmesh: {msg}"), None);
            }
            (clip(g.tris, half), half, None)
        };

        // The bake is measured around where the node ENDS UP — auto bounds may
        // have just moved it — so a world-space question can be turned into a
        // mesh-space one later without anybody having to remember the offset.
        let anchor = match shift {
            Some(c) => origin + DVec3::new(c.x as f64, c.y as f64, c.z as f64),
            None => origin,
        };
        // Volumes and links are measured around the anchor, which auto bounds
        // may have just moved — so both are gathered here rather than alongside
        // the triangles.
        let (volumes, mut areas, area_warnings) = gather_areas(&self.world, anchor);
        let (links, link_warnings) = gather_links(&self.world, anchor, &mut areas);
        for w in area_warnings.into_iter().chain(link_warnings) {
            self.console.push(LogLevel::Warn, format!("navmesh: {w}"), None);
        }

        // Over the wall. Everything from here is arithmetic on numbers that have
        // already been read out of the world, so nothing the main thread does
        // next can change the answer underneath it.
        let (tx, rx) = std::sync::mpsc::channel();
        let named = areas.clone();
        std::thread::Builder::new()
            .name("nav-bake".into())
            .spawn(move || {
                let (mesh, seconds) = bake_with(&tris, &settings, &volumes, links, &named);
                let mesh = mesh.map(|m| m.anchored_at([anchor.x, anchor.y, anchor.z]));
                // A send that fails means the editor moved on — a scene change,
                // a second bake, a close. Dropping the result is correct.
                let _ = tx.send((mesh, seconds));
            })
            .ok();
        self.nav_job = Some(NavJob {
            rx,
            entity: e,
            id,
            triangles,
            half,
            shift,
            areas: areas.len(),
            settings,
            stamp,
            reason,
        });
    }

    /// Put a finished bake in: the measured box, the file, the running game.
    fn finish_nav_bake(&mut self, job: NavJob, mesh: Option<NavMesh>, seconds: f32) {
        use floptle_script::LogLevel;
        let NavJob { entity: e, id, triangles, half, shift, areas, settings, stamp, reason, .. } =
            job;
        // Whatever came back describes this level, right or wrong — so the
        // watcher stops asking for the same bake again either way. An empty
        // result that kept re-queueing would bake a hopeless level forever.
        self.nav_baked_stamp = stamp;
        let Some(mesh) = mesh else {
            self.console.push(
                LogLevel::Warn,
                format!(
                    "navmesh: no walkable ground in {triangles} triangles. Nothing was flat \
                     enough, low enough or wide enough for a character {:.1} m wide and \
                     {:.1} m tall.",
                    settings.agent_radius * 2.0,
                    settings.agent_height
                ),
                None,
            );
            return;
        };

        // **A bake made while the game is running is not the level's bake.** It
        // describes whatever the game has spawned, knocked down or moved this
        // session, and writing that over the authored file — or over the node's
        // measured box — would leave the project holding a navmesh nobody made
        // and nobody can reproduce. Pressing Stop has to give the level back
        // exactly as it was. So a play-time bake reaches the running game and
        // goes no further.
        if self.playing {
            self.nav_baked = Some(mesh);
            self.publish_nav_mesh();
            self.nav_overlay = None;
            self.nav_seconds = seconds;
            self.nav_triangles = triangles;
            return;
        }

        // Write the measured box (and the move) back onto the node, so what the
        // Inspector shows is what was actually baked.
        if let Some(centre) = shift
            && let Some(t) = self.world.get_mut::<floptle_core::Transform>(e)
        {
            t.translation += DVec3::new(centre.x as f64, centre.y as f64, centre.z as f64);
        }
        if let Some(Matter::NavMesh { half_extents, .. }) = self.world.get_mut::<Matter>(e) {
            *half_extents = [half.x, half.y, half.z];
        }

        let path = self.nav_path(id);
        let polys = mesh.polys.len();
        let area = mesh.area();
        match save(&path, &mesh) {
            Ok(()) => self.nav_loaded_from = Some(path.clone()),
            Err(err) => {
                // A bake that cannot be written is a bake that is gone the
                // moment the scene is closed, so this is an error rather than a
                // note — the work is real and it is about to be lost.
                self.nav_loaded_from = None;
                self.console.push(
                    LogLevel::Error,
                    format!(
                        "navmesh: baked, but could not save {}: {err}. It will be gone when this \
                         scene is closed.",
                        path.display()
                    ),
                    None,
                );
            }
        }
        // A link whose end missed the ground does nothing, for ever, and looks
        // exactly like a route that simply preferred the long way. Naming the
        // ones that missed is the whole difference between a feature people
        // trust and one they stop using.
        let lost: Vec<String> = mesh.unresolved_links().map(|l| l.name.clone()).collect();
        if !lost.is_empty() {
            self.console.push(
                LogLevel::Warn,
                format!(
                    "navmesh: {} nav link{} could not find the ground at one end and will do \
                     nothing: {}. Move the ends onto walkable floor — a link's mouth has to be \
                     somewhere a character could stand.",
                    lost.len(),
                    if lost.len() == 1 { "" } else { "s" },
                    lost.join(", ")
                ),
                None,
            );
        }
        let crossings = mesh.off_links.len() - lost.len();

        self.nav_baked = Some(mesh);
        self.publish_nav_mesh();
        self.nav_overlay = None;
        self.nav_seconds = seconds;
        self.nav_triangles = triangles;
        self.scene_dirty = true;
        match reason {
            // An automatic bake says nothing when it worked. A line every time
            // you nudge a wall is a Console nobody reads, and the failures above
            // still speak — those are the ones worth interrupting for.
            BakeReason::Watched => return,
            // Work nobody asked for explains itself, once. This bake happened
            // because opening the scene found a bake it could not read, and a
            // thread quietly using the machine is exactly the sort of thing that
            // should never be a mystery.
            BakeReason::Reread => {
                self.console.push(
                    LogLevel::Debug,
                    format!(
                        "navmesh: made again for this version of the engine — {polys} polygons \
                         over {area:.0} m². A one-off: it loads with the scene from here on."
                    ),
                    None,
                );
                return;
            }
            BakeReason::Asked => {}
        }
        // Independent appends — each count says its piece when it has one.
        let mut extras = String::new();
        if crossings > 0 {
            extras.push_str(&format!(", {crossings} link(s)"));
        }
        if areas > 1 {
            extras.push_str(&format!(", {} area(s)", areas - 1));
        }
        self.console.push(
            LogLevel::Debug,
            format!(
                "navmesh: {polys} polygons over {area:.0} m², from {triangles} triangles in \
                 {seconds:.2}s{extras}"
            ),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exclusion has to beat everything else, because it is what you reach
    /// for when the general rule is right and one object is not.
    #[test]
    fn nav_mesh_exclude_wins_over_being_collidable() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
        w.insert(e, floptle_core::Collidable);
        assert!(counts(&w, e, &[]), "a collidable cube is level geometry");
        w.insert(e, floptle_core::NavMeshExclude);
        assert!(!counts(&w, e, &[]), "and excluding it must beat that");
    }

    /// A node with no collider is scenery, however solid it looks.
    #[test]
    fn geometry_with_no_collider_is_not_ground() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
        assert!(!counts(&w, e, &[]));
    }

    /// A switched-off node is off for the navmesh too.
    #[test]
    fn a_disabled_node_is_not_baked() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
        w.insert(e, floptle_core::Collidable);
        w.insert(e, floptle_core::Disabled);
        assert!(!counts(&w, e, &[]));
    }

    /// The filter includes by name, and an unnamed node is on Default — the
    /// same rule physics applies, so the two cannot disagree about a node.
    #[test]
    fn the_layer_filter_includes_by_name_and_defaults_like_physics() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
        w.insert(e, floptle_core::Collidable);

        assert!(counts(&w, e, &[]), "no filter means every layer");
        assert!(!counts(&w, e, &["Level".to_string()]), "it is not on Level");
        assert!(counts(&w, e, &["Default".to_string()]), "an unnamed node is on Default");

        w.insert(e, floptle_core::Layer("Level".into()));
        assert!(counts(&w, e, &["Level".to_string()]));
        assert!(!counts(&w, e, &["Default".to_string()]));
    }

    /// Terrain is ground without anybody ticking a box, because that is the one
    /// node type that exists to be stood on.
    #[test]
    fn terrain_counts_without_being_marked_collidable() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Matter::Terrain { id: 1 });
        assert!(counts(&w, e, &[]));
        w.insert(e, floptle_core::NavMeshExclude);
        assert!(!counts(&w, e, &[]), "and it can still be taken out by hand");
    }

    /// A box has to come out solid, or the ground on top of it is not there.
    #[test]
    fn a_box_bakes_into_ground_you_can_stand_on() {
        let mut tris = Vec::new();
        push_box(&mut tris, Vec3::new(0.0, 0.0, 0.0), Vec3::new(4.0, 0.5, 4.0), floptle_core::math::Quat::IDENTITY);
        assert_eq!(tris.len(), 12);
        let s = NavSettings { cell_size: 0.25, agent_radius: 0.2, agent_height: 1.0, ..Default::default() };
        let mesh = floptle_nav::bake(&tris, &s).expect("the top of a box is walkable");
        assert!(mesh.area() > 20.0, "an 8x8 top should mostly survive: {}", mesh.area());
        // And you can walk across it.
        let p = mesh.path([-3.0, 0.5, -3.0], [3.0, 0.5, 3.0]).expect("both ends are on it");
        assert!(p.complete, "{p:?}");
    }

    /// A bake has to survive the round trip, or the sidecar is decoration.
    #[test]
    fn a_bake_round_trips_through_its_file() {
        let mut tris = Vec::new();
        push_box(&mut tris, Vec3::ZERO, Vec3::new(4.0, 0.5, 4.0), floptle_core::math::Quat::IDENTITY);
        let s = NavSettings { cell_size: 0.25, agent_radius: 0.2, agent_height: 1.0, ..Default::default() };
        let mesh = floptle_nav::bake(&tris, &s).unwrap();

        let dir = std::env::temp_dir().join("floptle-nav-roundtrip");
        let path = dir.join("scene.1.fnav");
        save(&path, &mesh).expect("write");
        let back = load(&path).expect("read");
        let _ = std::fs::remove_file(&path);

        assert_eq!(back.polys.len(), mesh.polys.len());
        assert_eq!(back.settings, mesh.settings);
        let a = mesh.path([-3.0, 0.5, -3.0], [3.0, 0.5, 3.0]).unwrap();
        let b = back.path([-3.0, 0.5, -3.0], [3.0, 0.5, 3.0]).unwrap();
        assert_eq!(a, b, "the reloaded mesh must answer the same question the same way");
    }

    /// The bug that ate a level. A floor is usually a couple of enormous
    /// triangles, and its corners are nowhere near the box you draw around a
    /// room — so a corner test throws the floor away and keeps the furniture,
    /// which bakes into the tops of things and nothing to walk on between them.
    #[test]
    fn a_floor_bigger_than_the_box_survives_being_clipped_to_it() {
        // 80 m of floor as two triangles, and a 24 x 16 x 32 volume inside it.
        let floor = vec![
            Tri::new([-40.0, 0.0, -40.0], [40.0, 0.0, -40.0], [-40.0, 0.0, 40.0]),
            Tri::new([40.0, 0.0, -40.0], [40.0, 0.0, 40.0], [-40.0, 0.0, 40.0]),
        ];
        let half = Vec3::new(12.0, 8.0, 16.0);
        assert_eq!(clip(floor.clone(), half).len(), 2, "the floor covers the box");

        // And something genuinely elsewhere is still dropped.
        let far = vec![Tri::new(
            [500.0, 0.0, 500.0],
            [504.0, 0.0, 500.0],
            [500.0, 0.0, 504.0],
        )];
        assert!(clip(far, half).is_empty());

        // A wall straddling the edge keeps its far half rather than a notch.
        let straddle =
            vec![Tri::new([10.0, 0.0, 0.0], [20.0, 0.0, 0.0], [10.0, 6.0, 0.0])];
        assert_eq!(clip(straddle, half).len(), 1);
    }

    /// Every way a bake can fail to load has to be tellable apart, because each
    /// one has a different thing to say to the person whose level it is.
    #[test]
    fn a_bake_that_will_not_load_says_which_kind_of_not_loading_it_is() {
        // Absent is not an event: most scenes have no navmesh.
        let missing = std::path::Path::new("/nonexistent/never.fnav");
        assert!(matches!(load(missing), Err(LoadError::Missing)));
        assert!(load(missing).unwrap_err().message(missing).is_none());

        let dir = std::env::temp_dir().join("floptle-nav-load-kinds");
        let _ = std::fs::create_dir_all(&dir);

        // A bake from before the header existed — which is every bake made
        // before v0.60, and the reason this whole mechanism is here.
        let old = dir.join("old.1.fnav");
        std::fs::write(&old, b"\x01\x02postcard-ish bytes with no header").unwrap();
        let err = load(&old).unwrap_err();
        assert!(matches!(err, LoadError::Version { found: None }));
        let said = err.message(&old).expect("this one must be said out loud");
        assert!(said.contains("old.1.fnav"), "it has to name the file: {said}");
        assert!(said.contains("Bake"), "and say what fixes it: {said}");

        // A header this engine does not know.
        let future = dir.join("future.1.fnav");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&99u32.to_le_bytes());
        bytes.extend_from_slice(b"whatever comes next");
        std::fs::write(&future, &bytes).unwrap();
        assert!(matches!(load(&future), Err(LoadError::Version { found: Some(99) })));

        // The right header over damaged contents is a different sentence again.
        let torn = dir.join("torn.1.fnav");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(b"\x7f\x7f\x7f");
        std::fs::write(&torn, &bytes).unwrap();
        assert!(matches!(load(&torn), Err(LoadError::Corrupt(_))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bake this engine cannot read is work it can do again, and doing it is
    /// better than asking. The flag is the whole of that decision, and the
    /// distinction it has to keep is between "unreadable" and "absent" — a scene
    /// nobody has baked must stay unbaked.
    #[test]
    fn a_bake_it_cannot_read_is_made_again_rather_than_left_to_the_person() {
        use floptle_core::Transform;
        let dir = std::env::temp_dir().join("floptle-nav-heal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scenes")).unwrap();

        let mut ed = crate::Editor {
            project_root: dir.clone(),
            scene_rel: "scenes/level.ron".into(),
            ..Default::default()
        };
        let nav = ed.world.spawn();
        ed.world.insert(nav, Transform::IDENTITY);
        ed.world.insert(nav, Matter::default_nav_mesh(1));

        // Nothing on disk: the ordinary state of a scene nobody has baked, and
        // baking one unasked would put a file in a project that never wanted it.
        ed.load_nav();
        assert!(!ed.nav_heal, "a scene with no bake must not be baked behind your back");

        // A bake from an older engine — the one that cost Ty a rebake every time
        // he opened the project.
        std::fs::write(ed.nav_path(1), b"postcard bytes from before the header").unwrap();
        ed.load_nav();
        assert!(ed.nav_baked.is_none(), "it genuinely could not be read");
        assert!(ed.nav_heal, "and the level that made it is open right here");

        // Damaged reads the same way: the fix is identical.
        let mut torn = MAGIC.to_vec();
        torn.extend_from_slice(&VERSION.to_le_bytes());
        torn.extend_from_slice(b"\x7f\x7f\x7f");
        std::fs::write(ed.nav_path(1), &torn).unwrap();
        ed.load_nav();
        assert!(ed.nav_heal);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A navmesh of one corner of the map looks exactly like a navmesh of the
    /// map: it bakes, it counts polygons, it draws. The only place the
    /// difference exists is between the box and the geometry, and this is the
    /// one moment anything can compare them.
    #[test]
    fn a_box_smaller_than_the_level_says_so_with_both_numbers() {
        // 800 m of floor, a 24 x 32 m volume, and a scattering of props over
        // the whole thing — the shape of a real level, and of the report.
        let mut level = vec![
            Tri::new([-400.0, 0.0, -400.0], [400.0, 0.0, -400.0], [-400.0, 0.0, 400.0]),
            Tri::new([400.0, 0.0, -400.0], [400.0, 0.0, 400.0], [-400.0, 0.0, 400.0]),
        ];
        for i in 0..40 {
            let x = -380.0 + i as f32 * 19.0;
            level.push(Tri::new([x, 0.0, 100.0], [x + 2.0, 0.0, 100.0], [x, 2.0, 100.0]));
        }
        let said = coverage_warning(&level, Vec3::new(12.0, 8.0, 16.0))
            .expect("a box this much smaller than its level has to say so");
        assert!(said.contains("24 × 32 m"), "the box, in metres: {said}");
        assert!(said.contains("800 × 800 m"), "and the level: {said}");
        assert!(said.contains("fit the box"), "and the one tick that fixes it: {said}");

        // A box that covers the level says nothing, and neither does one that
        // the level merely spills over the edge of — a warning that fires on
        // every bake is a warning nobody reads by the second week.
        let room = vec![
            Tri::new([-10.0, 0.0, -10.0], [10.0, 0.0, -10.0], [-10.0, 0.0, 10.0]),
            Tri::new([10.0, 0.0, -10.0], [10.0, 0.0, 10.0], [-10.0, 0.0, 10.0]),
        ];
        assert!(coverage_warning(&room, Vec3::new(12.0, 8.0, 16.0)).is_none());
        let overhang =
            vec![Tri::new([-10.0, 0.0, -10.0], [12.4, 0.0, -10.0], [-10.0, 0.0, 10.0])];
        assert!(
            coverage_warning(&overhang, Vec3::new(12.0, 8.0, 16.0)).is_none(),
            "a hair over the edge is not worth a warning"
        );
        assert!(coverage_warning(&[], Vec3::splat(4.0)).is_none());
    }

    /// The watcher has to see a wall move and ignore everything else, or an
    /// automatic rebake is either useless or a machine that never stops baking.
    #[test]
    fn the_level_stamp_notices_geometry_and_ignores_everything_else() {
        use floptle_core::Transform;
        let mut ed = crate::Editor::default();
        let nav = ed.world.spawn();
        ed.world.insert(nav, Transform::IDENTITY);
        ed.world.insert(nav, Matter::default_nav_mesh(1));

        let wall = ed.world.spawn();
        ed.world.insert(wall, Transform::IDENTITY);
        ed.world.insert(
            wall,
            Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] },
        );
        ed.world.insert(wall, floptle_core::Collidable);
        let before = ed.nav_inputs_stamp();

        // Something with no collider is not level geometry, so moving it cannot
        // change what a bake would produce.
        let prop = ed.world.spawn();
        ed.world.insert(prop, Transform::from_translation(DVec3::new(4.0, 2.0, 4.0)));
        ed.world.insert(
            prop,
            Matter::Primitive { shape: floptle_core::Shape::Sphere, color: [1.0; 3] },
        );
        assert_eq!(before, ed.nav_inputs_stamp(), "a node with no collider is not the level");

        // Moving the wall is exactly what this exists to catch.
        if let Some(t) = ed.world.get_mut::<Transform>(wall) {
            t.translation.x += 1.0;
        }
        let moved = ed.nav_inputs_stamp();
        assert_ne!(before, moved, "a wall moved and the stamp did not");

        // …and putting it back is the same level again, which a dirty flag
        // could never say.
        if let Some(t) = ed.world.get_mut::<Transform>(wall) {
            t.translation.x -= 1.0;
        }
        assert_eq!(before, ed.nav_inputs_stamp(), "the same level must hash the same");

        // A link counts too, though it is not collidable and bakes into no
        // geometry at all.
        let link = ed.world.spawn();
        ed.world.insert(link, Transform::IDENTITY);
        ed.world.insert(link, Matter::default_nav_link(1));
        let with_link = ed.nav_inputs_stamp();
        assert_ne!(before, with_link, "a new link changes what a bake would produce");
        if let Some(Matter::NavLink { to, .. }) = ed.world.get_mut::<Matter>(link) {
            to[2] = 9.0;
        }
        assert_ne!(with_link, ed.nav_inputs_stamp(), "a link's far end moved");
    }

    /// The failure this cost a user: bake, close, reopen, and the bake is gone
    /// with nothing on screen saying so.
    ///
    /// A round trip through the real save and load, asserting the reloaded mesh
    /// answers the same question — which is the only definition of "the bake
    /// survived" that matters.
    #[test]
    fn a_bake_survives_being_saved_and_opened_again() {
        let floor = vec![
            Tri::new([-8.0, 0.0, -8.0], [8.0, 0.0, -8.0], [-8.0, 0.0, 8.0]),
            Tri::new([8.0, 0.0, -8.0], [8.0, 0.0, 8.0], [-8.0, 0.0, 8.0]),
        ];
        let settings = NavSettings { cell_size: 0.25, ..Default::default() };
        let ladder = OffLink::new(4, "ladder", [-6.0, 0.0, 0.0], [6.0, 0.0, 0.0]);
        let (mesh, _) = bake_with(
            &floor,
            &settings,
            &[],
            vec![ladder],
            &[floptle_nav::Area::walkable(), floptle_nav::Area::new("mud", 4.0)],
        );
        let mesh = mesh.expect("this floor bakes");

        let dir = std::env::temp_dir().join("floptle-nav-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("scene.1.fnav");
        save(&path, &mesh).expect("write");
        let back = load(&path).expect("a bake written by this engine must read back");
        let _ = std::fs::remove_dir_all(&dir);

        // Everything v0.60 added has to come back too — those are exactly the
        // fields whose addition broke the format in the first place.
        assert_eq!(back.polys.len(), mesh.polys.len());
        assert_eq!(back.settings, mesh.settings);
        assert_eq!(back.areas, mesh.areas, "the area names have to survive");
        assert_eq!(back.off_links, mesh.off_links, "and so do the links");
        let question = ([-6.0, 0.5, -6.0], [6.0, 0.5, 6.0]);
        assert_eq!(
            mesh.path(question.0, question.1),
            back.path(question.0, question.1),
            "the reloaded mesh must answer the same question the same way"
        );
    }
}
