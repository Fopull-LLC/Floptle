//! **Mirror-apply**: complete a Blender model whose Mirror modifier was never
//! applied at export (so half the geometry is missing), producing a plain, normal
//! `.glb`. Per object, from its geometry alone:
//!
//! - **Straddles the plane already** (real geometry on both sides of local X=0) →
//!   left untouched (the mirror was applied / it's a centered piece).
//! - **Touches the plane from one side** (a half cut at X=0 — a head, a torso) →
//!   the missing half is synthesized and **welded** into the same object.
//! - **Sits fully off the plane** (a left arm, a right leg) → its mirror image is
//!   synthesized as a **separate object** and the pair is renamed `<name>.L` /
//!   `<name>.R`.
//!
//! The result is written beside the source as `<stem>.mirrored.glb` (non-destructive)
//! via [`crate::glb_write`]. The mirror plane is the model's local **X = 0** (the
//! Blender object origin), matching the modifier's default.

use std::path::{Path, PathBuf};

use floptle_core::math::{Mat3, Mat4, Vec3};

use crate::gltf_import::ImportError;
use crate::glb_write::{write_glb, WriteMesh, WriteNode};

/// What the pass did, for the Console.
#[derive(Debug, Default)]
pub struct MirrorReport {
    pub output: PathBuf,
    /// Objects whose missing half was welded in (name unchanged).
    pub welded: Vec<String>,
    /// Objects split into a mirrored pair: (kept name, new name).
    pub split: Vec<(String, String)>,
    /// Objects left as-is (already symmetric across the plane).
    pub kept: Vec<String>,
}

/// One model-space object gathered from the source.
pub(crate) struct Obj {
    pub(crate) name: String,
    pub(crate) mesh: WriteMesh,
}

/// Run Mirror-apply on `model_path`, writing `<stem>.mirrored.glb` beside it.
pub fn mirror_apply(model_path: &Path) -> Result<MirrorReport, ImportError> {
    let (objs, textures) = gather_objects(model_path)?;

    // Model bounds → epsilon for "touches the plane" (1% of the largest extent).
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for o in &objs {
        for p in &o.mesh.positions {
            lo = lo.min(Vec3::from(*p));
            hi = hi.max(Vec3::from(*p));
        }
    }
    let size = (hi - lo).max_element().max(1e-4);
    // "Straddles the plane" (already double-sided) vs "sits fully to one side".
    let eps = size * 0.02;
    // A mirror SEAM sits on the plane (Blender welds it to x=0); a lateral limb's
    // inner edge is measurably offset. So the closest vertex approach to the plane —
    // NOT the min x — is what separates a half-head (seam at 0) from a leg (gap).
    let seam_eps = size * 0.002;

    let mut out_nodes: Vec<WriteNode> = Vec::new();
    let mut report = MirrorReport::default();

    for o in objs {
        let (min_x, max_x, mean_x, min_abs_x) = x_stats(&o.mesh.positions);
        let straddles = min_x < -eps && max_x > eps;
        let touches_plane = min_abs_x <= seam_eps;

        // Idempotency: an object already produced by a previous mirror pass (its name
        // ends in `.L`/`.R`) is one half of a pair that ALREADY exists in the model —
        // re-splitting it just stacks a second overlapping copy on each side (the
        // "duplicate limbs" bug from running Mirror-apply twice). Keep it as-is.
        let already_mirrored =
            o.name.ends_with(".L") || o.name.ends_with(".R") || o.name.ends_with(".l") || o.name.ends_with(".r");
        if straddles || already_mirrored {
            report.kept.push(o.name.clone());
            out_nodes.push(WriteNode::mesh_node(o.name, o.mesh));
        } else if touches_plane {
            // Weld: original + reflected copy into ONE object.
            let mut mesh = o.mesh.clone();
            append_mirror(&mut mesh, &o.mesh);
            report.welded.push(o.name.clone());
            out_nodes.push(WriteNode::mesh_node(o.name, mesh));
        } else {
            // Lateral limb: emit the original + a mirrored sibling, renamed L/R.
            let orig_is_left = mean_x >= 0.0; // +X = character's left (Blender facing -Y)
            let base = o.name.clone();
            let (orig_suffix, mirror_suffix) =
                if orig_is_left { (".L", ".R") } else { (".R", ".L") };
            let orig_name = format!("{base}{orig_suffix}");
            let mirror_name = format!("{base}{mirror_suffix}");

            let mut mirror_mesh = WriteMesh {
                positions: Vec::new(),
                normals: Vec::new(),
                uvs: o.mesh.uvs.as_ref().map(|_| Vec::new()),
                colors: o.mesh.colors.as_ref().map(|_| Vec::new()),
                joints: None,
                weights: None,
                indices: Vec::new(),
                base_color: o.mesh.base_color,
                texture: o.mesh.texture,
            };
            append_mirror(&mut mirror_mesh, &o.mesh);

            report.split.push((orig_name.clone(), mirror_name.clone()));
            out_nodes.push(WriteNode::mesh_node(orig_name, o.mesh));
            out_nodes.push(WriteNode::mesh_node(mirror_name, mirror_mesh));
        }
    }

    let bytes = write_glb(&out_nodes, &[], &textures);
    let output = model_path.with_extension("mirrored.glb");
    std::fs::write(&output, bytes).map_err(|e| ImportError::Gltf(gltf::Error::Io(e)))?;
    report.output = output;
    Ok(report)
}

