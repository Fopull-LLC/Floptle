//! What compound contact costs: a wheeled craft rolling on a big static mesh,
//! and a thousand-part assembly released onto terrain.
//!
//! Run:
//! ```text
//! cargo run --release -p floptle-physics --example contact_bench
//! ```
//!
//! Prints the mean and worst milliseconds of physics per 60 Hz frame (two
//! 120 Hz steps) for each case, and where each craft ends up, so a change to
//! the contact pass can be checked for cost and for behaviour in one run. Then
//! the thousand-part craft let go on the ground beside a base, lying down and
//! falling over, second by second: physics a frame by stage (the parts
//! `perf.parts()` names) and when it fell asleep. Last, what twenty
//! chase-camera sweeps past the craft cost, with the craft in `ignore` and
//! not.
use floptle_core::math::{Quat, Vec3};
use floptle_physics::*;

/// A `n`×`n` grid of quads `step` apart, gently rolling: `2·n²` triangles.
fn runway(n: usize, step: f32) -> TriMeshCollider {
    let half = n as f32 * step * 0.5;
    let mut verts = Vec::new();
    for z in 0..=n {
        for x in 0..=n {
            let (px, pz) = (x as f32 * step - half, z as f32 * step - half);
            verts.push(Vec3::new(px, 0.05 * (px * 0.11).sin() * (pz * 0.07).cos(), pz));
        }
    }
    let mut idx = Vec::new();
    let w = (n + 1) as u32;
    for z in 0..n as u32 {
        for x in 0..n as u32 {
            let (a, b, c, d) = (z * w + x, z * w + x + 1, (z + 1) * w + x, (z + 1) * w + x + 1);
            idx.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    TriMeshCollider::new(&verts, &idx)
}

fn part(geom: ShapeGeom, offset: Vec3, mass: f32, id: u64) -> CompoundShape {
    CompoundShape { geom, offset, rot: Quat::IDENTITY, mass, id }
}

/// Fifteen parts: a fuselage, wings, a tail and three wheels.
fn plane() -> Compound {
    let b = |h: [f32; 3]| ShapeGeom::Box { half: Vec3::from(h) };
    let mut s = vec![
        part(b([0.5, 0.5, 3.0]), Vec3::new(0.0, 1.2, 0.0), 400.0, 1),
        part(b([0.4, 0.4, 1.0]), Vec3::new(0.0, 1.2, 3.8), 100.0, 2),
        part(b([0.3, 0.3, 1.0]), Vec3::new(0.0, 1.3, -3.8), 80.0, 3),
        part(b([3.0, 0.08, 0.8]), Vec3::new(3.5, 1.3, 0.3), 60.0, 4),
        part(b([3.0, 0.08, 0.8]), Vec3::new(-3.5, 1.3, 0.3), 60.0, 5),
        part(b([1.2, 0.06, 0.4]), Vec3::new(1.4, 1.5, -4.4), 20.0, 6),
        part(b([1.2, 0.06, 0.4]), Vec3::new(-1.4, 1.5, -4.4), 20.0, 7),
        part(b([0.06, 0.8, 0.5]), Vec3::new(0.0, 2.2, -4.4), 15.0, 8),
        part(b([0.3, 0.3, 0.6]), Vec3::new(2.0, 1.0, 0.8), 30.0, 9),
        part(b([0.3, 0.3, 0.6]), Vec3::new(-2.0, 1.0, 0.8), 30.0, 10),
        part(b([0.1, 0.4, 0.1]), Vec3::new(0.0, 0.6, 3.5), 10.0, 11),
        part(b([0.1, 0.4, 0.1]), Vec3::new(1.5, 0.6, -0.5), 10.0, 12),
    ];
    s.push(part(ShapeGeom::Sphere { radius: 0.3 }, Vec3::new(0.0, 0.3, 3.5), 15.0, 13));
    s.push(part(ShapeGeom::Sphere { radius: 0.35 }, Vec3::new(1.5, 0.35, -0.5), 15.0, 14));
    s.push(part(ShapeGeom::Sphere { radius: 0.35 }, Vec3::new(-1.5, 0.35, -0.5), 15.0, 15));
    Compound::new(Vec3::new(0.0, 0.02, -150.0), Quat::IDENTITY, s)
}

/// A spine of 40 tanks, each ringed with fins, standing on its bottom tank:
/// 1,002 parts.
fn tower(base_y: f32) -> Compound {
    let mut s = Vec::new();
    let mut id = 0;
    for t in 0..40 {
        let y = base_y + 1.25 + t as f32 * 2.5;
        id += 1;
        s.push(part(ShapeGeom::Capsule { radius: 1.2, half_height: 0.05 }, Vec3::new(0.0, y, 0.0), 50.0, id));
        for f in 0..24 {
            let a = f as f32 / 24.0 * std::f32::consts::TAU;
            let o = Vec3::new(a.cos() * 1.45, y, a.sin() * 1.45);
            id += 1;
            let mut fin = part(ShapeGeom::Box { half: Vec3::new(0.25, 1.0, 0.04) }, o, 1.0, id);
            fin.rot = Quat::from_rotation_y(-a);
            s.push(fin);
        }
    }
    for k in 0..2 {
        id += 1;
        s.push(part(ShapeGeom::Sphere { radius: 0.6 }, Vec3::new(k as f32 * 1.6 - 0.8, base_y + 0.6, 0.0), 20.0, id));
    }
    let mut c = Compound::new(Vec3::ZERO, Quat::IDENTITY, s);
    c.anchored = true;
    c
}

/// A base round the pad at the origin: sheds, towers and walls within reach
/// of a craft's sphere but nowhere near most of its parts.
fn base(cols: &mut Vec<AnchoredCollider>) {
    for k in 0..60 {
        let a = k as f32 / 60.0 * std::f32::consts::TAU;
        let r = 8.0 + (k % 5) as f32 * 7.0;
        let h = 1.0 + (k % 7) as f32 * 2.0;
        cols.push(AnchoredCollider::world(Box::new(BoxShape::new(
            Vec3::new(a.cos() * r, h, a.sin() * r),
            Vec3::new(1.5, h, 1.5),
            Quat::IDENTITY,
        ))));
    }
    for k in 0..6 {
        let x = -15.0 + k as f32 * 6.0;
        let verts: Vec<Vec3> = (0..=10).flat_map(|z| (0..=10).map(move |y| Vec3::new(x, y as f32 * 3.0, -12.0 + z as f32))).collect();
        let mut idx = Vec::new();
        for z in 0..10u32 {
            for y in 0..10u32 {
                let (a, b, c, d) = (z * 11 + y, z * 11 + y + 1, (z + 1) * 11 + y, (z + 1) * 11 + y + 1);
                idx.extend_from_slice(&[a, c, b, b, c, d]);
            }
        }
        cols.push(AnchoredCollider::world(Box::new(TriMeshCollider::new(&verts, &idx))));
    }
}

/// Run `frames` 60 Hz frames of a craft let go on the ground beside the
/// base, and say per second what physics cost and in which stage, the
/// worst frame, and when every assembly was asleep.
fn release(name: &str, craft: Compound, frames: usize) {
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_slab(Vec3::new(-70.0, -6.0, -45.0), Vec3::new(70.0, 0.0, 60.0), 0.0, [0.5; 3]);
    let mut cols = vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field)))];
    base(&mut cols);
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.set_colliders(cols);
    let ci = w.add_compound(craft);
    w.time_steps(true);
    let mut ms = Vec::new();
    let mut asleep_at = None;
    println!("{name}:");
    let held = std::env::var("HELD").is_ok();
    for f in 0..frames {
        if held && f >= 120 {
            // A piloted craft: a script holding a small force on it every
            // frame, as an idle thruster or a hover trim does.
            let m = w.compounds[ci].mass;
            w.compounds[ci].apply_force(Vec3::new(0.0, 0.001 * m * 9.81, 0.0));
        }
        ms.push(frame(&mut w));
        if asleep_at.is_none() && w.compounds.iter().all(|c| c.asleep) {
            asleep_at = Some(f);
        }
        if (f + 1) % 60 == 0 {
            let t = w.take_timing().unwrap_or_default();
            let sec = &ms[f + 1 - 60..=f];
            let mean = sec.iter().sum::<f64>() / 60.0;
            let worst = sec.iter().cloned().fold(0.0, f64::max);
            let c = &w.compounds[ci];
            println!(
                "  second {}: mean {mean:.2} ms, worst {worst:.1}; per frame static {:.2}, sweeps {:.2}, broadphase {:.2}, pairs {:.2}, joints {:.2}, sleep {:.2}; centre {:.2} m up, spine {:.0} degrees from upright",
                (f + 1) / 60,
                t.static_ms / 60.0,
                t.sweeps_ms / 60.0,
                t.broadphase_ms / 60.0,
                t.pairs_ms / 60.0,
                t.joints_ms / 60.0,
                t.sleep_ms / 60.0,
                c.pos.y,
                (c.orient * Vec3::Y).angle_between(Vec3::Y).to_degrees(),
            );
        }
    }
    let c = &w.compounds[ci];
    match asleep_at {
        Some(f) => println!("  asleep from frame {f}; ends at {:?} turned {:?}", c.pos, c.orient),
        None => println!("  never asleep in {frames} frames; ends at {:?} moving {:?}", c.pos, c.vel),
    }
}

