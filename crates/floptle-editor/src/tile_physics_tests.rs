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
