//! Rigidbodies fitted to what a node draws.
//!
//! A body is the shape physics pushes around, and the node's mesh is the
//! shape you see. Typed in by hand, the two drift apart at the first resize,
//! and a crate that collides as a box a hand's width bigger than the one it
//! draws reads as the physics being broken. So a body starts as the mesh's
//! shape and size, **Fit to mesh** puts it back after an edit, and a Mesh
//! body reads the geometry itself every Play.
//!
//! Sizes are the node's own, before its scale, where the sim scales them (a
//! box, a mesh): scaling the node scales the body. A sphere's radius and a
//! capsule's are world units in the sim, so those are fitted at the node's
//! scale as it is now, and the Inspector says when a rescale has left them
//! behind.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use floptle_core::math::{Quat, Vec3};
use floptle_core::{BodyKind, BodyMode, Entity, Matter, RigidBody, Shape, World};
use floptle_physics::{BodyGeometry, Hull};

/// What a node draws, as physics sees it: its triangles, their convex hull
/// and their bounds, all local to the node and before its scale.
pub(crate) struct NodeShape {
    pub(crate) geometry: Arc<BodyGeometry>,
    pub(crate) hull: Option<Hull>,
    pub(crate) lo: Vec3,
    pub(crate) hi: Vec3,
    /// What it is, when it is one of the built-in shapes a body has an exact
    /// match for.
    pub(crate) primitive: Option<Shape>,
}

/// Density a fitted physics object is given: half water's, so a crate
/// floats half sunk and weighs what a wooden one would.
pub(crate) const FIT_DENSITY: f32 = 500.0;

thread_local! {
    /// The last shape measured per source, keyed by a fingerprint of the
    /// geometry it came from. A hull costs a pass over every vertex, and the
    /// Inspector asks every frame the node is selected.
    static CACHE: RefCell<HashMap<String, (u64, Rc<NodeShape>)>> = RefCell::new(HashMap::new());
}

fn fingerprint(verts: &[Vec3], indices: &[u32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    verts.len().hash(&mut h);
    indices.len().hash(&mut h);
    for v in verts {
        v.x.to_bits().hash(&mut h);
        v.y.to_bits().hash(&mut h);
        v.z.to_bits().hash(&mut h);
    }
    h.finish()
}

fn measured(key: String, verts: Vec<Vec3>, indices: Vec<u32>, primitive: Option<Shape>) -> Option<Rc<NodeShape>> {
    if verts.is_empty() {
        return None;
    }
    let fp = fingerprint(&verts, &indices);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).filter(|(f, _)| *f == fp).map(|(_, s)| s.clone())) {
        return Some(hit);
    }
    let (lo, hi) = verts
        .iter()
        .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| (lo.min(*p), hi.max(*p)));
    let hull = Hull::from_points(&verts);
    let shape = Rc::new(NodeShape { geometry: Arc::new(BodyGeometry { verts, indices }), hull, lo, hi, primitive });
    CACHE.with(|c| c.borrow_mut().insert(key, (fp, shape.clone())));
    Some(shape)
}

/// The geometry node `e` draws, or `None` for a node that draws none a body
/// can be fitted to (an empty, a light, a sprite: a sprite's body comes from
/// its art, `default_body_for`).
pub(crate) fn node_shape(
    world: &World,
    maps: &crate::map_edit::MapStore,
    project_root: &Path,
    e: Entity,
) -> Option<Rc<NodeShape>> {
    match world.get::<Matter>(e)? {
        Matter::Primitive { shape, .. } => {
            let m = crate::matter_catalog::primitive_mesh(*shape);
            let verts = m.vertices.iter().map(|v| Vec3::from(v.pos)).collect();
            measured(format!("prim:{shape:?}"), verts, m.indices.clone(), Some(*shape))
        }
        Matter::Blob { scale } => {
            let m = floptle_render::uv_sphere(0.85 * scale, 12, 16);
            let verts = m.vertices.iter().map(|v| Vec3::from(v.pos)).collect();
            measured(format!("blob:{}", scale.to_bits()), verts, m.indices.clone(), Some(Shape::Sphere))
        }
        Matter::MapMesh { id } => {
            let mesh = maps.meshes.get(id)?;
            let (verts, indices, _, _) =
                crate::map_edit::map_collision_geometry(mesh, floptle_core::math::Mat4::IDENTITY);
            measured(format!("map:{id}"), verts, indices, None)
        }
        Matter::Mesh { asset_path } => {
            let file = crate::project::resolve_asset_path(project_root, asset_path);
            let model = floptle_assets::gltf_import::geometry(&file).ok()?;
            let mut verts = Vec::new();
            let mut indices = Vec::new();
            for part in &model.parts {
                let base = verts.len() as u32;
                verts.extend(part.mesh.vertices.iter().map(|v| Vec3::from(v.pos)));
                indices.extend(part.mesh.indices.iter().map(|i| i + base));
            }
            measured(format!("mesh:{}", file.display()), verts, indices, None)
        }
        _ => None,
    }
}

