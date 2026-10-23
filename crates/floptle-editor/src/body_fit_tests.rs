//! A blockout mesh made a physics object, end to end: fitted on Add, shaped
//! by its own geometry on Play, collided where it is drawn, and kept out of
//! the blockout room it is thrown about in.

use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{BodyKind, BodyMode, Collidable, Matter, RigidBody};

/// An editor with level gravity and a blockout floor whose top is y = 0;
/// with `walls`, four blockout walls around it `half` from the middle.
fn level(half: f32, walls: bool) -> crate::Editor {
    let mut ed = crate::Editor::default();
    let g = ed.world.spawn();
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 500.0 });
    let t = 0.1;
    let mut pieces = vec![(Vec3::new(0.0, -t, 0.0), Vec3::new(half + t, t, half + t))];
    if walls {
        pieces.extend([
            (Vec3::new(half + t, 2.0, 0.0), Vec3::new(t, 2.0, half + t)),
            (Vec3::new(-half - t, 2.0, 0.0), Vec3::new(t, 2.0, half + t)),
            (Vec3::new(0.0, 2.0, half + t), Vec3::new(half + t, 2.0, t)),
            (Vec3::new(0.0, 2.0, -half - t), Vec3::new(half + t, 2.0, t)),
        ]);
    }
    for (i, (c, h)) in pieces.into_iter().enumerate() {
        blockout(&mut ed, 100 + i as u32, floptle_map::box_mesh(h), Transform::from_translation(c.as_dvec3()));
    }
    ed
}

/// A blockout node, Collidable the way the Map tool makes one.
fn blockout(ed: &mut crate::Editor, id: u32, mesh: floptle_map::MapMesh, at: Transform) -> floptle_core::Entity {
    ed.maps.meshes.insert(id, mesh);
    let e = ed.world.spawn();
    ed.world.insert(e, at);
    ed.world.insert(e, Matter::MapMesh { id });
    ed.world.insert(e, Collidable);
    e
}

/// The drawn corners of blockout box `e` (half-size `h` about `off`), in
/// the world, from its node's transform.
fn drawn_corners(ed: &crate::Editor, e: floptle_core::Entity, h: Vec3, off: Vec3) -> Vec<Vec3> {
    let wt = floptle_core::world_transform(&ed.world, e);
    let mut out = Vec::new();
    for i in 0..8 {
        let s = Vec3::new(
            if i & 1 == 0 { -1.0 } else { 1.0 },
            if i & 2 == 0 { -1.0 } else { 1.0 },
            if i & 4 == 0 { -1.0 } else { 1.0 },
        );
        out.push(wt.translation.as_vec3() + wt.rotation * ((off + s * h) * wt.scale));
    }
    out
}

/// A blockout box turned 30° about two axes and scaled unevenly, made a
/// physics object: on Play the body's corners are the drawn corners to the
/// centimetre, and once it has fallen it rests on a face, its drawn corners
/// on the floor.
#[test]
fn a_turned_scaled_blockout_box_collides_where_it_is_drawn() {
    let mut ed = level(6.0, false);
    let h = Vec3::new(0.5, 0.5, 0.5);
    let off = Vec3::new(0.2, 0.0, 0.0);
    let mut mesh = floptle_map::box_mesh(h);
    for v in &mut mesh.verts {
        *v += off;
    }
    let rot = Quat::from_euler(EulerRot::XYZ, 30f32.to_radians(), 0.0, 30f32.to_radians());
    let at = Transform { translation: DVec3::new(0.0, 3.0, 0.0), rotation: rot, scale: Vec3::new(1.6, 0.5, 0.9) };
    let e = blockout(&mut ed, 1, mesh, at);
    let rb = ed.default_body_for(e);
    assert_eq!(rb.kind, BodyKind::Mesh, "a blockout mesh is fitted as its own geometry");
    assert!(rb.rotates && rb.mode == BodyMode::Dynamic);
    assert!(rb.mass > 1.0, "weighs what its volume would: {}", rb.mass);
    ed.world.insert(e, rb);
    ed.world.remove::<Collidable>(e);
    ed.toggle_play();
    assert!(ed.playing);
    let drawn = drawn_corners(&ed, e, h, off);
    let hull = floptle_physics::mesh_hull(&ed.world, e).expect("Play gave the body its mesh");
    let snap = ed.sim.as_ref().unwrap().body_snapshot(e.index()).unwrap();
    for v in hull.verts() {
        let w = snap.pos.as_vec3() + snap.orient * *v;
        let near = drawn.iter().map(|d| d.distance(w)).fold(f32::MAX, f32::min);
        assert!(near < 0.01, "a body corner {w:?} is {near:.3} from every drawn corner");
    }
    assert_eq!(hull.verts().len(), 8);
    for _ in 0..300 {
        ed.play_step(1.0 / 60.0, true);
    }
    let ys: Vec<f32> = drawn_corners(&ed, e, h, off).iter().map(|c| c.y).collect();
    let low = ys.iter().copied().fold(f32::MAX, f32::min);
    assert!(low.abs() < 0.03, "the lowest drawn corner should be on the floor: {ys:?}");
    assert_eq!(ys.iter().filter(|y| **y < low + 0.03).count(), 4, "it rests on a face: {ys:?}");
}

