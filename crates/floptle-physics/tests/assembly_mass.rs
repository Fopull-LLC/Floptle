//! What a script reads of an assembly is what the solver uses, and what a
//! script writes to a part's mass is what the solver flies.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, Parent, RigidBody, Transform, World};
use floptle_physics::{GravityField, Sim};

const DT: f32 = 1.0 / 60.0;

fn build(ecs: &World) -> Sim {
    Sim::build_layered(
        ecs,
        &[],
        GravityField::uniform(Vec3::ZERO),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    )
}

/// An assembly root at `at` with box parts, each `(offset, half, scale, mass)`.
fn vessel(ecs: &mut World, at: DVec3, parts: &[(Vec3, [f32; 3], Vec3, f32)]) -> (floptle_core::Entity, Vec<floptle_core::Entity>) {
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(at));
    ecs.insert(root, RigidBody { assembly: true, gravity: false, ..Default::default() });
    let mut out = Vec::new();
    for &(offset, half, scale, mass) in parts {
        let p = ecs.spawn();
        ecs.insert(
            p,
            Transform { translation: offset.as_dvec3(), rotation: Quat::IDENTITY, scale },
        );
        ecs.insert(p, Parent(root));
        ecs.insert(p, RigidBody { kind: BodyKind::Box, half_extents: half, mass, gravity: false, ..Default::default() });
        out.push(p);
    }
    (root, out)
}

/// A scaled box part's inertia is the closed form for the box the solver
/// builds — half-extents times the node's scale — and `world_inertia`, what
/// `assembly.info().inertia` hands scripts, is the tensor the solver turns
/// torques with.
#[test]
fn a_scaled_box_assemblys_inertia_is_the_scaled_boxs() {
    let mut ecs = World::default();
    let (root, _) = vessel(&mut ecs, DVec3::ZERO, &[(Vec3::ZERO, [0.5, 1.0, 1.5], Vec3::splat(0.55), 0.1)]);
    let sim = build(&ecs);
    let c = sim.compound_of(root.index()).expect("compound");
    let d = Vec3::new(0.5, 1.0, 1.5) * 0.55 * 2.0;
    let m = 0.1f32;
    let want = Vec3::new(
        m / 12.0 * (d.y * d.y + d.z * d.z),
        m / 12.0 * (d.x * d.x + d.z * d.z),
        m / 12.0 * (d.x * d.x + d.y * d.y),
    );
    let i = c.world_inertia();
    let got = Vec3::new(i.x_axis.x, i.y_axis.y, i.z_axis.z);
    assert!((got - want).abs().max_element() < 1e-6, "inertia {got:?}, the scaled box's is {want:?}");
    // …and it is the inverse of what the solver integrates with.
    let solver = c.world_inv_inertia().inverse();
    for (a, b) in [(i.x_axis, solver.x_axis), (i.y_axis, solver.y_axis), (i.z_axis, solver.z_axis)] {
        assert!((a - b).abs().max_element() < 1e-6, "script {a:?} vs solver {b:?}");
    }
}

