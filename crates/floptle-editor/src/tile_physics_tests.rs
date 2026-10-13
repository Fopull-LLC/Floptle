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
