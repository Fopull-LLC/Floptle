//! The scene, as an extension sees it.
//!
//! Rebuilt once a frame from the ECS, before anything Lua runs. Extensions read
//! this and never the world: a mirror can be handed to a callback in the middle
//! of an egui pass, and a `&World` cannot.
//!
//! It carries what an authoring tool actually asks about a node — where it is,
//! how big it is, what it is made of, what it is called, what is attached — and
//! nothing that would make it expensive to build for a scene of ten thousand
//! nodes.

use std::collections::HashMap;

use floptle_core::math::DVec3;
use floptle_core::{Entity, Matter, World};

/// One node, flattened.
#[derive(Clone, Debug)]
pub(crate) struct MirrorNode {
    /// The entity index — the number Lua passes back for every edit.
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) parent: Option<u32>,
    pub(crate) children: Vec<u32>,
    /// A short kind name: `"mesh"`, `"camera"`, `"pointLight"`, `"empty"`…
    pub(crate) kind: &'static str,
    /// Local transform (relative to the parent).
    pub(crate) pos: [f64; 3],
    pub(crate) rot: [f32; 4],
    pub(crate) scale: [f32; 3],
    /// Absolute world position, parents applied.
    pub(crate) world_pos: [f64; 3],
    /// A bounding sphere in world units, or `None` for a node with no
    /// measurable geometry (a folder, a light, a camera).
    pub(crate) radius: Option<f32>,
    /// World-space half-extents of the node's oriented box, for
    /// [`SceneMirror::raycast`]. `None` for the same nodes `radius` is `None`
    /// for.
    pub(crate) half: Option<[f32; 3]>,
    /// What this node is as a piece of UI, or `None` if it is not one.
    ///
    /// A UI element is an ordinary node carrying an `ElementSpec`, so its
    /// `kind` reads `"empty"` — which leaves a package unable to tell a button
    /// from a folder. Anything that builds tooling for a screen needs this, and
    /// it is a few small fields rather than the whole spec.
    pub(crate) ui: Option<UiSummary>,
    pub(crate) tags: Vec<String>,
    pub(crate) layer: String,
    /// False when this node or any parent is switched off.
    pub(crate) visible: bool,
    /// Script kinds attached to this node.
    pub(crate) scripts: Vec<String>,
    /// The asset this node draws, when it draws one (`Mesh`'s glTF path).
    pub(crate) asset: Option<String>,
}

/// Every node in the open scene, plus the index that makes lookups cheap.
#[derive(Default)]
pub(crate) struct SceneMirror {
    pub(crate) nodes: Vec<MirrorNode>,
    /// id → position in `nodes`.
    by_id: HashMap<u32, usize>,
    /// Nodes with no parent, in scene order.
    pub(crate) roots: Vec<u32>,
    /// id → the node's full document, for `scene.doc`.
    ///
    /// **Empty unless a package has asked for one.** Building these means
    /// serialising every component of every node, which is far more work than
    /// the rest of the mirror put together — so a project whose packages never
    /// read a document pays nothing, and one that does pays only when the scene
    /// has actually changed (see `Editor::ext_mirror`).
    pub(crate) docs: HashMap<u32, serde_json::Value>,
}

impl SceneMirror {
    pub(crate) fn doc(&self, id: u32) -> Option<&serde_json::Value> {
        self.docs.get(&id)
    }

    pub(crate) fn get(&self, id: u32) -> Option<&MirrorNode> {
        self.by_id.get(&id).and_then(|i| self.nodes.get(*i))
    }

    /// Every node whose name matches, in scene order. Names are not unique and
    /// pretending they are is how a tool edits the wrong door.
    pub(crate) fn find_all(&self, name: &str) -> Vec<u32> {
        self.nodes.iter().filter(|n| n.name == name).map(|n| n.id).collect()
    }

