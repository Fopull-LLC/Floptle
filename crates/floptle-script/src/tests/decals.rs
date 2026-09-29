//! `decals.*`: marks laid on the static colliders.

use super::*;
use crate::decal_api::{lay, DecalSpec, LIFT};
use floptle_physics::{AnchoredCollider, BoxShape, CollisionShape, Plane};
use glam::Vec3;

fn boxed(center: Vec3, half: Vec3) -> AnchoredCollider {
    AnchoredCollider::world(Box::new(BoxShape::new(center, half, glam::Quat::IDENTITY)))
}

/// A decal facing up with the top of its picture toward -Z, so the picture's
/// right is +X.
fn spec(center: Vec3, width: f32, depth: f32) -> DecalSpec {
    DecalSpec {
        center,
        normal: Vec3::Y,
        up: Vec3::NEG_Z,
        width,
        height: 0.4,
        depth,
        uv: [0.0, 0.0, 1.0, 1.0],
        mask: !0,
        min_facing: crate::decal_api::DEFAULT_MAX_ANGLE.to_radians().cos(),
    }
}

/// **A mark laid over the edge of a box folds down its side, and nothing of
/// it hangs in the air.** The top of the box is at y = 1 and its side at
/// x = 1. The mark is centred 0.2 in from the edge and is 0.8 wide, so 0.2 of
/// it is past the edge: that 0.2 goes down the side, and the picture carries
/// on across the fold without a jump.
#[test]
fn a_decal_over_an_edge_folds_down_the_side() {
    let block = BoxShape::new(Vec3::new(0.0, 0.5, 0.0), Vec3::new(1.0, 0.5, 1.0), glam::Quat::IDENTITY);
    let cols = vec![boxed(Vec3::new(0.0, 0.5, 0.0), Vec3::new(1.0, 0.5, 1.0))];
    let s = spec(Vec3::new(0.8, 1.0, 0.0), 0.8, 0.8);
    let laid = lay(&cols, &s);
    assert!(!laid.indices.is_empty(), "nothing was laid");
    let at = |v: &crate::decal_api::DecalVert| s.center + v.pos;
    for v in &laid.verts {
        let d = block.distance(at(v));
        assert!((-1e-4..=LIFT * 1.5).contains(&d), "{} is {d} from the box: off the surface", at(v));
    }
    let side: Vec<_> = laid.verts.iter().filter(|v| at(v).x > 1.0 && at(v).y < 1.0 - 1e-3).collect();
    assert!(!side.is_empty(), "nothing went down the side");
    let lowest = side.iter().map(|v| at(v).y).fold(f32::INFINITY, f32::min);
    assert!((lowest - 0.8).abs() < 1e-3, "the fold went {} down the side, not the 0.2 past the edge", 1.0 - lowest);
    // The picture's right edge is at the bottom of the fold, and where the
    // top meets the side it is three quarters across on both faces.
    let bottom_u = side.iter().filter(|v| (at(v).y - 0.8).abs() < 1e-3).map(|v| v.uv[0]);
    assert!(bottom_u.clone().all(|u| (u - 1.0).abs() < 1e-3), "{:?}", bottom_u.collect::<Vec<_>>());
    let on_edge: Vec<f32> = laid
        .verts
        .iter()
        .filter(|v| (at(v).x - (1.0 + LIFT)).abs() < 2.0 * LIFT && (at(v).y - (1.0 + LIFT)).abs() < 2.0 * LIFT)
        .map(|v| v.uv[0])
        .collect();
    assert!(!on_edge.is_empty() && on_edge.iter().all(|u| (u - 0.75).abs() < 0.01), "{on_edge:?}");
}

/// **A mark on a floor by a wall climbs the wall** by as much of it as runs
/// into the wall.
#[test]
fn a_decal_by_a_wall_climbs_it() {
    let cols = vec![
        AnchoredCollider::world(Box::new(Plane::ground(0.0))),
        boxed(Vec3::new(1.5, 1.0, 0.0), Vec3::new(0.5, 1.0, 1.0)),
    ];
    let s = spec(Vec3::new(0.8, 0.0, 0.0), 0.8, 0.8);
    let laid = lay(&cols, &s);
    let on_wall: Vec<Vec3> = laid
        .verts
        .iter()
        .map(|v| s.center + v.pos)
        .filter(|p| (p.x - (1.0 - LIFT)).abs() < 1e-3 && p.y > 0.01)
        .collect();
    assert!(!on_wall.is_empty(), "nothing went up the wall");
    let top = on_wall.iter().map(|p| p.y).fold(0.0f32, f32::max);
    assert!((top - 0.2).abs() < 1e-3, "it climbed {top}, not the 0.2 that ran into the wall");
}