/// `base` with its shape and size fitted to node `e`'s geometry, or `None`
/// when the node draws nothing to fit to. Everything that is not shape or
/// size (mode, bounce, locks) is kept.
///
/// A cube or a plane is a Box, a sphere a Sphere and a capsule a Capsule,
/// each its drawn size; scaled unevenly, a sphere or a capsule is no longer
/// one, and is fitted as a Mesh. Anything else, a blockout mesh or a model,
/// is a Mesh: its own geometry, which the sim reads every Play.
#[cfg(test)]
pub(crate) fn fit(
    world: &World,
    maps: &crate::map_edit::MapStore,
    project_root: &Path,
    e: Entity,
    base: &RigidBody,
) -> Option<RigidBody> {
    let shape = node_shape(world, maps, project_root, e)?;
    Some(fit_to(&shape, floptle_core::world_transform(world, e).scale, base))
}

/// [`fit`] from an already measured shape, at world scale `scale`.
pub(crate) fn fit_to(shape: &NodeShape, scale: Vec3, base: &RigidBody) -> RigidBody {
    let mut rb = *base;
    let s = scale.abs();
    let even = |a: f32, b: f32| (a - b).abs() <= 1e-3 * a.max(b).max(1e-6);
    // Half-size about the node's origin: the box a Box or a fallback is.
    let half = shape.lo.abs().max(shape.hi.abs()).max(Vec3::splat(0.01));
    match shape.primitive {
        Some(Shape::Cube) | Some(Shape::Plane) => {
            rb.kind = BodyKind::Box;
            rb.half_extents = half.to_array();
        }
        Some(Shape::Sphere) if even(s.x, s.y) && even(s.y, s.z) => {
            rb.kind = BodyKind::Sphere;
            rb.radius = half.max_element() * s.x;
        }
        Some(Shape::Capsule) if even(s.x, s.z) => {
            rb.kind = BodyKind::Capsule;
            rb.radius = half.x * s.x;
            rb.height = (shape.hi.y - shape.lo.y) * s.y;
        }
        _ => {
            rb.kind = BodyKind::Mesh;
            rb.half_extents = half.to_array();
        }
    }
    rb
}

/// Whether `rb`'s shape and size are what [`fit_to`] would give it.
pub(crate) fn matches_fit(rb: &RigidBody, fitted: &RigidBody) -> bool {
    let near = |a: f32, b: f32| (a - b).abs() <= 1e-3 * (1.0 + a.abs().max(b.abs()));
    rb.kind == fitted.kind
        && match rb.kind {
            BodyKind::Sphere => near(rb.radius, fitted.radius),
            BodyKind::Capsule => near(rb.radius, fitted.radius) && near(rb.height, fitted.height),
            BodyKind::Box | BodyKind::Mesh => {
                (0..3).all(|i| near(rb.half_extents[i], fitted.half_extents[i]))
            }
            BodyKind::Polygon => true,
        }
}

/// The volume, in world units, of the body `rb` makes on a node of world
/// scale `scale` whose geometry is `shape`.
pub(crate) fn body_volume(rb: &RigidBody, scale: Vec3, shape: Option<&NodeShape>) -> f32 {
    let s = scale.abs();
    let pi = std::f32::consts::PI;
    match rb.kind {
        BodyKind::Sphere => 4.0 / 3.0 * pi * rb.radius.powi(3),
        BodyKind::Capsule => {
            let r = rb.radius;
            let cyl = (rb.height - 2.0 * r).max(0.0);
            pi * r * r * cyl + 4.0 / 3.0 * pi * r.powi(3)
        }
        BodyKind::Box => 8.0 * rb.half_extents.iter().product::<f32>() * s.x * s.y * s.z,
        BodyKind::Mesh => match shape.and_then(|n| n.hull) {
            Some(h) => h.volume() * s.x * s.y * s.z,
            None => 8.0 * rb.half_extents.iter().product::<f32>() * s.x * s.y * s.z,
        },
        BodyKind::Polygon => {
            let [hx, hy] = rb.outline.half_extents();
            8.0 * hx * hy * rb.half_extents[2] * s.z
        }
    }
}