/// The thousand-part craft broken up on the ground: four pieces of ten
/// tanks each, dropped a few centimetres onto rolling, sloping terrain at odd
/// poses: two lying side by side with their fins meeting, one laid diagonally
/// across the end of another, one tipped on the end of its spine and falling.
/// Each lies on the edges and corners of its fins. Says per second what
/// physics cost and which pieces were asleep, and when all of them were.
fn broken_up(frames: usize) {
    // A slope of about 7 degrees with a half-metre swell on it.
    let ground = |x: f32, z: f32| 0.12 * x + 0.25 * (0.35 * x).sin() * (0.27 * z).cos();
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_with(
        Vec3::new(-45.0, -10.0, -45.0),
        Vec3::new(45.0, 10.0, 45.0),
        |p| (p.y - ground(p.x, p.z)) * 0.95,
        |_| [0.5; 3],
    );
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.set_colliders(vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field)))]);
    let mut whole = tower(0.0);
    whole.anchored = false;
    // Tank `t`'s id is `1 + 25·t`, its fins the 24 after it.
    let mut pieces = Vec::new();
    for k in (1..4).rev() {
        let ids: Vec<u64> = (k * 250 + 1..=k * 250 + 250).collect();
        pieces.push(whole.split(&ids).expect("a piece splits off"));
    }
    pieces.push(whole);
    pieces.reverse();
    let along_x = Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2);
    let poses = [
        // Side by side along x, fins a hair apart, each rolled about its spine.
        (Vec3::new(-2.0, 0.0, -1.75), Quat::from_rotation_x(0.21) * along_x),
        (Vec3::new(-2.0, 0.0, 1.75), Quat::from_rotation_x(-0.09) * along_x),
        // Across the far end of the second, tilted, so it lies on it and on the ground.
        (Vec3::new(13.0, 1.6, 6.0), Quat::from_rotation_y(0.9) * Quat::from_rotation_x(0.3) * along_x),
        // Tipped on the end of its spine and falling over.
        (Vec3::new(-8.0, 10.0, -14.0), Quat::from_rotation_x(0.7) * Quat::from_rotation_y(0.4)),
    ];
    let mut idx = Vec::new();
    for (k, (mut c, (at, orient))) in pieces.into_iter().zip(poses).enumerate() {
        // Each piece is its own assembly, rooted at its own node.
        c.owner = 100 + k as u32;
        c.orient = orient;
        c.pos = Vec3::new(at.x, ground(at.x, at.z) + 1.85 + at.y, at.z);
        c.prev_pos = c.pos;
        c.prev_orient = c.orient;
        c.vel = Vec3::ZERO;
        c.ang_vel = Vec3::ZERO;
        idx.push(w.add_compound(c));
    }
    w.time_steps(true);
    let mut ms = Vec::new();
    let mut asleep_at = None;
    println!("1,002 parts broken into four pieces on a slope:");
    for f in 0..frames {
        ms.push(frame(&mut w));
        if asleep_at.is_none() && w.compounds.iter().all(|c| c.asleep) {
            asleep_at = Some(f);
        }
        if (f + 1) % 60 == 0 {
            let t = w.take_timing().unwrap_or_default();
            let sec = &ms[f + 1 - 60..=f];
            let mean = sec.iter().sum::<f64>() / 60.0;
            let worst = sec.iter().cloned().fold(0.0, f64::max);
            let states: Vec<String> = idx
                .iter()
                .map(|&ci| {
                    let c = &w.compounds[ci];
                    if c.asleep {
                        "asleep".to_string()
                    } else {
                        let moved = c.pos.distance(c.prev_pos) * 1000.0;
                        let turned = (c.orient * c.prev_orient.inverse()).normalize();
                        let angle = 2.0 * turned.xyz().length().atan2(turned.w.abs()) * 1000.0;
                        format!(
                            "awake v {:.3} w {:.3} step {moved:.2} mm {angle:.2} mrad{}",
                            c.vel.length(),
                            c.ang_vel.length(),
                            if c.grounded { "" } else { " (not grounded)" }
                        )
                    }
                })
                .collect();
            println!(
                "  second {}: mean {mean:.2} ms, worst {worst:.1}; per frame static {:.2}, sweeps {:.2}, pairs {:.2}, sleep {:.2}; {}",
                (f + 1) / 60,
                t.static_ms / 60.0,
                t.sweeps_ms / 60.0,
                t.pairs_ms / 60.0,
                t.sleep_ms / 60.0,
                states.join(" | "),
            );
        }
    }
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    match asleep_at {
        Some(f) => println!("  all asleep from frame {f}; mean {mean:.2} ms a frame"),
        None => println!("  not all asleep in {frames} frames; mean {mean:.2} ms a frame"),
    }
    for &ci in &idx {
        let c = &w.compounds[ci];
        println!("  piece {ci}: ends at {:?} turned {:?} moving {:?} spinning {:?}", c.pos, c.orient, c.vel, c.ang_vel);
    }
}

