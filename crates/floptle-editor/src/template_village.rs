//! Builds the `village2d` template: the Pixel Platformer's ground and player,
//! two people to talk to, and a dialogue box built from a conversation file.
//!
//! A demo of how a dialogue system can be made in a game's own scripts, not
//! an engine feature: everything that talks lives in `scripts/talk.lua` and
//! `data/*.json`. Run after editing anything below:
//!
//! ```text
//! cargo test -p floptle-editor --lib regenerate_village2d -- --ignored
//! ```
//!
//! and commit what it writes under `templates/village2d/`.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{BodyKind, Collidable, Material, Matter, Name, RigidBody, ScriptInst, Scripts, Tags};

use crate::template_gen::*;

const COLS: u32 = 40;
const ROWS: u32 = 12;

const CLOTH: Rgba = [90, 140, 220, 255];
const CLOTH_HI: Rgba = [150, 190, 250, 255];
const HAIR: Rgba = [120, 60, 50, 255];
const NAVY: Rgba = [28, 30, 52, 255];
const RIM: Rgba = [214, 196, 160, 255];
const RIM_DK: Rgba = [120, 98, 80, 255];

fn script(kind: &str) -> ScriptInst {
    ScriptInst { kind: kind.into(), enabled: true, params: Vec::new(), refs: Vec::new(), strs: Vec::new(), pinned_bools: None }
}

/// Mira: two 16-px frames of standing, in blue.
fn mira_png() -> Canvas {
    let mut c = Canvas::new(T * 2, T);
    for f in 0..2u32 {
        let bob = f;
        for x in 4..12u32 {
            for y in 0..(13 + bob).min(16) {
                let edge = x == 4 || x == 11 || y == 12 + bob;
                let col = if y >= 9 + bob {
                    if edge { INK } else if y >= 11 + bob { HAIR } else { SKIN_HI }
                } else if edge || y == 0 {
                    INK
                } else if x <= 6 {
                    CLOTH_HI
                } else {
                    CLOTH
                };
                c.set(f, x, y, col);
            }
        }
        c.set(f, 6, 10 + bob, INK);
        c.set(f, 9, 10 + bob, INK);
    }
    c
}

/// A signpost, one frame.
fn sign_png() -> Canvas {
    let mut c = Canvas::new(T, T);
    for y in 0..9 {
        c.set(0, 7, y, WOOD_LO);
        c.set(0, 8, y, WOOD_LO);
    }
    for x in 2..14 {
        for y in 8..14 {
            let edge = x == 2 || x == 13 || y == 8 || y == 13;
            c.set(0, x, y, if edge { WOOD_LO } else { WOOD_HI });
        }
    }
    for x in [4, 5, 6, 8, 9, 11] {
        c.set(0, x, 11, WOOD_LO);
    }
    for x in [4, 5, 7, 8, 10] {
        c.set(0, x, 10, WOOD_LO);
    }
    c
}

/// Mira's portraits, 32 px each: plain and smiling.
fn portraits_png() -> Canvas {
    const P: u32 = 32;
    let mut c = Canvas { w: P * 2, h: P, px: vec![0; (P * 2 * P * 4) as usize] };
    let put = |c: &mut Canvas, f: u32, x: u32, y: u32, col: Rgba| {
        let o = ((y * c.w + f * P + x) * 4) as usize;
        c.px[o..o + 4].copy_from_slice(&col);
    };
    for f in 0..2 {
        for y in 0..P {
            for x in 0..P {
                let (dx, dy) = (x as i32 - 16, y as i32 - 14);
                let face = dx * dx + dy * dy <= 100;
                let hair = (dx * dx + (dy + 3) * (dy + 3) <= 130 && y < 12) || (y < 24 && !(7..=24).contains(&x) && y > 6);
                let body = y >= 25 && (6..26).contains(&x);
                let col = if face && !(hair && y < 9) {
                    Some(SKIN_HI)
                } else if hair {
                    Some(HAIR)
                } else if body {
                    Some(if x < 12 { CLOTH_HI } else { CLOTH })
                } else {
                    None
                };
                if let Some(col) = col {
                    put(&mut c, f, x, y, col);
                }
            }
        }
        // Eyes, and a mouth that smiles in the second.
        for (x, y) in [(12, 14), (12, 15), (19, 14), (19, 15)] {
            put(&mut c, f, x, y, INK);
        }
        let mouth: &[(u32, u32)] = if f == 0 { &[(14, 20), (15, 20), (16, 20), (17, 20)] } else { &[(13, 19), (14, 20), (15, 20), (16, 20), (17, 20), (18, 19)] };
        for &(x, y) in mouth {
            put(&mut c, f, x, y, INK);
        }
    }
    c
}

