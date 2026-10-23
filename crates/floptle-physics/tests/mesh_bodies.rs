//! Bodies shaped by their meshes: a mesh's hull tumbles, bounces and comes to
//! rest on a face; a box does the same; a placed tilt and scale is collided
//! as drawn; and neither goes through a blockout wall or floor at speed.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, BodyMode, RigidBody, Transform, World};
use floptle_physics::{BodyGeometry, GravityField, Sim, StaticTag};

const DT: f32 = 1.0 / 60.0;

fn build(ecs: &World) -> Sim {
    Sim::build_layered(
        ecs,
        &[],
        GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    )
}

/// The triangles of a closed box of half-size `h` about `c`, wound outward.
fn box_mesh(c: Vec3, h: Vec3) -> (Vec<Vec3>, Vec<u32>) {
    let mut v = Vec::new();
    for i in 0..8 {
        let s = Vec3::new(
            if i & 1 == 0 { -1.0 } else { 1.0 },
            if i & 2 == 0 { -1.0 } else { 1.0 },
            if i & 4 == 0 { -1.0 } else { 1.0 },
        );
        v.push(c + s * h);
    }
    let quads = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
    let mut idx = Vec::new();
    for q in quads {
        idx.extend_from_slice(&[q[0], q[1], q[2], q[0], q[2], q[3]]);
    }
    (v, idx)
}

/// A wedge (a ramp block): its convex hull has five faces, none of which is
/// a box's.
fn wedge_mesh() -> (Vec<Vec3>, Vec<u32>) {
    let v = vec![
        Vec3::new(-0.5, -0.4, -0.4),
        Vec3::new(0.5, -0.4, -0.4),
        Vec3::new(0.5, 0.4, -0.4),
        Vec3::new(-0.5, -0.4, 0.4),
        Vec3::new(0.5, -0.4, 0.4),
        Vec3::new(0.5, 0.4, 0.4),
    ];
    let idx = vec![0, 2, 1, 3, 4, 5, 0, 1, 4, 0, 4, 3, 1, 2, 5, 1, 5, 4, 0, 3, 5, 0, 5, 2];
    (v, idx)
}

/// A node at `at` turned `rot`, scaled `scale`, with a Dynamic turning Mesh
/// body shaped by `mesh`.
fn mesh_node(ecs: &mut World, at: DVec3, rot: Quat, scale: Vec3, mesh: (Vec<Vec3>, Vec<u32>)) -> floptle_core::Entity {
    let e = ecs.spawn();
    ecs.insert(e, Transform { translation: at, rotation: rot, scale });
    ecs.insert(e, RigidBody { kind: BodyKind::Mesh, rotates: true, friction: 0.6, ..Default::default() });
    ecs.insert(e, BodyGeometry { verts: mesh.0, indices: mesh.1 });
    e
}

/// A blockout floor and four walls, each a closed triangle mesh, the room
/// `half` across inside and walls `t` thick, the floor's top at y = 0.
fn blockout_room(sim: &mut Sim, half: f32, t: f32) {
    let tag = StaticTag { layer: 0, eid: 900, sensor: false };
    let pieces = [
        (Vec3::new(0.0, -t, 0.0), Vec3::new(half + t, t, half + t)),
        (Vec3::new(half + t, 2.0, 0.0), Vec3::new(t, 2.0, half + t)),
        (Vec3::new(-half - t, 2.0, 0.0), Vec3::new(t, 2.0, half + t)),
        (Vec3::new(0.0, 2.0, half + t), Vec3::new(half + t, 2.0, t)),
        (Vec3::new(0.0, 2.0, -half - t), Vec3::new(half + t, 2.0, t)),
    ];
    for (c, h) in pieces {
        let (v, i) = box_mesh(Vec3::ZERO, h);
        sim.add_static_mesh(c.as_dvec3(), &v, &i, tag);
    }
}

fn run(sim: &mut Sim, ecs: &World, secs: f32, mut each: impl FnMut(&Sim)) {
    for _ in 0..(secs / DT).round() as usize {
        sim.sync_dynamic_params(ecs);
        sim.step_tick(DT, None);
        each(sim);
    }
}

/// The world corners of a body built from `mesh` under `scale`, as the sim
/// has it now.
fn corners(sim: &Sim, e: floptle_core::Entity, mesh: &[Vec3], scale: Vec3) -> Vec<Vec3> {
    let s = sim.body_snapshot(e.index()).unwrap();
    mesh.iter().map(|v| s.pos.as_vec3() + s.orient * (*v * scale)).collect()
}

/// Which local axis of the body points up: the face it is resting on is the
/// opposite one. At rest on a face, one axis is within a few degrees of
/// vertical.
fn up_alignment(sim: &Sim, e: floptle_core::Entity) -> f32 {
    let o = sim.body_snapshot(e.index()).unwrap().orient;
    [Vec3::X, Vec3::Y, Vec3::Z].iter().map(|a| (o * *a).y.abs()).fold(0.0, f32::max)
}