/// A six-wheeled rover parked on terrain: a chassis assembly with its wheels
/// hung on wheel joints, braked (`braked`) or free, on a gentle slope. Says
/// per second what physics cost and what was asleep.
fn parked_rover(braked: bool, frames: usize, carry: Option<Rails>) {
    use floptle_core::joint::{Joint, JointKind};
    use floptle_core::math::DVec3;
    use floptle_core::{Name, Parent, RigidBody, Transform, World};
    let slope = if braked { 8.0f32 } else { 0.0 };
    let tilt = Quat::from_rotation_z(slope.to_radians());
    let up = tilt * Vec3::Y;
    let along = tilt * Vec3::X;
    let mut field = floptle_field::ChunkField::new(0.75);
    // On rails, the ground's own frame is a planet's: its anchor the
    // planet's centre, a kilometre from where the rover stands.
    let local = if carry.is_some() { GROUND_LOCAL } else { Vec3::ZERO };
    field.fill_with(Vec3::new(-30.0, -8.0, -15.0) + local, Vec3::new(30.0, 8.0, 15.0) + local, |p| (p - local).dot(up), |_| [0.5; 3]);
    let mut ecs = World::default();
    let centre = up * 1.1;
    let chassis = ecs.spawn();
    ecs.insert(chassis, Transform { translation: centre.as_dvec3(), rotation: tilt, scale: Vec3::ONE });
    ecs.insert(chassis, Name("Chassis".into()));
    ecs.insert(chassis, RigidBody { assembly: true, ..Default::default() });
    for (k, (half, at, mass)) in [([1.6f32, 0.25, 0.9], Vec3::ZERO, 300.0f32), ([0.5, 0.4, 0.6], Vec3::new(0.9, 0.6, 0.0), 80.0)].into_iter().enumerate() {
        let part = ecs.spawn();
        ecs.insert(part, Transform::from_translation(at.as_dvec3()));
        ecs.insert(part, Parent(chassis));
        ecs.insert(part, Name(format!("Part{k}")));
        ecs.insert(part, RigidBody { kind: floptle_core::BodyKind::Box, half_extents: half, mass, ..Default::default() });
    }
    let mut wheels = Vec::new();
    for (dx, dz) in [(-1.3f32, -1.15f32), (0.0, -1.15), (1.3, -1.15), (-1.3, 1.15), (0.0, 1.15), (1.3, 1.15)] {
        let w = ecs.spawn();
        let at = centre + along * dx + tilt * Vec3::new(0.0, -0.6, dz);
        ecs.insert(w, Transform { translation: at.as_dvec3(), rotation: tilt, scale: Vec3::ONE });
        ecs.insert(w, RigidBody { radius: 0.45, mass: 20.0, friction: 1.0, rotates: true, ..Default::default() });
        ecs.insert(
            w,
            Joint {
                kind: JointKind::Wheel,
                other: "Chassis".into(),
                axis: [0.0, 1.0, 0.0],
                stiffness: 3.0,
                damping: 0.8,
                motor: braked,
                motor_speed: 0.0,
                max_force: 800.0,
                ..Default::default()
            },
        );
        wheels.push(w);
    }
    let mut tv = TerrainVolume::new(-local.as_dvec3(), &field);
    tv.eid = Some(TERRAIN_EID);
    let mut sim = Sim::build_layered(
        &ecs,
        &[tv],
        GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    );
    sim.world.time_steps(true);
    if carry.is_some_and(|r| r.far) {
        let k = std::env::var("FAR").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
        sim.world.rebase((FAR_ORIGIN * k).round());
    }
    let mut ms = Vec::new();
    let what = if braked { "braked on an 8 degree slope" } else { "on free wheels on the flat" };
    let riding = carry.map_or(String::new(), |r| r.name());
    println!("a six-wheeled rover parked {what}{riding}:");
    let mut asleep_at = None;
    let (mut diag_from, mut diag_max): (Vec<(Vec3, Quat)>, Vec<f32>) = (Vec::new(), Vec::new());
    for f in 0..frames {
        let t = std::time::Instant::now();
        let before: Vec<(Vec3, Quat)> = sim.world.bodies.iter().map(|b| (b.pos, b.orient)).chain(sim.world.compounds.iter().map(|c| (c.pos, c.orient))).collect();
        sim.sync_joints(&mut ecs);
        let focus = carry.map(|r| ride_rails(&mut sim, r, f));
        sim.step_tick(1.0 / 60.0, focus);
        if asleep_at.is_none() && sim.world.compounds.iter().all(|c| c.asleep) && sim.world.bodies.iter().all(|b| b.asleep) {
            asleep_at = Some(f);
        }
        if std::env::var("DIAG").is_ok() {
            let now: Vec<(Vec3, Quat)> = sim.world.bodies.iter().map(|b| (b.pos, b.orient)).chain(sim.world.compounds.iter().map(|c| (c.pos, c.orient))).collect();
            if f % 30 == 0 {
                diag_from = now.clone();
                diag_max = vec![0.0; now.len()];
            }
            for (k, (a, b)) in diag_from.iter().zip(&now).enumerate() {
                diag_max[k] = f32::max(diag_max[k], a.0.distance(b.0) * 1000.0);
            }
            if f % 30 == 29 {
                println!("    excursion over half a second, mm: {:?}", diag_max);
            }
        }
        if std::env::var("DIAG").is_ok() && f % 60 == 59 {
            let after: Vec<(Vec3, Quat)> = sim.world.bodies.iter().map(|b| (b.pos, b.orient)).chain(sim.world.compounds.iter().map(|c| (c.pos, c.orient))).collect();
            let m: Vec<String> = before.iter().zip(&after).map(|(a, b)| format!("{:.3}mm/{:.3}mrad", a.0.distance(b.0) * 1000.0, (b.1 * a.1.inverse()).normalize().xyz().length() * 2000.0)).collect();
            println!("    moved a frame: {}", m.join(" "));
        }
        ms.push(t.elapsed().as_secs_f64() * 1e3);
        if (f + 1) % 60 == 0 {
            let t = sim.world.take_timing().unwrap_or_default();
            let sec = &ms[f + 1 - 60..=f];
            let mean = sec.iter().sum::<f64>() / 60.0;
            let c = &sim.world.compounds[0];
            let wheels_asleep = sim.world.bodies.iter().filter(|b| b.asleep).count();
            println!(
                "  second {}: mean {mean:.3} ms; per frame static {:.3}, joints {:.3}, sleep {:.3}; chassis {} (v {:.4}, w {:.4}), {wheels_asleep} of {} wheels asleep",
                (f + 1) / 60,
                t.static_ms / 60.0,
                t.joints_ms / 60.0,
                t.sleep_ms / 60.0,
                if c.asleep { "asleep" } else { "awake" },
                c.vel.length(),
                c.ang_vel.length(),
                sim.world.bodies.len(),
            );
        }
    }
    let c = &sim.world.compounds[0];
    let ground = sim.world.colliders()[0].offset();
    match asleep_at {
        Some(f) => println!("  all asleep from frame {f}"),
        None => println!("  never all asleep in {frames} frames"),
    }
    println!("  chassis ends {:?} from its ground's anchor, moving {:?}", c.pos - ground, c.vel);
}