    /// Build the mirror from the world.
    ///
    /// `radius_of` is handed in rather than computed here because a mesh's size
    /// lives in the editor's import registry, which this module has no business
    /// knowing about.
    pub(crate) fn build(
        world: &World,
        radius_of: &dyn Fn(Entity, &Matter) -> Option<f32>,
        extent_of: &dyn Fn(Entity, &Matter) -> Option<[f32; 3]>,
    ) -> Self {
        let mut mirror = SceneMirror::default();
        let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();

        for (e, name) in world.query::<floptle_core::Name>() {
            let id = e.index();
            let matter = world.get::<Matter>(e);
            let t = world.get::<floptle_core::Transform>(e).copied().unwrap_or_default();
            let parent = world.get::<floptle_core::Parent>(e).map(|p| p.0.index());
            if let Some(p) = parent {
                kids.entry(p).or_default().push(id);
            } else {
                mirror.roots.push(id);
            }
            let world_t = floptle_core::world_transform(world, e);
            let node = MirrorNode {
                id,
                name: name.0.clone(),
                parent,
                children: Vec::new(),
                kind: matter.map(kind_name).unwrap_or("empty"),
                pos: [t.translation.x, t.translation.y, t.translation.z],
                rot: [t.rotation.x, t.rotation.y, t.rotation.z, t.rotation.w],
                scale: [t.scale.x, t.scale.y, t.scale.z],
                world_pos: [
                    world_t.translation.x,
                    world_t.translation.y,
                    world_t.translation.z,
                ],
                radius: matter.and_then(|m| radius_of(e, m)).map(|r| {
                    // Local radii are pre-scale; the caller's number is not.
                    let s = world_t.scale;
                    r * s.x.abs().max(s.y.abs()).max(s.z.abs())
                }),
                half: matter.and_then(|m| extent_of(e, m)).map(|h| {
                    let s = world_t.scale;
                    [h[0] * s.x.abs(), h[1] * s.y.abs(), h[2] * s.z.abs()]
                }),
                ui: world.get::<floptle_ui::ElementSpec>(e).map(ui_summary),
                tags: world.get::<floptle_core::Tags>(e).map(|t| t.0.clone()).unwrap_or_default(),
                layer: world
                    .get::<floptle_core::Layer>(e)
                    .map(|l| l.0.clone())
                    .unwrap_or_default(),
                visible: !floptle_core::is_disabled(world, e),
                scripts: world
                    .get::<floptle_core::Scripts>(e)
                    .map(|s| s.0.iter().map(|i| i.kind.clone()).collect())
                    .unwrap_or_default(),
                asset: match matter {
                    Some(Matter::Mesh { asset_path }) => Some(asset_path.clone()),
                    _ => None,
                },
            };
            mirror.by_id.insert(id, mirror.nodes.len());
            mirror.nodes.push(node);
        }

        for (parent, children) in kids {
            if let Some(i) = mirror.by_id.get(&parent) {
                mirror.nodes[*i].children = children;
            }
        }
        mirror
    }

    /// A world-space axis-aligned box around a node.
    ///
    /// Built from the node's ORIENTED half-extents where it has them — the box
    /// is rotated by the node's rotation and the result is the smallest
    /// axis-aligned box containing it, which is tight for anything square-on
    /// and correct for anything turned. A node with no measurable geometry
    /// falls back to its bounding sphere, which is loose on anything long and
    /// thin and is said so plainly here because it changes what the number is
    /// good for.
    ///
    /// The oriented box itself is on [`MirrorNode::half`] beside the rotation —
    /// that is the pair to use when a box's ORIENTATION matters.
    pub(crate) fn aabb(&self, id: u32) -> Option<([f64; 3], [f64; 3])> {
        let n = self.get(id)?;
        let c = DVec3::from(n.world_pos);
        let e = match n.half {
            Some(half) => {
                // The extent of a rotated box along each world axis is the sum
                // of |axis · column| over the box's three axes — i.e. the
                // absolute value of the rotation matrix times the half-extents.
                let rot = floptle_core::math::Quat::from_xyzw(n.rot[0], n.rot[1], n.rot[2], n.rot[3]);
                let rot = if rot.is_normalized() { rot } else { floptle_core::math::Quat::IDENTITY };
                let m = floptle_core::math::Mat3::from_quat(rot);
                let h = floptle_core::math::Vec3::from(half);
                let e = floptle_core::math::Vec3::new(
                    m.x_axis.x.abs() * h.x + m.y_axis.x.abs() * h.y + m.z_axis.x.abs() * h.z,
                    m.x_axis.y.abs() * h.x + m.y_axis.y.abs() * h.y + m.z_axis.y.abs() * h.z,
                    m.x_axis.z.abs() * h.x + m.y_axis.z.abs() * h.y + m.z_axis.z.abs() * h.z,
                );
                e.as_dvec3()
            }
            None => DVec3::splat(n.radius.unwrap_or(0.0) as f64),
        };
        Some(((c - e).into(), (c + e).into()))
    }
}

