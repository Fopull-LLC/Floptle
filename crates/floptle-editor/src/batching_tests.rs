//! Parts that look the same draw as one.
//!
//! A mesh draws instanced by its mesh, its images, and its shader binding. A
//! `.flsl` binding used to be made per node, so three thousand parts given the
//! same look through a shader were three thousand draws. Bindings are shared
//! by content now: equal shader, parameters and shader textures, one binding.

use floptle_core::math::{DVec3, Quat};
use floptle_core::{Material, Matter, ObjectMaterials, Transform};
use floptle_render::{Projection, RenderCamera};

const SHADER: &str = "shaders/toonPrint.flsl";

/// The look every part gets: the model's own maps plus a shader over them.
fn shaded(tint: [f32; 4]) -> Material {
    let mut m = Material {
        texture: Some("textures/base.png".into()),
        normal_map: Some("textures/normal.png".into()),
        shading: floptle_core::Shading::Physical,
        shader: Some(SHADER.into()),
        ..Default::default()
    };
    m.shader_params.insert("tint".into(), tint);
    m
}

/// A project with the panel model and the shader, and `n` panels in a grid,
/// each given `look(i)` on its one material.
fn scene(tag: &str, n: usize, look: &dyn Fn(usize) -> Material) -> Option<(crate::Editor, std::path::PathBuf, Vec<floptle_core::Entity>)> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle-batching-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    crate::imported_maps_tests::write_maps(&dir);
    crate::imported_maps_tests::write_gltf(&dir);
    std::fs::create_dir_all(dir.join("shaders")).unwrap();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shaders/examples/toonPrint.flsl");
    std::fs::copy(&src, dir.join(SHADER)).unwrap();
    ed.project_root = dir.clone();
    assert!(ed.import_model("models/panel.gltf"), "the fixture model did not import");
    let side = (n as f64).sqrt().ceil() as usize;
    let mut parts = Vec::new();
    for i in 0..n {
        let e = ed.world.spawn();
        let at = DVec3::new((i % side) as f64 * 2.2 - side as f64 * 1.1, (i / side) as f64 * 2.2 - side as f64 * 1.1, 0.0);
        ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
        ed.world.insert(e, Matter::Mesh { asset_path: "models/panel.gltf".into() });
        let mut om = std::collections::BTreeMap::new();
        om.insert("Hull".to_string(), look(i));
        ed.world.insert(e, ObjectMaterials(om));
        parts.push(e);
    }
    let sun = ed.world.spawn();
    let turn = Quat::from_rotation_y(0.6) * Quat::from_rotation_x(-0.5);
    ed.world.insert(sun, Transform { rotation: turn, ..Transform::IDENTITY });
    ed.world.insert(sun, floptle_core::Light { intensity: 3.0, ..Default::default() });
    ed.script_host.profile().borrow_mut().enable(true);
    Some((ed, dir, parts))
}

/// Distinct live bindings the parts draw with.
fn bindings(ed: &crate::Editor) -> usize {
    ed.obj_flsl_binds.values().map(|b| b.binding.0).collect::<std::collections::HashSet<_>>().len()
}

fn camera(n: usize) -> RenderCamera {
    let side = (n as f64).sqrt().ceil();
    RenderCamera::new(DVec3::new(0.0, 0.0, side * 2.6), Quat::IDENTITY, Projection::of_camera(1.0, false, 6.0, 0.05, 1000.0))
}

fn shoot(ed: &mut crate::Editor, n: usize, name: &str) -> Vec<u8> {
    let px = crate::shot::render_frame_pixels(ed, &camera(n), None, 160, 160, !0, false).expect("a frame");
    if let Ok(out) = std::env::var("BATCHING_PNG") {
        let _ = image::RgbaImage::from_raw(160, 160, px.clone()).map(|im| im.save(format!("{out}_{name}.png")));
    }
    px
}

