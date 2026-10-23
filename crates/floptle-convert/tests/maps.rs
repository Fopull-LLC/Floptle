//! **Every map arrives in the slot the artist plugged it into.**
//!
//! Each test converts a model whose materials are known picture by picture,
//! then reads the result with the engine's own importer and checks each slot
//! by what is *in* it: every picture here is a different flat colour, so a
//! map that lands in the wrong slot, or is packed into the wrong channel,
//! shows up as the wrong number rather than passing because something was
//! there.

mod support;

use std::path::Path;

use floptle_assets::gltf_import::{import, ImportedModel, ImportedPart};
use floptle_convert::{MapSlot, Report};
use support::*;

/// Convert `src`, write the result beside it, and import that.
fn convert_and_import(src: &Path) -> (ImportedModel, Report) {
    let (out, report) = floptle_convert::convert_file(src).unwrap_or_else(|e| panic!("{}: {e}", src.display()));
    (import(&out).expect("the engine reads what was written"), report)
}

fn part<'a>(m: &'a ImportedModel, material: &str) -> &'a ImportedPart {
    m.parts.iter().find(|p| p.material == material).unwrap_or_else(|| {
        panic!("no part for {material}: {:?}", m.parts.iter().map(|p| &p.material).collect::<Vec<_>>())
    })
}

/// The first pixel of texture `i`.
fn px(m: &ImportedModel, i: Option<usize>) -> [u8; 4] {
    let t = &m.textures[i.expect("the slot has a map")];
    [t.pixels[0], t.pixels[1], t.pixels[2], t.pixels[3]]
}

fn close(a: [u8; 4], b: [u8; 4], what: &str) {
    for k in 0..4 {
        assert!(a[k].abs_diff(b[k]) <= 2, "{what}: got {a:?}, expected {b:?}");
    }
}

/// A unit cube's eight corners and six quads, faces 0–2 on material 0 and
/// 3–5 on material 1.
fn cube(scale: f64) -> (Vec<[f64; 3]>, Vec<[i64; 4]>) {
    let s = scale;
    let p = vec![
        [-s, -s, -s], [s, -s, -s], [s, s, -s], [-s, s, -s],
        [-s, -s, s], [s, -s, s], [s, s, s], [-s, s, s],
    ];
    let q = vec![[0, 3, 2, 1], [4, 5, 6, 7], [0, 1, 5, 4], [2, 3, 7, 6], [1, 2, 6, 5], [0, 4, 7, 3]];
    (p, q)
}

