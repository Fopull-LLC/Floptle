//! A box falling onto a tile floor: the first thing a 2D game does, and what
//! a first 2D level did wrong.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{BodyKind, Collidable, Matter, Name, RigidBody};

const SET: &str = "tilesets/ground.tileset.ron";

/// A 2D scene with gravity: an 8×2 tilemap of `tile`-sized squares whose
/// bottom row is solid, and a Sprite with a 2D box body above it at `x`.
fn scene(tile: f32, x: f64, collidable: bool, tile_body: bool) -> (crate::Editor, floptle_core::Entity) {
    let mut ed = crate::Editor::default();
    let mut set = floptle_tiles::TileSet { name: "ground".into(), sheet_cols: 4, sheet_rows: 4, ..Default::default() };
    set.info_mut(0).collision = floptle_tiles::TileCollision::Full;
    ed.tiles.sets.insert(SET.into(), set);

    let g = ed.world.spawn();
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 80.0 });

    let map = ed.world.spawn();
    ed.world.insert(map, Transform::IDENTITY);
    ed.world.insert(map, Name("Ground".into()));
    let mut data = vec![floptle_core::EMPTY_TILE; 16];
    for c in &mut data[8..16] {
        *c = 0;
    }
    ed.world.insert(map, Matter::Tilemap { cols: 8, rows: 2, tile, data, tileset: SET.into() });
    if collidable {
        ed.world.insert(map, Collidable);
    }
    if tile_body {
        ed.world.insert(map, RigidBody { kind: BodyKind::Box, mode: floptle_core::BodyMode::Static, ..Default::default() });
    }

    let player = ed.world.spawn();
    ed.world.insert(
        player,
        Transform { translation: DVec3::new(x, 3.0, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE },
    );
    ed.world.insert(player, Name("Player".into()));
    ed.world.insert(player, Matter::Sprite { ppu: 32.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] });
    ed.world.insert(player, RigidBody { kind: BodyKind::Box, two_d: true, ..Default::default() });
    (ed, player)
}

/// Where the player comes to rest after three seconds of Play.
fn rest(mut ed: crate::Editor, player: floptle_core::Entity) -> f64 {
    ed.toggle_play();
    assert!(ed.playing);
    for _ in 0..180 {
        ed.play_step(1.0 / 60.0, true);
    }
    floptle_core::world_transform(&ed.world, player).translation.y
}

/// A 2D box stands ON the floor, its bottom on the floor's top. It used to
/// stand on its centre point, half sunk in: its eight 3D corners sat on the
/// faces of colliders exactly as deep as it, and never counted.
#[test]
fn a_2d_box_stands_on_a_tile_floor() {
    let (ed, p) = scene(1.0, 0.0, true, false);
    let y = rest(ed, p);
    assert!((y - 0.5).abs() < 0.05, "the box's bottom must rest on the floor at 0, centre at 0.5; got {y:.3}");
}

/// A floor of small tiles is thinner than the box is deep. The box used to
/// sink until its centre found the floor, and here the centre starts below
/// the floor's half-depth reach, so it fell through.
#[test]
fn a_2d_box_stands_on_a_thin_tile_floor() {
    let (ed, p) = scene(0.25, 0.0, true, false);
    let y = rest(ed, p);
    assert!((y - 0.5).abs() < 0.05, "centre at 0.5 above a floor whose top is 0; got {y:.3}");
}

/// A RigidBody on a tilemap means its tiles are solid. It used to make the
/// whole map one default-sized box at its middle: a box landing three units
/// from the middle fell straight past the level.
#[test]
fn a_rigidbody_on_a_tilemap_makes_its_tiles_solid() {
    let (ed, p) = scene(1.0, 3.0, false, true);
    let y = rest(ed, p);
    assert!((y - 0.5).abs() < 0.05, "the tiles under x=3 must hold the box; got {y:.3}");
}

/// An 8×2 layer whose TOP row is a one-way ledge (y 0..1) with nothing under
/// it, gravity of `g` (negative pulls up), and a 2D box at height `y`.
fn ledge_scene(g: f32, y: f64) -> (crate::Editor, floptle_core::Entity) {
    let (mut ed, player) = scene(1.0, 0.0, true, false);
    let set = ed.tiles.sets.get_mut(SET).unwrap();
    set.info_mut(0).one_way = true;
    for (_, m) in ed.world.query_mut::<Matter>() {
        match m {
            Matter::Tilemap { data, .. } => {
                let e = floptle_core::EMPTY_TILE;
                *data = [vec![0u32; 8], vec![e; 8]].concat();
            }
            Matter::GravityVolume { strength, .. } => *strength = g,
            _ => {}
        }
    }
    if let Some(t) = ed.world.get_mut::<Transform>(player) {
        t.translation.y = y;
    }
    (ed, player)
}

