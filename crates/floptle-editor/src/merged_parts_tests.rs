//! **A model whose parts differ only in material numbers is one instance, and
//! looks the same as it did as one instance per part** (card 0685).
//!
//! A wall made of three quads, one material each (the outside, the inside, a
//! trim), sharing no texture: different colours, roughness and glow. Imported,
//! it gets a merged mesh; drawn, it is one instance. A per-part override that
//! keeps it opaque leaves it one instance, and one that makes a part blend
//! sends it back to its parts. The picture is the same, byte for byte, drawn
//! merged and drawn part by part, with and without a tint.

use floptle_core::math::{DVec3, Mat4, Quat};
use floptle_core::{Matter, Transform};
use floptle_render::{Projection, RenderCamera};

fn wall_glb() -> Vec<u8> {
    use floptle_assets::glb_write::{write_glb, WriteMaterial, WriteMesh, WriteNode};
    let quad = |x: f32| WriteMesh {
        positions: vec![[x - 0.45, -0.5, 0.0], [x + 0.45, -0.5, 0.0], [x + 0.45, 0.5, 0.0], [x - 0.45, 0.5, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 4],
        uvs: Some(vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]),
        indices: vec![0, 1, 2, 0, 2, 3],
        base_color: [1.0; 4],
        ..Default::default()
    };
    let part = |name: &str, x: f32, color: [f32; 4], roughness: f32, emissive: [f32; 3]| {
        let mut n = WriteNode::mesh_node(name, quad(x));
        n.material = Some(WriteMaterial {
            name: name.to_string(),
            base_color: color,
            roughness,
            metallic: 0.0,
            emissive,
            ..Default::default()
        });
        n
    };
    let glb = write_glb(
        &[
            part("Outside", -1.0, [0.8, 0.3, 0.2, 1.0], 0.3, [0.0; 3]),
            part("Inside", 0.0, [0.2, 0.7, 0.4, 1.0], 0.9, [0.0; 3]),
            part("Trim", 1.0, [0.3, 0.3, 0.8, 1.0], 0.5, [0.2, 0.1, 0.0]),
        ],
        &[],
        &[],
        &[],
    );
    one_object(&glb)
}

/// The writer gives each material its own object; a building kit's piece is
/// ONE object whose mesh has a primitive per material (Solar's kit: 1 object,
/// 2-4 primitives), which imports as a static model. Regroup the file that
/// way: one mesh holding every primitive, on one node.
fn one_object(glb: &[u8]) -> Vec<u8> {
    let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    let mut j: serde_json::Value = serde_json::from_slice(&glb[20..20 + json_len]).unwrap();
    let prims: Vec<serde_json::Value> =
        j["meshes"].as_array().unwrap().iter().flat_map(|m| m["primitives"].as_array().unwrap().clone()).collect();
    j["meshes"] = serde_json::json!([{ "name": "wall", "primitives": prims }]);
    j["nodes"] = serde_json::json!([{ "name": "wall", "mesh": 0 }]);
    j["scenes"] = serde_json::json!([{ "nodes": [0] }]);
    j["scene"] = serde_json::json!(0);
    let mut text = serde_json::to_vec(&j).unwrap();
    while !text.len().is_multiple_of(4) {
        text.push(b' ');
    }
    let rest = &glb[20 + json_len..];
    let total = 12 + 8 + text.len() + rest.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&glb[0..8]);
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&text);
    out.extend_from_slice(rest);
    out
}

