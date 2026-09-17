//! The host's tests, one file per API surface. Helpers shared by every
//! file live here.

use std::path::Path;
use floptle_core::math::EulerRot;
use floptle_core::{Matter, ParticleSystem, RigidBody, Visible};
use super::*;
use crate::preprocess::*;
use floptle_core::transform::Transform;
use floptle_core::{Scripts, World};
use std::io::Write;

mod components;
mod host;
mod input;
mod logs;
mod materials;
mod math;
mod net;
mod params;
mod physics;
mod preprocess;
mod refs;
mod ticks;
mod ui;

pub(super) fn write_script(dir: &Path, name: &str, body: &str) {
    let mut f = std::fs::File::create(dir.join(format!("{name}.lua"))).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

/// The two-material floor a an earlier task test needs: `x < 0` is "Grass",
/// `x > 0` is "Boards", tagged as node 7.
pub(super) fn labelled_floor(eid: u32) -> floptle_physics::AnchoredCollider {
    let verts = [
        glam::Vec3::new(-4.0, 0.0, -4.0),
        glam::Vec3::new(0.0, 0.0, -4.0),
        glam::Vec3::new(0.0, 0.0, 4.0),
        glam::Vec3::new(-4.0, 0.0, 4.0),
        glam::Vec3::new(4.0, 0.0, -4.0),
        glam::Vec3::new(4.0, 0.0, 4.0),
    ];
    let mut c = floptle_physics::AnchoredCollider::world(Box::new(
        floptle_physics::TriMeshCollider::labelled(
            &verts,
            &[0, 1, 2, 0, 2, 3, 1, 4, 5, 1, 5, 2],
            &[0, 0, 1, 1],
            vec!["Grass".into(), "Boards".into()],
        ),
    ));
    c.eid = Some(eid);
    c
}

/// A collider that counts how many times anything asked it for a surface
/// label. The whole of criterion 5 in an earlier task is that this stays at
/// zero for a query nobody asks.
pub(super) struct CountsLabelAsks {
    inner: floptle_physics::TriMeshCollider,
    asks: std::sync::atomic::AtomicU32,
}

impl floptle_physics::CollisionShape for CountsLabelAsks {
    fn distance(&self, p: glam::Vec3) -> f32 {
        self.inner.distance(p)
    }
    fn normal(&self, p: glam::Vec3) -> glam::Vec3 {
        self.inner.normal(p)
    }
    fn face_label(&self, p: glam::Vec3) -> Option<&str> {
        self.asks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.face_label(p)
    }
}

pub(super) fn world_with_script(kind: &str) -> (World, Entity) {
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: kind.into(),
        enabled: true,
        params: vec![], refs: Vec::new(),
        strs: Vec::new(),
    }]));
    (world, e)
}

/// Build a world with a menu node (carrying `script`) and `n` buttons named
/// `Btn1`…`Btn<n>`, plus one plain box named `Scenery`.
pub(super) fn menu_world(script: &str, n: usize) -> (World, floptle_core::Entity, Vec<u32>) {
    let mut world = World::default();
    let menu = world.spawn();
    world.insert(menu, Transform::IDENTITY);
    world.insert(menu, floptle_core::Name("Menu".into()));
    world.insert(
        menu,
        Scripts(vec![floptle_core::ScriptInst {
            kind: script.into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut ids = Vec::new();
    for i in 1..=n {
        let b = world.spawn();
        world.insert(b, Transform::IDENTITY);
        world.insert(b, floptle_core::Name(format!("Btn{i}")));
        world.insert(b, floptle_ui::ElementSpec { button: true, ..Default::default() });
        ids.push(b.index());
    }
    let scenery = world.spawn();
    world.insert(scenery, Transform::IDENTITY);
    world.insert(scenery, floptle_core::Name("Scenery".into()));
    world.insert(scenery, floptle_ui::ElementSpec::default());
    (world, menu, ids)
}

pub(super) fn hull(eid: u32, x: f32) -> floptle_physics::BodyHull {
    floptle_physics::BodyHull {
        eid,
        pos: glam::Vec3::new(x, 0.0, 0.0),
        radius: 0.4,
        shape: floptle_physics::BodyShape::Capsule { half_height: 0.6 },
        up: glam::Vec3::Y,
        layer: 0,
    }
}