/// The part of a UI element a tool needs to reason about it.
#[derive(Clone, Debug)]
pub(crate) struct UiSummary {
    /// `"button"`, `"slider"`, `"text"`, `"image"`, `"scroll"` or `"panel"`.
    ///
    /// One word rather than the flags it is derived from: a package asking
    /// "is this clickable" should not have to know that a button is a shape
    /// with `button: true` on it.
    pub(crate) element: &'static str,
    /// The label it draws, where it draws one. What a tool matches a name
    /// against — a button called "Button (3)" whose text says "Start Game" is
    /// the ordinary case, and the text is the useful half.
    pub(crate) text: String,
    /// Does it take a click at all? A panel is not interactive; a button is.
    pub(crate) interactive: bool,
    pub(crate) disabled: bool,
}

/// One word for what an element is, from the flags that make it that.
fn ui_summary(spec: &floptle_ui::ElementSpec) -> UiSummary {
    // Ordered by how specific each is: a slider that also carries text is a
    // slider, and a button that also carries text is a button.
    let element = if spec.slider.is_some() {
        "slider"
    } else if spec.button {
        "button"
    } else if spec.scroll.is_some() {
        "scroll"
    } else if spec.image.is_some() {
        "image"
    } else if spec.text.is_some() {
        "text"
    } else {
        "panel"
    };
    UiSummary {
        element,
        text: spec.text.as_ref().map(|t| t.text.clone()).unwrap_or_default(),
        interactive: spec.button || spec.slider.is_some() || spec.scroll.is_some(),
        disabled: spec.disabled,
    }
}

/// What a ray hit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RayHit {
    pub(crate) node: u32,
    /// Distance along the ray.
    pub(crate) t: f64,
    pub(crate) point: [f64; 3],
    pub(crate) normal: [f32; 3],
}

impl SceneMirror {
    /// Cast a world-space ray at the scene and return the nearest node it hits.
    ///
    /// **This tests each node's oriented BOX, not its triangles.** Said plainly
    /// because it decides what the answer is good for: it is exact for the
    /// built-in shapes, right to within an import bound for a model, and wrong
    /// for a doorway in a wall — a ray through the gap still hits the wall's
    /// box. It is the answer available from a per-frame mirror, and it is
    /// enough for the things tools actually do with a ray: find the ground under
    /// a point, pick what is in front of the camera, snap to a surface.
    pub(crate) fn raycast(&self, origin: [f64; 3], dir: [f64; 3], max: f64) -> Option<RayHit> {
        let ro = DVec3::from(origin);
        let rd = DVec3::from(dir);
        if rd.length_squared() < 1e-18 {
            return None;
        }
        let rd = rd.normalize();
        let mut best: Option<RayHit> = None;
        for n in &self.nodes {
            if !n.visible {
                continue;
            }
            let Some(half) = n.half else { continue };
            let rot = floptle_core::math::Quat::from_xyzw(n.rot[0], n.rot[1], n.rot[2], n.rot[3]);
            let rot = if rot.is_normalized() { rot } else { floptle_core::math::Quat::IDENTITY };
            // Into the box's own frame, where the test is three slabs.
            let inv = rot.inverse();
            let centre = DVec3::from(n.world_pos);
            let lo = (inv * (ro - centre).as_vec3()).as_dvec3();
            let ld = (inv * rd.as_vec3()).as_dvec3();
            let Some((t, axis, sign)) = ray_box(lo, ld, half) else { continue };
            if t < 0.0 || t > max || best.is_some_and(|b| t >= b.t) {
                continue;
            }
            let mut local_n = [0.0f32; 3];
            local_n[axis] = sign;
            let world_n = rot * floptle_core::math::Vec3::from(local_n);
            best = Some(RayHit {
                node: n.id,
                t,
                point: (ro + rd * t).into(),
                normal: [world_n.x, world_n.y, world_n.z],
            });
        }
        best
    }
}