/// "Make it a physics object": a Dynamic body fitted to node `e`, that turns,
/// with the mass its volume would have at [`FIT_DENSITY`].
pub(crate) fn physics_object(
    world: &World,
    maps: &crate::map_edit::MapStore,
    project_root: &Path,
    e: Entity,
) -> Option<RigidBody> {
    let shape = node_shape(world, maps, project_root, e)?;
    let scale = floptle_core::world_transform(world, e).scale;
    let base = RigidBody { mode: BodyMode::Dynamic, rotates: true, restitution: 0.2, friction: 0.5, ..Default::default() };
    let mut rb = fit_to(&shape, scale, &base);
    rb.mass = (body_volume(&rb, scale, Some(&shape)) * FIT_DENSITY).clamp(0.05, 100_000.0);
    Some(rb)
}

/// Give every Mesh-shaped body in `ents` (every one in the world when
/// `None`) the geometry it is shaped by, for the sim to read as it builds
/// it. A node whose geometry cannot be read loses any it had, and its body
/// falls back to the box in its `half_extents`.
pub(crate) fn attach_geometry(
    world: &mut World,
    maps: &crate::map_edit::MapStore,
    project_root: &Path,
    ents: Option<&[Entity]>,
) {
    let targets: Vec<Entity> = match ents {
        Some(list) => list.to_vec(),
        None => world.query::<RigidBody>().map(|(e, _)| e).collect(),
    };
    for e in targets {
        if world.get::<RigidBody>(e).is_none_or(|rb| rb.kind != BodyKind::Mesh) {
            continue;
        }
        match node_shape(world, maps, project_root, e) {
            Some(s) => world.insert(e, (*s.geometry).clone()),
            None => {
                world.remove::<BodyGeometry>(e);
            }
        }
    }
}