/// Thrown at 20 m/s into a blockout room's wall, and dropped 20 m onto its
/// floor, a fitted crate stays in the room.
#[test]
fn a_thrown_or_dropped_physics_object_stays_in_a_blockout_room() {
    let half = 4.0;
    for (start, vel) in [(DVec3::new(0.0, 1.0, 0.0), Vec3::new(20.0, 0.0, 0.0)), (DVec3::new(0.0, 20.0, 0.0), Vec3::ZERO)] {
        let mut ed = level(half, true);
        let h = Vec3::splat(0.3);
        let e = blockout(&mut ed, 1, floptle_map::box_mesh(h), Transform::from_translation(start));
        let rb = ed.default_body_for(e);
        ed.world.insert(e, rb);
        ed.toggle_play();
        ed.sim.as_mut().unwrap().set_body_velocity(e.index(), vel);
        let mut worst = 0.0f32;
        for _ in 0..240 {
            ed.play_step(1.0 / 60.0, true);
            for c in drawn_corners(&ed, e, h, Vec3::ZERO) {
                worst = worst.max((c.x.abs() - half).max(c.z.abs() - half).max(-c.y));
            }
        }
        assert!(worst < 0.05, "from {start:?} at {vel:?}: a drawn corner went {worst:.3} into the room's walls or floor");
    }
}

/// A cube primitive fitted on Add is a Box its drawn size, and stays a Box
/// that turns: it lands and settles flat.
#[test]
fn a_cube_given_a_rigidbody_is_a_crate_its_size() {
    let mut ed = level(6.0, false);
    let e = ed.world.spawn();
    ed.world.insert(
        e,
        Transform { translation: DVec3::new(0.0, 2.0, 0.0), rotation: Quat::from_rotation_z(0.6), scale: Vec3::splat(0.5) },
    );
    ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
    let rb = ed.default_body_for(e);
    assert_eq!(rb.kind, BodyKind::Box);
    assert!((rb.half_extents[0] - crate::matter_catalog::PRIMITIVE_HALF).abs() < 1e-4);
    assert!(rb.rotates);
    ed.world.insert(e, rb);
    ed.toggle_play();
    for _ in 0..300 {
        ed.play_step(1.0 / 60.0, true);
    }
    let wt = floptle_core::world_transform(&ed.world, e);
    let up = [Vec3::X, Vec3::Y, Vec3::Z].iter().map(|a| (wt.rotation * *a).y.abs()).fold(0.0, f32::max);
    assert!(up > 0.995, "not flat on a face: {up}");
    assert!((wt.translation.y as f32 - 0.35).abs() < 0.03, "resting at {}", wt.translation.y);
}

/// Fit to mesh is what Add gave, and an edit to the mesh shows as a body
/// that no longer matches.
#[test]
fn a_rescaled_sphere_no_longer_matches_its_fit() {
    let ed = crate::Editor::default();
    let mut w = floptle_core::World::default();
    let e = w.spawn();
    w.insert(e, Transform { translation: DVec3::ZERO, rotation: Quat::IDENTITY, scale: Vec3::splat(2.0) });
    w.insert(e, Matter::Primitive { shape: floptle_core::Shape::Sphere, color: [1.0; 3] });
    let shape = crate::body_fit::node_shape(&w, &ed.maps, &ed.project_root, e).unwrap();
    let rb = crate::body_fit::fit_to(&shape, Vec3::splat(2.0), &RigidBody::default());
    assert!(crate::body_fit::matches_fit(&rb, &crate::body_fit::fit_to(&shape, Vec3::splat(2.0), &rb)));
    assert!(!crate::body_fit::matches_fit(&rb, &crate::body_fit::fit_to(&shape, Vec3::splat(3.0), &rb)));
}
