//! **FBX and OBJ**, both through ufbx.
//!
//! One reader for two formats on purpose. They are read by the same library, so
//! sharing the path means they cannot end up disagreeing about which way is up
//! or which way a triangle winds — which is exactly what happens when OBJ gets
//! its own hand-rolled parser next to a real FBX one.
//!
//! # The four things that make an FBX import wrong
//!
//! **Axes.** 3ds Max writes Z-up, Maya writes Y-up, and glTF is always
//! right-handed Y-up. Asked for explicitly below rather than left at ufbx's
//! default, because the default preserves the source and preserving the source
//! is the bug.
//!
//! **Units.** FBX records its unit and most exporters write centimetres. glTF
//! is metres. A model a hundred times too big reads as a broken importer.
//!
//! **Geometric transforms.** FBX has a per-node "geometric transform" that is
//! applied to the mesh but *not* inherited by children — a concept glTF does
//! not have at all. Ignoring it puts parts of a model in the wrong place, and
//! it is the reason a converted prop often arrives with one piece floating.
//! Handled by baking every mesh into world space.
//!
//! **Winding.** Any of the above can involve a mirror, and mirroring inverts
//! triangle winding. A model with every face backwards is invisible from
//! outside and solid from within — which reads as "the converter lost my
//! model", not as a winding problem.
//!
//! # What is flattened, and why that is the right trade
//!
//! Node transforms are **baked into vertex positions**, so the output is a flat
//! list of world-space meshes. That gives up the transform hierarchy and
//! sidesteps every FBX transform subtlety at once: pre/post-rotation, rotation
//! orders, inherit modes, geometric transforms. Parts stay *separate and
//! named* — one mesh per node per material — so nothing a person would want to
//! select is merged away. For a converter whose job is "make this model
//! usable", that is the trade worth making; a rigged character wanting its
//! joint hierarchy is a different feature and is honestly reported as dropped.

use floptle_render::TextureData;
use std::collections::HashMap;
use std::path::Path;

use crate::common::{Scene, SubMesh};
use crate::{ConvertError, Report};

/// Read an `.fbx` or `.obj` into the common scene.
pub fn read(src: &Path) -> Result<Scene, ConvertError> {
    let opts = ufbx::LoadOpts {
        // glTF's coordinate system, always. See the header.
        target_axes: ufbx::CoordinateAxes::right_handed_y_up(),
        // **Units are applied below, not here.** See `unit_scale`: one real
        // exporter declares a unit its geometry is not actually in, and the
        // decision about whom to believe belongs in code that can be read and
        // tested rather than hidden in a loader flag.
        target_unit_meters: 0.0,
        // Fold the axis/unit change into the geometry rather than leaving a
        // correction on the root node: a root with a -1 scale is legal glTF and
        // trips up half the tools that read it afterwards.
        space_conversion: ufbx::SpaceConversion::ModifyGeometry,
        // FBX's geometric transform has no glTF equivalent. Let ufbx bake it in
        // rather than silently dropping it, which is what moves one piece of a
        // prop off on its own.
        geometry_transform_handling: ufbx::GeometryTransformHandling::ModifyGeometry,
        // A file with a face that has no vertices is damaged, not unreadable.
        // Better to convert the other 20,000 faces and say so.
        allow_empty_faces: true,
        generate_missing_normals: true,
        ..Default::default()
    };

    // ufbx takes a str path; a name that is not valid UTF-8 is a real thing on
    // Linux and is reported rather than lost in a lossy conversion.
    let path = src
        .to_str()
        .ok_or_else(|| ConvertError::Io("that file's name is not valid text".into()))?;
    let scene = ufbx::load_file(path, opts)
        .map_err(|e| ConvertError::Malformed(e.description.to_string()))?;

    let mut out = Scene::default();
    let unit = unit_scale(&scene, &mut out.report);

    // Textures are collected once and shared: a model with one atlas used by
    // forty materials must embed it once, or the .glb is forty times too big.
    let mut tex_index: HashMap<String, usize> = HashMap::new();

    for node in scene.nodes.iter() {
        let Some(mesh) = node.mesh.as_ref() else { continue };
        if node.is_root {
            continue;
        }

        // World space, including the geometric transform, and then into metres.
        let xform = scaled(&node.geometry_to_world, unit);
        let normals = normal_matrix(&xform);
        // A mirroring transform reverses winding. Detected from the transform
        // itself rather than trusted from a flag, because this has to stay
        // right whatever ufbx did upstream to get here.
        let flipped = ufbx::matrix_determinant(&xform) < 0.0;

        let name = if node.element.name.is_empty() {
            "mesh".to_string()
        } else {
            node.element.name.to_string()
        };

        // **One SubMesh per material.** A glTF primitive has exactly one
        // material; an FBX mesh can have many, and merging them makes a model
        // that is one colour where it should be several.
        let parts: Vec<&ufbx::MeshPart> = if mesh.material_parts.is_empty() {
            Vec::new()
        } else {
            mesh.material_parts.iter().collect()
        };

        if parts.is_empty() {
            if let Some(sm) = build_part(
                mesh,
                None,
                &xform,
                normals,
                flipped,
                &name,
                None,
                &mut out.textures,
                &mut tex_index,
                src,
                &mut out.report,
            ) {
                out.meshes.push(sm);
            }
            continue;
        }

        for part in parts {
            if part.num_triangles == 0 {
                continue;
            }
            let material = mesh.materials.get(part.index as usize);
            let part_name = match material {
                Some(m) if !m.element.name.is_empty() && parts_len(mesh) > 1 => {
                    format!("{name}_{}", m.element.name)
                }
                _ => name.clone(),
            };
            if let Some(sm) = build_part(
                mesh,
                Some(part),
                &xform,
                normals,
                flipped,
                &part_name,
                material.map(|r| &**r),
                &mut out.textures,
                &mut tex_index,
                src,
                &mut out.report,
            ) {
                out.meshes.push(sm);
            }
        }
    }

    note_dropped(&scene, &mut out.report);
    out.report.materials = scene.materials.len();
    Ok(out)
}

