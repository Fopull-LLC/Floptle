//! Builds the `platformer2d` starter template: its pixel art, its tileset and
//! its level.
//!
//! Generated rather than hand-written because a scene file typed by hand is
//! the kind of data that loads and is quietly wrong, and art drawn here can be
//! changed by changing a number. Run it after editing anything below:
//!
//! ```text
//! cargo test -p floptle-editor --lib regenerate_platformer2d -- --ignored
//! ```
//!
//! and commit what it writes under `templates/platformer2d/`.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{BodyKind, Collidable, Material, Matter, Name, RigidBody, ScriptInst, Scripts};

/// One 16-px tile per entry, left to right in `tiles.png`.
pub(crate) const GRASS: u32 = 0;
pub(crate) const DIRT: u32 = 1;
pub(crate) const RAMP_UP: u32 = 2;
pub(crate) const RAMP_DOWN: u32 = 3;
pub(crate) const LEDGE: u32 = 4;
pub(crate) const STONE: u32 = 5;
pub(crate) const TUFT: u32 = 6;
pub(crate) const FLOWER: u32 = 7;

pub(crate) const T: u32 = 16;
const COLS: u32 = 60;
const ROWS: u32 = 16;

pub(crate) type Rgba = [u8; 4];
pub(crate) const GRASS_HI: Rgba = [167, 240, 112, 255];
pub(crate) const GRASS_LO: Rgba = [56, 183, 100, 255];
pub(crate) const DIRT_HI: Rgba = [176, 110, 66, 255];
pub(crate) const DIRT_LO: Rgba = [122, 72, 46, 255];
pub(crate) const DIRT_DK: Rgba = [86, 50, 40, 255];
pub(crate) const STONE_HI: Rgba = [148, 176, 194, 255];
pub(crate) const STONE_LO: Rgba = [86, 108, 134, 255];
pub(crate) const WOOD_HI: Rgba = [214, 152, 86, 255];
pub(crate) const WOOD_LO: Rgba = [140, 86, 48, 255];
pub(crate) const INK: Rgba = [41, 36, 64, 255];
pub(crate) const SKIN: Rgba = [239, 125, 87, 255];
pub(crate) const SKIN_HI: Rgba = [255, 205, 117, 255];
pub(crate) const WHITE: Rgba = [244, 244, 244, 255];
pub(crate) const PETAL: Rgba = [255, 120, 160, 255];

pub(crate) struct Canvas {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) px: Vec<u8>,
}

impl Canvas {
    pub(crate) fn new(w: u32, h: u32) -> Self {
        Self { w, h, px: vec![0; (w * h * 4) as usize] }
    }
    /// Set a pixel of cell `cell`, with `y` counted UP from the cell's bottom.
    pub(crate) fn set(&mut self, cell: u32, x: u32, y_up: u32, c: Rgba) {
        let (gx, gy) = (cell * T + x, T - 1 - y_up);
        if gx < self.w && gy < self.h {
            let o = ((gy * self.w + gx) * 4) as usize;
            self.px[o..o + 4].copy_from_slice(&c);
        }
    }
}

/// Dirt with a little texture: darker speckles on a fixed pattern.
fn dirt(x: u32, y: u32) -> Rgba {
    match (x * 7 + y * 13) % 11 {
        0 => DIRT_DK,
        1 | 2 => DIRT_LO,
        _ => DIRT_HI,
    }
}