/// A one-way ledge holds a box that lands on it…
#[test]
fn a_box_lands_on_a_one_way_ledge() {
    let (ed, p) = ledge_scene(9.81, 3.0);
    let y = rest(ed, p);
    assert!((y - 1.5).abs() < 0.05, "standing on the ledge's top at y = 1; got {y:.3}");
}

/// …and lets one rising from below pass up through it. Pulled upward, a box
/// under an ordinary ledge stops against its underside at y = -0.5.
#[test]
fn a_box_rises_through_a_one_way_ledge() {
    let (ed, p) = ledge_scene(-9.81, -2.0);
    let y = rest(ed, p);
    assert!(y > 3.0, "it must come out of the top and keep going; got {y:.3}");
}

/// A capsule — the usual character body — stands on a one-way ledge too.
#[test]
fn a_capsule_lands_on_a_one_way_ledge() {
    let (mut ed, p) = ledge_scene(9.81, 3.0);
    ed.world.insert(p, RigidBody { kind: BodyKind::Capsule, two_d: true, ..Default::default() });
    let y = rest(ed, p);
    assert!(y > 1.0 && y < 2.5, "standing on the ledge's top at y = 1; got {y:.3}");
}

/// Stand `kind` on the ledge, start a drop-through, and say where it is a
/// second later.
fn drop_from_ledge(kind: BodyKind) -> (f64, f64) {
    let (mut ed, p) = ledge_scene(9.81, 3.0);
    ed.world.insert(p, RigidBody { kind, two_d: true, ..Default::default() });
    ed.toggle_play();
    for _ in 0..120 {
        ed.play_step(1.0 / 60.0, true);
    }
    let on = floptle_core::world_transform(&ed.world, p).translation.y;
    ed.sim.as_mut().unwrap().set_body_drop_through(p.index(), 0.25);
    for _ in 0..60 {
        ed.play_step(1.0 / 60.0, true);
    }
    (on, floptle_core::world_transform(&ed.world, p).translation.y)
}

/// …and lets a body standing on it drop through when told to, falling on
/// past it. It used to catch the body again as the drop timer ran out with
/// the body half through the ledge.
#[test]
fn a_box_drops_through_a_one_way_ledge() {
    let (on, y) = drop_from_ledge(BodyKind::Box);
    assert!((on - 1.5).abs() < 0.05, "standing on the ledge first; got {on:.3}");
    assert!(y < -1.0, "it must fall through the ledge; got {y:.3}");
}

#[test]
fn a_capsule_drops_through_a_one_way_ledge() {
    let (on, y) = drop_from_ledge(BodyKind::Capsule);
    assert!(on > 1.0, "standing on the ledge first; got {on:.3}");
    assert!(y < -1.0, "it must fall through the ledge; got {y:.3}");
}

/// A picture taken outside the frame loop — `floptle shot`, `camera.capture` —
/// draws the tile layers. Nothing on that path built their meshes, so a
/// photograph of a 2D level was its sprites over an empty sky.
#[test]
#[cfg(feature = "editor-ui")]
fn a_shot_draws_the_tile_layers() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-tile-shot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(crate::new_project(&dir, "0.0.0-test", "platformer2d", true), 0);
    ed.open_project(dir.clone());
    ed.open_scene_file(&dir.join("scenes/first.ron").to_string_lossy());
    let cam_e = floptle_core::active_camera(&ed.world).expect("the template has a camera");
    let wt = floptle_core::world_transform(&ed.world, cam_e);
    let Some(Matter::Camera { ortho_height, .. }) = ed.world.get::<Matter>(cam_e).cloned() else { panic!() };
    let cam = floptle_render::RenderCamera::new(
        wt.translation,
        wt.rotation,
        floptle_render::Projection::of_camera(1.0, true, ortho_height, 0.05, 1000.0),
    );
    let (w, h) = (96u32, 54u32);
    let px = crate::shot::render_frame_pixels(&mut ed, &cam, Some(cam_e), w, h, u32::MAX, false).expect("no device");
    // The bottom rows of the view are the level's dirt: brown, not sky blue.
    let at = |x: u32, y: u32| {
        let o = ((y * w + x) * 4) as usize;
        [px[o], px[o + 1], px[o + 2]]
    };
    let ground = at(w / 2, h - 3);
    assert!(ground[0] as i32 > ground[2] as i32 + 30, "the ground drew as {ground:?}: a sky-blue pixel is a missing tile layer");
    let _ = std::fs::remove_dir_all(&dir);
}