/// **Three thousand parts with one mesh and one shader look draw in a handful
/// of draws, and a quiet frame does not walk their materials.** One part
/// changed later gets a binding of its own and the rest keep theirs.
#[test]
fn identical_shader_materials_share_one_batch() {
    const N: usize = 3000;
    let Some((mut ed, dir, parts)) = scene("same", N, &|_| shaded([0.8, 0.7, 0.6, 1.0])) else { return };
    // Twice: the first frame compiles the shader and uploads the images.
    let _ = shoot(&mut ed, N, "warm");
    let _ = shoot(&mut ed, N, "same");
    let c = ed.script_host.profile().borrow().counts();
    assert_eq!(bindings(&ed), 1, "{N} equal materials made {} bindings", bindings(&ed));
    assert_eq!(ed.flsl_pool.live(), 1);
    assert!(c.draws <= 3, "{N} parts with one mesh and one look cost {} draws ({:?})", c.draws, c.unbatched);
    assert_eq!(c.unbatched.total(), 0, "nothing should draw alone: {:?}", c.unbatched);

    // A frame where no material changed skips the walk over every material.
    let walks = ed.flsl_gate.walks;
    let planned = ed.flsl_gate.planned;
    let _ = shoot(&mut ed, N, "quiet");
    let _ = shoot(&mut ed, N, "quiet");
    assert_eq!(ed.flsl_gate.walks, walks, "a quiet frame walked every material again");

    // One part tinted differently moves alone: two bindings, one lone draw,
    // and the reason says so.
    if let Some(om) = ed.world.get_mut::<ObjectMaterials>(parts[7])
        && let Some(m) = om.0.get_mut("Hull")
    {
        m.shader_params.insert("tint".into(), [0.2, 0.9, 0.3, 1.0]);
    }
    let _ = shoot(&mut ed, N, "one-changed");
    let c = ed.script_host.profile().borrow().counts();
    assert_eq!(ed.flsl_gate.walks, walks + 1, "a material changed and nothing noticed");
    assert_eq!(ed.flsl_gate.planned, planned + 1, "one part changed, and the walk planned more than it");
    assert_eq!(ed.flsl_pool.live(), 2, "copy on write: the changed part and the rest");
    assert_eq!(c.unbatched.shader_params, 1, "{:?}", c.unbatched);
    assert!(c.draws <= 4, "one changed part cost {} draws", c.draws);
    assert_eq!(bindings(&ed), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Sharing a binding draws the same picture.** Sixteen parts in two looks
/// (two bindings), against the same parts with a binding each: every part
/// must look as its own material says.
#[test]
fn shared_bindings_draw_each_part_as_its_material_says() {
    const N: usize = 16;
    let tint = |i: usize| if i.is_multiple_of(5) { [0.9, 0.2, 0.2, 1.0] } else { [0.3, 0.6, 0.9, 1.0] };
    let Some((mut ed, dir, _)) = scene("mixed", N, &|i| shaded(tint(i))) else { return };
    let _ = shoot(&mut ed, N, "mixed-warm");
    let mixed = shoot(&mut ed, N, "mixed");
    assert_eq!(bindings(&ed), 2, "two looks, two bindings");
    let _ = std::fs::remove_dir_all(&dir);
    // The reference: the same parts with each tint nudged by far less than
    // one 8-bit step, so no two share a binding — a binding per part, as
    // every part used to have.
    let Some((mut ed, dir, _)) = scene("unique", N, &|i| {
        let mut t = tint(i);
        t[0] += i as f32 * 1e-6;
        shaded(t)
    }) else { return };
    let _ = shoot(&mut ed, N, "unique-warm");
    let unique = shoot(&mut ed, N, "unique");
    assert_eq!(bindings(&ed), N, "each part its own binding in the reference");
    let _ = std::fs::remove_dir_all(&dir);
    let lit = mixed.chunks(4).filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 60).count();
    assert!(lit > 160 * 160 / 10, "the panels drew almost nothing ({lit} lit pixels)");
    let worst = mixed.iter().zip(&unique).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
    assert!(worst <= 2, "shared bindings changed the picture by up to {worst}");
}

/// **A script's `setMaterial` with the same table on every model batches
/// too.** The built-in path: images interned to one set, every number in the
/// instance.
#[test]
fn identical_material_patches_share_one_batch() {
    const N: usize = 200;
    let Some((mut ed, dir, parts)) = scene("patch", N, &|_| Material::default()) else { return };
    for &e in &parts {
        ed.world.remove::<ObjectMaterials>(e);
        let mut p = floptle_core::MaterialPatch::default();
        p.mat.texture = Some("textures/base.png".into());
        p.mat.normal_map = Some("textures/normal.png".into());
        p.mat.roughness_map = Some("textures/mr.png".into());
        p.mat.ao_map = Some("textures/ao.png".into());
        p.mat.shading = floptle_core::Shading::Physical;
        p.mat.roughness = 0.9;
        p.mat.metallic = 0.5;
        for k in ["texture", "normalMap", "roughnessMap", "occlusionMap", "shading", "roughness", "metallic"] {
            p.keys.push(k.into());
        }
        ed.world.insert(e, p);
    }
    let _ = shoot(&mut ed, N, "patch-warm");
    let _ = shoot(&mut ed, N, "patch");
    let c = ed.script_host.profile().borrow().counts();
    assert!(c.draws <= 3, "{N} models given one setMaterial table cost {} draws ({:?})", c.draws, c.unbatched);
    let _ = std::fs::remove_dir_all(&dir);
}