pub(crate) fn tiles_png() -> Canvas {
    let mut c = Canvas::new(T * 8, T);
    for x in 0..T {
        for y in 0..T {
            // Grass on dirt.
            let g = if y >= 13 { if y == 15 || (x + y) % 5 == 0 { GRASS_HI } else { GRASS_LO } } else { dirt(x, y) };
            c.set(GRASS, x, y, g);
            c.set(DIRT, x, y, dirt(x, y));
            // Ramps: solid under the diagonal, grass along it.
            if y <= x {
                c.set(RAMP_UP, x, y, if x - y < 3 { GRASS_LO } else { dirt(x, y) });
            }
            if y <= T - 1 - x {
                c.set(RAMP_DOWN, x, y, if T - 1 - x - y < 3 { GRASS_LO } else { dirt(x, y) });
            }
            // A plank ledge across the top quarter.
            if y >= 12 {
                let edge = y == 12 || x % 8 == 0;
                c.set(LEDGE, x, y, if y == 15 { WOOD_HI } else if edge { WOOD_LO } else { WOOD_HI });
            }
            // Stone bricks, offset every other row.
            let brick_row = y / 4;
            let mortar = y % 4 == 0 || (x + brick_row * 4) % 8 == 0;
            c.set(STONE, x, y, if mortar { STONE_LO } else { STONE_HI });
        }
    }
    // Decoration: a grass tuft and a flower, a few pixels each, walk-through.
    for (x, h) in [(5u32, 3u32), (7, 5), (8, 4), (10, 2)] {
        for y in 0..h {
            c.set(TUFT, x, y, GRASS_LO);
        }
        c.set(TUFT, x, h, GRASS_HI);
    }
    for y in 0..5 {
        c.set(FLOWER, 8, y, GRASS_LO);
    }
    for (dx, dy) in [(0i32, 6i32), (-1, 5), (1, 5), (0, 4)] {
        c.set(FLOWER, (8 + dx) as u32, dy as u32, PETAL);
    }
    c.set(FLOWER, 8, 5, SKIN_HI);
    c
}

/// Six 16-px frames: two standing (a breath), four running.
pub(crate) fn player_png() -> Canvas {
    let mut c = Canvas::new(T * 6, T);
    for f in 0..6u32 {
        let bob = if f == 1 || f == 3 || f == 5 { 1 } else { 0 };
        // Body: a rounded block, outlined.
        for x in 3..13u32 {
            for y in (3 + bob)..(14 + bob).min(16) {
                let edge = x == 3 || x == 12 || y == 3 + bob || y == 13 + bob;
                let corner = (x == 3 || x == 12) && (y == 3 + bob || y == 13 + bob);
                if corner {
                    continue;
                }
                let col = if edge { INK } else if y >= 10 + bob && x <= 6 { SKIN_HI } else { SKIN };
                c.set(f, x, y, col);
            }
        }
        // An eye, looking the way it runs.
        c.set(f, 9, 9 + bob, WHITE);
        c.set(f, 10, 9 + bob, WHITE);
        c.set(f, 10, 8 + bob, INK);
        c.set(f, 9, 8 + bob, WHITE);
        // Feet: together standing, apart on alternate running frames.
        let (l, r) = match f {
            2 | 4 => (3u32, 10u32),
            3 | 5 => (5, 8),
            _ => (4, 9),
        };
        for (fx, h) in [(l, 3u32), (r, 3)] {
            for x in fx..fx + 3 {
                for y in 0..h + bob {
                    c.set(f, x, y, INK);
                }
            }
        }
    }
    c
}

/// The level, row-major from the top. Returns the grid.
fn level() -> Vec<u32> {
    let e = floptle_core::EMPTY_TILE;
    let mut g = vec![e; (COLS * ROWS) as usize];
    let mut put = |x: u32, y: u32, t: u32| g[(y * COLS + x) as usize] = t;
    // Ground: three rows, with a gap to jump.
    for x in 0..COLS {
        if (23..26).contains(&x) {
            continue;
        }
        put(x, 13, GRASS);
        put(x, 14, DIRT);
        put(x, 15, DIRT);
    }
    // A hill with a ramp each side.
    put(34, 12, RAMP_UP);
    for x in 35..45 {
        put(x, 12, GRASS);
        put(x, 13, DIRT);
    }
    put(45, 12, RAMP_DOWN);
    // Ledges to climb.
    for x in 8..11 {
        put(x, 10, LEDGE);
    }
    for x in 13..16 {
        put(x, 7, LEDGE);
    }
    for x in 27..31 {
        put(x, 9, LEDGE);
    }
    for x in 49..53 {
        put(x, 8, LEDGE);
    }
    // Stone walls at both ends, so the level has edges.
    for y in 4..13 {
        put(0, y, STONE);
        put(COLS - 1, y, STONE);
    }
    // Decoration on the grass.
    for (x, t) in [(5, TUFT), (12, FLOWER), (19, TUFT), (29, TUFT), (38, FLOWER), (41, TUFT), (55, FLOWER)] {
        let y = if (35..45).contains(&x) { 11 } else { 12 };
        put(x, y, t);
    }
    g
}

