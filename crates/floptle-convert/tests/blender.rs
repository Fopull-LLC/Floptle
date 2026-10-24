//! **The Blender route, against the real Blender.** A `.blend` made by
//! Blender itself — a cube with a material, a picture in base colour and a
//! normal map — and the same scene exported as USD, converted the way the
//! editor converts them, then read back by the engine's importer.
//!
//! Skipped, and says nothing, where Blender is not installed (CI has none):
//! the stand-in-script test in `src/blender.rs` covers the plumbing there.

mod support;

use std::path::Path;
use std::process::Command;

/// Builds the scene with Blender's own Python and saves it as `.blend` and
/// `.usdc`. The pictures are saved beside it in `textures/`, the way an
/// artist's project keeps them.
const MAKE: &str = r#"
import bpy, sys, os
out = sys.argv[sys.argv.index("--") + 1]
bpy.ops.wm.read_factory_settings(use_empty=True)
def image(name, color):
    img = bpy.data.images.new(name, 8, 8)
    img.pixels = list(color) * 64
    img.filepath_raw = os.path.join(out, "textures", name + ".png")
    img.file_format = 'PNG'
    img.save()
    return img
bpy.ops.mesh.primitive_cube_add(size=2)
cube = bpy.context.active_object
mat = bpy.data.materials.new("CrateMat")
mat.use_nodes = True
nt = mat.node_tree
bsdf = nt.nodes["Principled BSDF"]
bsdf.inputs["Metallic"].default_value = 0.8
bsdf.inputs["Roughness"].default_value = 0.3
t = nt.nodes.new("ShaderNodeTexImage"); t.image = image("crate_d", (0.8, 0.3, 0.1, 1.0))
nt.links.new(t.outputs["Color"], bsdf.inputs["Base Color"])
tn = nt.nodes.new("ShaderNodeTexImage"); tn.image = image("crate_n", (0.5, 0.5, 1.0, 1.0))
tn.image.colorspace_settings.name = "Non-Color"
nm = nt.nodes.new("ShaderNodeNormalMap")
nt.links.new(tn.outputs["Color"], nm.inputs["Color"])
nt.links.new(nm.outputs["Normal"], bsdf.inputs["Normal"])
cube.data.materials.append(mat)
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "crate.blend"), relative_remap=True)
bpy.ops.wm.usd_export(filepath=os.path.join(out, "crate.usdc"))
"#;

fn check(model: &Path, label: &str) {
    let (out, report) = floptle_convert::convert_file(model).unwrap_or_else(|e| panic!("{label}: {e}"));
    assert_eq!((report.meshes, report.triangles), (1, 12), "{label}: one cube");
    let m = floptle_assets::import(&out).expect("the engine reads it");
    assert!((m.size - 2.0).abs() < 1e-3, "{label}: two metres across, got {}", m.size);
    let p = &m.parts[0];
    assert_eq!(p.material, "CrateMat", "{label}");
    let base = &m.textures[p.texture.expect("a base colour map")];
    assert!(base.pixels[0].abs_diff(204) <= 3 && base.pixels[2].abs_diff(26) <= 3, "{label}: base colour {:?}", &base.pixels[..4]);
    let n = &m.textures[p.maps.normal.expect("a normal map")];
    assert!(n.pixels[2] > 240 && n.pixels[0].abs_diff(128) <= 3, "{label}: normal {:?}", &n.pixels[..4]);
    assert!((p.maps.metallic - 0.8).abs() < 1e-3 && (p.maps.roughness - 0.3).abs() < 1e-3, "{label}: {:?}", p.maps);
    assert!(report.missing.is_empty(), "{label}: {:?}", report.missing);
}

#[test]
fn a_blend_and_a_usd_convert_through_the_installed_blender() {
    let Some(blender) = floptle_convert::blender() else { return };
    let dir = support::scratch("real-blender");
    let made = Command::new(&blender)
        .args(["--background", "--factory-startup", "--python-expr", MAKE, "--"])
        .arg(&dir)
        .output()
        .expect("run Blender");
    assert!(dir.join("crate.blend").is_file(), "Blender did not make the scene: {}", String::from_utf8_lossy(&made.stderr));
    check(&dir.join("crate.blend"), ".blend");
    std::fs::remove_file(dir.join("crate.glb")).unwrap();
    check(&dir.join("crate.usdc"), ".usdc");
    assert!(
        !std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("floptle-blender")),
        "the working file Blender wrote was cleared away"
    );

    // Collada: a triangle. Blender 5 has no Collada importer and says so by
    // name; an older one converts it.
    std::fs::write(
        dir.join("tri.dae"),
        r##"<?xml version="1.0" encoding="utf-8"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <asset><unit name="meter" meter="1"/><up_axis>Y_UP</up_axis></asset>
  <library_geometries><geometry id="tri" name="tri"><mesh>
    <source id="p"><float_array id="pa" count="9">0 0 0 1 0 0 0 1 0</float_array>
      <technique_common><accessor source="#pa" count="3" stride="3"><param name="X" type="float"/><param name="Y" type="float"/><param name="Z" type="float"/></accessor></technique_common></source>
    <vertices id="v"><input semantic="POSITION" source="#p"/></vertices>
    <triangles count="1"><input semantic="VERTEX" source="#v" offset="0"/><p>0 1 2</p></triangles>
  </mesh></geometry></library_geometries>
  <library_visual_scenes><visual_scene id="s"><node id="n"><instance_geometry url="#tri"/></node></visual_scene></library_visual_scenes>
  <scene><instance_visual_scene url="#s"/></scene>
</COLLADA>"##,
    )
    .unwrap();
    match floptle_convert::convert(&dir.join("tri.dae")) {
        Ok((_, report)) => assert_eq!(report.triangles, 1),
        Err(e) => assert!(e.to_string().contains("no longer imports Collada"), "{e}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