fn parts_len(mesh: &ufbx::Mesh) -> usize {
    mesh.material_parts.len()
}

/// Multiply every position by this to land in metres.
///
/// glTF is metres. FBX records whatever the author worked in, and most
/// exporters write centimetres — so this is usually 0.01, and getting it wrong
/// is the hundredfold error that reads as a broken importer.
///
/// **One exporter declares a unit its geometry is not in.** Blender's legacy
/// ASCII (pre-7000) FBX writer records `UnitScaleFactor` = 100 while writing
/// vertex positions already in metres; the modern binary writer records the
/// same 100 and writes centimetres, as it should. Believing the header on the
/// ASCII files makes every one of them a hundred times too small — a default
/// Blender sphere converts to two centimetres across instead of two metres.
///
/// The rule is narrow on purpose: this exact exporter, which ufbx identifies by
/// name, at file versions below 7000. It is not a guess about the geometry, and
/// it is pinned by a test that converts the SAME model exported both ways and
/// requires the two to come out the same size — which is the property that
/// actually matters and the one that would catch this coming back.
fn unit_scale(scene: &ufbx::Scene, report: &mut Report) -> f64 {
    let declared = scene.settings.unit_meters;
    if declared > 0.0 {
        report.source_unit_meters = Some(declared as f32);
    }

    let quirky = scene.metadata.exporter == ufbx::Exporter::BlenderAscii
        && scene.metadata.version < 7000;
    if quirky && declared > 0.0 && (declared - 1.0).abs() > 1e-6 {
        report.warnings.push(format!(
            "This file says it is in {} but its geometry is already in metres — a known \
             fault in the old Blender ASCII exporter. Converted at its actual size.",
            describe_unit(declared as f32)
        ));
        return 1.0;
    }

    if declared > 0.0 && (declared - 1.0).abs() > 1e-6 {
        report.warnings.push(format!(
            "The source measured in {}; it has been scaled to metres.",
            describe_unit(declared as f32)
        ));
        return declared;
    }
    1.0
}