fn editor_with_wall(tag: &str) -> Option<(crate::Editor, floptle_core::Entity, std::path::PathBuf)> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let root = std::env::temp_dir().join(format!("floptle_merged_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("models")).unwrap();
    std::fs::write(root.join("models/wall.glb"), wall_glb()).unwrap();
    ed.project_root = root.clone();
    let lighting = ed.world.spawn();
    ed.world.insert(lighting, floptle_core::Light { direction: [0.4, 0.6, 0.7], shadows: false, ..Default::default() });
    let wall = ed.world.spawn();
    ed.world.insert(wall, Transform::from_translation(DVec3::new(0.0, 0.0, -3.0)));
    ed.world.insert(wall, Matter::Mesh { asset_path: "models/wall.glb".into() });
    assert!(ed.import_model("models/wall.glb"), "the wall did not import");
    Some((ed, wall, root))
}

/// How many instances (and shader draws) `push_mesh_instances` makes of the
/// wall, as the frame's gather would.
fn instances_of(ed: &mut crate::Editor, wall: floptle_core::Entity) -> usize {
    let crate::Editor { gpu, raster, mesh_registry, texture_registry, world, .. } = ed;
    let (gpu, raster) = (gpu.as_ref().unwrap(), raster.as_mut().unwrap());
    let asset = mesh_registry.get("models/wall.glb").expect("imported");
    let mut instances = Vec::new();
    let mut skins = Vec::new();
    let mut flsl_out = Vec::new();
    crate::mesh_instances::push_mesh_instances(
        gpu,
        raster,
        asset,
        None,
        Mat4::IDENTITY,
        None,
        None,
        world.get::<floptle_core::ObjectMaterials>(wall),
        None,
        texture_registry,
        None,
        wall,
        &mut crate::anim::SkinVariants::default(),
        &mut Vec::new(),
        &mut instances,
        &mut skins,
        None,
        &mut flsl_out,
        &crate::shaders::ObjFlslBinds::default(),
    );
    instances.len() + skins.len() + flsl_out.len()
}

fn picture(ed: &mut crate::Editor) -> Vec<u8> {
    let cam = RenderCamera::new(DVec3::ZERO, Quat::IDENTITY, Projection::Perspective { fov_y: 0.9, near: 0.05, far: 1000.0 });
    let (w, h) = (160u32, 90u32);
    let tex = crate::capture::render_frame_texture(ed, &cam, None, w, h, u32::MAX, false, false).expect("a GPU editor renders");
    crate::offscreen::read_back_frame(ed.gpu.as_ref().unwrap(), &tex).expect("the frame reads back")
}

#[test]
fn a_wall_of_three_materials_is_one_instance_until_a_part_blends() {
    let Some((mut ed, wall, root)) = editor_with_wall("count") else { return };
    assert!(
        ed.mesh_registry.get("models/wall.glb").is_some_and(|a| a.parts.len() == 3 && a.merged.is_some()),
        "the three-part wall got no merged mesh: {:?}", ed.mesh_registry.get("models/wall.glb").map(|a| (a.parts.len(), a.rig.is_some(), a.part_meta.iter().map(|m| m.textured).collect::<Vec<_>>()))
    );
    assert_eq!(instances_of(&mut ed, wall), 1, "the wall draws as more than one instance");
    // An override that keeps it opaque: still one, with that slot changed.
    let mut om = floptle_core::ObjectMaterials::default();
    // Physical, like the imported parts it sits beside: a paint over a
    // slot keeps the surface's shading model and changes its numbers.
    om.0.insert(
        "Inside".into(),
        floptle_core::Material { shading: floptle_core::Shading::Physical, color: [1.0, 1.0, 0.0], roughness: 0.1, ..Default::default() },
    );
    ed.world.insert(wall, om.clone());
    assert_eq!(instances_of(&mut ed, wall), 1, "a colour override split the wall");
    // A part in blend mode at full alpha: no alpha says so, but it still
    // draws in the transparent pass, so the wall goes back to its parts.
    let mut blend_full = om.clone();
    blend_full.0.insert(
        "Trim".into(),
        floptle_core::Material { shading: floptle_core::Shading::Physical, blend: floptle_core::BlendMode::Blend, ..Default::default() },
    );
    ed.world.insert(wall, blend_full);
    assert_eq!(instances_of(&mut ed, wall), 3, "a blend-mode part at full alpha was folded into an opaque instance");
    // One that makes a part blend: that part routes apart, so the wall draws
    // part by part again.
    om.0.insert(
        "Trim".into(),
        floptle_core::Material { shading: floptle_core::Shading::Physical, alpha: 0.5, blend: floptle_core::BlendMode::Blend, ..Default::default() },
    );
    ed.world.insert(wall, om);
    assert_eq!(instances_of(&mut ed, wall), 3, "a blended part was folded into an opaque instance");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_merged_wall_draws_the_same_pixels_as_its_parts() {
    let Some((mut ed, wall, root)) = editor_with_wall("pixels") else { return };
    // Plain, tinted, and with one slot painted over (a physical override
    // binding white where the imported parts bind a set with no maps).
    for step in 0..3 {
        let tinted = step == 1;
        if tinted {
            ed.world.insert(wall, floptle_core::Tint { color: [0.6, 0.9, 1.0], alpha: 1.0, ..Default::default() });
        }
        if step == 2 {
            ed.world.remove::<floptle_core::Tint>(wall);
            let mut om = floptle_core::ObjectMaterials::default();
            om.0.insert(
                "Inside".into(),
                floptle_core::Material { shading: floptle_core::Shading::Physical, color: [1.0, 1.0, 0.0], roughness: 0.1, ..Default::default() },
            );
            ed.world.insert(wall, om);
        }
        crate::mesh_instances::merge_parts_off_here(true);
        let parts = picture(&mut ed);
        crate::mesh_instances::merge_parts_off_here(false);
        let merged = picture(&mut ed);
        let lit = parts.chunks(4).filter(|p| p[0] > 10 || p[1] > 10 || p[2] > 10).count();
        assert!(lit > 160 * 90 / 10, "the wall is not in the picture: {lit} lit pixels");
        let differ = parts.chunks(4).zip(merged.chunks(4)).filter(|(a, b)| a != b).count();
        assert_eq!(differ, 0, "step {step}: merged, the wall differs from its parts in {differ} pixels");
    }
    let _ = std::fs::remove_dir_all(&root);
}
