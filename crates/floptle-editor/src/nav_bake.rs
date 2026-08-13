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
use floptle_nav::{NavMesh, NavSettings, Tri};

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
    baked: Option<&NavMesh>,
    seconds: f32,
    triangles: usize,
) -> NavStatus {
    let mut st = NavStatus { seconds, triangles, ..Default::default() };
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

/// Everything a bake needs from the scene, gathered before anything is built.
pub(crate) struct Gathered {
    pub tris: Vec<Tri>,
    pub sources: usize,
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
    // Winding does not matter here: the baker takes |normal.y|, because a floor
    // whose triangles face down is still a floor and a level with one flipped
    // face should not have a hole in its navmesh.
    const FACES: [[usize; 4]; 6] = [
        [0, 1, 2, 3],
        [4, 5, 6, 7],
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

/// Cut the gathered triangles down to the volume's box.
///
/// A triangle is kept when any vertex is inside, which keeps the floor under a
/// wall that straddles the edge rather than leaving a notch there. Nothing is
/// clipped: the baker's own bounds grow to what it is given, and a metre of
/// overhang costs a metre of grid.
pub(crate) fn clip(tris: Vec<Tri>, half: Vec3) -> Vec<Tri> {
    let inside = |p: [f32; 3]| {
        p[0].abs() <= half.x && p[1].abs() <= half.y && p[2].abs() <= half.z
    };
    tris.into_iter().filter(|t| inside(t.a) || inside(t.b) || inside(t.c)).collect()
}

/// Bake, and say how long it took.
pub(crate) fn bake(tris: &[Tri], settings: &NavSettings) -> (Option<NavMesh>, f32) {
    let t = std::time::Instant::now();
    let mesh = floptle_nav::bake(tris, settings);
    (mesh, t.elapsed().as_secs_f32())
}

/// Write a bake.
pub(crate) fn save(path: &std::path::Path, mesh: &NavMesh) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = postcard::to_stdvec(mesh)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    std::fs::write(path, bytes)
}

/// Read a bake back.
///
/// A file that will not parse comes back as `None` rather than an error the
/// caller has to decide about: a stale or truncated build artefact means "there
/// is no bake", and the answer to that is to bake again.
pub(crate) fn load(path: &std::path::Path) -> Option<NavMesh> {
    let bytes = std::fs::read(path).ok()?;
    postcard::from_bytes(&bytes).ok()
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

    /// Load this scene's navmesh, if it has one baked.
    pub(crate) fn load_nav(&mut self) {
        self.nav_baked = None;
        self.nav_seconds = 0.0;
        self.nav_triangles = 0;
        let Some((_, Matter::NavMesh { id, .. })) = nav_node(&self.world) else {
            self.script_host.set_nav_mesh(None);
            return;
        };
        self.nav_baked = load(&self.nav_path(id));
        self.script_host.set_nav_mesh(self.nav_baked.clone());
    }

    /// Bake the scene's navmesh, reporting what happened either way.
    ///
    /// Synchronous, unlike the light bake. A navmesh over a level takes a
    /// fraction of a second where a GI bake takes minutes, and a progress bar
    /// for something that is already finished is more machinery than it is
    /// worth.
    pub(crate) fn bake_nav(&mut self) {
        use floptle_script::LogLevel;
        let Some((e, matter)) = nav_node(&self.world) else {
            self.console.push(
                LogLevel::Warn,
                "nothing to bake: the scene has no Nav Mesh node".into(),
                None,
            );
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
            (moved, half, Some(centre))
        } else {
            let half = Vec3::from(half_extents);
            (clip(g.tris, half), half, None)
        };

        // The bake is measured around where the node ENDS UP — auto bounds may
        // have just moved it — so a world-space question can be turned into a
        // mesh-space one later without anybody having to remember the offset.
        let anchor = match shift {
            Some(c) => origin + DVec3::new(c.x as f64, c.y as f64, c.z as f64),
            None => origin,
        };
        let (mesh, seconds) = bake(&tris, &settings);
        let mesh = mesh.map(|m| m.anchored_at([anchor.x, anchor.y, anchor.z]));
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
        if let Err(err) = save(&path, &mesh) {
            self.console.push(
                LogLevel::Error,
                format!("navmesh: baked, but could not save {}: {err}", path.display()),
                None,
            );
        }
        self.script_host.set_nav_mesh(Some(mesh.clone()));
        self.nav_baked = Some(mesh);
        self.nav_seconds = seconds;
        self.nav_triangles = triangles;
        self.scene_dirty = true;
        self.console.push(
            LogLevel::Debug,
            format!(
                "navmesh: {polys} polygons over {area:.0} m², from {triangles} triangles in \
                 {seconds:.2}s"
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

    /// Nothing to read is not an error to decide about — it is "bake again".
    #[test]
    fn an_unreadable_bake_reads_as_no_bake() {
        assert!(load(std::path::Path::new("/nonexistent/never.fnav")).is_none());
        let junk = std::env::temp_dir().join("floptle-nav-junk.fnav");
        std::fs::write(&junk, b"not a navmesh").unwrap();
        assert!(load(&junk).is_none());
        let _ = std::fs::remove_file(&junk);
    }
}