/// The matrix a NORMAL is transformed by: the inverse transpose of the upper
/// 3×3.
///
/// Not the matrix positions use. Under non-uniform scale the two differ, and a
/// node squashed on one axis lights as though its surfaces face somewhere they
/// do not. For a rigid transform they coincide — which is why reusing the
/// position matrix works on most models and fails on the ones with a squashed
/// part in them.
fn normal_matrix(m: &ufbx::Matrix) -> [[f64; 3]; 3] {
    let a = [
        [m.m00, m.m01, m.m02],
        [m.m10, m.m11, m.m12],
        [m.m20, m.m21, m.m22],
    ];
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if det.abs() < 1e-20 {
        return a;
    }
    let inv = 1.0 / det;
    // cofactor(a) / det, which IS transpose(inverse(a)). Written out rather
    // than composed from an inverse and a transpose because getting one of the
    // two the wrong way round produces a matrix that is correct for every
    // rigid transform and wrong for every scaled one — which passes any test
    // built from unscaled fixtures.
    [
        [
            (a[1][1] * a[2][2] - a[1][2] * a[2][1]) * inv,
            (a[1][2] * a[2][0] - a[1][0] * a[2][2]) * inv,
            (a[1][0] * a[2][1] - a[1][1] * a[2][0]) * inv,
        ],
        [
            (a[0][2] * a[2][1] - a[0][1] * a[2][2]) * inv,
            (a[0][0] * a[2][2] - a[0][2] * a[2][0]) * inv,
            (a[0][1] * a[2][0] - a[0][0] * a[2][1]) * inv,
        ],
        [
            (a[0][1] * a[1][2] - a[0][2] * a[1][1]) * inv,
            (a[0][2] * a[1][0] - a[0][0] * a[1][2]) * inv,
            (a[0][0] * a[1][1] - a[0][1] * a[1][0]) * inv,
        ],
    ]
}

