use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Matter, WaterKind};
use floptle_field::{WaterLevel, WaterSurface};

use crate::terrain_edit::EditorTerrain;

fn water(kind: WaterKind, radius: f32, half: [f32; 3]) -> Matter {
    Matter::WaterVolume {
        kind,
        radius,
        half_extents: half,
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        tint: [0.1, 0.3, 0.4],
        visibility: 28.0,
        underwater: true,
    }
}

/// A small generated planet, terrain id 7, at `at`.
fn planet(ed: &mut crate::Editor, at: DVec3, radius: f32) -> floptle_core::ecs::Entity {
    let fill = floptle_field::procgen::PlanetFill { seed: 3, radius, voxel: 2.0, relief: 3.0, cave_depth: 0.0, ..Default::default() };
    let e = ed.world.spawn();
    ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
    ed.world.insert(e, Matter::Terrain { id: 7, collision: Default::default() });
    ed.world.insert(e, floptle_core::CelestialBody { body_radius: f64::from(radius), ..Default::default() });
    ed.terrains.insert(e, EditorTerrain::new(floptle_field::procgen::generate_planet(&fill)));
    e
}

fn spawn(ed: &mut crate::Editor, t: Transform, m: Matter, parent: Option<floptle_core::ecs::Entity>) -> floptle_core::ecs::Entity {
    let e = ed.world.spawn();
    ed.world.insert(e, t);
    ed.world.insert(e, m);
    if let Some(p) = parent {
        ed.world.insert(e, floptle_core::Parent(p));
    }
    e
}

/// **A terrain's water is the water declared on it, unless a game says
/// otherwise.** A Water Volume sea centred on a planet is that planet's
/// level, in the planet's own frame wherever the planet is; another world's
/// sea is not, and neither is a pool that is no child of the terrain. A
/// level a script sets replaces what is declared, and `false` takes it away.
/// With no water at all a terrain is given none, and its field is the field
/// it always was.
#[test]
fn a_terrains_water_is_what_is_declared_on_it() {
    let mut ed = crate::Editor::default();
    let at = DVec3::new(4000.0, -250.0, 90.0);
    let e = planet(&mut ed, at, 40.0);
    assert!(ed.terrain_water_levels().is_empty(), "a planet with no water was given a level");

    // Its own sea, a sibling at its centre; and a far world's sea.
    let sea = spawn(&mut ed, Transform { translation: at, ..Transform::IDENTITY }, water(WaterKind::Sea, 43.0, [0.0; 3]), None);
    spawn(&mut ed, Transform { translation: at + DVec3::X * 500.0, ..Transform::IDENTITY }, water(WaterKind::Sea, 480.0, [0.0; 3]), None);
    // A pool beside it that is nobody's child: it could be a ship's.
    spawn(&mut ed, Transform { translation: at + DVec3::Y * 41.0, ..Transform::IDENTITY }, water(WaterKind::Pool, 0.0, [5.0, 1.0, 5.0]), None);
    let got = ed.terrain_water_levels();
    let levels = &got[&e];
    assert_eq!(levels.len(), 1, "{levels:?}");
    let WaterSurface::Sphere { centre, radius } = levels[0].surface else { panic!("a sea is a sphere: {levels:?}") };
    assert!(Vec3::from(centre).length() < 1e-3 && (radius - 43.0).abs() < 1e-4, "{levels:?}");
    assert_eq!((levels[0].within, levels[0].margin, levels[0].flatten), (None, WaterLevel::MARGIN, false));

    // A pool that is the terrain's child: flat water over its own footprint.
    let pool = spawn(&mut ed, Transform { translation: DVec3::Y * 41.0, ..Transform::IDENTITY }, water(WaterKind::Pool, 0.0, [3.0, 1.0, 4.0]), Some(e));
    let got = ed.terrain_water_levels();
    let levels = &got[&e];
    assert_eq!(levels.len(), 2, "{levels:?}");
    let WaterSurface::Plane { up, height } = levels[1].surface else { panic!("the widest water comes first: {levels:?}") };
    assert!((Vec3::from(up) - Vec3::Y).length() < 1e-5 && (height - 42.0).abs() < 1e-3, "{levels:?}");
    let (mid, reach) = levels[1].within.expect("a pool is water over its own footprint");
    assert!((Vec3::from(mid) - Vec3::Y * 42.0).length() < 1e-3 && (reach - 5.0).abs() < 1e-4, "{levels:?}");
    ed.world.despawn(pool);

    // A script's level replaces the sea, and its options ride with it.
    use floptle_script::{TerrainWater, TerrainWaterRule};
    ed.terrain_water_rules.insert(7, TerrainWaterRule { water: TerrainWater::At(45.5), margin: Some(1.0), flatten: true, flat: None });
    assert_eq!(
        ed.terrain_water_levels()[&e],
        [WaterLevel { surface: WaterSurface::Sphere { centre: [0.0; 3], radius: 45.5 }, within: None, margin: 1.0, flatten: true }],
        "a planet's level is a radius"
    );
    ed.terrain_water_rules.insert(7, TerrainWaterRule { water: TerrainWater::At(2.0), flat: Some(true), ..Default::default() });
    assert_eq!(ed.terrain_water_levels()[&e], [WaterLevel::flat(2.0)]);
    // Options alone dress the declared sea.
    ed.terrain_water_rules.insert(7, TerrainWaterRule { margin: Some(2.0), ..Default::default() });
    assert_eq!(ed.terrain_water_levels()[&e][0].margin, 2.0);
    ed.terrain_water_rules.insert(7, TerrainWaterRule { water: TerrainWater::Off, ..Default::default() });
    assert!(ed.terrain_water_levels().is_empty(), "a terrain told it has no water kept its sea");
    ed.terrain_water_rules.clear();

    // A switched-off sea is no sea.
    ed.world.insert(sea, floptle_core::Disabled);
    assert!(ed.terrain_water_levels().is_empty(), "a switched-off sea still set a level");
}