/// **An FBX with classic Phong materials**, the most common thing in a bought
/// asset pack, with its pictures scattered the way packs scatter them: one
/// embedded in the file, one in a `Textures` folder under a different case
/// and an absolute path from the artist's machine, one recorded as `.tga`
/// that was re-saved as `.png`, one beside the model, and one in a sibling
/// folder one level up. And one that does not exist anywhere.
#[test]
fn an_fbx_keeps_every_map_in_its_slot_wherever_the_pictures_are() {
    let root = scratch("fbx-maps");
    let dir = root.join("Models");
    std::fs::create_dir_all(&dir).unwrap();
    let diffuse = png(&root.join("unused.png"), [200, 100, 50, 255]);
    png(&dir.join("Textures/Crate_N.PNG"), [128, 128, 255, 255]);
    png(&dir.join("crate_gloss.png"), [200, 200, 200, 255]);
    png(&dir.join("crate_e.png"), [10, 250, 10, 255]);
    // Left half clear, right half solid: a cut-out.
    png_with(&root.join("textures/crate_mask.png"), 4, 4, |x, _| if x < 2 { [0, 0, 0, 255] } else { [255, 255, 255, 255] });

    let (pos, quads) = cube(1.0);
    let mut f = Fbx::new(100.0);
    f.object(&geometry(100, "Box", &pos, &quads, true, &[0, 0, 0, 1, 1, 1]))
        .object(&model(200, "Box", "Mesh", [0.0; 3]))
        .object(&phong(300, "Crate", "\t\t\tP: \"DiffuseColor\", \"Color\", \"\", \"A\",0.3,0.3,0.3\n"))
        .object(&phong(301, "Trim", "\t\t\tP: \"DiffuseColor\", \"Color\", \"\", \"A\",0.9,0.9,0.9\n"))
        .object(&texture(400, 500, "Diffuse", "C:\\Users\\artist\\crate_d.png", "crate_d.png", Some(&diffuse)))
        .object(&texture(401, 501, "Normal", "D:\\pack\\Maps\\crate_n.png", "..\\..\\Maps\\crate_n.png", None))
        .object(&texture(402, 502, "Gloss", "crate_gloss.tga", "crate_gloss.tga", None))
        .object(&texture(403, 503, "Glow", "crate_e.png", "crate_e.png", None))
        .object(&texture(404, 504, "Mask", "/home/artist/pack/textures/crate_mask.png", "textures/crate_mask.png", None))
        .object(&texture(405, 505, "TrimD", "trim_d.png", "trim_d.png", None))
        .oo(200, 0)
        .oo(100, 200)
        .oo(300, 200)
        .oo(301, 200);
    for (t, v) in [(400, 500), (401, 501), (402, 502), (403, 503), (404, 504), (405, 505)] {
        f.oo(v, t);
    }
    f.op(400, 300, "DiffuseColor")
        .op(401, 300, "NormalMap")
        .op(402, 300, "ShininessExponent")
        .op(403, 300, "EmissiveColor")
        .op(404, 300, "TransparentColor")
        .op(405, 301, "DiffuseColor");
    let src = dir.join("crate.fbx");
    f.write(&src);

    let (m, report) = convert_and_import(&src);
    let crate_ = part(&m, "Crate");
    // Base colour: the embedded picture, its alpha cut by the mask.
    let base = &m.textures[crate_.texture.expect("a base colour map")];
    assert_eq!(base.pixels[..3], [200, 100, 50], "the embedded diffuse is the base colour");
    assert_eq!(base.pixels[3], 0, "the mask's clear half is clear");
    assert_eq!(base.pixels[(3 * 4) as usize + 3], 255, "and its solid half solid");
    assert_eq!(crate_.base_color, [1.0, 1.0, 1.0], "a classic material's swatch colour does not tint its map");
    close(px(&m, crate_.maps.normal), [128, 128, 255, 255], "normal map");
    // Shininess is glossiness: the packed roughness is its inverse, in green,
    // and metallic (no map) is white in blue, set by the factor.
    close(px(&m, crate_.maps.metallic_roughness), [255, 55, 255, 255], "roughness from gloss");
    assert_eq!((crate_.maps.roughness, crate_.maps.metallic), (1.0, 0.0));
    close(px(&m, crate_.maps.emissive), [10, 250, 10, 255], "emissive map");
    assert!(crate_.maps.emissive_factor.iter().all(|&c| c > 0.0), "an emissive map with no factor still glows");
    assert_eq!(crate_.maps.occlusion, None, "no occlusion map was given");

    // The report names every assignment, and the one picture it could not find.
    for slot in [MapSlot::BaseColor, MapSlot::Normal, MapSlot::Roughness, MapSlot::Emissive, MapSlot::Opacity] {
        assert!(report.maps.iter().any(|u| u.material == "Crate" && u.slot == slot), "{slot:?} not reported: {:?}", report.maps);
    }
    assert_eq!(report.missing.len(), 1, "{:?}", report.missing);
    let miss = &report.missing[0];
    assert_eq!((miss.material.as_str(), miss.slot, miss.name.as_str()), ("Trim", MapSlot::BaseColor, "trim_d.png"));
    assert!(report.missing_lines()[0].contains("trim_d.png") && report.missing_lines()[0].contains("Trim"));
    // Trim still converts, at its own colour.
    let trim = part(&m, "Trim");
    assert_eq!(trim.texture, None);
    assert!((trim.base_color[0] - 0.9).abs() < 1e-3);
    let _ = std::fs::remove_dir_all(&root);
}

