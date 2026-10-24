//! **An imported model draws the material its file describes.**
//!
//! glTF says how a material's alpha is read (OPAQUE ignores it, MASK cuts at
//! its own threshold, BLEND blends), whether a face is drawn from behind, how
//! its base map is offset and tiled, and what its metallic and roughness are
//! with or without a picture for them. Each of those used to be dropped on
//! import: every model cut its picture's alpha at one half, drew both sides,
//! sampled its maps untransformed, and drew as plastic unless it had a
//! metallic-roughness map. This shoots one quad per rule, and two balls, and
//! reads the pixels.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::path::{Path, PathBuf};
use std::process::Command;

use floptle_assets::glb_write::{write_glb, AlphaMode, WriteMaterial, WriteMesh, WriteNode};
use floptle_render::TextureData;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flshotim-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn scaffold(dir: &Path) {
    let out = Command::new(bin())
        .args(["new", &dir.to_string_lossy(), "--template", "platformer"])
        .output()
        .expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A unit quad facing +Z, UVs top-left to bottom-right.
fn quad() -> WriteMesh {
    WriteMesh {
        positions: vec![[-0.5, -0.5, 0.0], [0.5, -0.5, 0.0], [0.5, 0.5, 0.0], [-0.5, 0.5, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 4],
        uvs: Some(vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]),
        indices: vec![0, 1, 2, 0, 2, 3],
        base_color: [1.0; 4],
        ..Default::default()
    }
}

/// A UV sphere of radius one.
fn ball() -> WriteMesh {
    let (rows, cols) = (24, 48);
    let mut m = WriteMesh { base_color: [1.0; 4], ..Default::default() };
    for i in 0..=rows {
        let t = std::f32::consts::PI * i as f32 / rows as f32;
        for j in 0..=cols {
            let p = std::f32::consts::TAU * j as f32 / cols as f32;
            let n = [t.sin() * p.cos(), t.cos(), -t.sin() * p.sin()];
            m.positions.push(n);
            m.normals.push(n);
        }
    }
    for i in 0..rows {
        for j in 0..cols {
            let a = i * (cols + 1) + j;
            let b = a + cols + 1;
            m.indices.extend_from_slice(&[a, b, b + 1, a, b + 1, a + 1]);
        }
    }
    m
}

fn model(path: &Path, mesh: WriteMesh, material: WriteMaterial, textures: &[TextureData], transform: Option<serde_json::Value>) {
    let mut node = WriteNode::mesh_node("m", mesh);
    node.material = Some(material);
    let mut glb = write_glb(&[node], &[], textures, &[]);
    if let Some(t) = transform {
        // The writer has no texture transform of its own; a file from
        // elsewhere carries one on its base-colour texture.
        let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
        let mut doc: serde_json::Value = serde_json::from_slice(&glb[20..20 + json_len]).unwrap();
        doc["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["extensions"] =
            serde_json::json!({ "KHR_texture_transform": t });
        doc["extensionsUsed"] = serde_json::json!(["KHR_texture_transform"]);
        let mut json = serde_json::to_vec(&doc).unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let rest = glb[20 + json_len..].to_vec();
        let mut out = Vec::new();
        out.extend_from_slice(&glb[0..8]);
        out.extend_from_slice(&((20 + json.len() + rest.len()) as u32).to_le_bytes());
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(&glb[16..20]);
        out.extend_from_slice(&json);
        out.extend_from_slice(&rest);
        glb = out;
    }
    std::fs::write(path, glb).unwrap();
}

fn node(name: &str, asset: &str, x: f32, back: bool) -> String {
    let r = if back { "(0.0, 1.0, 0.0, 0.0)" } else { "(0.0, 0.0, 0.0, 1.0)" };
    format!(
        "(name: \"{name}\", matter: Mesh(asset_path: \"{asset}\"), transform: (translation: ({x}, 0.0, -6.0), rotation: {r}, scale: (1.6, 1.6, 1.6))),\n"
    )
}

fn shoot(d: &Path, scene: &str, body: &str) -> Option<image::RgbaImage> {
    let text = format!(
        "(\n    name: \"{scene}\",\n    nodes: [\n        (name: \"Camera\", matter: Camera(fov_y: 1.0, active: true)),\n{body}    ],\n)\n"
    );
    std::fs::write(d.join(format!("scenes/{scene}.ron")), text).unwrap();
    let out = d.join(format!("{scene}.png"));
    let r = Command::new(bin())
        .args(["shot", &d.to_string_lossy(), "--scene", scene, "--size", "320x160", "--out", &out.to_string_lossy()])
        .output()
        .expect("run shot");
    if !r.status.success() {
        let why = String::from_utf8_lossy(&r.stderr);
        assert!(
            why.contains("no GPU") || why.contains("could not build the renderer"),
            "shot failed for a reason that is not the adapter:\n{why}"
        );
        return None;
    }
    Some(image::open(&out).expect("a PNG was written").to_rgba8())
}

/// The centre pixel of a quad at `x` world units across.
fn at(img: &image::RgbaImage, x: f32, dx: f32) -> [u8; 4] {
    // fov 1.0 at six metres: half the picture's height is 6·tan(0.5) metres.
    let half_h = 6.0 * 0.5f32.tan();
    let px = 160.0 + (x + dx) / half_h * 80.0;
    img.get_pixel(px as u32, 80).0
}

/// Solidly red: lit or not, red well above the other two.
fn red(p: [u8; 4]) -> bool {
    let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
    r > 120 && r > g + 80 && r > b + 80
}

#[test]
fn an_imported_material_draws_its_alpha_sides_transform_and_factors() {
    let d = temp("rules");
    scaffold(&d);
    std::fs::create_dir_all(d.join("models")).unwrap();
    // Red, at alpha 0.35 everywhere: under the old fixed cut of one half,
    // every quad wearing it vanished.
    let faint = TextureData { pixels: [230, 20, 20, 89].repeat(16), width: 4, height: 4 };
    let solid = TextureData { pixels: [230, 20, 20, 255].repeat(16), width: 4, height: 4 };
    // Left half green, right half blue.
    let halves = TextureData {
        pixels: (0..16).flat_map(|i| if i % 4 < 2 { [20, 220, 20, 255] } else { [20, 20, 220, 255] }).collect(),
        width: 4,
        height: 4,
    };
    let mat = |alpha, double_sided| WriteMaterial {
        base_texture: Some(0),
        alpha,
        double_sided,
        roughness: 1.0,
        ..Default::default()
    };
    let m = d.join("models");
    model(&m.join("mask_low.glb"), quad(), mat(AlphaMode::Mask(0.2), true), std::slice::from_ref(&faint), None);
    model(&m.join("mask_high.glb"), quad(), mat(AlphaMode::Mask(0.5), true), std::slice::from_ref(&faint), None);
    model(&m.join("opaque.glb"), quad(), mat(AlphaMode::Opaque, true), &[faint], None);
    model(&m.join("one_side.glb"), quad(), mat(AlphaMode::Opaque, false), std::slice::from_ref(&solid), None);
    model(&m.join("two_sides.glb"), quad(), mat(AlphaMode::Opaque, true), &[solid], None);
    model(
        &m.join("shifted.glb"),
        quad(),
        mat(AlphaMode::Opaque, true),
        &[halves],
        Some(serde_json::json!({ "offset": [0.5, 0.0] })),
    );
    let mut body = String::new();
    for (i, (name, back)) in
        [("mask_low", false), ("mask_high", false), ("opaque", false), ("one_side", true), ("two_sides", true), ("shifted", false)]
            .iter()
            .enumerate()
    {
        body.push_str(&node(name, &format!("models/{name}.glb"), -5.0 + 2.0 * i as f32, *back));
    }
    let Some(img) = shoot(&d, "rules", &body) else {
        let _ = std::fs::remove_dir_all(&d);
        return;
    };
    let _ = img.save(d.join("..").join("floptle-imported-materials-rules.png"));
    let x = |i: usize| -5.0 + 2.0 * i as f32;
    assert!(red(at(&img, x(0), 0.0)), "MASK 0.2 keeps alpha 0.35: got {:?}", at(&img, x(0), 0.0));
    assert!(!red(at(&img, x(1), 0.0)), "MASK 0.5 cuts alpha 0.35: got {:?}", at(&img, x(1), 0.0));
    assert!(red(at(&img, x(2), 0.0)), "OPAQUE ignores alpha: got {:?}", at(&img, x(2), 0.0));
    assert!(!red(at(&img, x(3), 0.0)), "a single-sided quad seen from behind is not drawn: got {:?}", at(&img, x(3), 0.0));
    assert!(red(at(&img, x(4), 0.0)), "a double-sided one is: got {:?}", at(&img, x(4), 0.0));
    // Shifted by half its width, the quad's left half samples the picture's
    // right half.
    let left = at(&img, x(5), -0.4);
    assert!(left[2] > 120 && left[1] < 100, "the texture transform moved the picture: left half is {left:?}");

    // A gold ball with metallic 1 and no maps against one with metallic 0:
    // metal has no diffuse, so the two must not be the same picture.
    let gold = |metallic| WriteMaterial { base_color: [1.0, 0.75, 0.3, 1.0], metallic, roughness: 0.2, ..Default::default() };
    model(&m.join("metal.glb"), ball(), gold(1.0), &[], None);
    model(&m.join("plastic.glb"), ball(), gold(0.0), &[], None);
    // A sky for the metal to reflect: blue above a brown ground. Metal is
    // its reflection, so in an empty grey room it would only be dark.
    let sky = image::RgbaImage::from_fn(64, 32, |_, y| {
        if y < 16 { image::Rgba([90, 150, 230, 255]) } else { image::Rgba([110, 80, 50, 255]) }
    });
    std::fs::create_dir_all(d.join("textures")).unwrap();
    sky.save(d.join("textures/sky.png")).unwrap();
    let body = "(name: \"Sky\", matter: Skybox(texture: Some(\"textures/sky.png\"))),\n".to_string()
        + &node("metal", "models/metal.glb", -1.5, false)
        + &node("plastic", "models/plastic.glb", 1.5, false);
    let img = shoot(&d, "balls", &body).expect("it rendered once");
    let _ = img.save(d.join("..").join("floptle-imported-materials-balls.png"));
    // Metal is its reflection, tinted by its colour: the upper half of the
    // metal ball shows the blue sky, where the gold plastic stays warm.
    let half_h = 6.0 * 0.5f32.tan();
    let upper = |cx: f32| {
        let c = (160.0 + cx / half_h * 80.0) as i32;
        let (mut r, mut b) = (0u64, 0u64);
        for dy in -24i32..=-8 {
            for dx in -10i32..=10 {
                let p = img.get_pixel((c + dx) as u32, (80 + dy) as u32).0;
                r += p[0] as u64;
                b += p[2] as u64;
            }
        }
        (r, b)
    };
    let (mr, mb) = upper(-1.5);
    let (pr, pb) = upper(1.5);
    assert!(mb > mr, "the metal ball does not reflect the sky (red {mr}, blue {mb})");
    assert!(pr > pb, "the plastic one is not its own warm colour (red {pr}, blue {pb})");

    // **The pass comes from the blend mode.** A BLEND model whose factor is 1
    // blends its picture's alpha; a node material set opaque at opacity 0.3
    // draws solid; an old scene's material at opacity 0.3, with no blend
    // written, blends as it always did.
    model(&m.join("blended.glb"), quad(), mat(AlphaMode::Blend, true), std::slice::from_ref(&faint_red()), None);
    let cube = |name: &str, x: f32, material: &str| {
        format!(
            "(name: \"{name}\", matter: Primitive(shape: Cube, color: (1.0, 1.0, 1.0)), material: Some({material}), \
             transform: (translation: ({x}, 0.0, -6.0), scale: (1.2, 1.2, 0.1))),\n"
        )
    };
    let body = node("blended", "models/blended.glb", -3.0, false)
        + &cube("solid", 0.0, "(color: (0.9, 0.1, 0.1), alpha: 0.3, blend: Some(Opaque))")
        + &cube("old", 3.0, "(color: (0.9, 0.1, 0.1), alpha: 0.3)");
    let img = shoot(&d, "blends", &body).expect("it rendered once");
    let _ = img.save(d.join("..").join("floptle-imported-materials-blends.png"));
    let bg = img.get_pixel(160, 10).0;
    let see_through = |p: [u8; 4]| p[1] > 60 && !red(p) && p[0] as i32 > p[1] as i32 + 10;
    let b = at(&img, -3.0, 0.0);
    assert!(see_through(b), "a BLEND model blends its picture's alpha: {b:?} over {bg:?}");
    let solid = at(&img, 0.0, 0.0);
    assert!(red(solid), "an opaque material at opacity 0.3 draws solid: {solid:?}");
    let old = at(&img, 3.0, 0.0);
    assert!(see_through(old), "an old scene's faded material still blends: {old:?} over {bg:?}");

    // **Classic (0.116) draws a factor-only metal as non-metal**, as that
    // release did: the same metal ball, in a project set to the old rules.
    let cfg_path = d.join("project.ron");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap();
    let cfg = if cfg.contains("imported_materials: Some(AsFile)") {
        cfg.replace("imported_materials: Some(AsFile)", "imported_materials: Some(Classic)")
    } else {
        let end = cfg.rfind(')').unwrap();
        format!("{}    imported_materials: Some(Classic),\n)", &cfg[..end])
    };
    std::fs::write(&cfg_path, cfg).unwrap();
    let body = "(name: \"Sky\", matter: Skybox(texture: Some(\"textures/sky.png\"))),\n".to_string()
        + &node("metal", "models/metal.glb", -1.5, false)
        + &node("plastic", "models/plastic.glb", 1.5, false);
    let img = shoot(&d, "classic", &body).expect("it rendered once");
    let _ = img.save(d.join("..").join("floptle-imported-materials-classic.png"));
    let half_h = 6.0 * 0.5f32.tan();
    let c = (160.0 - 1.5 / half_h * 80.0) as i32;
    let (mut r, mut bl) = (0u64, 0u64);
    for dy in -24i32..=-8 {
        for dx in -10i32..=10 {
            let p = img.get_pixel((c + dx) as u32, (80 + dy) as u32).0;
            r += p[0] as u64;
            bl += p[2] as u64;
        }
    }
    assert!(r > bl, "under Classic the factor-only metal is not metal (red {r}, blue {bl})");
    let _ = std::fs::remove_dir_all(&d);
}

/// Red at alpha 0.35.
fn faint_red() -> TextureData {
    TextureData { pixels: [230, 20, 20, 89].repeat(16), width: 4, height: 4 }
}