/// The 1,002-part craft lying on terrain, its ground standing still
/// (`carry` none) or carried by `carry` a tick on rails, the way the editor
/// carries a planet: re-anchored, the craft shifted with it, the floating
/// origin following the craft. Says when it fell asleep, what physics cost
/// a frame before and after, and where it ends relative to its ground.
fn carried_craft(rails: Rails, frames: usize) {
    let mut field = floptle_field::ChunkField::new(0.75);
    let local = GROUND_LOCAL;
    field.fill_slab(Vec3::new(-40.0, -6.0, -20.0) + local, Vec3::new(40.0, 0.0, 20.0) + local, local.y, [0.5; 3]);
    let mut ground = AnchoredCollider::world(Box::new(ChunkTerrain::new(field)));
    ground.re_anchor(-local.as_dvec3(), floptle_core::math::DVec3::ZERO);
    ground.eid = Some(TERRAIN_EID);
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.set_colliders(vec![ground]);
    let mut lying = tower(0.0);
    lying.anchored = false;
    lying.orient = Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2);
    lying.pos = Vec3::new(0.0, 1.71, 0.0);
    lying.prev_pos = lying.pos;
    lying.prev_orient = lying.orient;
    let ci = w.add_compound(lying);
    if rails.far {
        w.rebase(FAR_ORIGIN);
    }
    let mut ms = Vec::new();
    let mut asleep_at = None;
    let mut trail = Vec::new();
    for f in 0..frames {
        let d = rails.d;
        if rails.exact {
            w.ride(d, |c| c.eid == Some(TERRAIN_EID));
        } else {
            let origin = w.origin;
            w.colliders[0].re_anchor(d * (f + 1) as f64 - local.as_dvec3(), origin);
            let c = &mut w.compounds[ci];
            c.pos += d.as_vec3();
            c.prev_pos += d.as_vec3();
        }
        let focus = w.compounds[ci].pos.as_dvec3() + w.origin;
        if (focus - w.origin).length() >= 4096.0 {
            let o = w.origin + (focus - w.origin).round();
            w.rebase(o);
        }
        ms.push(frame(&mut w));
        let c = &w.compounds[ci];
        trail.push((c.pos - w.colliders[0].offset(), c.orient, c.vel, c.ang_vel));
        if asleep_at.is_none() && c.asleep {
            asleep_at = Some(f);
        }
    }
    let name = rails.name();
    let mean = |r: &[f64]| r.iter().sum::<f64>() / r.len().max(1) as f64;
    let c = &w.compounds[ci];
    match asleep_at {
        Some(f) => println!(
            "1,002 parts lying on the ground{name}: asleep from frame {f}; {:.2} ms a frame before, {:.3} after",
            mean(&ms[..f]),
            mean(&ms[f + 1..])
        ),
        None => println!(
            "1,002 parts lying on the ground{name}: never asleep in {frames} frames, {:.2} ms a frame; moving {:.4} m/s turning {:.4} rad/s",
            mean(&ms),
            c.vel.length(),
            c.ang_vel.length()
        ),
    }
    let (p, q, _, _) = trail[trail.len() - 1];
    println!("  ends {p:?} from its ground's anchor, turned {q:?}");
}