/// **An OBJ's `.mtl` maps**, including the two packings glTF needs: separate
/// roughness and metallic pictures into one, and a bump map that is a height
/// map into a normal map.
#[test]
fn an_obj_keeps_its_mtl_maps_and_packs_roughness_with_metallic() {
    let dir = scratch("obj-maps");
    png(&dir.join("textures/crate_d.png"), [200, 100, 50, 255]);
    png(&dir.join("crate_r.png"), [180, 180, 180, 255]);
    png(&dir.join("crate_m.png"), [30, 30, 30, 255]);
    png(&dir.join("crate_e.png"), [10, 250, 10, 255]);
    // A slope, not a flat: what comes out must lean.
    png_with(&dir.join("crate_h.png"), 8, 8, |x, _| { let v = (x * 30) as u8; [v, v, v, 255] });
    std::fs::write(
        dir.join("crate.mtl"),
        "newmtl Crate\nKd 1 1 1\nNs 50\nmap_Kd crate_d.png\nmap_Bump crate_h.png\nmap_Pr crate_r.png\n\
         map_Pm crate_m.png\nmap_Ke crate_e.png\nKe 1 1 1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("crate.obj"),
        "mtllib crate.mtl\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvn 0 0 1\n\
         usemtl Crate\nf 1/1/1 2/2/1 3/3/1 4/4/1\n",
    )
    .unwrap();

    let (m, report) = convert_and_import(&dir.join("crate.obj"));
    let p = part(&m, "Crate");
    close(px(&m, p.texture), [200, 100, 50, 255], "map_Kd found in textures/");
    // R is occlusion (none: white), G roughness, B metallic.
    close(px(&m, p.maps.metallic_roughness), [255, 180, 30, 255], "map_Pr in green, map_Pm in blue");
    assert_eq!((p.maps.roughness, p.maps.metallic), (1.0, 1.0), "the maps set the values, the factors pass them");
    let n = &m.textures[p.maps.normal.expect("the bump map became a normal map")];
    assert!(n.pixels[2] > 200 && n.pixels[0] < 128, "a slope rising to the right leans the normal left: {:?}", &n.pixels[..4]);
    close(px(&m, p.maps.emissive), [10, 250, 10, 255], "map_Ke");
    assert!(report.maps.iter().any(|u| u.slot == MapSlot::Normal && u.note == Some("from a height map")), "{:?}", report.maps);
    assert!(report.missing.is_empty(), "{:?}", report.missing);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A loose glTF packs verbatim**: every map, factor and sampler filter the
/// engine reads from the loose file, it reads the same from the packed one.
/// And a picture that is not there is reported against its material and
/// slot, the rest of the model packing without it.
#[test]
fn a_loose_gltf_keeps_every_map_factor_and_filter() {
    use floptle_assets::glb_write::{write_glb, AlphaMode, WriteMaterial, WriteMesh, WriteNode};
    use floptle_render::{TexFilter, TextureData};
    let dir = scratch("gltf-maps");
    let flat = |c: [u8; 4]| TextureData { pixels: c.repeat(4), width: 2, height: 2 };
    let textures = vec![
        flat([200, 100, 50, 255]),
        flat([128, 128, 255, 255]),
        flat([255, 90, 20, 255]),
        flat([77, 255, 255, 255]),
        flat([10, 250, 10, 255]),
    ];
    let filters = [Some(TexFilter::Pixelated), None, Some(TexFilter::Smooth), None, Some(TexFilter::SmoothMipmaps)];
    let mut node = WriteNode::mesh_node(
        "panel",
        WriteMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]),
            indices: vec![0, 1, 2],
            base_color: [1.0; 4],
            ..Default::default()
        },
    );
    node.material = Some(WriteMaterial {
        name: "Panel".into(),
        base_color: [0.9, 0.8, 0.7, 1.0],
        base_texture: Some(0),
        metallic: 0.4,
        roughness: 0.6,
        metallic_roughness: Some(2),
        normal: Some(1),
        normal_scale: 0.5,
        occlusion: Some(3),
        occlusion_strength: 0.7,
        emissive: [0.2, 0.3, 0.4],
        emissive_texture: Some(4),
        alpha: AlphaMode::Mask(0.4),
        double_sided: true,
    });
    let glb = write_glb(&[node], &[], &textures, &filters);
    // Split into the loose form: JSON, a .bin, and each picture its own file.
    let (json, bin) = split(&glb);
    let mut doc: serde_json::Value = serde_json::from_slice(&json).unwrap();
    for i in 0..textures.len() {
        let view = doc["images"][i]["bufferView"].as_u64().unwrap() as usize;
        let v = &doc["bufferViews"][view];
        let (o, l) = (v["byteOffset"].as_u64().unwrap() as usize, v["byteLength"].as_u64().unwrap() as usize);
        std::fs::create_dir_all(dir.join("tex")).unwrap();
        std::fs::write(dir.join(format!("tex/map {i}.png")), &bin[o..o + l]).unwrap();
        doc["images"][i] = serde_json::json!({ "uri": format!("tex/map%20{i}.png") });
    }
    doc["buffers"][0]["uri"] = serde_json::json!("panel.bin");
    std::fs::write(dir.join("panel.bin"), &bin).unwrap();
    std::fs::write(dir.join("panel.gltf"), serde_json::to_vec(&doc).unwrap()).unwrap();

    let loose = import(&dir.join("panel.gltf")).unwrap();
    let (packed, report) = convert_and_import(&dir.join("panel.gltf"));
    let (a, b) = (&loose.parts[0], &packed.parts[0]);
    assert_eq!(a.maps, b.maps, "every map and factor as the loose file had it");
    assert_eq!((a.texture, a.base_color), (b.texture, b.base_color));
    assert_eq!(loose.texture_filters, packed.texture_filters, "each picture's filter");
    assert_eq!(packed.texture_filters, filters);
    for (x, y) in loose.textures.iter().zip(&packed.textures) {
        assert_eq!(x.pixels, y.pixels);
    }
    assert!(b.maps.normal.is_some() && b.maps.occlusion.is_some() && b.maps.emissive.is_some());
    assert_eq!(report.maps.iter().filter(|u| u.material == "Panel").count(), 6, "{:?}", report.maps);

    // Take one picture away: reported, by material and slot, and left out.
    std::fs::remove_file(dir.join("tex/map 1.png")).unwrap();
    std::fs::remove_file(dir.join("panel.glb")).unwrap();
    let (gone, report) = convert_and_import(&dir.join("panel.gltf"));
    assert_eq!(gone.parts[0].maps.normal, None, "no normal map samples a placeholder");
    assert_eq!(gone.parts[0].maps.emissive, b.maps.emissive, "the others are where they were");
    assert_eq!(report.missing.len(), 1);
    assert_eq!((report.missing[0].slot, report.missing[0].material.as_str()), (MapSlot::Normal, "Panel"));
    assert_eq!(report.missing[0].name, "map 1.png", "named as the file is, not as the URI spells it");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pull the JSON and BIN chunks back out of a `.glb`.
fn split(glb: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut off = 12;
    let (mut json, mut bin) = (Vec::new(), Vec::new());
    while off + 8 <= glb.len() {
        let len = u32::from_le_bytes(glb[off..off + 4].try_into().unwrap()) as usize;
        let kind = u32::from_le_bytes(glb[off + 4..off + 8].try_into().unwrap());
        let body = &glb[off + 8..off + 8 + len];
        match kind {
            0x4E4F534A => json = body.to_vec(),
            0x004E4942 => bin = body.to_vec(),
            _ => {}
        }
        off += 8 + len;
    }
    (json, bin)
}