/// Halving every part's mass through its RigidBody doubles the Δv a fixed
/// impulse gives, with no rebuild: the next parameter sync re-derives the
/// compound.
#[test]
fn a_part_mass_written_by_script_is_the_mass_the_solver_flies() {
    let mut ecs = World::default();
    let parts = [
        (Vec3::new(0.0, -1.0, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 2.0),
        (Vec3::new(0.0, 1.0, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 6.0),
    ];
    let (root, ps) = vessel(&mut ecs, DVec3::ZERO, &parts);
    let mut sim = build(&ecs);
    let kick = |sim: &mut Sim| {
        let at = sim.compound_of(root.index()).unwrap().pos.as_dvec3() + sim.world.origin;
        let v0 = sim.compound_of(root.index()).unwrap().vel;
        sim.compound_impulse(root.index(), Vec3::new(4.0, 0.0, 0.0), at);
        sim.compound_of(root.index()).unwrap().vel - v0
    };
    let dv_full = kick(&mut sim);
    let com_before = sim.compound_of(root.index()).unwrap().pos;
    for &p in &ps {
        ecs.get_mut::<RigidBody>(p).unwrap().mass *= 0.5;
    }
    sim.sync_dynamic_params(&ecs);
    let c = sim.compound_of(root.index()).unwrap();
    assert!((c.mass - 4.0).abs() < 1e-6, "mass {}", c.mass);
    assert!((c.pos - com_before).length() < 1e-6, "halving every part keeps the centre of mass: {:?} -> {:?}", com_before, c.pos);
    let dv_half = kick(&mut sim);
    assert!((dv_half.x - 2.0 * dv_full.x).abs() < 1e-5, "Δv {} at half mass, {} at full", dv_half.x, dv_full.x);

    // One part only: the centre of mass moves toward the heavier part, the
    // assembly origin stays where it is in the world.
    let origin_before = sim.compound_of(root.index()).unwrap().origin();
    ecs.get_mut::<RigidBody>(ps[1]).unwrap().mass = 1.0;
    sim.sync_dynamic_params(&ecs);
    let c = sim.compound_of(root.index()).unwrap();
    assert!((c.mass - 2.0).abs() < 1e-6, "mass {}", c.mass);
    assert!((c.pos.y - 0.0).abs() < 1e-5, "equal parts put the CoM midway: {:?}", c.pos);
    assert!((c.origin() - origin_before).length() < 1e-5, "the origin moved: {:?} -> {:?}", origin_before, c.origin());
}

/// `assembly.rebuild` on a moving, spinning vessel keeps its motion: the new
/// centre of mass takes the old body's velocity at that point.
#[test]
fn a_rebuilt_moving_assembly_keeps_its_velocity_and_spin() {
    let mut ecs = World::default();
    let parts = [
        (Vec3::new(0.0, -1.0, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 2.0),
        (Vec3::new(0.0, 1.0, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 2.0),
    ];
    let (root, _) = vessel(&mut ecs, DVec3::ZERO, &parts);
    let mut sim = build(&ecs);
    {
        let c = sim.compound_of_mut(root.index()).unwrap();
        c.vel = Vec3::new(100.0, 0.0, 0.0);
        c.ang_vel = Vec3::new(0.0, 0.0, 0.0);
    }
    sim.step_tick(DT, None);
    sim.writeback_interpolated(&mut ecs, 1.0);
    let before = sim.compound_of(root.index()).unwrap().vel;
    assert!(sim.rebuild_compound(root.index(), &ecs));
    let after = sim.compound_of(root.index()).unwrap().vel;
    assert!((after - before).length() < 1e-6, "rebuild changed the velocity: {before:?} -> {after:?}");

    // Spinning: the spin survives, and so does the centre of mass's speed.
    sim.compound_of_mut(root.index()).unwrap().ang_vel = Vec3::new(0.0, 0.0, 2.0);
    sim.step_tick(DT, None);
    sim.writeback_interpolated(&mut ecs, 1.0);
    let (v0, w0) = {
        let c = sim.compound_of(root.index()).unwrap();
        (c.vel, c.ang_vel)
    };
    sim.rebuild_compound(root.index(), &ecs);
    let c = sim.compound_of(root.index()).unwrap();
    assert!((c.ang_vel - w0).length() < 1e-6, "spin {w0:?} -> {:?}", c.ang_vel);
    assert!((c.vel - v0).length() < 1e-3, "velocity {v0:?} -> {:?}", c.vel);
}

/// Two copies of one moving, spinning craft: `a` is rebuilt with the same
/// parts mid-flight and `b` is not. Each tick's node pose is written at the
/// start of the tick (alpha 0), the most a node can lag its body.
fn rebuilt_and_twin(vel: Vec3, spin: Vec3) -> (Vec3, Vec3, Vec3, Vec3) {
    let parts = [
        (Vec3::new(0.0, -1.0, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 3.0),
        (Vec3::new(1.0, 1.5, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 1.0),
    ];
    let fly = |rebuild: bool| {
        let mut ecs = World::default();
        let (root, _) = vessel(&mut ecs, DVec3::new(10.0, 20.0, 30.0), &parts);
        let mut sim = build(&ecs);
        {
            let c = sim.compound_of_mut(root.index()).unwrap();
            c.vel = vel;
            c.ang_vel = spin;
        }
        for _ in 0..5 {
            sim.step_tick(DT, None);
            sim.writeback_interpolated(&mut ecs, 0.0);
        }
        if rebuild {
            assert!(sim.rebuild_compound(root.index(), &ecs));
        }
        sim.step_tick(DT, None);
        let c = sim.compound_of(root.index()).unwrap();
        (c.origin(), c.vel)
    };
    let (pa, va) = fly(true);
    let (pb, vb) = fly(false);
    (pa, va, pb, vb)
}

/// A rebuild keeps a fast craft where its body is, not where its node was
/// last drawn a tick behind it.
#[test]
fn a_rebuild_does_not_set_a_fast_craft_back_a_tick() {
    let (pa, _, pb, _) = rebuilt_and_twin(Vec3::new(130.0, 0.0, 0.0), Vec3::ZERO);
    assert!((pa - pb).length() < 0.01, "the rebuilt craft is {} m from its twin", (pa - pb).length());
}

/// A rebuild with the same parts leaves a spinning craft's centre of mass
/// moving as it was.
#[test]
fn a_rebuild_keeps_a_spinning_crafts_centre_of_mass_velocity() {
    let (pa, va, pb, vb) = rebuilt_and_twin(Vec3::new(130.0, 0.0, 0.0), Vec3::new(0.0, 0.0, 5.0));
    assert!((va - vb).length() < 0.01, "velocity {va:?}, its twin's {vb:?}");
    assert!((pa - pb).length() < 0.01, "the rebuilt craft is {} m from its twin", (pa - pb).length());
}

/// The half of an assembly a split keeps is still that assembly: it keeps
/// its root node as owner, so a pair the game ignores stays ignored, and an
/// anchored craft stays anchored.
#[test]
fn the_half_a_split_keeps_is_still_its_assembly() {
    let mut ecs = World::default();
    let parts = [
        (Vec3::new(0.0, 0.0, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 2.0),
        (Vec3::new(2.0, 0.0, 0.0), [0.5, 0.5, 0.5], Vec3::ONE, 1.0),
    ];
    let (root, ps) = vessel(&mut ecs, DVec3::ZERO, &parts);
    let mut sim = build(&ecs);
    sim.set_compound_anchored(root.index(), true);
    let stage = ecs.spawn();
    ecs.insert(stage, Transform::IDENTITY);
    assert!(sim.split_compound(root.index(), &[ps[1].index()], stage, &mut ecs));
    let kept = sim.compound_of(root.index()).unwrap();
    assert_eq!(kept.owner, root.index(), "the kept half lost its owner");
    assert!(kept.anchored, "the kept half of an anchored craft let go");
}