/// Pushed off a ledge, a mesh body tips over the edge, falls, and comes to
/// rest flat on one of its faces on the floor below — not balanced on an
/// edge or a corner, and not still level the way a body that cannot turn
/// would land.
#[test]
fn a_hull_body_tumbles_off_a_ledge_and_rests_on_a_face() {
    let mut ecs = World::default();
    let mesh = box_mesh(Vec3::ZERO, Vec3::new(0.5, 0.25, 0.4));
    let e = mesh_node(&mut ecs, DVec3::new(0.2, 2.26, 0.0), Quat::IDENTITY, Vec3::ONE, mesh.clone());
    let mut sim = build(&ecs);
    let tag = StaticTag { layer: 0, eid: 901, sensor: false };
    // A ledge whose edge the body overhangs by more than half, and a floor.
    sim.add_static_box(DVec3::new(-2.0, 1.0, 0.0), Vec3::new(2.0, 1.0, 2.0), Quat::IDENTITY, tag);
    sim.add_static_box(DVec3::new(0.0, -0.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Quat::IDENTITY, tag);
    let mut most_tilt = 0.0f32;
    run(&mut sim, &ecs, 4.0, |sim| {
        most_tilt = most_tilt.max(1.0 - up_alignment(sim, e));
    });
    let s = sim.body_snapshot(e.index()).unwrap();
    assert!(most_tilt > 0.05, "it never tipped over the edge: most tilt {most_tilt}");
    assert!(s.pos.y < 1.0, "it never left the ledge: y = {:.3}", s.pos.y);
    let lowest = corners(&sim, e, &mesh.0, Vec3::ONE).iter().map(|c| c.y).fold(f32::MAX, f32::min);
    assert!(lowest > -0.03 && lowest < 0.03, "a corner is in the floor or the body floats: lowest {lowest:.3}");
    assert!(up_alignment(&sim, e) > 0.995, "not resting on a face: {}", up_alignment(&sim, e));
    assert!(s.vel.length() < 0.05 && s.ang_vel.length() < 0.05, "still moving: {:?} {:?}", s.vel, s.ang_vel);
}

/// A wedge's hull is no box: it falls, lands on a slanted or flat face and
/// stays out of the floor, its centre of mass where its volume is.
#[test]
fn a_wedge_lands_on_one_of_its_own_faces() {
    let mut ecs = World::default();
    let mesh = wedge_mesh();
    let e = mesh_node(
        &mut ecs,
        DVec3::new(0.0, 2.0, 0.0),
        Quat::from_euler(floptle_core::math::EulerRot::XYZ, 0.7, 0.3, 1.1),
        Vec3::ONE,
        mesh.clone(),
    );
    let mut sim = build(&ecs);
    sim.add_static_box(DVec3::new(0.0, -0.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Quat::IDENTITY, StaticTag { layer: 0, eid: 901, sensor: false });
    run(&mut sim, &ecs, 5.0, |_| {});
    let lowest: Vec<f32> = corners(&sim, e, &mesh.0, Vec3::ONE).iter().map(|c| c.y).collect();
    let min = lowest.iter().copied().fold(f32::MAX, f32::min);
    assert!(min > -0.03 && min < 0.03, "lowest corner {min:.3}");
    // Resting on a face: at least three corners on the floor.
    let touching = lowest.iter().filter(|y| **y < min + 0.03).count();
    assert!(touching >= 3, "resting on {touching} corners: {lowest:?}");
    let s = sim.body_snapshot(e.index()).unwrap();
    assert!(s.vel.length() < 0.05, "still moving: {:?}", s.vel);
}

/// A Box body that turns is a crate: off a ledge it tips, and it ends flat
/// on a face like the hull does.
#[test]
fn a_box_body_tumbles_off_a_ledge_and_rests_on_a_face() {
    let mut ecs = World::default();
    let e = ecs.spawn();
    ecs.insert(e, Transform::from_translation(DVec3::new(0.2, 2.26, 0.0)));
    ecs.insert(
        e,
        RigidBody { kind: BodyKind::Box, half_extents: [0.5, 0.25, 0.4], rotates: true, friction: 0.6, ..Default::default() },
    );
    let mut sim = build(&ecs);
    let tag = StaticTag { layer: 0, eid: 901, sensor: false };
    sim.add_static_box(DVec3::new(-2.0, 1.0, 0.0), Vec3::new(2.0, 1.0, 2.0), Quat::IDENTITY, tag);
    sim.add_static_box(DVec3::new(0.0, -0.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Quat::IDENTITY, tag);
    let mut most_tilt = 0.0f32;
    run(&mut sim, &ecs, 4.0, |sim| most_tilt = most_tilt.max(1.0 - up_alignment(sim, e)));
    let mesh = box_mesh(Vec3::ZERO, Vec3::new(0.5, 0.25, 0.4)).0;
    let lowest = corners(&sim, e, &mesh, Vec3::ONE).iter().map(|c| c.y).fold(f32::MAX, f32::min);
    assert!(most_tilt > 0.05, "it never tipped: {most_tilt}");
    assert!(lowest > -0.03 && lowest < 0.03, "lowest corner {lowest:.3}");
    assert!(up_alignment(&sim, e) > 0.995, "not on a face: {}", up_alignment(&sim, e));
}

/// A box placed turned 30° about two axes and scaled unevenly collides as it
/// is drawn: the body's corners are the drawn corners at the start, and once
/// it has fallen it rests on one of its faces, its drawn corners on the floor.
/// A body that does not turn holds the placed angle too, rather than
/// colliding square while it is drawn tilted. On a box floor and on a thin
/// blockout floor, whose distance has no inside, so a corner swung through
/// it in one substep would have hooked under it.
#[test]
fn a_tilted_scaled_box_collides_where_it_is_drawn() {
    let rot = Quat::from_rotation_x(30f32.to_radians()) * Quat::from_rotation_z(30f32.to_radians());
    let scale = Vec3::new(1.6, 0.5, 0.9);
    let mesh = box_mesh(Vec3::new(0.1, 0.0, 0.0), Vec3::splat(0.5));
    for (rotates, mesh_floor) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut ecs = World::default();
        let e = mesh_node(&mut ecs, DVec3::new(0.0, 3.0, 0.0), rot, scale, mesh.clone());
        ecs.get_mut::<RigidBody>(e).unwrap().rotates = rotates;
        let mut sim = build(&ecs);
        let tag = StaticTag { layer: 0, eid: 901, sensor: false };
        if mesh_floor {
            let (v, i) = box_mesh(Vec3::ZERO, Vec3::new(6.0, 0.1, 6.0));
            sim.add_static_mesh(DVec3::new(0.0, -0.1, 0.0), &v, &i, tag);
        } else {
            sim.add_static_box(DVec3::new(0.0, -0.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Quat::IDENTITY, tag);
        }
        let drawn: Vec<Vec3> = mesh.0.iter().map(|v| Vec3::new(0.0, 3.0, 0.0) + rot * (*v * scale)).collect();
        let body = corners(&sim, e, &mesh.0, scale);
        for (d, b) in drawn.iter().zip(&body) {
            assert!(d.distance(*b) < 0.01, "rotates {rotates}: drawn {d:?} body {b:?}");
        }
        // The body's own sampled corners agree with the drawn ones (the hull
        // is in the node's frame about its origin, off-centre mesh included).
        let hull = floptle_physics::mesh_hull(&ecs, e).unwrap();
        for v in hull.verts() {
            let w = Vec3::new(0.0, 3.0, 0.0) + rot * *v;
            assert!(drawn.iter().any(|d| d.distance(w) < 0.01), "rotates {rotates}: hull corner {w:?} is not drawn");
        }
        let mut ys = Vec::new();
        run(&mut sim, &ecs, 5.0, |_| {});
        for c in corners(&sim, e, &mesh.0, scale) {
            ys.push(c.y);
        }
        let min = ys.iter().copied().fold(f32::MAX, f32::min);
        assert!(min > -0.03 && min < 0.03, "rotates {rotates}: lowest corner {min:.3}");
        if rotates {
            let touching = ys.iter().filter(|y| **y < min + 0.03).count();
            assert_eq!(touching, 4, "rotates: rests on a face, corners {ys:?}");
        } else {
            let o = sim.body_snapshot(e.index()).unwrap().orient;
            assert!(o.angle_between(rot) < 1e-3, "a body that does not turn keeps its placed angle");
        }
    }
}

/// Thrown at 20 m/s straight at a blockout wall, and dropped from 20 m onto
/// a blockout floor, a mesh body and a box body both stay inside the room,
/// turning or not. The walls and floor are triangle meshes, whose distance
/// has no inside: a corner that gets past a face is not pushed back.
#[test]
fn fast_bodies_stay_out_of_blockout_walls_and_floors() {
    let half = 4.0;
    for (kind, rotates) in [(BodyKind::Mesh, true), (BodyKind::Box, true), (BodyKind::Mesh, false), (BodyKind::Box, false)] {
        for (start, vel) in [
            (DVec3::new(0.0, 1.0, 0.0), Vec3::new(20.0, 0.0, 0.0)),
            (DVec3::new(0.5, 1.0, 0.3), Vec3::new(14.0, 0.0, -14.0)),
            (DVec3::new(0.0, 20.0, 0.0), Vec3::ZERO),
        ] {
            let mut ecs = World::default();
            let mesh = box_mesh(Vec3::ZERO, Vec3::splat(0.25));
            let e = mesh_node(&mut ecs, start, Quat::IDENTITY, Vec3::ONE, mesh.clone());
            {
                let rb = ecs.get_mut::<RigidBody>(e).unwrap();
                rb.kind = kind;
                rb.rotates = rotates;
                rb.half_extents = [0.25; 3];
                rb.restitution = 0.3;
            }
            let mut sim = build(&ecs);
            blockout_room(&mut sim, half, 0.1);
            sim.set_body_velocity(e.index(), vel);
            let mut worst = 0.0f32;
            run(&mut sim, &ecs, 4.0, |sim| {
                for c in corners(sim, e, &mesh.0, Vec3::ONE) {
                    let out = (c.x.abs() - half).max(c.z.abs() - half).max(-c.y);
                    worst = worst.max(out);
                }
            });
            assert!(worst < 0.05, "{kind:?} rotates {rotates} from {start:?} at {vel:?}: a corner went {worst:.3} into the wall or floor");
            let p = sim.body_snapshot(e.index()).unwrap().pos;
            assert!(p.x.abs() < half as f64 && p.z.abs() < half as f64 && p.y > 0.0, "{kind:?}: ended outside the room at {p:?}");
        }
    }
}

/// A Static Mesh body collides with the node's exact triangles, so a body
/// can fall into the notch of an L-shaped block, which a hull would have
/// filled in.
#[test]
fn a_static_mesh_body_is_its_exact_triangles() {
    let mut ecs = World::default();
    // Two boxes side by side with a gap between: their hull is solid across it.
    let (mut v, mut i) = box_mesh(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(0.5, 0.5, 1.0));
    let (v2, i2) = box_mesh(Vec3::new(1.5, 0.0, 0.0), Vec3::new(0.5, 0.5, 1.0));
    let base = v.len() as u32;
    v.extend(v2);
    i.extend(i2.iter().map(|k| k + base));
    let floor = ecs.spawn();
    ecs.insert(floor, Transform::from_translation(DVec3::ZERO));
    ecs.insert(floor, RigidBody { kind: BodyKind::Mesh, mode: BodyMode::Static, ..Default::default() });
    ecs.insert(floor, BodyGeometry { verts: v, indices: i });
    let ball = ecs.spawn();
    ecs.insert(ball, Transform::from_translation(DVec3::new(0.0, 2.0, 0.0)));
    ecs.insert(ball, RigidBody { radius: 0.3, ..Default::default() });
    let mut sim = build(&ecs);
    sim.add_static_box(DVec3::new(0.0, -2.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Quat::IDENTITY, StaticTag { layer: 0, eid: 901, sensor: false });
    run(&mut sim, &ecs, 2.0, |_| {});
    let y = sim.body_snapshot(ball.index()).unwrap().pos.y;
    assert!((y - -1.7).abs() < 0.05, "the ball should fall through the gap to the floor below, y = {y:.3}");
}

/// Set down on one edge, a little off balance, a crate falls onto a face.
/// Each step's tip is too slow to read as moving, and a body stopped
/// whenever it is slow and touching the floor stood on its edge for good.
#[test]
fn a_crate_set_down_on_its_edge_falls_onto_a_face() {
    for kind in [BodyKind::Mesh, BodyKind::Box] {
        let mut ecs = World::default();
        let h = Vec3::new(0.4, 0.4, 0.4);
        let rot = Quat::from_rotation_z(44f32.to_radians());
        // The lowest edge just clear of the floor.
        let drop = (rot * Vec3::new(h.x, -h.y, 0.0)).y.min((rot * Vec3::new(-h.x, -h.y, 0.0)).y);
        let e = mesh_node(&mut ecs, DVec3::new(0.0, (-drop + 0.005) as f64, 0.0), rot, Vec3::ONE, box_mesh(Vec3::ZERO, h));
        {
            let rb = ecs.get_mut::<RigidBody>(e).unwrap();
            rb.kind = kind;
            rb.half_extents = h.to_array();
        }
        let mut sim = build(&ecs);
        sim.add_static_box(DVec3::new(0.0, -0.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Quat::IDENTITY, StaticTag { layer: 0, eid: 901, sensor: false });
        run(&mut sim, &ecs, 4.0, |_| {});
        assert!(up_alignment(&sim, e) > 0.995, "{kind:?}: still on its edge, alignment {}", up_alignment(&sim, e));
        let y = sim.body_snapshot(e.index()).unwrap().pos.y;
        assert!((y - 0.4).abs() < 0.03, "{kind:?}: resting at {y:.3}, not on a face");
    }
}