/// **A level is in the terrain's own frame, whatever its node's placement.**
/// A planet turned and drawn at twice its size has a sea of twice the
/// radius in the world; in its field that is the radius it was authored
/// with, and a quarter-metre margin in the world is an eighth in the field.
#[test]
fn a_scaled_planets_sea_is_read_in_its_own_frame() {
    let mut ed = crate::Editor::default();
    let at = DVec3::new(-700.0, 30.0, 12.0);
    let e = planet(&mut ed, at, 40.0);
    ed.world.insert(e, Transform { translation: at, rotation: Quat::from_rotation_z(0.7), scale: Vec3::splat(2.0) });
    spawn(&mut ed, Transform { translation: at, ..Transform::IDENTITY }, water(WaterKind::Sea, 86.0, [0.0; 3]), None);
    let got = ed.terrain_water_levels();
    let WaterSurface::Sphere { radius, .. } = got[&e][0].surface else { panic!("{got:?}") };
    assert!((radius - 43.0).abs() < 1e-4, "a sea of 86 round a planet drawn at twice its size is 43 in its field, not {radius}");
    assert!((got[&e][0].margin - WaterLevel::MARGIN * 0.5).abs() < 1e-6);
}

/// The far meshes a terrain draws, by chunk.
fn far_meshes(ed: &crate::Editor, e: floptle_core::ecs::Entity) -> std::collections::HashMap<[i32; 3], Vec<[f32; 3]>> {
    ed.terrain_render[&e].far.iter().map(|(c, m)| (*c, m.positions.clone())).collect()
}