/// Slab test of a ray against a box centred on the origin. Returns the entry
/// distance plus which axis it entered through and from which side, so the
/// caller can report a surface normal.
fn ray_box(ro: DVec3, rd: DVec3, half: [f32; 3]) -> Option<(f64, usize, f32)> {
    let (mut tmin, mut tmax) = (f64::NEG_INFINITY, f64::INFINITY);
    let (mut axis, mut sign) = (0usize, 1.0f32);
    for i in 0..3 {
        let h = half[i].abs() as f64;
        let (o, d) = (ro[i], rd[i]);
        if d.abs() < 1e-12 {
            // Parallel to this slab: a miss unless the ray starts inside it.
            if o < -h || o > h {
                return None;
            }
            continue;
        }
        let (mut t1, mut t2) = ((-h - o) / d, (h - o) / d);
        let mut s = -1.0f32;
        if t1 > t2 {
            std::mem::swap(&mut t1, &mut t2);
            s = 1.0;
        }
        if t1 > tmin {
            tmin = t1;
            axis = i;
            sign = s;
        }
        tmax = tmax.min(t2);
        if tmin > tmax {
            return None;
        }
    }
    // The box is entirely BEHIND the ray. Without this the clamp below turns
    // every box behind the camera into a hit at zero distance — which reads as
    // "there is something right here" everywhere you point.
    if tmax < 0.0 {
        return None;
    }
    // A ray starting INSIDE the box hits it at zero, not behind itself.
    Some((if tmin < 0.0 { 0.0 } else { tmin }, axis, sign))
}