/// **A mark on one side of a thin wall stays on that side.** The box reaches
/// through to the back face, which faces away and gets nothing.
#[test]
fn a_decal_never_reaches_the_back_of_a_thin_wall() {
    let cols = vec![boxed(Vec3::new(0.05, 1.0, 0.0), Vec3::new(0.05, 1.0, 1.0))];
    let s = DecalSpec { normal: Vec3::X, up: Vec3::Y, center: Vec3::new(0.1, 1.0, 0.0), ..spec(Vec3::ZERO, 0.4, 0.6) };
    let laid = lay(&cols, &s);
    assert!(!laid.indices.is_empty());
    for v in &laid.verts {
        let p = s.center + v.pos;
        assert!(p.x > 0.05, "{p} is on the back of the wall");
    }
}

fn run_decals(src: &str, cols: Vec<AnchoredCollider>) -> (ScriptHost, Vec<String>) {
    let dir = std::env::temp_dir().join(format!("floptle_decals_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "marks", src);
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "marks".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.set_colliders(cols, glam::DVec3::ZERO);
    host.run(&mut world, &dir, 0.1, 0.1);
    let _ = host.take_colliders();
    let said = host.drain_logs().into_iter().map(|l| l.msg).collect();
    (host, said)
}

/// **The store keeps what it is told to, drops the oldest past its budget,
/// and says so in `perf.counts()`.** A mark over nothing is nil, and a layer
/// the project does not have is an error naming the ones it does.
#[test]
fn decals_are_added_set_budgeted_and_counted() {
    let (host, said) = run_decals(
        "function start(node)\n\
         \x20 local m = { texture = 'blood.png', pos = vec3(0, 0, 0), normal = vec3(0, 1, 0), size = 1 }\n\
         \x20 local a = decals.add(m)\n\
         \x20 print('first ' .. tostring(a) .. ' count ' .. decals.count())\n\
         \x20 print('set ' .. tostring(decals.set(a, { alpha = 0.5, color = {1, 0, 0} })))\n\
         \x20 decals.setMax(2)\n\
         \x20 local b = decals.add(m)\n\
         \x20 local c = decals.add(m)\n\
         \x20 print('after ' .. decals.count() .. ' oldest ' .. tostring(decals.set(a, { alpha = 1 })))\n\
         \x20 local p = perf.counts()\n\
         \x20 print('perf ' .. p.decals .. ' ' .. p.decalTris)\n\
         \x20 print('air ' .. tostring(decals.add({ texture = 'x.png', pos = vec3(0, 50, 0), normal = vec3(0, 1, 0), size = 1 })))\n\
         \x20 print('removed ' .. tostring(decals.remove(b)) .. ' ' .. decals.count())\n\
         \x20 local ok, err = pcall(decals.add, { texture = 'x.png', pos = vec3(0, 0, 0), normal = vec3(0, 1, 0), size = 1, layers = 'Nope' })\n\
         \x20 print('layer ' .. tostring(err))\n\
         end\n",
        vec![AnchoredCollider::world(Box::new(Plane::ground(0.0)))],
    );
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let text = said.join("\n");
    assert!(text.contains("first 1 count 1"), "{text}");
    assert!(text.contains("set true"), "{text}");
    assert!(text.contains("after 2 oldest false"), "the oldest was kept past the budget: {text}");
    assert!(text.contains("air nil"), "{text}");
    assert!(text.contains("removed true 1"), "{text}");
    assert!(text.contains("no layer named 'Nope'"), "{text}");
    let store = host.decals().borrow();
    assert_eq!(store.len(), 1);
    let d = store.with_texture("blood.png").next().expect("the survivor");
    let each = d.indices.len() / 3;
    assert!(each >= 2, "a square on a plane is at least two triangles: {each}");
    assert!(text.contains(&format!("perf 2 {}", 2 * each)), "perf counted other than two marks of {each}: {text}");
    assert_eq!(store.triangles(), each);
}
