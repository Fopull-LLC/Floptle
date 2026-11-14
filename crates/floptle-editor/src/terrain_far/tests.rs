use floptle_core::math::{DVec3, Vec3};
#[cfg(feature = "editor-ui")]
use floptle_core::math::Quat;
use floptle_core::transform::Transform;
use floptle_core::Matter;

use super::{backstop_radius, ground_floor};
use crate::terrain_edit::EditorTerrain;

fn fill(radius: f32, voxel: f32) -> floptle_field::procgen::PlanetFill {
    floptle_field::procgen::PlanetFill {
        seed: 11,
        radius,
        voxel,
        relief: 8.0,
        surface_a: floptle_field::procgen::LayerPaint { color: [0.45, 0.6, 0.25], slot: 0 },
        surface_b: floptle_field::procgen::LayerPaint { color: [0.7, 0.62, 0.4], slot: 0 },
        ..Default::default()
    }
}

/// A generated planet as a streamed world arrives far from the camera:
/// every chunk a stand-in.
fn far_planet(ed: &mut crate::Editor, radius: f32, voxel: f32, occluder: f64) -> floptle_core::ecs::Entity {
    let mut field = floptle_field::procgen::generate_planet_near(&fill(radius, voxel), Vec3::splat(1.0e6), 0.0);
    field.set_spec("a planet".into());
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Matter::Terrain { id: 7, collision: Default::default() });
    ed.world.insert(e, floptle_core::CelestialBody { body_radius: f64::from(radius), occluder_radius: occluder, ..Default::default() });
    ed.terrains.insert(e, EditorTerrain::new(field));
    e
}

/// The radii at which rays toward a field's centre meet its ground, read
/// along `n`² rays a cube face, sorted.
fn ground_radii(field: &floptle_field::ChunkField, radius: f32, n: usize) -> Vec<f32> {
    let reach = radius * 1.6 + 20.0;
    let mut out = Vec::new();
    for face in 0..6 {
        for i in 0..n {
            for j in 0..n {
                let (u, v) = (-1.0 + (i as f32 + 0.5) * 2.0 / n as f32, -1.0 + (j as f32 + 0.5) * 2.0 / n as f32);
                let d = super::cube_dir(face, u, v);
                if let Some(hit) = field.raycast(d * reach, -d, reach) {
                    out.push(hit.length());
                }
            }
        }
    }
    out.sort_unstable_by(f32::total_cmp);
    out
}

/// **A planet's floor is the bottom of its ground.** Read off the
/// stand-ins a streamed planet keeps, as a planet the camera was just put
/// near has nothing else, and checked against the ground at nearly ten
/// thousand places: under all but the odd ravine floor (a hundredth of the
/// ground at most), and never under the deepest of it.
#[test]
fn the_floor_is_the_bottom_of_the_ground() {
    let (radius, voxel) = (930.0, 1.5);
    let mut field = floptle_field::procgen::generate_planet_near(&fill(radius, voxel), Vec3::splat(1.0e6), 0.0);
    field.set_spec("a planet".into());
    let t = std::time::Instant::now();
    let floor = ground_floor(&field);
    let took = t.elapsed();
    let ground = ground_radii(&field, radius, 40);
    let under = ground.iter().filter(|r| **r < floor).count() as f32 / ground.len() as f32;
    let (low, high) = (ground[0], ground[ground.len() - 1]);
    floptle_say::say_err!(
        "floor {floor:.2} in {:.1} ms; the ground from {low:.2} to {high:.2}, {:.2}% of it under the floor",
        took.as_secs_f64() * 1e3,
        under * 100.0
    );
    assert!(under <= 0.01, "{:.1}% of the ground lies under the floor {floor:.2}", under * 100.0);
    assert!(floor >= low, "the floor {floor:.2} is under the deepest ground, {low:.2}");
    // Nothing to stand in for: no ground, no floor.
    assert_eq!(ground_floor(&floptle_field::ChunkField::new(voxel)), 0.0);
}

/// **The backstop grows from the Occluder ball to the floor as the camera
/// climbs, never in a jump.** Near the ground it is the Occluder ball,
/// exactly as before (`None`); from afar it is the floor; in between it
/// moves a hair a metre. A body with no Occluder has no ball near the
/// ground, and the floor's from afar.
#[test]
fn the_backstop_grows_with_height_and_never_jumps() {
    let (occ, floor) = (558.0, 920.0);
    assert_eq!(backstop_radius(occ, floor, floor + 50.0), None, "near the ground the backstop is not the Occluder ball");
    assert_eq!(backstop_radius(occ, floor, floor + 2000.0), Some(floor));
    assert_eq!(backstop_radius(occ, floor, floor + 3000.0), Some(floor));
    assert_eq!(backstop_radius(0.0, floor, floor + 2000.0), Some(floor), "a body with no Occluder has no backstop from afar");
    assert_eq!(backstop_radius(0.0, floor, floor + 10.0), None);
    // A declared Occluder bigger than the floor stands.
    assert_eq!(backstop_radius(950.0, floor, floor + 2000.0), None);
    let mut last = occ;
    for m in 0..3000 {
        let r = backstop_radius(occ, floor, floor + f64::from(m)).unwrap_or(occ);
        assert!(r >= last && r - last < 1.5, "at {m} m up the backstop went from {last:.2} to {r:.2}");
        last = r;
    }
}