/// Gather per-object model-space geometry + the decoded texture set from a model
/// (rigged/structured import preferred so objects keep their names; static import
/// → one object). The returned `texture` indices in each object index into the
/// returned texture vec.
pub(crate) fn gather_objects(
    path: &Path,
) -> Result<(Vec<Obj>, Vec<floptle_render::TextureData>), ImportError> {
    if let Some(model) = crate::import_rigged(path)? {
        let mut rest_world = Vec::new();
        model.skeleton.world_matrices(&model.skeleton.rest_pose(), &mut rest_world);
        let mut objs = Vec::new();
        let mut used: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for part in &model.parts {
            let w = rest_world.get(part.node).copied().unwrap_or(Mat4::IDENTITY);
            objs.push(Obj {
                name: unique_name(&mut used, &model.skeleton.nodes[part.node].name),
                mesh: bake_part(&part.mesh, w, part.base_color, part.texture),
            });
        }
        Ok((objs, model.textures))
    } else {
        let model = crate::gltf_import::import(path)?;
        let mut objs = Vec::new();
        let mut used: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for (i, part) in model.parts.iter().enumerate() {
            let name = if model.parts.len() == 1 {
                model.name.clone()
            } else {
                format!("{}_{i}", model.name)
            };
            objs.push(Obj {
                name: unique_name(&mut used, &name),
                mesh: bake_part(&part.mesh, Mat4::IDENTITY, part.base_color, part.texture),
            });
        }
        Ok((objs, model.textures))
    }
}

fn unique_name(used: &mut std::collections::HashMap<String, u32>, base: &str) -> String {
    let n = used.entry(base.to_string()).or_insert(0);
    *n += 1;
    if *n == 1 {
        base.to_string()
    } else {
        format!("{base}.{:03}", *n - 1)
    }
}

/// Bake a node-local part into a model-space [`WriteMesh`].
fn bake_part(
    mesh: &floptle_render::MeshData,
    world: Mat4,
    base_color: [f32; 3],
    texture: Option<usize>,
) -> WriteMesh {
    let m3 = Mat3::from_mat4(world);
    let nmat = if m3.determinant().abs() > 1e-12 { m3.inverse().transpose() } else { m3 };
    let mut positions = Vec::with_capacity(mesh.vertices.len());
    let mut normals = Vec::with_capacity(mesh.vertices.len());
    let mut uvs = Vec::with_capacity(mesh.vertices.len());
    for v in &mesh.vertices {
        positions.push(world.transform_point3(Vec3::from(v.pos)).to_array());
        normals.push((nmat * Vec3::from(v.normal)).normalize_or_zero().to_array());
        uvs.push(v.uv);
    }
    WriteMesh {
        positions,
        normals,
        uvs: Some(uvs),
        colors: mesh.colors.clone(),
        joints: None,
        weights: None,
        indices: mesh.indices.clone(),
        base_color: [base_color[0], base_color[1], base_color[2], 1.0],
        texture,
    }
}

/// (min_x, max_x, mean_x, min|x|) — the last is the closest vertex approach to the
/// mirror plane (used to detect a seam sitting on x=0).
fn x_stats(positions: &[[f32; 3]]) -> (f32, f32, f32, f32) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    let mut min_abs = f32::INFINITY;
    let mut sum = 0.0;
    for p in positions {
        lo = lo.min(p[0]);
        hi = hi.max(p[0]);
        min_abs = min_abs.min(p[0].abs());
        sum += p[0];
    }
    let mean = if positions.is_empty() { 0.0 } else { sum / positions.len() as f32 };
    (lo, hi, mean, min_abs)
}