pub(crate) fn place(x: f64, y: f64) -> Transform {
    Transform { translation: DVec3::new(x, y, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE }
}

#[test]
#[ignore]
fn regenerate_platformer2d() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/platformer2d");
    std::fs::create_dir_all(&dir).unwrap();

    let tiles = tiles_png();
    let player = player_png();
    floptle_image::io::save_png(&dir.join("tiles.png"), &tiles.px, tiles.w, tiles.h).unwrap();
    floptle_image::io::save_png(&dir.join("player.png"), &player.px, player.w, player.h).unwrap();

    // The tileset, its collision read off the art like a dropped sheet's.
    let mut set = floptle_tiles::TileSet {
        name: "ground".into(),
        texture: "textures/tiles.png".into(),
        sheet_cols: 8,
        sheet_rows: 1,
        ..Default::default()
    };
    for cell in 0..8 {
        if let Some(coll) = floptle_tiles::collision_from_art(&tiles.px, tiles.w, tiles.h, 8, 1, cell) {
            set.info_mut(cell).collision = coll;
        }
    }
    set.info_mut(LEDGE).tags = vec!["ledge".into()];
    // Planks are jumped up through and landed on.
    set.info_mut(LEDGE).one_way = true;
    set.prune();
    std::fs::write(dir.join("ground.tileset.ron"), set.to_ron().unwrap()).unwrap();

    let mut w = floptle_core::World::default();
    let sky = w.spawn();
    w.insert(sky, Name("Sky".into()));
    w.insert(sky, Transform::IDENTITY);
    w.insert(
        sky,
        Matter::Skybox {
            color: [0.42, 0.72, 0.92],
            size: 500.0,
            texture: None,
            tint: [1.0, 1.0, 1.0],
            shader: None,
            shader_params: Default::default(),
        },
    );

    let g = w.spawn();
    w.insert(g, Name("Gravity".into()));
    w.insert(g, Transform::IDENTITY);
    w.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 30.0, radius: 80.0 });

    // The map's top-left square sits at (0, 16), so square (c, r) spans
    // x c..c+1 and y 15-r..16-r: world units and squares agree.
    let map = w.spawn();
    w.insert(map, Name("Level".into()));
    w.insert(map, place(COLS as f64 * 0.5, ROWS as f64 * 0.5));
    w.insert(
        map,
        Matter::Tilemap { cols: COLS, rows: ROWS, tile: 1.0, data: level(), tileset: "tilesets/ground.tileset.ron".into() },
    );
    w.insert(map, Material { unlit: true, ..Default::default() });
    w.insert(map, Collidable);

    let p = w.spawn();
    w.insert(p, Name("Player".into()));
    w.insert(p, place(4.0, 6.0));
    w.insert(p, Matter::Sprite { ppu: 16.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] });
    w.insert(
        p,
        Material { texture: Some("textures/player.png".into()), sheet_cols: 6, sheet_rows: 1, unlit: true, ..Default::default() },
    );
    w.insert(
        p,
        RigidBody { kind: BodyKind::Box, two_d: true, half_extents: [0.4, 0.5, 0.5], friction: 0.0, mass: crate::scene_ops::CHARACTER_MASS, ..Default::default() },
    );
    w.insert(
        p,
        Scripts(vec![ScriptInst {
            kind: "pixelPlayer".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]),
    );

    let cam = w.spawn();
    w.insert(cam, Name("Camera".into()));
    w.insert(cam, Transform { translation: DVec3::new(8.0, 7.0, 20.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
    w.insert(
        cam,
        Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: true,
            ortho_height: 11.0,
            stack: Vec::new(),
        },
    );
    w.insert(
        cam,
        floptle_core::camera2d::Camera2D {
            follow: "Player".into(),
            smoothing: 0.12,
            dead_zone: [1.5, 1.0],
            limits_on: true,
            limit_min: [0.0, 0.0],
            limit_max: [COLS as f32, ROWS as f32],
            pixel_snap: 16.0,
            look_ahead: [2.0, 0.0],
            ..Default::default()
        },
    );

    let doc = floptle_scene::to_doc("first", &w);
    std::fs::write(dir.join("first.ron"), floptle_scene::to_ron(&doc).unwrap()).unwrap();
    floptle_say::say!("wrote {}", dir.display());
}
