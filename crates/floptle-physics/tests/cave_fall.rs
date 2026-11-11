//! A body loose in a cave falls as it does in open air: the collider holds
//! the cave's void, made exactly or still standing in as a coarse lattice.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, Parent, RigidBody, Transform, World};
use floptle_field::{Brush, BrushProfile, ChunkField};
use floptle_physics::{GravityField, Sim, TerrainVolume};

/// A one-part box assembly released at `at` over `field`; its height after
/// each of `ticks` steps.
fn drop_in(field: &ChunkField, at: DVec3, ticks: usize) -> Vec<f32> {
    let mut ecs = World::default();
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(at));
    ecs.insert(root, RigidBody { assembly: true, restitution: 0.05, ..Default::default() });
    let part = ecs.spawn();
    ecs.insert(part, Transform { translation: DVec3::ZERO, rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ecs.insert(part, Parent(root));
    ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: [0.6, 0.4, 0.5], mass: 40.0, ..Default::default() });
    let mut sim = Sim::build_layered(
        &ecs,
        &[TerrainVolume::new(DVec3::ZERO, field)],
        GravityField::uniform(Vec3::new(0.0, -8.16, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    );
    (0..ticks)
        .map(|_| {
            sim.step_tick(1.0 / 60.0, None);
            sim.compound_of(root.index()).unwrap().pos.y
        })
        .collect()
}

#[test]
fn a_body_loose_in_a_cave_falls_freely() {
    let mut field = ChunkField::new(1.0);
    field.fill_slab(Vec3::new(-64.0, -60.0, -64.0), Vec3::new(64.0, 0.0, 64.0), 0.0, [0.5; 3]);
    for _ in 0..6 {
        field.sculpt(Brush::Lower, Vec3::new(0.0, -20.0, 0.0), 9.0, 1.0, BrushProfile::default());
    }
    let floor = field.raycast(Vec3::new(0.0, -20.0, 0.0), Vec3::NEG_Y, 30.0).expect("a cave floor").y;
    let roof = field.raycast(Vec3::new(0.0, -20.0, 0.0), Vec3::Y, 30.0).expect("a cave roof").y;
    eprintln!("cave from {floor:.2} to {roof:.2}");
    assert!(roof - floor > 10.0, "the cave is {:.1} m tall", roof - floor);
    let start = roof - 2.0;
    let ys = drop_in(&field, DVec3::new(0.0, start as f64, 0.0), 30);
    // Half a second of free fall at 8.16 m/s² is just over a metre.
    let fell = start - ys[29];
    let want = 0.5 * 8.16 * 0.5 * 0.5;
    eprintln!("fell {fell:.3} m in 0.5 s, free fall {want:.3}");
    assert!((fell - want).abs() < 0.1, "in the cave it fell {fell:.3} m in half a second; free fall is {want:.3} m");
}

/// A generated planet's own caves: a body set loose in one falls toward the
/// centre as freely as in open air, with every chunk made exactly and with
/// the cave's chunks still standing in as coarse lattices.
#[test]
fn a_body_loose_in_a_generated_cave_falls_freely() {
    use floptle_physics::GravitySource;
    let radius = 150.0;
    let spec = floptle_field::procgen::PlanetFill {
        seed: 11,
        radius,
        voxel: 1.5,
        relief: radius * 0.065,
        cave_depth: radius * 0.35,
        core_r: 10.5,
        ..Default::default()
    };
    let exact = floptle_field::procgen::generate_planet(&spec);
    // A point in a cave: open, at least 8 m under the ground above it, with a
    // floor 4–20 m under it and 3 m of air over it.
    let mut found = None;
    'dirs: for i in 0..4000 {
        let a = i as f32 * 2.399_963;
        let y = 1.0 - (i as f32 + 0.5) / 2000.0;
        let dir = Vec3::new(a.cos() * (1.0 - y * y).max(0.0).sqrt(), y, a.sin() * (1.0 - y * y).max(0.0).sqrt());
        let Some(ground) = exact.raycast(dir * radius * 1.5, -dir, radius) else { continue };
        let ground_r = ground.length();
        let mut r = ground_r - 8.0;
        while r > ground_r - 50.0 {
            let p = dir * r;
            if exact.d(p) > 2.5
                && exact.raycast(p, dir, 3.0).is_none()
                && let Some(f) = exact.raycast(p, -dir, 20.0)
                && (p - f).length() > 4.0
            {
                found = Some((dir, r));
                break 'dirs;
            }
            r -= 0.75;
        }
    }
    let (dir, r) = found.expect("a cave in the planet");
    let near_far = floptle_field::procgen::generate_planet_near(&spec, -dir * radius, 30.0);
    for (name, field) in [("exact", &exact), ("stand-in", &near_far)] {
        let mut ecs = World::default();
        let root = ecs.spawn();
        ecs.insert(root, Transform { translation: (dir * r).as_dvec3(), rotation: Quat::from_rotation_arc(Vec3::Y, dir), scale: Vec3::ONE });
        ecs.insert(root, RigidBody { assembly: true, restitution: 0.05, ..Default::default() });
        let part = ecs.spawn();
        ecs.insert(part, Transform::default());
        ecs.insert(part, Parent(root));
        ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: [0.6, 0.4, 0.5], mass: 40.0, ..Default::default() });
        let mut sim = Sim::build_layered(
            &ecs,
            &[TerrainVolume::new(DVec3::ZERO, field)],
            GravityField { sources: vec![GravitySource::Point { center: Vec3::ZERO, strength: 8.16, radius: 0.0 }] },
            DVec3::ZERO,
            floptle_core::Layers::default(),
        );
        let mut rs = Vec::new();
        for _ in 0..30 {
            sim.step_tick(1.0 / 60.0, None);
            let c = sim.compound_of(root.index()).unwrap();
            rs.push((c.pos.length(), c.vel.dot(dir)));
        }
        let fell = r - rs[29].0;
        let want = 0.5 * 8.16 * 0.25;
        eprintln!("{name}: d at start {:.2}, fell {fell:.3} m in 0.5 s (free fall {want:.3}); radial speeds {:?}", field.d(dir * r), rs.iter().step_by(5).map(|x| (x.1 * 100.0).round() / 100.0).collect::<Vec<_>>());
        assert!((fell - want).abs() < 0.15, "{name}: in the cave it fell {fell:.3} m in half a second; free fall is {want:.3} m");
    }
}