/// Where the floating origin sits when a carried case starts: the craft
/// about 3.2 km from it, as it is most of the time while its planet carries
/// it away from where the origin was last set.
const FAR_ORIGIN: floptle_core::math::DVec3 = floptle_core::math::DVec3::new(-2600.0, -300.0, 1800.0);

/// Where a carried case's ground lies in its own frame: about a kilometre
/// from the frame's anchor, as a planet's surface is from its centre.
const GROUND_LOCAL: Vec3 = Vec3::new(900.0, -300.0, 200.0);

/// The entity the bench's terrain is tagged with, so the rails can move it.
const TERRAIN_EID: u32 = 999;

/// How a carried case's ground moves: `d` a tick, carried `exact`ly (the
/// sim's frame rides it) or by shifting the craft and re-anchoring the
/// ground as the rails did before, starting `far` from the floating origin
/// or on it.
#[derive(Clone, Copy)]
struct Rails {
    d: floptle_core::math::DVec3,
    exact: bool,
    far: bool,
}

impl Rails {
    fn name(&self) -> String {
        let at = if self.far { ", 3.2 km from the floating origin" } else { "" };
        match (self.d == floptle_core::math::DVec3::ZERO, self.exact) {
            (true, _) => format!(" standing still{at}"),
            (false, true) => format!(" carried on rails, the frame riding it{at}"),
            (false, false) => format!(" carried on rails by shifting each body{at}"),
        }
    }
}

/// The parked rover and the lying craft on ground standing still and on
/// ground carried on rails at 347 m/s (5.79 m a tick, as on Solar's
/// planet): carried the old way, kilometres out, and carried exactly.
fn carried(frames: usize) {
    use floptle_core::math::DVec3;
    let d = DVec3::new(4.1, 0.6, -4.05);
    let still = Rails { d: DVec3::ZERO, exact: true, far: false };
    let cases = [
        still,
        Rails { d, exact: false, far: true },
        Rails { d, exact: true, far: false },
        Rails { d, exact: true, far: true },
        Rails { far: true, ..still },
    ];
    // `CASE=k` runs the rover in case `k` alone.
    if let Some(k) = std::env::var("CASE").ok().and_then(|v| v.parse::<usize>().ok()) {
        parked_rover(true, frames, Some(cases[k]));
        return;
    }
    for r in cases {
        parked_rover(true, frames, Some(r));
    }
    for r in cases {
        carried_craft(r, frames);
    }
}

/// One tick of a planet on rails carrying everything on it by `rails.d`:
/// the frame riding it, or the terrain re-anchored at its new place and
/// every body and assembly shifted with it. The camera on the craft is the
/// floating origin's focus. Returns that focus.
fn ride_rails(sim: &mut Sim, rails: Rails, tick: usize) -> floptle_core::math::DVec3 {
    let d = rails.d;
    if rails.exact {
        sim.ride_frame(d, &[TERRAIN_EID]);
    } else {
        let anchor = d * (tick + 1) as f64 - GROUND_LOCAL.as_dvec3();
        sim.set_collider_anchor(TERRAIN_EID, anchor);
        for (eid, _) in sim.body_positions() {
            sim.shift_body(eid, d);
        }
        for (eid, _) in sim.compound_positions() {
            sim.shift_compound(eid, d);
        }
    }
    sim.compound_positions().first().map_or(floptle_core::math::DVec3::ZERO, |p| p.1)
}

