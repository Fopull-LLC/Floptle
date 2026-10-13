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

/// A polygon body drops through a one-way ledge the same way.
#[test]
fn a_polygon_drops_through_a_one_way_ledge() {
    let (on, y) = drop_from_ledge(BodyKind::Polygon);
    assert!((on - 1.5).abs() < 0.05, "standing on the ledge first; got {on:.3}");
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

/// The tile floor scene with the player's body swapped for a polygon of
/// `pts`.
fn polygon_scene(pts: &[[f32; 2]], x: f64) -> (crate::Editor, floptle_core::Entity) {
    let (mut ed, player) = scene(1.0, x, true, false);
    let rb = ed.world.get_mut::<RigidBody>(player).unwrap();
    rb.kind = BodyKind::Polygon;
    rb.outline = floptle_core::outline::Outline::convex(pts);
    (ed, player)
}

/// A polygon body stands on its outline, not on a box round the node: this
/// one is drawn from the node up, the way a sprite pivoted at its feet is,
/// so it rests with its node on the floor. A box the same size rests with
/// its centre half a unit up.
#[test]
fn a_polygon_body_stands_on_its_outline() {
    let (ed, p) = polygon_scene(&[[-0.4, 0.0], [0.4, 0.0], [0.4, 1.0], [-0.4, 1.0]], 0.0);
    let y = rest(ed, p);
    assert!(y.abs() < 0.05, "the outline's bottom is the node's origin, so the node rests at 0; got {y:.3}");
}

/// A diamond reaching a unit below its node balances on that tip: the corner
/// is one of the points the solver tests, so nothing sinks past it.
#[test]
fn a_polygon_body_rests_on_a_corner() {
    let (ed, p) = polygon_scene(&[[0.0, -1.0], [0.5, 0.0], [0.0, 0.5], [-0.5, 0.0]], 0.0);
    let y = rest(ed, p);
    assert!((y - 1.0).abs() < 0.05, "the tip is 1 under the node; got {y:.3}");
}

/// A static polygon is a ramp: a box landing on it is held above the floor,
/// and slides down it the way the slope falls.
#[test]
fn a_static_polygon_body_is_a_ramp() {
    let (mut ed, player) = scene(1.0, 0.5, true, false);
    let ramp = ed.world.spawn();
    ed.world.insert(ramp, Transform::IDENTITY);
    ed.world.insert(
        ramp,
        RigidBody {
            kind: BodyKind::Polygon,
            mode: floptle_core::BodyMode::Static,
            two_d: true,
            outline: floptle_core::outline::Outline::convex(&[[-2.0, 0.0], [2.0, 0.0], [2.0, 2.0]]),
            ..Default::default()
        },
    );
    ed.toggle_play();
    let mut low = f64::MAX;
    for _ in 0..60 {
        ed.play_step(1.0 / 60.0, true);
        let t = floptle_core::world_transform(&ed.world, player).translation;
        // The ramp's surface under x is (x + 2) / 2, rising to the right, so
        // the box meets it with its bottom-right corner.
        if t.y < 2.5 {
            low = low.min(t.y - 0.5 - ((t.x + 0.5 + 2.0) / 2.0));
        }
    }
    let t = floptle_core::world_transform(&ed.world, player).translation;
    assert!(t.x < 0.4, "it slid down the slope, to the left; x = {:.3}", t.x);
    assert!(t.y > 1.0, "held up by the ramp, well above the floor at 0.5; y = {:.3}", t.y);
    assert!(low > -0.1, "never sank into the ramp; deepest {low:.3}");
}

/// A flipped sprite's polygon body is the mirror image, like the picture.
#[test]
fn a_flipped_sprite_mirrors_its_polygon_body() {
    let (mut ed, p) = polygon_scene(&[[0.0, -0.5], [1.0, -0.5], [1.0, 0.5], [0.0, 0.5]], 0.0);
    let rb = *ed.world.get::<RigidBody>(p).unwrap();
    let right = floptle_physics::polygon_outline(&ed.world, p, &rb);
    assert!(right.points().iter().all(|q| q[0] >= 0.0));
    if let Some(Matter::Sprite { flip_x, .. }) = ed.world.get_mut::<Matter>(p) {
        *flip_x = true;
    }
    let left = floptle_physics::polygon_outline(&ed.world, p, &rb);
    assert!(left.points().iter().all(|q| q[0] <= 0.0), "{:?}", left.points());
    assert!(left.distance(-0.5, 0.0) < 0.0 && left.distance(0.5, 0.0) > 0.0);
}

/// A second 2D box in the tile floor scene, at `(x, y)`.
fn add_crate(ed: &mut crate::Editor, x: f64, y: f64) -> floptle_core::Entity {
    let c = ed.world.spawn();
    ed.world.insert(c, Transform { translation: DVec3::new(x, y, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ed.world.insert(c, Matter::Sprite { ppu: 32.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] });
    ed.world.insert(c, RigidBody { kind: BodyKind::Box, two_d: true, ..Default::default() });
    c
}

/// Moving bodies collide with each other: a crate dropped on a crate stacks
/// on it. They used to pass straight through each other and land in the
/// same place.
#[test]
fn a_crate_lands_on_a_crate() {
    let (mut ed, bottom) = scene(1.0, 0.0, true, false);
    let top = add_crate(&mut ed, 0.1, 5.0);
    ed.toggle_play();
    for _ in 0..240 {
        ed.play_step(1.0 / 60.0, true);
    }
    let y = |e| floptle_core::world_transform(&ed.world, e).translation.y;
    assert!((y(bottom) - 0.5).abs() < 0.08, "the bottom crate rests on the floor; got {:.3}", y(bottom));
    assert!((y(top) - 1.5).abs() < 0.1, "the top crate rests on the bottom one; got {:.3}", y(top));
}

/// A body moving into a crate on the floor pushes it along.
#[test]
fn a_moving_body_pushes_a_crate() {
    let (mut ed, player) = scene(1.0, -2.0, true, false);
    if let Some(t) = ed.world.get_mut::<Transform>(player) {
        t.translation.y = 0.6;
    }
    let crate_ = add_crate(&mut ed, 0.0, 0.6);
    ed.toggle_play();
    for _ in 0..30 {
        ed.play_step(1.0 / 60.0, true);
    }
    let x0 = floptle_core::world_transform(&ed.world, crate_).translation.x;
    for _ in 0..90 {
        ed.sim.as_mut().unwrap().set_body_velocity(player.index(), floptle_core::math::Vec3::new(3.0, 0.0, 0.0));
        ed.play_step(1.0 / 60.0, true);
    }
    let px = floptle_core::world_transform(&ed.world, player).translation.x;
    let cx = floptle_core::world_transform(&ed.world, crate_).translation.x;
    assert!(cx > x0 + 1.0, "the crate was pushed; {x0:.2} → {cx:.2}");
    assert!(cx - px > 0.8, "and it stayed in front of the body pushing it; body {px:.2}, crate {cx:.2}");
}

/// A 2D crate that turns, at `(x, y)`, `half` across each way.
fn turning_crate(ed: &mut crate::Editor, x: f64, y: f64, half: f32) -> floptle_core::Entity {
    let c = add_crate(ed, x, y);
    let rb = ed.world.get_mut::<RigidBody>(c).unwrap();
    rb.rotates = true;
    rb.half_extents = [half, half, 0.5];
    c
}

fn z_angle(ed: &crate::Editor, e: floptle_core::Entity) -> f32 {
    let r = floptle_core::world_transform(&ed.world, e).rotation;
    let (_, _, z) = r.to_euler(floptle_core::math::EulerRot::XYZ);
    z
}

fn play(ed: &mut crate::Editor, seconds: f32) {
    if !ed.playing {
        ed.toggle_play();
    }
    for _ in 0..(seconds * 60.0) as usize {
        ed.play_step(1.0 / 60.0, true);
    }
}

/// A crate that turns, set down flat on the floor, stays exactly as it was:
/// no creep, no wobble, no turning by itself.
#[test]
fn a_turning_crate_rests_flat() {
    let (mut ed, player) = scene(1.0, -3.0, true, false);
    ed.world.remove::<RigidBody>(player);
    let c = turning_crate(&mut ed, 0.0, 0.52, 0.5);
    play(&mut ed, 3.0);
    let t = floptle_core::world_transform(&ed.world, c).translation;
    assert!((t.y - 0.5).abs() < 0.03 && t.x.abs() < 0.03, "it stayed put: {t:?}");
    assert!(z_angle(&ed, c).abs() < 0.02, "and level: {}", z_angle(&ed, c));
}

/// A crate hanging most of the way off a ledge tips over the edge and falls,
/// turning as it goes. One that does not turn stays balanced on the corner
/// of the ledge, which no crate ever does.
#[test]
fn a_turning_crate_tips_off_a_ledge() {
    // The ledge scene's top row is a solid floor from x = -4 to 4.
    let (mut ed, player) = scene(1.0, -3.0, true, false);
    ed.world.remove::<RigidBody>(player);
    // Centre 0.3 past the right-hand end of the floor (x = 4).
    let c = turning_crate(&mut ed, 4.3, 0.55, 0.5);
    play(&mut ed, 2.0);
    let t = floptle_core::world_transform(&ed.world, c).translation;
    assert!(t.y < -1.0, "it fell off the ledge: {t:?}");
    assert!(z_angle(&ed, c).abs() > 0.3, "turning as it went: {}", z_angle(&ed, c));
}

/// A body that does not turn keeps its rotation whatever hits it.
#[test]
fn a_crate_that_does_not_turn_hangs_on_the_ledge() {
    let (mut ed, player) = scene(1.0, -3.0, true, false);
    ed.world.remove::<RigidBody>(player);
    let c = add_crate(&mut ed, 4.3, 0.55);
    play(&mut ed, 2.0);
    assert_eq!(z_angle(&ed, c), 0.0);
}

/// A ball on a slope rolls: it turns as it goes down, the way it is going.
#[test]
fn a_ball_rolls_down_a_ramp() {
    let (mut ed, player) = scene(1.0, -3.0, true, false);
    ed.world.remove::<RigidBody>(player);
    let ramp = ed.world.spawn();
    ed.world.insert(ramp, Transform::IDENTITY);
    ed.world.insert(
        ramp,
        RigidBody {
            kind: BodyKind::Polygon,
            mode: floptle_core::BodyMode::Static,
            two_d: true,
            outline: floptle_core::outline::Outline::convex(&[[-4.0, 0.0], [4.0, 0.0], [4.0, 3.0]]),
            ..Default::default()
        },
    );
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform { translation: DVec3::new(2.5, 3.0, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ed.world.insert(ball, RigidBody { kind: BodyKind::Sphere, radius: 0.4, two_d: true, rotates: true, friction: 0.8, ..Default::default() });
    play(&mut ed, 1.0);
    let t = floptle_core::world_transform(&ed.world, ball).translation;
    assert!(t.x < 1.5, "it rolled down to the left: {t:?}");
    // Rolling left is turning anticlockwise, about +Z.
    assert!(z_angle(&ed, ball) > 0.5 || z_angle(&ed, ball) < -2.0, "and it turned: {}", z_angle(&ed, ball));
}

/// Three turning crates stacked stay stacked: no jitter shakes the tower
/// down and nothing turns by itself.
#[test]
fn a_tower_of_turning_crates_stands() {
    let (mut ed, player) = scene(1.0, -3.0, true, false);
    ed.world.remove::<RigidBody>(player);
    let crates: Vec<_> = (0..3).map(|i| turning_crate(&mut ed, 0.0, 0.52 + i as f64 * 1.02, 0.5)).collect();
    play(&mut ed, 4.0);
    for (i, &c) in crates.iter().enumerate() {
        let t = floptle_core::world_transform(&ed.world, c).translation;
        assert!((t.y - (0.5 + i as f64)).abs() < 0.08, "crate {i} is where it was stacked: {t:?}");
        assert!(t.x.abs() < 0.1, "crate {i} did not slide off: {t:?}");
        assert!(z_angle(&ed, c).abs() < 0.05, "crate {i} stayed level: {}", z_angle(&ed, c));
    }
}



/// The same tower of crates that do not turn stands as still: every corner
/// in contact used to push the whole overlap, so a stack bounced itself apart.
#[test]
fn a_tower_of_crates_stands_still() {
    let (mut ed, player) = scene(1.0, -3.0, true, false);
    ed.world.remove::<RigidBody>(player);
    let crates: Vec<_> = (0..3).map(|i| add_crate(&mut ed, 0.0, 0.52 + i as f64 * 1.02)).collect();
    play(&mut ed, 4.0);
    for (i, &c) in crates.iter().enumerate() {
        let t = floptle_core::world_transform(&ed.world, c).translation;
        assert!((t.y - (0.5 + i as f64)).abs() < 0.03, "crate {i} is where it was stacked: {t:?}");
    }
}

/// A crate fired at a thin wall stops at it. At 60 units a second it moves
/// half a unit a step, past a wall a fifth of a unit thick, and used to come
/// out the far side; a turning crate is held the same way.
#[test]
fn a_fast_crate_stops_at_a_thin_wall() {
    for rotates in [false, true] {
        let (mut ed, player) = scene(1.0, -3.0, true, false);
        ed.world.remove::<RigidBody>(player);
        let wall = ed.world.spawn();
        ed.world.insert(wall, Transform { translation: DVec3::new(3.0, 2.0, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
        ed.world.insert(
            wall,
            RigidBody {
                kind: BodyKind::Box,
                mode: floptle_core::BodyMode::Static,
                half_extents: [0.1, 2.0, 0.5],
                ..Default::default()
            },
        );
        let c = add_crate(&mut ed, 0.0, 1.0);
        if let Some(rb) = ed.world.get_mut::<RigidBody>(c) {
            rb.rotates = rotates;
            rb.gravity = false;
        }
        ed.toggle_play();
        ed.sim.as_mut().unwrap().set_body_velocity(c.index(), floptle_core::math::Vec3::new(60.0, 0.0, 0.0));
        for _ in 0..30 {
            ed.play_step(1.0 / 60.0, true);
        }
        let x = floptle_core::world_transform(&ed.world, c).translation.x;
        assert!(x < 2.45, "rotates = {rotates}: the crate stayed on its side of the wall at 2.9; x = {x:.3}");
    }
}

// ---- joints --------------------------------------------------------------

use floptle_core::joint::{Joint, JointKind};

/// The floor scene with no player, for joint tests.
fn joint_scene() -> crate::Editor {
    let (mut ed, player) = scene(1.0, -3.0, true, false);
    ed.world.despawn(player);
    ed
}

/// A turning 2D box named `name` at `(x, y)`, `half` across.
fn link(ed: &mut crate::Editor, name: &str, x: f64, y: f64, half: f32) -> floptle_core::Entity {
    let e = turning_crate(ed, x, y, half);
    ed.world.insert(e, Name(name.into()));
    e
}

fn pos(ed: &crate::Editor, e: floptle_core::Entity) -> DVec3 {
    floptle_core::world_transform(&ed.world, e).translation
}

/// A pendulum: a box hinged to the world a unit to its side swings down and
/// through, its centre always a unit from the pivot.
#[test]
fn a_hinged_box_swings_as_a_pendulum() {
    let mut ed = joint_scene();
    let bob = link(&mut ed, "Bob", 1.0, 5.0, 0.2);
    ed.world.insert(bob, Joint { kind: JointKind::Hinge, anchor: [-1.0, 0.0, 0.0], ..Default::default() });
    ed.toggle_play();
    let (mut min_x, mut max_dev) = (f64::MAX, 0.0f64);
    for _ in 0..90 {
        ed.play_step(1.0 / 60.0, true);
        let p = pos(&ed, bob);
        min_x = min_x.min(p.x);
        // Pivot at (0, 5); the bob's centre is a unit from it, however it has turned.
        max_dev = max_dev.max((((p.x - 0.0).powi(2) + (p.y - 5.0).powi(2)).sqrt() - 1.0).abs());
    }
    assert!(min_x < -0.5, "it swung through to the other side; least x {min_x:.3}");
    assert!(max_dev < 0.03, "the pivot held: the bob strayed {max_dev:.3} from its circle");
}

/// A rope holds a body at its length and goes slack when the body is closer.
#[test]
fn a_rope_holds_at_its_length() {
    let mut ed = joint_scene();
    let w = link(&mut ed, "Weight", 0.0, 6.0, 0.2);
    ed.world.insert(w, Joint { kind: JointKind::Rope, length: 2.0, ..Default::default() });
    // The rope's top is where the weight starts: (0, 6). Lift it a unit first.
    ed.toggle_play();
    for _ in 0..120 {
        ed.play_step(1.0 / 60.0, true);
    }
    let y = pos(&ed, w).y;
    assert!((y - 4.0).abs() < 0.05, "hanging at the rope's length below its top; y = {y:.3}");
}

/// A chain of links, each hinged to the one above, hangs as one piece.
#[test]
fn a_chain_hangs_together() {
    let mut ed = joint_scene();
    let mut prev = String::new();
    let mut links = Vec::new();
    for i in 0..5 {
        let name = format!("Link{i}");
        let e = link(&mut ed, &name, 0.6 * (i + 1) as f64, 6.0, 0.15);
        ed.world.insert(e, Joint { kind: JointKind::Hinge, other: prev.clone(), anchor: [-0.3, 0.0, 0.0], other_anchor: [0.3, 0.0, 0.0], ..Default::default() });
        links.push(e);
        prev = name;
    }
    ed.toggle_play();
    let mut lowest = f64::MAX;
    for _ in 0..180 {
        ed.play_step(1.0 / 60.0, true);
        lowest = lowest.min(pos(&ed, *links.last().unwrap()).y);
    }
    for w in links.windows(2) {
        let d = (pos(&ed, w[1]) - pos(&ed, w[0])).length();
        assert!(d < 0.66, "neighbouring links stay joined; {d:.3} apart");
    }
    // The first link is pinned to the world at (0.3, 6), and the free end,
    // laid out three units to the side, swings down under it.
    let first = pos(&ed, links[0]);
    assert!(first.distance(DVec3::new(0.3, 6.0, 0.0)) < 0.35, "the first link stays at its pin: {first:?}");
    assert!(lowest < 4.0, "and the chain swings down from it; the free end got no lower than {lowest:.2}");
}

/// A spring stretches under a weight, settles, and holds it above where it
/// would fall.
#[test]
fn a_spring_settles_under_a_weight() {
    let mut ed = joint_scene();
    let w = link(&mut ed, "Weight", 0.0, 6.0, 0.2);
    ed.world.insert(w, Joint { kind: JointKind::Spring, length: 0.0, stiffness: 2.0, damping: 0.7, ..Default::default() });
    ed.toggle_play();
    for _ in 0..360 {
        ed.play_step(1.0 / 60.0, true);
    }
    let y1 = pos(&ed, w).y;
    for _ in 0..30 {
        ed.play_step(1.0 / 60.0, true);
    }
    let y2 = pos(&ed, w).y;
    assert!(y1 < 6.0 && y1 > 2.0, "stretched down some, held well above the floor; y = {y1:.3}");
    assert!((y1 - y2).abs() < 0.01, "and settled: {y1:.3} then {y2:.3}");
}

/// Two welded boxes fall as one: the gap and the angle between them hold.
#[test]
fn welded_boxes_move_as_one() {
    let mut ed = joint_scene();
    let a = link(&mut ed, "A", 0.0, 4.0, 0.3);
    let b = link(&mut ed, "B", 0.9, 4.6, 0.3);
    ed.world.insert(b, Joint { kind: JointKind::Fixed, other: "A".into(), ..Default::default() });
    let _ = a;
    ed.toggle_play();
    for _ in 0..180 {
        ed.play_step(1.0 / 60.0, true);
    }
    let d = pos(&ed, b) - pos(&ed, a);
    let rel = z_angle(&ed, b) - z_angle(&ed, a);
    assert!((d.length() - (0.9f64.hypot(0.6))).abs() < 0.03, "the gap held: {d:?}");
    assert!(rel.abs() < 0.03, "and so did the angle: {rel}");
    assert!(pos(&ed, a).y < 2.0, "they fell together");
}

/// A slider along X keeps a body on its rail: it does not fall, and a push
/// along the rail carries it.
#[test]
fn a_slider_keeps_its_body_on_the_rail() {
    let mut ed = joint_scene();
    let s = link(&mut ed, "Car", 0.0, 4.0, 0.2);
    ed.world.insert(s, Joint { kind: JointKind::Slider, axis: [1.0, 0.0, 0.0], ..Default::default() });
    ed.toggle_play();
    ed.sim.as_mut().unwrap().set_body_velocity(s.index(), floptle_core::math::Vec3::new(2.0, 0.0, 0.0));
    for _ in 0..60 {
        ed.play_step(1.0 / 60.0, true);
    }
    let p = pos(&ed, s);
    assert!((p.y - 4.0).abs() < 0.03, "on the rail; y = {:.3}", p.y);
    assert!(p.x > 1.0, "carried along it; x = {:.3}", p.x);
}

/// A motor turns a hinge at its speed.
#[test]
fn a_hinge_motor_turns_the_body() {
    let mut ed = joint_scene();
    let wheel = link(&mut ed, "Wheel", 0.0, 4.0, 0.4);
    if let Some(rb) = ed.world.get_mut::<RigidBody>(wheel) {
        rb.gravity = false;
    }
    ed.world.insert(wheel, Joint { kind: JointKind::Hinge, motor: true, motor_speed: 90.0, max_force: 1000.0, ..Default::default() });
    ed.toggle_play();
    for _ in 0..30 {
        ed.play_step(1.0 / 60.0, true);
    }
    // Half a second at 90° a second is 45°, give or take the spin-up.
    let a = z_angle(&ed, wheel).to_degrees();
    assert!(a > 30.0 && a < 50.0, "turned about 45°: {a:.1}");
    assert!(pos(&ed, wheel).distance(DVec3::new(0.0, 4.0, 0.0)) < 0.01, "about its pin");
}

/// A joint asked to carry more than its break force breaks, says so, and
/// lets go.
#[test]
fn an_overloaded_joint_breaks() {
    let mut ed = joint_scene();
    let w = link(&mut ed, "Weight", 0.0, 5.0, 0.3);
    if let Some(rb) = ed.world.get_mut::<RigidBody>(w) {
        rb.mass = 10.0;
    }
    ed.world.insert(w, Joint { kind: JointKind::Hinge, anchor: [0.0, 0.3, 0.0], break_force: 20.0, ..Default::default() });
    ed.toggle_play();
    for _ in 0..120 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.world.get::<Joint>(w).unwrap().broken, "the joint reports it broke");
    assert!(pos(&ed, w).y < 1.0, "and the weight fell to the floor; y = {:.3}", pos(&ed, w).y);
}


/// A hinge with limits swings no further than them.
#[test]
fn a_hinge_stops_at_its_limits() {
    let mut ed = joint_scene();
    let bob = link(&mut ed, "Bob", 1.0, 5.0, 0.2);
    ed.world.insert(bob, Joint { kind: JointKind::Hinge, anchor: [-1.0, 0.0, 0.0], limit: true, min: -20.0, max: 20.0, ..Default::default() });
    ed.toggle_play();
    let mut widest = 0.0f32;
    for _ in 0..120 {
        ed.play_step(1.0 / 60.0, true);
        widest = widest.max(z_angle(&ed, bob).abs().to_degrees());
    }
    assert!(widest > 15.0 && widest < 23.0, "it swung to its limit and stopped there: {widest:.1}°");
}

/// A target joint pulls its body to the point a script set, against gravity.
#[test]
fn a_target_joint_pulls_to_its_point() {
    let mut ed = joint_scene();
    let held = link(&mut ed, "Held", 0.0, 3.0, 0.2);
    ed.world.insert(held, Joint { kind: JointKind::Target, stiffness: 5.0, damping: 1.0, target: Some([2.0, 4.0, 0.0]), ..Default::default() });
    ed.toggle_play();
    for _ in 0..180 {
        ed.play_step(1.0 / 60.0, true);
    }
    let p = pos(&ed, held);
    assert!(p.distance(DVec3::new(2.0, 4.0, 0.0)) < 0.15, "it was carried to the target: {p:?}");
}

/// A body on two driven wheels drives along the floor, riding on their
/// suspension rather than dragging on the ground. Each wheel carries its own
/// joint, naming the body.
#[test]
fn a_car_drives_on_its_wheels() {
    let mut ed = joint_scene();
    // At the left end of the eight-unit floor, so a second's drive stays on it.
    let body = link(&mut ed, "Body", -3.0, 1.6, 0.5);
    if let Some(rb) = ed.world.get_mut::<RigidBody>(body) {
        rb.half_extents = [1.0, 0.25, 0.5];
    }
    for (i, x) in [-3.7f64, -2.3].into_iter().enumerate() {
        let w = ed.world.spawn();
        ed.world.insert(w, Transform { translation: DVec3::new(x, 0.95, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
        ed.world.insert(w, Name(format!("Wheel{i}")));
        ed.world.insert(w, RigidBody { kind: BodyKind::Sphere, radius: 0.35, two_d: true, rotates: true, friction: 1.2, ..Default::default() });
        // Suspension along the body's Y; driven clockwise to roll right.
        ed.world.insert(
            w,
            Joint {
                kind: JointKind::Wheel,
                other: "Body".into(),
                axis: [0.0, 1.0, 0.0],
                stiffness: 4.0,
                damping: 0.7,
                motor: true,
                motor_speed: -720.0,
                max_force: 50.0,
                ..Default::default()
            },
        );
    }
    ed.toggle_play();
    for _ in 0..60 {
        ed.play_step(1.0 / 60.0, true);
    }
    let p = pos(&ed, body);
    assert!(p.x > -1.5, "it drove right: {p:?}");
    assert!(p.y > 0.9, "riding on its wheels, not lying on the floor: {p:?}");
}

/// Set the floor square (the bottom row) of the scene's tilemap at column
/// `col` to `v`, the way a script's `tm:set` does while playing.
fn set_column(ed: &mut crate::Editor, col: usize, v: u32) {
    for (_, m) in ed.world.query_mut::<Matter>() {
        if let Matter::Tilemap { data, cols, .. } = m {
            data[*cols as usize + col] = v;
        }
    }
}

/// Tiles changed while playing change the collision: a crate standing on
/// the floor falls when the tiles under it are cleared, and lands on tiles
/// put back under it. A script's `tm:set` used to change the picture and
/// not the collider, so a floor built at runtime was walked straight through.
#[test]
fn tiles_changed_while_playing_collide() {
    // Columns 0..8 cover x = -4..4; the crate at x = 0.5 stands on column 4.
    let (mut ed, c) = scene(1.0, 0.5, true, false);
    ed.toggle_play();
    for _ in 0..60 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!((pos(&ed, c).y - 0.5).abs() < 0.05, "standing on the floor first");
    // A hole three tiles wide, so its corners are not left on the edges
    // either side.
    for col in 3..6 {
        set_column(&mut ed, col, floptle_core::EMPTY_TILE);
    }
    for _ in 0..15 {
        ed.play_step(1.0 / 60.0, true);
    }
    let falling = pos(&ed, c).y;
    assert!(falling < 0.4, "the floor went from under it; y = {falling:.3}");
    // The floor back while it is still sinking into the gap: it is pushed
    // up and stands on it again.
    for col in 3..6 {
        set_column(&mut ed, col, 0);
    }
    for _ in 0..120 {
        ed.play_step(1.0 / 60.0, true);
    }
    let y = pos(&ed, c).y;
    assert!((y - 0.5).abs() < 0.08, "it stands on the tiles put back; y = {y:.3}");
}

/// A static block moved while playing takes its collider with it, through the
/// real frame loop: a crate standing on it falls when it slides away. The
/// move was followed while the colliders were lent to the scripts, so it
/// changed nothing and the crate stood on air.
#[test]
fn a_static_block_moved_while_playing_takes_its_collider() {
    let (mut ed, c) = scene(1.0, 0.0, true, false);
    if let Some(t) = ed.world.get_mut::<Transform>(c) {
        t.translation.y = 3.5;
    }
    let block = ed.world.spawn();
    ed.world.insert(block, Transform { translation: DVec3::new(0.0, 2.0, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ed.world.insert(block, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
    ed.world.insert(block, Collidable);
    ed.toggle_play();
    for _ in 0..90 {
        ed.play_step(1.0 / 60.0, true);
    }
    let on = pos(&ed, c).y;
    assert!(on > 2.5, "standing on the block first; y = {on:.3}");
    if let Some(t) = ed.world.get_mut::<Transform>(block) {
        t.translation.x = 3.0;
    }
    for _ in 0..90 {
        ed.play_step(1.0 / 60.0, true);
    }
    let y = pos(&ed, c).y;
    assert!(y < 1.0, "the block slid away and it fell to the floor; y = {y:.3}");
}

/// Whether `e`'s body is grounded right now.
fn grounded(ed: &crate::Editor, e: floptle_core::Entity) -> bool {
    ed.sim.as_ref().unwrap().body_states().find(|r| r.entity == e).is_some_and(|r| r.grounded)
}

/// A body standing on a one-way ledge is grounded whether the engine pulls
/// it down or a script does, as it is on solid ground: four cases. It used to
/// be ungrounded on a one-way ledge as soon as a script owned its gravity.
#[test]
fn a_body_on_a_one_way_ledge_is_grounded_however_it_falls() {
    for one_way in [false, true] {
        for scripted in [false, true] {
            let (mut ed, p) = ledge_scene(9.81, 1.6);
            ed.tiles.sets.get_mut(SET).unwrap().info_mut(0).one_way = one_way;
            if scripted {
                ed.world.get_mut::<RigidBody>(p).unwrap().gravity = false;
            }
            ed.toggle_play();
            let mut last = false;
            for _ in 0..90 {
                if scripted {
                    // The controller's own gravity: a steady push down.
                    ed.sim.as_mut().unwrap().set_body_velocity(p.index(), floptle_core::math::Vec3::new(0.0, -2.0, 0.0));
                }
                ed.play_step(1.0 / 60.0, true);
                last = grounded(&ed, p);
            }
            let y = pos(&ed, p).y;
            assert!((y - 1.5).abs() < 0.06, "one_way {one_way}, scripted {scripted}: standing on the ledge; y = {y:.3}");
            assert!(last, "one_way {one_way}, scripted {scripted}: grounded");
            if one_way {
                // Dropping through, it is not standing on the ledge it is
                // leaving.
                ed.sim.as_mut().unwrap().set_body_drop_through(p.index(), 0.25);
                ed.play_step(1.0 / 60.0, true);
                assert!(!grounded(&ed, p), "scripted {scripted}: not grounded while dropping through");
            }
        }
    }
}