const FOV: f32 = 0.9;

/// A picture of the world from `eye`, looking at the origin.
#[cfg(feature = "editor-ui")]
fn picture(ed: &mut crate::Editor, eye: DVec3, w: u32, h: u32) -> Vec<u8> {
    let look = floptle_core::math::Mat4::look_at_rh(eye.as_vec3(), Vec3::ZERO, Vec3::Y).inverse();
    let cam = floptle_render::RenderCamera::new(eye, Quat::from_mat4(&look), floptle_render::Projection::of_camera(FOV, false, 1.0, 0.5, 20_000.0));
    let _ = crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false);
    crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame")
}

/// The disc a picture's world is drawn as, in pixels from the middle: the
/// radius of a circle of its area, and its farthest pixel. A pixel is the
/// world's where it is not the sky's, read off a corner.
#[cfg(feature = "editor-ui")]
fn disc(px: &[u8], w: u32, h: u32) -> (f32, f32) {
    let sky = [px[0], px[1], px[2]];
    let (mut area, mut far) = (0u32, 0f32);
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if (0..3).any(|k| px[i + k].abs_diff(sky[k]) > 6) {
                area += 1;
                far = far.max((x as f32 + 0.5 - w as f32 / 2.0).hypot(y as f32 + 0.5 - h as f32 / 2.0));
            }
        }
    }
    ((area as f32 / std::f32::consts::PI).sqrt(), far)
}

/// The radius in pixels a ball of `radius` is drawn at from `dist`.
#[cfg(feature = "editor-ui")]
fn limb_px(radius: f64, dist: f64, h: u32) -> f32 {
    let f = f64::from(h) / 2.0 / f64::from(FOV / 2.0).tan();
    (f * (radius / dist).asin().tan()) as f32
}

/// **A planet seen from a few kilometres is drawn at its own radius from the
/// first frame.** A 930 m planet the camera was just put 2 and 3 km above,
/// with an Occluder of six tenths of its radius: its silhouette sits where
/// its ground is, between the bottom and the top of it, on the first frame
/// (nothing meshed yet), while its ground streams in, and once it has. Drawn
/// with its Occluder ball as the backstop, it was a rock 0.58 of its size
/// until its limb streamed in, many seconds later. `terrain.viewReady`'s share
/// climbs to 1 as it settles.
#[cfg(feature = "editor-ui")]
#[test]
fn a_far_planet_is_drawn_at_its_radius_cold_and_settled() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let (radius, voxel) = (930.0f32, 3.0f32);
    let e = far_planet(&mut ed, radius, voxel, f64::from(radius) * 0.6);
    let (w, h) = (640u32, 360u32);
    let out = std::env::var("OUT").ok().map(std::path::PathBuf::from);
    // The ground's own span, from its deepest to its highest.
    let ground = ground_radii(&ed.terrains[&e].field, radius, 32);
    let (bottom, top) = (ground[0], ground[ground.len() - 1]);
    for up in [2000.0, 3000.0] {
        let eye = DVec3::new(0.3, 0.5, 3.0).normalize() * (f64::from(radius) + up);
        let dist = eye.length();
        // The ground's silhouette lies between these, and so must the drawing:
        // a pixel and a half either way, and one more over the top for a
        // coarse cell that stands a little proud of the ground it simplifies.
        let (lo, hi) = (
            limb_px(f64::from(bottom), dist, h) - 1.5,
            limb_px(f64::from(top), dist, h) + 2.5,
        );
        if let Some(mut r) = ed.terrain_render.remove(&e) {
            r.free_all(ed.raster.as_mut().unwrap());
        }
        ed.sync_terrain_meshes(true, eye);
        let mut frames = 0;
        for (when, wait) in [("cold", 0u32), ("streaming", 10)] {
            for _ in 0..wait {
                ed.sync_terrain_meshes(false, eye);
                frames += 1;
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            let px = picture(&mut ed, eye, w, h);
            let (area, edge) = disc(&px, w, h);
            let r = &ed.terrain_render[&e];
            floptle_say::say_err!("{up} m up, {when} after {frames} frames: {} drawn, {} pending, {} waiting | disc {area:.1} px by area, {edge:.1} at its edge; the ground's limb {lo:.1}..{hi:.1}", r.slots.len(), r.pending.len(), r.waiting);
            if let Some(dir) = &out {
                floptle_image::io::save_png(&dir.join(format!("far_{up}m_{when}.png")), &px, w, h).unwrap();
            }
            assert!(!r.pending.is_empty() || r.waiting > 0, "the fixture: the planet had finished meshing {when}");
            assert!(
                area >= lo && area <= hi && edge >= lo && edge <= hi,
                "{up} m up, {when}: the planet is drawn {area:.1} px across by area, {edge:.1} at its edge; its ground's limb is {lo:.1}..{hi:.1} px"
            );
            let share = ed.terrain_view_shares()[&7];
            assert!(share < 1.0, "{when}: the view reads finished at {share}");
        }
        assert!(ed.settle_terrain_meshes(eye, std::time::Duration::from_secs(300)), "the planet did not finish meshing");
        // Past the scan throttle: every chunk was scanned since it landed.
        for _ in 0..8 {
            ed.sync_terrain_meshes(false, eye);
        }
        let px = picture(&mut ed, eye, w, h);
        let (area, edge) = disc(&px, w, h);
        floptle_say::say_err!("{up} m up, settled: disc {area:.1} px by area, {edge:.1} at its edge");
        if let Some(dir) = &out {
            floptle_image::io::save_png(&dir.join(format!("far_{up}m_settled.png")), &px, w, h).unwrap();
        }
        assert!(
            area >= lo && area <= hi && edge >= lo && edge <= hi,
            "{up} m up, settled: the planet is drawn {area:.1} px across by area, {edge:.1} at its edge; its ground's limb is {lo:.1}..{hi:.1} px"
        );
        assert_eq!(ed.terrain_view_shares()[&7], 1.0, "a settled view did not read finished");
    }
}