/// What makes a slow sweep frame: the plane rolling down terrain at
/// takeoff speed, frame by frame, with the chunks its queries had to mesh on
/// the frame. Prints the worst frames, then the frames with no new mesh.
fn sweep_frames(speed: f32) {
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_with(
        Vec3::new(-60.0, -8.0, -200.0),
        Vec3::new(60.0, 8.0, 400.0),
        |p| p.y - 0.4 * (p.x * 0.05).sin() * (p.z * 0.03).cos(),
        |_| [0.5; 3],
    );
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.set_colliders(vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field)))]);
    let mut c = plane();
    c.pos.y += 0.5;
    let ci = w.add_compound(c);
    w.time_steps(true);
    let meshed = |w: &PhysicsWorld| w.colliders[0].shape.chunk_terrain().map_or(0, |t| t.meshed_by_queries());
    let mut rows = Vec::new();
    for f in 0..540 {
        if f >= 30 {
            let v = w.compounds[ci].vel;
            w.compounds[ci].vel = Vec3::new(v.x, v.y, speed);
        }
        let m0 = meshed(&w);
        let ms = frame(&mut w);
        let t = w.take_timing().unwrap_or_default();
        rows.push((ms, t.sweeps_ms, t.static_ms, meshed(&w) - m0));
    }
    let rows = &rows[30..];
    type Row = (f64, f32, f32, u64);
    let mean = |f: &dyn Fn(&Row) -> f64| rows.iter().map(f).sum::<f64>() / rows.len() as f64;
    println!(
        "plane on terrain at {speed} m/s: mean {:.2} ms a frame (sweeps {:.2}, static {:.2}), {} chunks meshed by queries",
        mean(&|r| r.0),
        mean(&|r| r.1 as f64),
        mean(&|r| r.2 as f64),
        rows.iter().map(|r| r.3).sum::<u64>()
    );
    let mut top: Vec<_> = rows.to_vec();
    top.sort_by(|a, b| b.0.total_cmp(&a.0));
    for r in top.iter().take(5) {
        println!("  {:.1} ms: sweeps {:.1}, static {:.1}, {} chunks meshed", r.0, r.1, r.2, r.3);
    }
    let quiet: Vec<_> = rows.iter().filter(|r| r.3 == 0).collect();
    let worst = quiet.iter().map(|r| r.0).fold(0.0, f64::max);
    println!("  frames that meshed nothing: worst {worst:.1} ms of {}", quiet.len());
}

/// The plane flying level at `h` over chunk terrain of `voxel` at 50 m/s,
/// as it does lifting off: what a frame costs, its sweeps and its static
/// contact. A part inside the band, a few metres up, asked for the ground
/// under it however far below its radius that was.
fn liftoff(voxel: f32) {
    let mut field = floptle_field::ChunkField::new(voxel);
    field.fill_with(
        Vec3::new(-60.0, -16.0, -200.0),
        Vec3::new(60.0, 16.0, 400.0),
        |p| p.y - 0.4 * (p.x * 0.05).sin() * (p.z * 0.03).cos(),
        |_| [0.5; 3],
    );
    let band = field.band();
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::ZERO));
    w.set_colliders(vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field)))]);
    let ci = w.add_compound(plane());
    w.time_steps(true);
    for h in [1.0f32, 2.0, 4.0, 6.0, 8.0, 10.0] {
        let c = &mut w.compounds[ci];
        c.pos = Vec3::new(0.0, h, -150.0);
        c.prev_pos = c.pos;
        let (mut ms, mut sweeps, mut stat) = (0.0, 0.0, 0.0);
        let n = 60;
        for _ in 0..n {
            w.compounds[ci].vel = Vec3::new(0.0, 0.0, 50.0);
            w.compounds[ci].ang_vel = Vec3::ZERO;
            ms += frame(&mut w);
            let t = w.take_timing().unwrap_or_default();
            sweeps += t.sweeps_ms;
            stat += t.static_ms;
        }
        let n = n as f64;
        println!(
            "voxel {voxel} (band {band}), {h} m up: {:.2} ms a frame (sweeps {:.2}, static {:.2})",
            ms / n,
            sweeps as f64 / n,
            stat as f64 / n
        );
    }
}

fn frame(w: &mut PhysicsWorld) -> f64 {
    let t = std::time::Instant::now();
    w.step(1.0 / 120.0);
    w.step(1.0 / 120.0);
    t.elapsed().as_secs_f64() * 1e3
}

fn report(name: &str, ms: &[f64], c: &Compound) {
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    let mut top: Vec<(usize, f64)> = ms.iter().cloned().enumerate().collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    let worst: Vec<String> = top.iter().take(3).map(|(i, ms)| format!("{ms:.1} (frame {i})")).collect();
    println!(
        "{name}: mean {mean:.3} ms over {} frames, worst {}; ends at {:?} moving {:?}",
        ms.len(),
        worst.join(", "),
        c.pos,
        c.vel
    );
}

