//! A model's own maps draw as authored.
//!
//! glTF import used to keep the base colour alone, so every metal-rough,
//! normal, occlusion and emissive map in the file was dropped, and a game had
//! to repeat each one by hand in `object_materials`. The part now draws its
//! file's maps through the same function a hand-set material does.

use floptle_core::math::{DVec3, Quat};
use floptle_core::{Matter, Transform};
use floptle_render::{Projection, RenderCamera};

/// Five 16×16 images — base colour, metal-rough, normal, occlusion, emissive —
/// each with a pattern that shows if it is dropped or swapped.
pub(crate) fn write_maps(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("textures")).unwrap();
    let img = |name: &str, f: &dyn Fn(u32, u32) -> [u8; 4]| {
        let mut im = image::RgbaImage::new(16, 16);
        for (x, y, p) in im.enumerate_pixels_mut() {
            *p = image::Rgba(f(x, y));
        }
        im.save(dir.join("textures").join(name)).unwrap();
    };
    img("base.png", &|x, y| if (x / 4 + y / 4) % 2 == 0 { [200, 180, 150, 255] } else { [90, 110, 160, 255] });
    // Roughness in green, metallic in blue, glTF's packing.
    img("mr.png", &|x, _| [0, 40 + (x * 13) as u8, if x < 8 { 255 } else { 0 }, 255]);
    // Ridges across x: the normal leans left, then right.
    img("normal.png", &|x, _| if (x / 2) % 2 == 0 { [40, 128, 220, 255] } else { [215, 128, 220, 255] });
    img("ao.png", &|_, y| [if y < 8 { 255 } else { 70 }, 0, 0, 255]);
    // A red glow in one corner only.
    img("emit.png", &|x, y| if x >= 10 && y >= 10 { [255, 40, 20, 255] } else { [0, 0, 0, 255] });
}

/// A 2×2 quad facing +Z, its one material naming all five maps.
pub(crate) fn write_gltf(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("models")).unwrap();
    let mut bin: Vec<u8> = Vec::new();
    for p in [[-1.0f32, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0], [-1.0, 1.0, 0.0]] {
        p.iter().for_each(|v| bin.extend(v.to_le_bytes()));
    }
    for _ in 0..4 {
        [0.0f32, 0.0, 1.0].iter().for_each(|v| bin.extend(v.to_le_bytes()));
    }
    for uv in [[0.0f32, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]] {
        uv.iter().for_each(|v| bin.extend(v.to_le_bytes()));
    }
    for i in [0u16, 1, 2, 0, 2, 3] {
        bin.extend(i.to_le_bytes());
    }
    std::fs::write(dir.join("models/panel.bin"), &bin).unwrap();
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
"nodes":[{{"name":"Panel","mesh":0}}],
"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1,"TEXCOORD_0":2}},"indices":3,"material":0}}]}}],
"materials":[{{"name":"Hull","pbrMetallicRoughness":{{"baseColorTexture":{{"index":0}},"metallicRoughnessTexture":{{"index":1}},"metallicFactor":0.8,"roughnessFactor":0.9}},
 "normalTexture":{{"index":2,"scale":1.0}},"occlusionTexture":{{"index":3,"strength":1.0}},"emissiveTexture":{{"index":4}},"emissiveFactor":[1.0,1.0,1.0]}}],
"textures":[{{"source":0}},{{"source":1}},{{"source":2}},{{"source":3}},{{"source":4}}],
"images":[{{"uri":"../textures/base.png"}},{{"uri":"../textures/mr.png"}},{{"uri":"../textures/normal.png"}},{{"uri":"../textures/ao.png"}},{{"uri":"../textures/emit.png"}}],
"buffers":[{{"byteLength":{len},"uri":"panel.bin"}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":48}},{{"buffer":0,"byteOffset":48,"byteLength":48}},{{"buffer":0,"byteOffset":96,"byteLength":32}},{{"buffer":0,"byteOffset":128,"byteLength":12}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":4,"type":"VEC3","min":[-1,-1,0],"max":[1,1,0]}},
 {{"bufferView":1,"componentType":5126,"count":4,"type":"VEC3"}},
 {{"bufferView":2,"componentType":5126,"count":4,"type":"VEC2"}},
 {{"bufferView":3,"componentType":5123,"count":6,"type":"SCALAR"}}]}}"#,
        len = bin.len()
    );
    std::fs::write(dir.join("models/panel.gltf"), json).unwrap();
}