/// The box: a 24-px frame to 9-slice, dark inside with a light rim.
fn frame_png() -> Canvas {
    const F: u32 = 24;
    let mut c = Canvas { w: F, h: F, px: vec![0; (F * F * 4) as usize] };
    for y in 0..F {
        for x in 0..F {
            let d = x.min(y).min(F - 1 - x).min(F - 1 - y);
            let corner = (x.min(F - 1 - x) == 0 && y.min(F - 1 - y) <= 1) || (y.min(F - 1 - y) == 0 && x.min(F - 1 - x) <= 1);
            if corner {
                continue;
            }
            let col = match d {
                0 => RIM_DK,
                1 => RIM,
                2 => RIM_DK,
                _ => NAVY,
            };
            let o = ((y * F + x) * 4) as usize;
            c.px[o..o + 4].copy_from_slice(&col);
        }
    }
    c
}

/// The bundled pixel font as a BMFont: printable ASCII rasterized at ten
/// pixels, a pixel to each cell of the font's grid, on a 16-wide sheet.
fn pixel_font() -> (Canvas, String) {
    let ttf: &[u8] = include_bytes!("../../../packages/fofighter-kit/assets/fonts/Fofighter.ttf");
    let font = fontdue::Font::from_bytes(ttf, fontdue::FontSettings::default()).unwrap();
    const SIZE: f32 = 10.0;
    const CELL: u32 = 12;
    let line = font.horizontal_line_metrics(SIZE).unwrap();
    let base = line.ascent.round() as i32;
    let mut c = Canvas { w: CELL * 16, h: CELL * 6, px: vec![0; (CELL * 16 * CELL * 6 * 4) as usize] };
    let mut chars = String::new();
    for (i, ch) in (32u8..127).map(char::from).enumerate() {
        let (m, bm) = font.rasterize(ch, SIZE);
        let (cx, cy) = ((i as u32 % 16) * CELL, (i as u32 / 16) * CELL);
        for y in 0..m.height.min(CELL as usize) {
            for x in 0..m.width.min(CELL as usize) {
                if bm[y * m.width + x] >= 128 {
                    let o = (((cy + y as u32) * c.w + cx + x as u32) * 4) as usize;
                    c.px[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
        let yoff = base - m.height as i32 - m.ymin;
        chars.push_str(&format!(
            "char id={} x={cx} y={cy} width={} height={} xoffset={} yoffset={yoff} xadvance={} page=0\n",
            ch as u32,
            m.width,
            m.height,
            m.xmin,
            m.advance_width.round() as i32,
        ));
    }
    let fnt = format!(
        "info face=\"Fofighter\" size={} bold=0 italic=0\ncommon lineHeight={} base={base} scaleW={} scaleH={} pages=1\npage id=0 file=\"pixel.png\"\nchars count=95\n{chars}",
        SIZE as u32,
        (line.new_line_size.round() as u32).max(SIZE as u32 + 2),
        c.w,
        c.h,
    );
    (c, fnt)
}

fn level() -> Vec<u32> {
    let e = floptle_core::EMPTY_TILE;
    let mut g = vec![e; (COLS * ROWS) as usize];
    let mut put = |x: u32, y: u32, t: u32| g[(y * COLS + x) as usize] = t;
    for x in 0..COLS {
        put(x, 9, GRASS);
        put(x, 10, DIRT);
        put(x, 11, DIRT);
    }
    for x in 16..20 {
        put(x, 6, LEDGE);
    }
    for y in 2..9 {
        put(0, y, STONE);
        put(COLS - 1, y, STONE);
    }
    for (x, t) in [(4, TUFT), (9, FLOWER), (14, TUFT), (23, FLOWER), (27, TUFT), (33, FLOWER)] {
        put(x, 8, t);
    }
    g
}

const CONVERSATION_MIRA: &str = r#"{
  "start": "hello",
  "lines": {
    "hello": { "who": "Mira", "face": 1, "text": "Oh, hello! You're new around here.", "next": "ask" },
    "ask": {
      "who": "Mira", "face": 0, "text": "Have you seen the old well at the end of the path?",
      "choices": [
        { "text": "Not yet.", "next": "notYet" },
        { "text": "What's so special about it?", "next": "special" }
      ]
    },
    "notYet": { "who": "Mira", "face": 1, "text": "Go and look! Jump up on the planks to see over the wall.", "next": "bye" },
    "special": { "who": "Mira", "face": 0, "text": "They say if you shout into it, it shouts back. I've never dared.", "next": "bye" },
    "bye": { "who": "Mira", "face": 1, "text": "See you around." }
  }
}
"#;

const CONVERSATION_SIGN: &str = r#"{
  "start": "read",
  "lines": {
    "read": { "who": "Sign", "text": "WELCOME TO THE VILLAGE. Press E to talk to people.", "next": "more" },
    "more": { "who": "Sign", "text": "Every word they say lives in data/<their name>.json. Change it and talk again." }
  }
}
"#;

#[test]
#[ignore]
fn regenerate_village2d() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/village2d");
    std::fs::create_dir_all(&dir).unwrap();
    let save = |name: &str, c: &Canvas| floptle_image::io::save_png(&dir.join(name), &c.px, c.w, c.h).unwrap();
    save("mira.png", &mira_png());
    save("sign.png", &sign_png());
    save("portraits.png", &portraits_png());
    save("dialogue_frame.png", &frame_png());
    let (font_png, fnt) = pixel_font();
    save("pixel.png", &font_png);
    std::fs::write(dir.join("pixel.fnt"), fnt).unwrap();
    std::fs::write(dir.join("Mira.json"), CONVERSATION_MIRA).unwrap();
    std::fs::write(dir.join("Sign.json"), CONVERSATION_SIGN).unwrap();

    let mut w = floptle_core::World::default();
    let sky = w.spawn();
    w.insert(sky, Name("Sky".into()));
    w.insert(sky, Transform::IDENTITY);
    w.insert(
        sky,
        Matter::Skybox {
            color: [0.98, 0.78, 0.62],
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

    let map = w.spawn();
    w.insert(map, Name("Level".into()));
    w.insert(map, place(COLS as f64 * 0.5, ROWS as f64 * 0.5));
    w.insert(
        map,
        Matter::Tilemap { cols: COLS, rows: ROWS, tile: 1.0, data: level(), tileset: "tilesets/ground.tileset.ron".into() },
    );
    w.insert(map, Material { unlit: true, ..Default::default() });
    w.insert(map, Collidable);

    let sprite = |w: &mut floptle_core::World, name: &str, x: f64, tex: &str, cols: u32| {
        let e = w.spawn();
        w.insert(e, Name(name.into()));
        w.insert(e, place(x, 3.5));
        w.insert(e, Matter::Sprite { ppu: 16.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] });
        w.insert(e, Material { texture: Some(tex.into()), sheet_cols: cols, sheet_rows: 1, unlit: true, ..Default::default() });
        e
    };
    let sign = sprite(&mut w, "Sign", 6.0, "textures/sign.png", 1);
    w.insert(sign, Tags(vec!["npc".into()]));
    let mira = sprite(&mut w, "Mira", 24.0, "textures/mira.png", 2);
    w.insert(mira, Tags(vec!["npc".into()]));
    w.insert(mira, Matter::Sprite { ppu: 16.0, size: 1.0, cell: 0, flip_x: true, flip_y: false, pivot: [0.5, 0.5] });

    let p = sprite(&mut w, "Player", 3.0, "textures/player.png", 6);
    w.insert(
        p,
        RigidBody { kind: BodyKind::Box, two_d: true, half_extents: [0.4, 0.5, 0.5], friction: 0.0, mass: crate::scene_ops::CHARACTER_MASS, ..Default::default() },
    );
    w.insert(p, Scripts(vec![script("pixelPlayer"), script("talk")]));

    let ui = w.spawn();
    w.insert(ui, Name("Dialogue".into()));
    w.insert(ui, Transform::IDENTITY);
    w.insert(ui, Matter::Empty);
    w.insert(ui, floptle_ui::UiLayer { design_height: 270.0, pixel_scale: true, ..Default::default() });

    let cam = w.spawn();
    w.insert(cam, Name("Camera".into()));
    w.insert(cam, Transform { translation: DVec3::new(8.0, 5.0, 20.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
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
            ortho_height: 9.0,
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
            look_ahead: [1.5, 0.0],
            ..Default::default()
        },
    );

    let doc = floptle_scene::to_doc("first", &w);
    std::fs::write(dir.join("first.ron"), floptle_scene::to_ron(&doc).unwrap()).unwrap();
    floptle_say::say!("wrote {}", dir.display());
}

/// Open Mira's conversation in the village and photograph it to `$OUT`, to
/// look at the dialogue box.
#[cfg(feature = "editor-ui")]
#[test]
#[ignore]
fn probe_village_dialogue_shot() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let dir = std::env::temp_dir().join(format!("floptle-village-shot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(crate::new_project(&dir, "0.0.0-test", "village2d", true), 0);
    ed.open_project(dir.clone());
    ed.open_scene_file(&dir.join("scenes/first.ron").to_string_lossy());
    let player = ed.world.query::<Name>().find(|(_, n)| n.0 == "Player").map(|(e, _)| e).unwrap();
    if let Some(t) = ed.world.get_mut::<Transform>(player) {
        t.translation.x = 22.8;
    }
    ed.toggle_play();
    for _ in 0..60 {
        ed.play_step(1.0 / 60.0, true);
    }
    let presses: usize = std::env::var("PRESSES").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for _ in 0..presses {
        use floptle_input::{Key, Source};
        ed.raw_input.keys.insert(Key::KeyE);
        ed.raw_input.pressed.insert(Source::Key(Key::KeyE));
        ed.tick_input_edges.0.insert(Source::Key(Key::KeyE));
        ed.play_step(1.0 / 60.0, true);
        ed.raw_input.pressed.clear();
        ed.raw_input.keys.remove(&Key::KeyE);
        for _ in 0..120 {
            ed.play_step(1.0 / 60.0, true);
        }
    }
    let cam_e = floptle_core::active_camera(&ed.world).unwrap();
    let wt = floptle_core::world_transform(&ed.world, cam_e);
    let Some(Matter::Camera { ortho_height, .. }) = ed.world.get::<Matter>(cam_e).cloned() else { panic!() };
    let cam = floptle_render::RenderCamera::new(
        wt.translation,
        wt.rotation,
        floptle_render::Projection::of_camera(960.0 / 540.0, true, ortho_height, 0.05, 1000.0),
    );
    let (w, h) = (960u32, 540u32);
    let px = crate::shot::render_frame_pixels(&mut ed, &cam, Some(cam_e), w, h, u32::MAX, true).unwrap();
    floptle_image::io::save_png(std::path::Path::new(&std::env::var("OUT").unwrap()), &px, w, h).unwrap();
}