/// Append `src` mirrored across X=0 onto `dst` (positions/normals X negated,
/// triangle winding flipped so the reflected faces stay outward). Streams stay
/// parallel; the index base offsets by dst's current vertex count.
fn append_mirror(dst: &mut WriteMesh, src: &WriteMesh) {
    let base = dst.positions.len() as u32;
    for p in &src.positions {
        dst.positions.push([-p[0], p[1], p[2]]);
    }
    for n in &src.normals {
        dst.normals.push([-n[0], n[1], n[2]]);
    }
    if let (Some(du), Some(su)) = (dst.uvs.as_mut(), src.uvs.as_ref()) {
        du.extend_from_slice(su);
    }
    if let (Some(dc), Some(sc)) = (dst.colors.as_mut(), src.colors.as_ref()) {
        dc.extend_from_slice(sc);
    }
    // Flip winding: reflection reverses orientation, so swap the last two of each tri.
    for tri in src.indices.chunks_exact(3) {
        dst.indices.push(base + tri[0]);
        dst.indices.push(base + tri[2]);
        dst.indices.push(base + tri[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An axis-aligned box from `min` to `max` (positions + triangles only; normals
    /// point +Y — classification/round-trip don't depend on them).
    fn box_mesh(min: [f32; 3], max: [f32; 3]) -> WriteMesh {
        let c = |x: usize, y: usize, z: usize| {
            [
                if x == 0 { min[0] } else { max[0] },
                if y == 0 { min[1] } else { max[1] },
                if z == 0 { min[2] } else { max[2] },
            ]
        };
        let positions = vec![
            c(0, 0, 0), c(1, 0, 0), c(1, 1, 0), c(0, 1, 0),
            c(0, 0, 1), c(1, 0, 1), c(1, 1, 1), c(0, 1, 1),
        ];
        let faces = [
            [0, 1, 2, 3], [5, 4, 7, 6], [4, 0, 3, 7],
            [1, 5, 6, 2], [3, 2, 6, 7], [4, 5, 1, 0],
        ];
        let mut indices = Vec::new();
        for f in faces {
            indices.extend_from_slice(&[f[0], f[1], f[2], f[0], f[2], f[3]]);
        }
        WriteMesh {
            positions,
            normals: vec![[0.0, 1.0, 0.0]; 8],
            uvs: None,
            colors: None,
            joints: None,
            weights: None,
            indices,
            base_color: [0.8, 0.8, 0.8, 1.0],
            texture: None,
        }
    }

    #[test]
    fn writer_roundtrips_and_mirror_welds_seam_but_splits_limb() {
        // "Head": a half-box whose inner face sits ON the plane (x∈[0,0.5]) → weld.
        // "Arm": a box fully off the plane (x∈[0.7,1.3]) → split into L/R.
        let head = box_mesh([0.0, 0.0, 0.0], [0.5, 1.0, 0.3]);
        let arm = box_mesh([0.7, 0.0, 0.0], [1.3, 0.5, 0.3]);
        let nodes =
            vec![WriteNode::mesh_node("Head", head), WriteNode::mesh_node("Arm", arm)];
        let bytes = write_glb(&nodes, &[], &[]);
        assert!(bytes.len() > 12 && &bytes[0..4] == b"glTF", "valid GLB header");

        let dir = std::env::temp_dir().join("floptle_mirror_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mirror_case.glb");
        std::fs::write(&path, &bytes).unwrap();

        let r = mirror_apply(&path).expect("mirror-apply");
        assert!(r.welded.iter().any(|n| n == "Head"), "Head welded: {:?}", r.welded);
        assert!(
            r.split.iter().any(|(l, _)| l.starts_with("Arm")),
            "Arm split L/R: {:?}",
            r.split
        );

        // The output must re-import, and the welded Head must now straddle the plane.
        let model = crate::import_rigged(&r.output).expect("import").expect("structured");
        let mut rw = Vec::new();
        model.skeleton.world_matrices(&model.skeleton.rest_pose(), &mut rw);
        let mut head_min = f32::INFINITY;
        let mut head_max = f32::NEG_INFINITY;
        for part in &model.parts {
            if model.skeleton.nodes[part.node].name == "Head" {
                for v in &part.mesh.vertices {
                    let x = rw[part.node].transform_point3(Vec3::from(v.pos)).x;
                    head_min = head_min.min(x);
                    head_max = head_max.max(x);
                }
            }
        }
        assert!(head_min < -0.1 && head_max > 0.1, "welded Head straddles: [{head_min}, {head_max}]");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&r.output);
    }
}