fn main() {
    // `ONLY=lying` the thousand-part craft let go lying beside the base, alone
    // (`HELD=1` holds a small force on it from the third second, as a piloted
    // craft's script does).
    if std::env::var("ONLY").is_ok_and(|v| v == "lying") {
        let mut lying = tower(0.0);
        lying.anchored = false;
        lying.orient = Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2);
        lying.pos = Vec3::new(0.0, 1.71, 40.0);
        lying.prev_pos = lying.pos;
        lying.prev_orient = lying.orient;
        release("1,002 parts let go lying beside a base", lying, 600);
        return;
    }
    // `ONLY=broken` runs the broken-up craft alone.
    if std::env::var("ONLY").is_ok_and(|v| v == "broken") {
        broken_up(1800);
        return;
    }
    // `ONLY=queries`: what one terrain distance costs near the ground, over
    // a chunk's middle, on a face between two chunks and at a corner of
    // four, at heights a sweep asks about.
    if std::env::var("ONLY").is_ok_and(|v| v == "queries") {
        let mut field = floptle_field::ChunkField::new(0.75);
        field.fill_with(Vec3::new(-60.0, -8.0, -200.0), Vec3::new(60.0, 8.0, 400.0), |p| p.y - 0.4 * (p.x * 0.05).sin() * (p.z * 0.03).cos(), |_| [0.5; 3]);
        let t = ChunkTerrain::new(field);
        for (x0, z0, what) in [(12.0f32, 12.0f32, "mid-chunk"), (0.0, 12.0, "on a face"), (0.0, 0.0, "at a corner")] {
            for h in [0.02f32, 0.3, 1.0, 2.0] {
                // Meshed first: the cost of a query on meshed ground.
                for z in -3..3 {
                    for x in -3..3 {
                        let _ = t.distance(Vec3::new(x0 + x as f32, h, z0 + z as f32));
                    }
                }
                let n = 20000;
                let t0 = std::time::Instant::now();
                let mut sum = 0.0;
                for k in 0..n {
                    let (x, z) = (x0 + (k % 100) as f32 * 0.02 - 1.0, z0 + (k / 100) as f32 * 0.01 - 1.0);
                    sum += t.distance(Vec3::new(x, h, z));
                }
                println!("{what}, {h} m up: {:.3} us a query (distances sum to {sum})", t0.elapsed().as_secs_f64() * 1e6 / n as f64);
            }
        }
        return;
    }
    // `ONLY=sweeps` the plane's sweep frames alone.
    if std::env::var("ONLY").is_ok_and(|v| v == "sweeps") {
        sweep_frames(40.0);
        sweep_frames(70.0);
        return;
    }
    // `ONLY=heights`: what one terrain distance costs, asked whole and
    // within half a metre, from the ground to past the band.
    if std::env::var("ONLY").is_ok_and(|v| v == "heights") {
        for voxel in [0.75f32, 2.0] {
            let mut field = floptle_field::ChunkField::new(voxel);
            field.fill_with(Vec3::new(-60.0, -16.0, -200.0), Vec3::new(60.0, 16.0, 400.0), |p| p.y - 0.4 * (p.x * 0.05).sin() * (p.z * 0.03).cos(), |_| [0.5; 3]);
            let t = ChunkTerrain::new(field);
            for h in [0.3f32, 1.0, 2.0, 4.0, 6.0, 8.0, 10.0] {
                let at = |k: usize| Vec3::new(12.0 + (k % 100) as f32 * 0.02 - 1.0, h, 12.0 + (k / 100) as f32 * 0.01 - 1.0);
                let _ = t.distance(at(0));
                let n = 20000;
                let t0 = std::time::Instant::now();
                let mut sum = 0.0;
                for k in 0..n {
                    sum += t.distance(at(k));
                }
                let whole = t0.elapsed().as_secs_f64() * 1e6 / n as f64;
                let t0 = std::time::Instant::now();
                for k in 0..n {
                    sum += t.distance_within(at(k), 0.5);
                }
                let within = t0.elapsed().as_secs_f64() * 1e6 / n as f64;
                println!("voxel {voxel}, {h} m up: {whole:.3} us whole, {within:.3} us within 0.5 ({sum:.0})");
            }
        }
        return;
    }
    // `ONLY=liftoff` the plane flying level a few metres up.
    if std::env::var("ONLY").is_ok_and(|v| v == "liftoff") {
        liftoff(0.75);
        liftoff(2.0);
        return;
    }
    // `ONLY=rover` the parked rovers alone.
    if std::env::var("ONLY").is_ok_and(|v| v == "rover") {
        parked_rover(true, 600, None);
        parked_rover(false, 600, None);
        return;
    }
    // `ONLY=carried` the parked rover and the big craft on ground carried on
    // rails at 350 m/s, beside the same on ground that stands still.
    if std::env::var("ONLY").is_ok_and(|v| v == "carried") {
        carried(1200);
        return;
    }
    let g = || GravityField::uniform(Vec3::new(0.0, -9.81, 0.0));

    let mut w = PhysicsWorld::new(g());
    let mesh = runway(100, 4.0);
    w.set_colliders(vec![AnchoredCollider::world(Box::new(mesh))]);
    let ci = w.add_compound(plane());
    for _ in 0..30 {
        frame(&mut w);
    }
    let mut ms = Vec::new();
    for _ in 0..300 {
        let v = w.compounds[ci].vel;
        w.compounds[ci].vel = Vec3::new(v.x, v.y, 40.0);
        ms.push(frame(&mut w));
    }
    report("plane on a 20,000-triangle mesh at 40 m/s", &ms, &w.compounds[ci]);

    for (name, with_mesh) in [("plane on terrain at 40 m/s", false), ("plane on a mesh over terrain at 40 m/s", true)] {
        let mut field = floptle_field::ChunkField::new(0.75);
        field.fill_slab(Vec3::new(-60.0, -6.0, -200.0), Vec3::new(60.0, 0.0, 200.0), 0.0, [0.5; 3]);
        let t = ChunkTerrain::new(field);
        let mut w = PhysicsWorld::new(g());
        let mut cols = vec![AnchoredCollider::world(Box::new(t))];
        if with_mesh {
            cols.push(AnchoredCollider::world(Box::new(runway(100, 4.0))));
        }
        w.set_colliders(cols);
        let mut c = plane();
        c.pos.y += 0.5;
        let ci = w.add_compound(c);
        for _ in 0..30 {
            frame(&mut w);
        }
        let mut ms = Vec::new();
        for _ in 0..300 {
            let v = w.compounds[ci].vel;
            w.compounds[ci].vel = Vec3::new(v.x, v.y, 40.0);
            ms.push(frame(&mut w));
        }
        report(name, &ms, &w.compounds[ci]);
    }

    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_slab(Vec3::new(-20.0, -6.0, -20.0), Vec3::new(20.0, 0.0, 20.0), 0.0, [0.5; 3]);
    let t = ChunkTerrain::new(field);
    let mut w = PhysicsWorld::new(g());
    let mut cols = vec![AnchoredCollider::world(Box::new(t))];
    // `NO_BASE=1` leaves the pad bare.
    if std::env::var("NO_BASE").is_err() {
        base(&mut cols);
    }
    w.set_colliders(cols);
    let ci = w.add_compound(tower(0.01));
    let mut ms = Vec::new();
    for _ in 0..60 {
        ms.push(frame(&mut w));
    }
    report("1,002 parts anchored on terrain", &ms, &w.compounds[ci]);
    w.compounds[ci].anchored = false;
    let mut ms = Vec::new();
    for _ in 0..120 {
        ms.push(frame(&mut w));
    }
    report("1,002 parts released onto terrain", &ms, &w.compounds[ci]);
    println!("asleep after release: {}", w.compounds[ci].asleep);
    let mut ms = Vec::new();
    for _ in 0..180 {
        ms.push(frame(&mut w));
    }
    report("1,002 parts resting on terrain", &ms, &w.compounds[ci]);
    println!("asleep after resting: {}", w.compounds[ci].asleep);

    // A raft of 1,000 boxes lying on the ground, every part on it: the case
    // a per-part cull cannot skip, since every part is near the terrain.
    // Then the same raft split in two, so the halves also meet each other.
    for split in [false, true] {
        let mut field = floptle_field::ChunkField::new(0.75);
        field.fill_slab(Vec3::new(-40.0, -6.0, -40.0), Vec3::new(40.0, 0.0, 40.0), 0.0, [0.5; 3]);
        let mut w = PhysicsWorld::new(g());
        w.set_colliders(vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field)))]);
        let shapes: Vec<CompoundShape> = (0..1000u64)
            .map(|k| {
                let o = Vec3::new((k % 32) as f32 * 1.02 - 16.0, 0.26, (k / 32) as f32 * 1.02 - 16.0);
                part(ShapeGeom::Box { half: Vec3::new(0.5, 0.25, 0.5) }, o, 5.0, k + 1)
            })
            .collect();
        let raft = w.add_compound(Compound::new(Vec3::ZERO, Quat::IDENTITY, shapes));
        if split {
            let half: Vec<u64> = (1..=500).collect();
            if let Some(other) = w.compounds[raft].split(&half) {
                w.add_compound(other);
            }
        }
        let mut ms = Vec::new();
        for _ in 0..90 {
            ms.push(frame(&mut w));
        }
        let name = if split { "1,000 boxes on the ground, split in two" } else { "1,000 boxes lying on the ground" };
        report(name, &ms, &w.compounds[raft]);
        println!("  asleep: {:?}", w.compounds.iter().map(|c| c.asleep).collect::<Vec<_>>());
    }

    // A big craft let go beside a base: the 1,002 parts lying with the spine
    // along the ground, a centimetre up; and the same craft standing, tipped
    // and already toppling over.
    let mut lying = tower(0.0);
    lying.anchored = false;
    // Laid about its centre with its spine along x, on the ground at z = 40,
    // clear of the base: the fins reach 1.7 from the spine.
    lying.orient = Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2);
    lying.pos = Vec3::new(0.0, 1.71, 40.0);
    lying.prev_pos = lying.pos;
    lying.prev_orient = lying.orient;
    release("1,002 parts let go lying beside a base", lying, 600);
    // Tipped 6 degrees about its centre and already toppling toward -x.
    let mut tipped = tower(0.0);
    tipped.anchored = false;
    tipped.orient = Quat::from_rotation_z(0.1);
    tipped.ang_vel = Vec3::new(0.0, 0.0, 0.2);
    tipped.pos += Vec3::new(40.0, 0.1, 44.0);
    tipped.prev_pos = tipped.pos;
    tipped.prev_orient = tipped.orient;
    release("1,002 parts let go standing beside a base, falling over", tipped, 900);

    // A chase camera's casts past the craft: its parts as the hulls queries
    // see, each owned by the craft's root (node 7 here).
    let c = &w.compounds[ci];
    let hulls: Vec<BodyHull> = (0..c.shapes.len())
        .map(|si| {
            let s = &c.shapes[si];
            let orient = (c.orient * s.rot).normalize();
            let (radius, shape) = match s.geom {
                ShapeGeom::Sphere { radius } => (radius, BodyShape::Sphere),
                ShapeGeom::Capsule { radius, half_height } => (radius, BodyShape::Capsule { half_height }),
                ShapeGeom::Box { half } => (half.min_element(), BodyShape::Box { half }),
            };
            BodyHull { eid: 100 + si as u32, owner: 7, pos: c.shape_center(si), radius, shape, up: orient * Vec3::Y, layer: 0, orient }
        })
        .collect();
    for (name, ignore) in [("not ignored", vec![]), ("ignored", vec![7u32])] {
        let mut best = f64::MAX;
        let mut hits = 0;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            hits = 0;
            for k in 0..20 {
                let a = k as f32 * 0.7;
                let from = Vec3::new(a.cos() * 12.0, 3.0 + k as f32 * 4.0, a.sin() * 12.0);
                let to = Vec3::new(0.0, 2.0 + k as f32 * 4.5, 0.0);
                if spherecast(&w.colliders, &hulls, from, to - from, 0.25, 14.0, &ignore, !0, false).is_some() {
                    hits += 1;
                }
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("20 spherecasts past 1,002 parts, {name}: {best:.3} ms ({hits} hit)");
    }

    broken_up(1800);
    parked_rover(true, 600, None);
    parked_rover(false, 600, None);
    carried(1200);
    sweep_frames(40.0);
    sweep_frames(70.0);
}