/// The same five maps, set by hand the way a prefab's `object_materials` sets them.
fn by_hand() -> floptle_core::Material {
    floptle_core::Material {
        texture: Some("textures/base.png".into()),
        shading: floptle_core::Shading::Physical,
        metallic: 0.8,
        roughness: 0.9,
        metallic_map: Some("textures/mr.png".into()),
        roughness_map: Some("textures/mr.png".into()),
        normal_map: Some("textures/normal.png".into()),
        ao_map: Some("textures/ao.png".into()),
        emissive: [1.0, 1.0, 1.0],
        emissive_strength: 1.0,
        emissive_map: Some("textures/emit.png".into()),
        ..Default::default()
    }
}

/// The panel lit from the side, photographed head on, with `override_mat` on its
/// one material (or none: the file's own look).
fn photograph(tag: &str, override_mat: Option<floptle_core::Material>) -> Option<Vec<u8>> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle-imported-maps-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_maps(&dir);
    write_gltf(&dir);
    ed.project_root = dir.clone();
    assert!(ed.import_model("models/panel.gltf"), "the fixture model did not import");
    let panel = ed.world.spawn();
    ed.world.insert(panel, Transform::IDENTITY);
    ed.world.insert(panel, Matter::Mesh { asset_path: "models/panel.gltf".into() });
    if let Some(m) = override_mat {
        let mut om = std::collections::BTreeMap::new();
        om.insert("Hull".to_string(), m);
        ed.world.insert(panel, floptle_core::ObjectMaterials(om));
    }
    let sun = ed.world.spawn();
    let turn = Quat::from_rotation_y(0.9) * Quat::from_rotation_x(-0.5);
    ed.world.insert(sun, Transform { rotation: turn, ..Transform::IDENTITY });
    ed.world.insert(sun, floptle_core::Light { intensity: 1.2, ..Default::default() });
    let cam = RenderCamera::new(DVec3::new(0.0, 0.0, 2.6), Quat::IDENTITY, Projection::of_camera(1.0, false, 6.0, 0.05, 1000.0));
    // Twice: the first frame uploads the hand-set material's textures.
    let _ = crate::shot::render_frame_pixels(&mut ed, &cam, None, 96, 96, !0, false)?;
    let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, 96, 96, !0, false)?;
    if let Ok(out) = std::env::var("IMPORTED_MAPS_PNG") {
        let _ = image::RgbaImage::from_raw(96, 96, px.clone()).map(|im| im.save(format!("{out}_{tag}.png")));
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some(px)
}

fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
    let sum: u64 = a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum();
    sum as f64 / a.len() as f64
}

/// **A glTF with all five maps draws exactly as the same maps set by hand.**
/// And it is the maps that make the picture: the base colour alone, set by
/// hand, comes out clearly different.
#[test]
fn a_gltfs_own_maps_draw_like_the_same_maps_set_by_hand() {
    let Some(file) = photograph("file", None) else { return };
    let Some(hand) = photograph("hand", Some(by_hand())) else { return };
    let base_only = floptle_core::Material { texture: Some("textures/base.png".into()), ..Default::default() };
    let Some(plain) = photograph("plain", Some(base_only)) else { return };
    let same = mean_diff(&file, &hand);
    let apart = mean_diff(&hand, &plain);
    assert!(apart > 6.0, "the fixture: the maps should change the picture (base colour alone differs by {apart:.2})");
    assert!(same < 0.5, "the file's maps differ from the same maps set by hand by {same:.2} a channel");
    // The emissive map's corner glows red on the panel's lower right (uv 10..16
    // is the right and bottom of the image).
    let at = |px: &[u8], x: usize, y: usize| {
        let i = (y * 96 + x) * 4;
        [px[i], px[i + 1], px[i + 2]]
    };
    let corner = at(&file, 64, 64);
    assert!(corner[0] > corner[1].saturating_add(60), "no red glow where the emissive map has it: {corner:?}");
}