/// **A sea that appears draws the far ground again over frames, not in
/// one.** A planet seen from orbit is some hundreds of coarse chunks. The
/// frame its water changes meshes none of them: each is marked, the workers
/// take a few at a time as they do any streaming, and every chunk draws its
/// old mesh until its new one lands. When they have, each far chunk is the
/// mesh its field makes under the water, and one made under the old water
/// that was still on its way was not taken for it.
#[test]
fn a_sea_that_changes_remeshes_far_ground_over_frames() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let e = planet(&mut ed, DVec3::ZERO, 90.0);
    let cam = DVec3::new(0.0, 0.0, 420.0);
    assert!(ed.settle_terrain_meshes(cam, std::time::Duration::from_secs(120)), "the planet did not finish meshing");
    let before = far_meshes(&ed, e);
    assert!(before.len() > 40, "the fixture: only {} chunks are drawn at the coarsest level", before.len());
    assert!(ed.terrains[&e].field.water_levels().is_empty());

    // A sea over all of it, drawn flat: every far chunk changes.
    use floptle_script::{TerrainWater, TerrainWaterRule};
    ed.terrain_water_rules.insert(7, TerrainWaterRule { water: TerrainWater::At(99.0), flatten: true, ..Default::default() });
    ed.sync_terrain_meshes(false, cam);
    assert_eq!(ed.terrains[&e].field.water_levels().len(), 1, "the field was not given its level");
    let r = &ed.terrain_render[&e];
    let coarse = r.slots.values().filter(|(_, lod)| *lod > 0).count();
    assert!(r.stale.len() + r.pending.len() >= coarse, "{} of {coarse} coarse chunks were asked for again", r.stale.len() + r.pending.len());
    assert!(r.stale.iter().all(|c| r.slots[c].1 > 0), "a full-detail chunk was asked for again: it never reads the level");
    assert_eq!(far_meshes(&ed, e), before, "the frame the sea changed in re-meshed far chunks itself");
    assert!(coarse > 16, "the fixture: {coarse} coarse chunks are no more than the workers take at once");

    // Frame after frame, until none is left to do.
    let mut frames = 0;
    let (mut most_at_once, mut most_asked) = (0, ed.terrain_render[&e].pending.len());
    let mut last = before.clone();
    loop {
        ed.sync_terrain_meshes(false, cam);
        frames += 1;
        let now = far_meshes(&ed, e);
        most_at_once = most_at_once.max(now.iter().filter(|(c, m)| last.get(*c) != Some(*m)).count());
        last = now;
        let r = &ed.terrain_render[&e];
        most_asked = most_asked.max(r.pending.len());
        if r.stale.is_empty() && r.pending.is_empty() {
            break;
        }
        assert!(frames < 20_000, "the far ground never finished: {} stale, {} on their way", r.stale.len(), r.pending.len());
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(frames >= 4, "every far chunk was made again within {frames} frames");
    assert!(most_asked <= 16, "{most_asked} far chunks were on the workers at once: the whole planet was asked for in a frame");
    assert!(most_at_once <= 16, "{most_at_once} far chunks landed in one frame: more than the workers hold at once");
    let field = &ed.terrains[&e].field;
    let mut changed = 0;
    for (c, m) in &ed.terrain_render[&e].far {
        let want = floptle_field::mesh_chunk(field, *c, 8, true);
        assert_eq!(m.positions, want.positions, "far chunk {c:?} is not the mesh its field makes under the sea");
        changed += usize::from(before.get(c) != Some(&m.positions));
    }
    assert!(changed > 40, "the fixture: only {changed} far chunks differ under the sea");

    // The same level again is no change, and asks for nothing.
    ed.sync_terrain_meshes(false, cam);
    assert!(ed.terrain_render[&e].stale.is_empty(), "an unchanged level asked for meshes again");
}

/// **A world all under a sea draws its far ball under it too.** A body
/// whose declared radius is past its sea's would otherwise cover the sea
/// from afar and uncover it up close. Ground that stands out of the water
/// anywhere the ball is sampled leaves the ball alone.
#[test]
fn a_drowned_worlds_ball_stays_under_its_sea() {
    let fill = floptle_field::procgen::PlanetFill { seed: 3, radius: 40.0, voxel: 2.0, relief: 3.0, cave_depth: 0.0, ..Default::default() };
    let mut field = floptle_field::procgen::generate_planet(&fill);
    assert_eq!(super::impostor_water_cap(&field, 40.0), f32::INFINITY, "a world with no sea has a cap");
    field.set_water_levels(vec![WaterLevel::sea(50.0)]);
    assert_eq!(super::impostor_water_cap(&field, 40.0), 50.0 - WaterLevel::MARGIN);
    field.set_water_levels(vec![WaterLevel::sea(38.0)]);
    assert_eq!(super::impostor_water_cap(&field, 40.0), f32::INFINITY, "a world with land above its sea was capped");
}