/// Apply [`normal_matrix`] and renormalise.
fn normal_dir(n: [[f64; 3]; 3], v: ufbx::Vec3) -> [f64; 3] {
    let o = [
        n[0][0] * v.x + n[0][1] * v.y + n[0][2] * v.z,
        n[1][0] * v.x + n[1][1] * v.y + n[1][2] * v.z,
        n[2][0] * v.x + n[2][1] * v.y + n[2][2] * v.z,
    ];
    let len = (o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt();
    if len > 1e-12 { [o[0] / len, o[1] / len, o[2] / len] } else { [0.0, 1.0, 0.0] }
}

/// `m` with a uniform scale applied to its translation and basis.
fn scaled(m: &ufbx::Matrix, s: f64) -> ufbx::Matrix {
    if (s - 1.0).abs() < 1e-12 {
        return *m;
    }
    let mut o = *m;
    for v in [
        &mut o.m00, &mut o.m10, &mut o.m20, &mut o.m01, &mut o.m11, &mut o.m21,
        &mut o.m02, &mut o.m12, &mut o.m22, &mut o.m03, &mut o.m13, &mut o.m23,
    ] {
        *v *= s;
    }
    o
}

fn describe_unit(m: f32) -> String {
    // The four that actually appear in exports, named rather than printed as a
    // float: "0.01" means nothing, "centimetres" means everything.
    for (v, n) in [(0.01, "centimetres"), (0.001, "millimetres"), (0.0254, "inches"), (0.3048, "feet")] {
        if (m - v).abs() < 1e-6 {
            return n.to_string();
        }
    }
    format!("units of {m} m")
}

/// Say what was left behind. Never silently.
fn note_dropped(scene: &ufbx::Scene, report: &mut Report) {
    if !scene.anim_stacks.is_empty() {
        report.dropped.push(format!(
            "{} animation{}",
            scene.anim_stacks.len(),
            if scene.anim_stacks.len() == 1 { "" } else { "s" }
        ));
    }
    if !scene.skin_deformers.is_empty() {
        report
            .dropped
            .push("skinning (the mesh is converted in its bind pose)".to_string());
    }
    if !scene.cameras.is_empty() {
        report.dropped.push(format!("{} camera(s)", scene.cameras.len()));
    }
    if !scene.lights.is_empty() {
        report.dropped.push(format!("{} light(s)", scene.lights.len()));
    }
    if !scene.blend_deformers.is_empty() {
        report.dropped.push("blend shapes".to_string());
    }
}

/// Turn one material part (or the whole mesh) into a [`SubMesh`].
#[allow(clippy::too_many_arguments)]
fn build_part(
    mesh: &ufbx::Mesh,
    part: Option<&ufbx::MeshPart>,
    xform: &ufbx::Matrix,
    normals: [[f64; 3]; 3],
    flipped: bool,
    name: &str,
    material: Option<&ufbx::Material>,
    textures: &mut Vec<TextureData>,
    tex_index: &mut HashMap<String, usize>,
    src: &Path,
    report: &mut Report,
) -> Option<SubMesh> {
    let mut sm = SubMesh {
        name: name.to_string(),
        base_color: [1.0, 1.0, 1.0, 1.0],
        ..Default::default()
    };

    let has_uv = mesh.vertex_uv.exists;
    let has_color = mesh.vertex_color.exists;
    if has_uv {
        sm.uvs = Some(Vec::new());
    }
    if has_color {
        sm.colors = Some(Vec::new());
    }

    // ufbx triangulates a face into at most `max_face_triangles` triangles.
    let mut tri = vec![0u32; mesh.max_face_triangles.max(1) * 3];

    // **De-indexed, one vertex per triangle corner.** FBX indexes position,
    // normal and UV separately, so a corner where two faces meet at a hard edge
    // is one position with two normals — a thing glTF cannot express. Splitting
    // every corner is correct for those; identical corners are welded back
    // afterwards so the common case does not pay for it.
    let faces: Vec<u32> = match part {
        Some(p) => p.face_indices.iter().copied().collect(),
        None => (0..mesh.faces.len() as u32).collect(),
    };

    for fi in faces {
        let Some(face) = mesh.faces.get(fi as usize) else { continue };
        let n = mesh.triangulate_face(&mut tri, *face);
        for t in 0..n as usize {
            let corners = [tri[t * 3], tri[t * 3 + 1], tri[t * 3 + 2]];
            // Winding is reversed by swapping two corners of each triangle, not
            // by reversing the whole index list — that would only reverse the
            // order triangles are drawn in, which changes nothing.
            let order = if flipped { [0usize, 2, 1] } else { [0usize, 1, 2] };
            for o in order {
                let ix = corners[o] as usize;
                let p = ufbx::transform_position(xform, mesh.vertex_position[ix]);
                sm.positions.push([p.x as f32, p.y as f32, p.z as f32]);

                if mesh.vertex_normal.exists {
                    // **Transformed, never negated.** Reflecting a surface
                    // reflects its outward normal too, so the transformed
                    // normal is already right; it is the WINDING a mirror
                    // inverts, and reordering the corners above is the whole of
                    // the correction. Negating as well undoes it and leaves
                    // every normal disagreeing with its own face.
                    let nv = normal_dir(normals, mesh.vertex_normal[ix]);
                    sm.normals.push([nv[0] as f32, nv[1] as f32, nv[2] as f32]);
                }
                if let Some(uvs) = sm.uvs.as_mut() {
                    let uv = mesh.vertex_uv[ix];
                    // **glTF's V axis points the other way.** Every texture on
                    // every converted model is upside down without this, which
                    // reads as the texture being wrong rather than the importer.
                    uvs.push([uv.x as f32, 1.0 - uv.y as f32]);
                }
                if let Some(cols) = sm.colors.as_mut() {
                    let c = mesh.vertex_color[ix];
                    cols.push([
                        to_u8(c.x),
                        to_u8(c.y),
                        to_u8(c.z),
                        to_u8(c.w),
                    ]);
                }
            }
        }
    }

    if sm.positions.is_empty() {
        return None;
    }
    sm.indices = (0..sm.positions.len() as u32).collect();
    weld(&mut sm);

    if let Some(m) = material {
        let base = &m.pbr.base_color;
        if base.has_value {
            let v = base.value_vec4;
            sm.base_color = [v.x as f32, v.y as f32, v.z as f32, 1.0];
        }
        // Opacity rides in a separate map, and a converted model that loses its
        // transparency is one somebody has to fix by hand in every material.
        let op = &m.pbr.opacity;
        if op.has_value {
            sm.base_color[3] = (op.value_vec4.x as f32).clamp(0.0, 1.0);
        }
        if let Some(t) = base.texture.as_ref() {
            sm.texture = load_texture(t, textures, tex_index, src, report);
        }
    }

    Some(sm)
}

fn to_u8(v: ufbx::Real) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Merge identical corners back together.
///
/// Splitting every corner is correct and triples the vertex count of a smooth
/// mesh. An exact byte-equality weld puts that back for every corner the source
/// actually shared, and leaves genuinely different ones — the hard edges this
/// all exists for — alone. Exact rather than tolerant on purpose: a tolerance
/// welds a hard edge into a smooth one, which is a change to the model.
fn weld(sm: &mut SubMesh) {
    let mut map: HashMap<Vec<u8>, u32> = HashMap::with_capacity(sm.positions.len());
    let mut pos = Vec::with_capacity(sm.positions.len());
    let mut nrm = Vec::with_capacity(sm.normals.len());
    let mut uvs: Option<Vec<[f32; 2]>> = sm.uvs.as_ref().map(|_| Vec::new());
    let mut cols: Option<Vec<[u8; 4]>> = sm.colors.as_ref().map(|_| Vec::new());
    let mut remap = Vec::with_capacity(sm.positions.len());

    for i in 0..sm.positions.len() {
        let mut key = Vec::with_capacity(40);
        for v in sm.positions[i] {
            key.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        if let Some(n) = sm.normals.get(i) {
            for v in n {
                key.extend_from_slice(&v.to_bits().to_le_bytes());
            }
        }
        if let Some(u) = sm.uvs.as_ref().and_then(|u| u.get(i)) {
            for v in u {
                key.extend_from_slice(&v.to_bits().to_le_bytes());
            }
        }
        if let Some(c) = sm.colors.as_ref().and_then(|c| c.get(i)) {
            key.extend_from_slice(c);
        }
        let next = pos.len() as u32;
        let idx = *map.entry(key).or_insert_with(|| {
            pos.push(sm.positions[i]);
            if let Some(n) = sm.normals.get(i) {
                nrm.push(*n);
            }
            if let (Some(dst), Some(u)) = (uvs.as_mut(), sm.uvs.as_ref().and_then(|u| u.get(i))) {
                dst.push(*u);
            }
            if let (Some(dst), Some(c)) = (cols.as_mut(), sm.colors.as_ref().and_then(|c| c.get(i)))
            {
                dst.push(*c);
            }
            next
        });
        remap.push(idx);
    }

    sm.indices = remap;
    sm.positions = pos;
    sm.normals = nrm;
    sm.uvs = uvs;
    sm.colors = cols;
}

/// Find a material's base-colour image and embed it.
///
/// Three places it can be, in the order worth trying: **inside the file**
/// (FBX can embed), then beside the file, then wherever the exporter's absolute
/// path pointed. That last one is somebody else's machine and almost never
/// exists, which is exactly why the relative try comes first.
fn load_texture(
    tex: &ufbx::Texture,
    out: &mut Vec<TextureData>,
    seen: &mut HashMap<String, usize>,
    src: &Path,
    report: &mut Report,
) -> Option<usize> {
    let key = if !tex.filename.is_empty() {
        tex.filename.to_string()
    } else {
        tex.element.name.to_string()
    };
    if let Some(i) = seen.get(&key) {
        return Some(*i);
    }

    // Embedded in the FBX itself.
    let bytes: Option<Vec<u8>> = if !tex.content.is_empty() {
        Some(tex.content.iter().copied().collect())
    } else {
        let dir = src.parent().unwrap_or(Path::new("."));
        let mut found = None;
        for cand in [tex.relative_filename.to_string(), tex.filename.to_string()] {
            if cand.is_empty() {
                continue;
            }
            // Exporters write Windows separators; a converter on Linux that
            // does not translate them finds nothing and blames the file.
            let cleaned = cand.replace('\\', "/");
            let p = dir.join(&cleaned);
            if p.is_file() {
                found = std::fs::read(&p).ok();
                break;
            }
            // Just the file name, beside the model. The commonest real layout:
            // the exporter recorded `../textures/x.png` from a folder that no
            // longer exists, and `x.png` is sitting right there.
            if let Some(base) = Path::new(&cleaned).file_name() {
                let p2 = dir.join(base);
                if p2.is_file() {
                    found = std::fs::read(&p2).ok();
                    break;
                }
            }
        }
        if found.is_none() && !tex.absolute_filename.is_empty() {
            let p = Path::new(tex.absolute_filename.as_ref());
            if p.is_file() {
                found = std::fs::read(p).ok();
            }
        }
        found
    };

    let Some(bytes) = bytes else {
        report.warnings.push(format!(
            "Could not find the texture `{}` — the model is converted without it.",
            short_name(&key)
        ));
        return None;
    };

    // Guesses the format from the bytes, so a JPEG named `.png` — which asset
    // packs are full of — still loads.
    match floptle_assets::texture::decode_png(&bytes) {
        Some(t) => {
            let i = out.len();
            out.push(t);
            seen.insert(key, i);
            Some(i)
        }
        None => {
            report.warnings.push(format!(
                "The texture `{}` is in a format Floptle cannot read — the model is \
                 converted without it.",
                short_name(&key)
            ));
            None
        }
    }
}

fn short_name(p: &str) -> String {
    p.replace('\\', "/").rsplit('/').next().unwrap_or(p).to_string()
}