/// FNV-1a over 32-bit words.
fn fnv(h: &mut u64, v: u32) {
    for b in v.to_le_bytes() {
        *h = (*h ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
}

/// **Near the ground, a planet draws exactly as it always did.** Forty
/// metres over a small planet: every chunk drawn at the level it always
/// was, with the mesh it always had, and the streaming backstop the same
/// Occluder ball to the bit. The stamps are of the drawing before a
/// backstop could grow.
#[cfg(feature = "editor-ui")]
#[test]
fn near_the_ground_a_planet_draws_as_it_always_did() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let radius = 300.0f32;
    let field = floptle_field::procgen::generate_planet(&fill(radius, 1.5));
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Matter::Terrain { id: 7, collision: Default::default() });
    ed.world.insert(e, floptle_core::CelestialBody { body_radius: f64::from(radius), occluder_radius: f64::from(radius) * 0.6, ..Default::default() });
    ed.terrains.insert(e, EditorTerrain::new(field));
    let eye = DVec3::new(0.2, 1.0, 0.1).normalize() * f64::from(radius + 40.0);
    assert!(ed.settle_terrain_meshes(eye, std::time::Duration::from_secs(300)), "the planet did not finish meshing");
    // The chunks drawn, the level each is drawn at, and its mesh.
    let mut meshes = 0xcbf29ce484222325u64;
    let r = &ed.terrain_render[&e];
    let mut drawn: Vec<([i32; 3], u8)> = r.slots.iter().map(|(c, &(_, lod))| (*c, lod)).collect();
    drawn.sort_unstable();
    for (c, lod) in &drawn {
        c.iter().for_each(|v| fnv(&mut meshes, *v as u32));
        fnv(&mut meshes, u32::from(*lod));
        let m = floptle_field::mesh_chunk(&ed.terrains[&e].field, *c, 1 << lod, true);
        for p in m.positions.iter().chain(&m.normals) {
            p.iter().for_each(|v| fnv(&mut meshes, v.to_bits()));
        }
        m.indices.iter().for_each(|i| fnv(&mut meshes, *i));
    }
    // A chunk still to come, so the backstop draws.
    ed.terrain_render.get_mut(&e).unwrap().pending.insert([9999, 0, 0], (3, 0));
    let sphere = ed.mesh_ids[floptle_core::Shape::Sphere as usize];
    let view_proj = floptle_core::math::Mat4::perspective_rh(1.0, 1.0, 0.5, 10000.0)
        * floptle_core::math::Mat4::look_at_rh(eye.as_vec3(), Vec3::ZERO, Vec3::X);
    let mut instances = Vec::new();
    crate::terrain_edit::push_terrain_instances(
        &ed.terrain_render,
        &ed.terrains,
        &ed.world,
        ed.raster.as_ref().unwrap(),
        &floptle_render::MaterialParams::flat([0.5, 0.5, 0.5]).into(),
        eye,
        view_proj,
        sphere,
        None,
        &mut instances,
    );
    let mut ball = 0xcbf29ce484222325u64;
    let balls: Vec<_> = instances.iter().filter(|(m, ..)| *m == sphere).collect();
    for (_, _, raw) in &balls {
        raw.model.iter().flatten().chain(&raw.color).for_each(|v| fnv(&mut ball, v.to_bits()));
    }
    floptle_say::say_err!("{} chunks drawn, stamp {meshes}; {} backstop balls, stamp {ball}", drawn.len(), balls.len());
    assert_eq!(balls.len(), 1, "the backstop is not one ball");
    assert_eq!(meshes, NEAR_MESHES, "the chunks drawn near the ground, or their meshes, changed");
    assert_eq!(ball, NEAR_BACKSTOP, "the backstop near the ground changed");
}

/// The stamps of [`near_the_ground_a_planet_draws_as_it_always_did`], made
/// before a backstop could grow.
const NEAR_MESHES: u64 = 8103829589978298837;
const NEAR_BACKSTOP: u64 = 5070849163051441734;