/// Pictures of a planet under a shallow sea, from orbit and along its limb,
/// with and without its water declared: `OUT=<dir>` names where they go.
/// Not a guard: the settled look of the far ground is judged by eye.
///
/// The planet is put away to the stand-ins far ground is kept as, and the
/// sea is an opaque blue ball, so ground standing out of it shows plainly.
///
/// Built with the authoring half, which holds the frame it photographs.
#[cfg(feature = "editor-ui")]
#[test]
#[ignore = "a probe: writes pictures to $OUT"]
fn probe_far_ground_under_a_sea() {
    let Ok(out) = std::env::var("OUT") else { return };
    let out = std::path::PathBuf::from(out);
    let radius = 150.0f32;
    let sea = std::env::var("SEA").ok().and_then(|s| s.parse().ok()).unwrap_or(156.0f32);
    for (name, rule) in [
        ("before", Some(floptle_script::TerrainWaterRule { water: floptle_script::TerrainWater::Off, ..Default::default() })),
        ("after", None),
        ("flatten", Some(floptle_script::TerrainWaterRule { flatten: true, ..Default::default() })),
    ] {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let fill = floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            bump_freq: 4.5,
            cave_depth: 0.0,
            surface_a: floptle_field::procgen::LayerPaint { color: [0.45, 0.6, 0.25], slot: 0 },
            surface_b: floptle_field::procgen::LayerPaint { color: [0.7, 0.62, 0.4], slot: 0 },
            ..Default::default()
        };
        let mut field = floptle_field::procgen::generate_planet(&fill);
        field.set_spec("a planet".into());
        let all = field.chunk_coords();
        field.evict(&all);
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(e, Matter::Terrain { id: 7, collision: Default::default() });
        ed.world.insert(e, floptle_core::CelestialBody { body_radius: f64::from(radius), ..Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(field));
        // The water itself, and a ball to see it by: the ball's flat faces
        // dip under a true sphere, so it is drawn that much larger.
        spawn(&mut ed, Transform::IDENTITY, water(WaterKind::Sea, sea, [0.0; 3]), Some(e));
        let ball = spawn(
            &mut ed,
            Transform { scale: Vec3::splat(sea * 1.006 / 0.85), ..Transform::IDENTITY },
            Matter::Primitive { shape: floptle_core::Shape::Sphere, color: [0.05, 0.25, 0.8] },
            None,
        );
        let _ = ball;
        if let Some(rule) = rule {
            ed.terrain_water_rules.insert(7, rule);
        }
        let r = f64::from(radius);
        for (view, eye, at) in [
            ("orbit", DVec3::new(0.3, 0.5, 3.0) * r, DVec3::ZERO),
            ("limb", DVec3::new(0.0, 0.35, 1.25) * r, DVec3::new(0.0, 1.05, 0.0) * r),
        ] {
            assert!(ed.settle_terrain_meshes(eye, std::time::Duration::from_secs(300)), "the planet did not finish meshing");
            let look = floptle_core::math::Mat4::look_at_rh(eye.as_vec3(), at.as_vec3(), Vec3::Y).inverse();
            let cam = floptle_render::RenderCamera::new(
                eye,
                Quat::from_mat4(&look),
                floptle_render::Projection::of_camera(0.9, false, 1.0, 0.5, 5000.0),
            );
            let (w, h) = (1280u32, 720u32);
            let _ = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false);
            let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false).expect("a frame");
            floptle_image::io::save_png(&out.join(format!("{view}_{name}.png")), &px, w, h).unwrap();
        }
    }
}