/// The outline of a Mesh body, as segments in the node's turned frame about
/// its origin (add the node's position): the hull's edges for a body that
/// moves, every triangle edge for a Static one, which is what each collides
/// with.
pub(crate) fn mesh_body_segments(shape: &NodeShape, scale: Vec3, rot: Quat, mode: BodyMode) -> Vec<(Vec3, Vec3)> {
    let at = |v: Vec3| rot * (v * scale);
    if mode == BodyMode::Static {
        let g = &shape.geometry;
        let mut edges: Vec<(u32, u32)> = g
            .indices
            .as_chunks::<3>().0.iter()
            .flat_map(|t| [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])])
            .map(|(a, b)| (a.min(b), a.max(b)))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        return edges
            .into_iter()
            .take(6000)
            .map(|(a, b)| (at(g.verts[a as usize]), at(g.verts[b as usize])))
            .collect();
    }
    let Some(h) = shape.hull.and_then(|h| h.scaled(scale)) else { return Vec::new() };
    let v = h.verts();
    h.edges().into_iter().map(|(a, b)| (rot * v[a], rot * v[b])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::DVec3;
    use floptle_core::Transform;

    fn node(world: &mut World, matter: Matter, scale: Vec3) -> Entity {
        let e = world.spawn();
        world.insert(e, Transform { translation: DVec3::ZERO, rotation: Quat::IDENTITY, scale });
        world.insert(e, matter);
        e
    }

    /// A Mesh body survives a save and a load, and a scene written before
    /// the shape existed still loads as the shape it was.
    #[test]
    fn a_mesh_body_round_trips_and_old_scenes_load_unchanged() {
        let rb = RigidBody { kind: BodyKind::Mesh, half_extents: [1.0, 2.0, 3.0], ..Default::default() };
        let doc = floptle_scene::RigidBodyDoc::from_rigidbody(&rb);
        let text = ron::to_string(&doc).unwrap();
        let back: floptle_scene::RigidBodyDoc = ron::from_str(&text).unwrap();
        assert_eq!(back.to_rigidbody().kind, BodyKind::Mesh);
        let old: floptle_scene::RigidBodyDoc = ron::from_str("(boxed: true, pushbox_only: true)").unwrap();
        let old = old.to_rigidbody();
        assert_eq!(old.kind, BodyKind::Box);
        assert!(old.pushbox_only);
        assert!(!ron::to_string(&floptle_scene::RigidBodyDoc::from_rigidbody(&old)).unwrap().contains("mesh"));
    }

    /// A cube fits as a Box its drawn size, and the size is the node's own,
    /// so scaling the node scales the body the sim builds.
    #[test]
    fn a_cube_fits_as_a_box_its_drawn_size() {
        let maps = crate::map_edit::MapStore::default();
        let mut w = World::default();
        let e = node(&mut w, Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] }, Vec3::new(2.0, 1.0, 0.5));
        let rb = fit(&w, &maps, Path::new("."), e, &RigidBody::default()).unwrap();
        assert_eq!(rb.kind, BodyKind::Box);
        for h in rb.half_extents {
            assert!((h - crate::matter_catalog::PRIMITIVE_HALF).abs() < 1e-4, "{:?}", rb.half_extents);
        }
    }

    /// A sphere fits as a Sphere of its drawn radius at the node's scale, and
    /// one scaled unevenly, which is no sphere, as a Mesh.
    #[test]
    fn a_sphere_fits_its_radius_and_an_uneven_one_its_mesh() {
        let maps = crate::map_edit::MapStore::default();
        let mut w = World::default();
        let e = node(&mut w, Matter::Primitive { shape: Shape::Sphere, color: [1.0; 3] }, Vec3::splat(2.0));
        let rb = fit(&w, &maps, Path::new("."), e, &RigidBody::default()).unwrap();
        assert_eq!(rb.kind, BodyKind::Sphere);
        assert!((rb.radius - 0.85 * 2.0).abs() < 1e-3, "radius {}", rb.radius);
        let squashed = node(&mut w, Matter::Primitive { shape: Shape::Sphere, color: [1.0; 3] }, Vec3::new(2.0, 1.0, 2.0));
        let rb = fit(&w, &maps, Path::new("."), squashed, &RigidBody::default()).unwrap();
        assert_eq!(rb.kind, BodyKind::Mesh);
    }

    /// A blockout mesh fits as a Mesh body: its own geometry, the box around
    /// it in `half_extents`, and a hull whose corners are the mesh's under
    /// the node's scale, off-centre modelling included.
    #[test]
    fn a_blockout_mesh_fits_as_its_own_geometry_under_scale() {
        let mut maps = crate::map_edit::MapStore::default();
        let mut mesh = floptle_map::box_mesh(Vec3::new(1.0, 0.5, 0.25));
        for v in &mut mesh.verts {
            *v += Vec3::new(0.25, 0.0, 0.0);
        }
        maps.meshes.insert(7, mesh);
        let mut w = World::default();
        let scale = Vec3::new(1.5, 2.0, 0.5);
        let e = node(&mut w, Matter::MapMesh { id: 7 }, scale);
        let rb = fit(&w, &maps, Path::new("."), e, &RigidBody::default()).unwrap();
        assert_eq!(rb.kind, BodyKind::Mesh);
        assert_eq!(rb.half_extents, [1.25, 0.5, 0.25]);
        let shape = node_shape(&w, &maps, Path::new("."), e).unwrap();
        assert!((shape.lo - Vec3::new(-0.75, -0.5, -0.25)).length() < 1e-5, "{:?}", shape.lo);
        assert!((shape.hi - Vec3::new(1.25, 0.5, 0.25)).length() < 1e-5, "{:?}", shape.hi);
        // What the sim builds from it: the drawn corners, scaled.
        attach_geometry(&mut w, &maps, Path::new("."), None);
        w.insert(e, rb);
        attach_geometry(&mut w, &maps, Path::new("."), None);
        let hull = floptle_physics::mesh_hull(&w, e).expect("the body is shaped by the mesh");
        let (lo, hi) = hull.bounds();
        assert!((lo - Vec3::new(-0.75, -0.5, -0.25) * scale).length() < 1e-4, "{lo:?}");
        assert!((hi - Vec3::new(1.25, 0.5, 0.25) * scale).length() < 1e-4, "{hi:?}");
        assert!((body_volume(&rb, scale, Some(&shape)) - 2.0 * 1.0 * 0.5 * 1.5).abs() < 1e-3);
    }
}