/// The short kind name Lua sees. Deliberately not `format!("{matter:?}")`:
/// these are a public vocabulary an extension matches on, and they must not
/// change because a variant gained a field.
pub(crate) fn kind_name(m: &Matter) -> &'static str {
    match m {
        Matter::Primitive { .. } => "primitive",
        Matter::Blob { .. } => "blob",
        Matter::Mesh { .. } => "mesh",
        Matter::Empty => "empty",
        Matter::MapMesh { .. } => "mapMesh",
        Matter::Terrain { .. } => "terrain",
        Matter::Camera { .. } => "camera",
        Matter::PointLight { .. } => "pointLight",
        Matter::GravityVolume { .. } => "gravityVolume",
        Matter::NavMesh { .. } => "navMesh",
        Matter::NavLink { .. } => "navLink",
        Matter::NavArea { .. } => "navArea",
        Matter::FieldShape { .. } => "fieldShape",
        Matter::LightProbes { .. } => "lightProbes",
        Matter::ReflectionProbe { .. } => "reflectionProbe",
        Matter::Skybox { .. } => "skybox",
        Matter::PostProcess { .. } => "postProcess",
        Matter::Tilemap { .. } => "tilemap",
        Matter::SpriteBatch { .. } => "spriteBatch",
        Matter::WaterVolume { .. } => "waterVolume",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_a_parent_and_child() -> (World, Entity, Entity) {
        let mut w = World::new();
        let parent = w.spawn();
        w.insert(parent, floptle_core::Name("Room".into()));
        w.insert(parent, floptle_core::Matter::Empty);
        w.insert(
            parent,
            floptle_core::Transform::from_translation(DVec3::new(10.0, 0.0, 0.0)),
        );

        let child = w.spawn();
        w.insert(child, floptle_core::Name("Lamp".into()));
        w.insert(
            child,
            floptle_core::Matter::PointLight {
                color: [1.0; 3],
                intensity: 1.0,
                range: 5.0,
                shape: floptle_core::LightShape::Point,
                shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25,
            },
        );
        w.insert(child, floptle_core::Transform::from_translation(DVec3::new(0.0, 2.0, 0.0)));
        w.insert(child, floptle_core::Parent(parent));
        (w, parent, child)
    }

    #[test]
    fn the_mirror_carries_the_tree_and_both_transforms() {
        let (w, parent, child) = world_with_a_parent_and_child();
        let m = SceneMirror::build(&w, &|_, _| None, &|_, _| None);
        assert_eq!(m.nodes.len(), 2);
        let p = m.get(parent.index()).unwrap();
        let c = m.get(child.index()).unwrap();
        assert_eq!(p.name, "Room");
        assert_eq!(c.parent, Some(parent.index()));
        assert_eq!(p.children, vec![child.index()]);
        assert_eq!(m.roots, vec![parent.index()]);
        // LOCAL is what was authored; WORLD has the parent applied.
        assert_eq!(c.pos, [0.0, 2.0, 0.0]);
        assert_eq!(c.world_pos, [10.0, 2.0, 0.0]);
        assert_eq!(c.kind, "pointLight");
    }

    #[test]
    fn a_switched_off_parent_hides_its_children() {
        let (mut w, parent, child) = world_with_a_parent_and_child();
        w.insert(parent, floptle_core::Disabled);
        let m = SceneMirror::build(&w, &|_, _| None, &|_, _| None);
        assert!(!m.get(child.index()).unwrap().visible);
    }

    #[test]
    fn a_radius_is_scaled_into_world_units() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Box".into()));
        w.insert(
            e,
            floptle_core::Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] },
        );
        let mut t = floptle_core::Transform::IDENTITY;
        t.scale = floptle_core::math::Vec3::new(3.0, 1.0, 1.0);
        w.insert(e, t);
        let m = SceneMirror::build(&w, &|_, _| Some(2.0), &|_, _| Some([2.0, 2.0, 2.0]));
        assert_eq!(m.get(e.index()).unwrap().radius, Some(6.0));
    }

    /// A box turned 45° about Y is wider on both world axes than its own
    /// half-extents — the whole reason the oriented box is kept separately.
    /// A node with no oriented box still answers `bounds`, from its sphere.
    #[test]
    fn a_node_with_only_a_radius_falls_back_to_its_sphere() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Blob".into()));
        w.insert(e, floptle_core::Matter::Empty);
        w.insert(e, floptle_core::Transform::from_translation(DVec3::new(1.0, 0.0, 0.0)));
        let m = SceneMirror::build(&w, &|_, _| Some(3.0), &|_, _| None);
        let (min, max) = m.aabb(e.index()).unwrap();
        assert_eq!(min, [-2.0, -3.0, -3.0]);
        assert_eq!(max, [4.0, 3.0, 3.0]);
        assert!(m.get(e.index()).unwrap().half.is_none());
    }

    #[test]
    fn a_turned_box_widens_the_world_aligned_one() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Crate".into()));
        w.insert(e, floptle_core::Matter::Empty);
        let mut t = floptle_core::Transform::IDENTITY;
        t.rotation = floptle_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_4);
        w.insert(e, t);
        let m = SceneMirror::build(&w, &|_, _| Some(1.0), &|_, _| Some([1.0, 1.0, 1.0]));
        let (min, max) = m.aabb(e.index()).unwrap();
        // √2 on X and Z, untouched on Y.
        assert!((max[0] - 2.0f64.sqrt()).abs() < 1e-5, "{}", max[0]);
        assert!((max[2] - 2.0f64.sqrt()).abs() < 1e-5, "{}", max[2]);
        assert!((max[1] - 1.0).abs() < 1e-6, "{}", max[1]);
        assert!((min[0] + 2.0f64.sqrt()).abs() < 1e-5);
    }

    #[test]
    fn a_ray_finds_the_nearest_box_and_reports_where_it_entered() {
        let mut w = World::new();
        for (i, name) in ["Near", "Far"].iter().enumerate() {
            let e = w.spawn();
            w.insert(e, floptle_core::Name((*name).into()));
            w.insert(e, floptle_core::Matter::Empty);
            w.insert(
                e,
                floptle_core::Transform::from_translation(DVec3::new(0.0, 0.0, -(i as f64 * 10.0 + 5.0))),
            );
        }
        let m = SceneMirror::build(&w, &|_, _| Some(1.0), &|_, _| Some([1.0, 1.0, 1.0]));
        let hit = m.raycast([0.0, 0.0, 0.0], [0.0, 0.0, -1.0], 100.0).unwrap();
        assert_eq!(m.get(hit.node).unwrap().name, "Near");
        // The near face of a unit box centred 5 back is at 4.
        assert!((hit.t - 4.0).abs() < 1e-9, "{}", hit.t);
        assert_eq!(hit.normal, [0.0, 0.0, 1.0]);
        // A ray pointing the other way hits nothing…
        assert!(m.raycast([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], 100.0).is_none());
        // …and neither does one that stops short.
        assert!(m.raycast([0.0, 0.0, 0.0], [0.0, 0.0, -1.0], 3.0).is_none());
    }

    #[test]
    fn a_node_with_no_geometry_is_not_hit() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Folder".into()));
        w.insert(e, floptle_core::Matter::Empty);
        let m = SceneMirror::build(&w, &|_, _| None, &|_, _| None);
        assert!(m.raycast([0.0, 0.0, 10.0], [0.0, 0.0, -1.0], 100.0).is_none());
        assert!(m.get(e.index()).unwrap().half.is_none());
    }

    /// A switched-off node is not in the way — the same rule the viewport
    /// follows when you click through something you have hidden.
    #[test]
    fn a_hidden_node_is_not_hit() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Wall".into()));
        w.insert(e, floptle_core::Matter::Empty);
        w.insert(e, floptle_core::Disabled);
        let m = SceneMirror::build(&w, &|_, _| Some(1.0), &|_, _| Some([1.0, 1.0, 1.0]));
        assert!(m.raycast([0.0, 0.0, 10.0], [0.0, 0.0, -1.0], 100.0).is_none());
    }

    #[test]
    fn a_ray_that_starts_inside_a_box_hits_it_at_zero() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Room".into()));
        w.insert(e, floptle_core::Matter::Empty);
        let m = SceneMirror::build(&w, &|_, _| Some(1.0), &|_, _| Some([5.0, 5.0, 5.0]));
        let hit = m.raycast([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], 100.0).unwrap();
        assert_eq!(hit.node, e.index());
        assert_eq!(hit.t, 0.0);
    }

    /// The half-extents are scaled by the node's world scale, so a stretched
    /// floor is hit where it looks like it is.
    #[test]
    fn a_scaled_node_is_hit_at_its_scaled_edge() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Floor".into()));
        w.insert(e, floptle_core::Matter::Empty);
        let mut t = floptle_core::Transform::IDENTITY;
        t.scale = floptle_core::math::Vec3::new(10.0, 1.0, 10.0);
        w.insert(e, t);
        let m = SceneMirror::build(&w, &|_, _| Some(1.0), &|_, _| Some([1.0, 1.0, 1.0]));
        // 9 units out along X is still over the floor at scale 10.
        let hit = m.raycast([9.0, 20.0, 0.0], [0.0, -1.0, 0.0], 100.0).unwrap();
        assert_eq!(hit.node, e.index());
        assert!((hit.t - 19.0).abs() < 1e-9, "{}", hit.t);
        assert_eq!(hit.normal, [0.0, 1.0, 0.0]);
        // 11 units out is past its edge.
        assert!(m.raycast([11.0, 20.0, 0.0], [0.0, -1.0, 0.0], 100.0).is_none());
    }

    #[test]
    fn names_are_not_unique_and_find_says_so() {
        let mut w = World::new();
        for _ in 0..3 {
            let e = w.spawn();
            w.insert(e, floptle_core::Name("Door".into()));
            w.insert(e, floptle_core::Matter::Empty);
        }
        let m = SceneMirror::build(&w, &|_, _| None, &|_, _| None);
        assert_eq!(m.find_all("Door").len(), 3);
        assert!(m.find_all("Window").is_empty());
    }

    #[test]
    fn a_square_on_box_is_its_own_extents_around_the_node() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Box".into()));
        w.insert(
            e,
            floptle_core::Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] },
        );
        w.insert(e, floptle_core::Transform::from_translation(DVec3::new(1.0, 2.0, 3.0)));
        let m = SceneMirror::build(&w, &|_, _| Some(2.0), &|_, _| Some([2.0, 2.0, 2.0]));
        let (min, max) = m.aabb(e.index()).unwrap();
        assert_eq!(min, [-1.0, 0.0, 1.0]);
        assert_eq!(max, [3.0, 4.0, 5.0]);
    }

}
