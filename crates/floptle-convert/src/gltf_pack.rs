//! **A loose `.gltf` into one `.glb`.**
//!
//! Not a format conversion — it is already glTF — but it is the thing people
//! most often need and least expect to. A `.gltf` is JSON that *points at* its
//! geometry (`scene.bin`) and its textures (`textures/colour.png`), so it is
//! three to thirty files that have to travel together. Half of them go missing
//! in a zip, a Drive share, or a move between folders, and what is left opens
//! as an empty scene with no error worth reading.
//!
//! Packing it into a `.glb` puts all of it in one file. That is the whole point
//! of `.glb`, and it is why this sits beside the real converters rather than
//! being treated as a no-op.
//!
//! **Faithful, not normalised.** Unlike the FBX path there is nothing to fix:
//! the source is already right-handed Y-up in metres with triangles. The
//! engine's own `gltf_import` is deliberately not reused here because it
//! *recenters* what it loads — right for placing a prop in a scene, wrong for a
//! conversion, which must give back the model that went in.

use std::path::Path;

use crate::common::{Scene, SubMesh};
use crate::ConvertError;

pub fn read(src: &Path) -> Result<Scene, ConvertError> {
    let (doc, buffers, images) =
        gltf::import(src).map_err(|e| ConvertError::Malformed(e.to_string()))?;

    let mut out = Scene::default();

    // Every image the file references, decoded once. glTF already de-duplicates
    // them, so the indices line up with the document's.
    for img in &images {
        if let Some(t) = to_texture(img) {
            out.textures.push(t);
        } else {
            out.report
                .warnings
                .push("A texture could not be decoded and was left out.".into());
            // A placeholder keeps every later index correct — dropping it would
            // silently shift every material's texture to its neighbour's.
            out.textures.push(floptle_render::TextureData {
                pixels: vec![255, 255, 255, 255],
                width: 1,
                height: 1,
            });
        }
    }

    // **Walk the scene's node tree, not the mesh list.** A glTF mesh can be
    // instanced by several nodes at different transforms; iterating meshes
    // would place every instance at the origin, collapsing a row of pillars
    // into one.
    for scene in doc.scenes() {
        for node in scene.nodes() {
            walk(&node, [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ], &buffers, &mut out);
        }
    }

    if doc.animations().count() > 0 {
        out.report.dropped.push(format!("{} animation(s)", doc.animations().count()));
    }
    if doc.skins().count() > 0 {
        out.report.dropped.push("skinning (converted in its bind pose)".into());
    }
    out.report.materials = doc.materials().count();
    Ok(out)
}

fn to_texture(img: &gltf::image::Data) -> Option<floptle_render::TextureData> {
    use gltf::image::Format;
    let (w, h) = (img.width, img.height);
    let px = match img.format {
        Format::R8G8B8A8 => img.pixels.clone(),
        Format::R8G8B8 => img
            .pixels
            .as_chunks::<3>().0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        Format::R8 => img.pixels.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        Format::R8G8 => img
            .pixels
            .as_chunks::<2>().0
            .iter()
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        // 16-bit and float images are legal and rare. Rather than guess at a
        // tone mapping, say it was dropped.
        _ => return None,
    };
    Some(floptle_render::TextureData { pixels: px, width: w, height: h })
}

fn mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut m = [[0.0f32; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            for k in 0..4 {
                *cell += a[k][j] * b[i][k];
            }
        }
    }
    m
}

fn point(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0],
        m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1],
        m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2],
    ]
}

/// The matrix a NORMAL is transformed by: the inverse transpose of the upper
/// 3×3.
///
/// Not the matrix positions use. Under non-uniform scale the two differ — a
/// node squashed on one axis tilts its normals the wrong way if the position
/// matrix is reused, and the model lights as though its surfaces face somewhere
/// they do not. For a rigid or mirroring transform the two happen to coincide,
/// which is exactly why this is easy to get wrong and hard to notice.
fn normal_matrix(m: &[[f32; 4]; 4]) -> [[f32; 3]; 3] {
    let a = [
        [m[0][0], m[0][1], m[0][2]],
        [m[1][0], m[1][1], m[1][2]],
        [m[2][0], m[2][1], m[2][2]],
    ];
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[1][0] * (a[0][1] * a[2][2] - a[0][2] * a[2][1])
        + a[2][0] * (a[0][1] * a[1][2] - a[0][2] * a[1][1]);
    if det.abs() < 1e-20 {
        // Degenerate (a node scaled flat to nothing). The plain matrix is the
        // best available and cannot be worse than dividing by zero.
        return a;
    }
    let inv = 1.0 / det;
    // cofactor(a)/det == transpose(inverse(a)), which is the matrix wanted.
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

fn dir(n: &[[f32; 3]; 3], p: [f32; 3]) -> [f32; 3] {
    let v = [
        n[0][0] * p[0] + n[1][0] * p[1] + n[2][0] * p[2],
        n[0][1] * p[0] + n[1][1] * p[1] + n[2][1] * p[2],
        n[0][2] * p[0] + n[1][2] * p[1] + n[2][2] * p[2],
    ];
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-12 { [v[0] / len, v[1] / len, v[2] / len] } else { [0.0, 1.0, 0.0] }
}

fn det3(m: &[[f32; 4]; 4]) -> f32 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[1][0] * (m[0][1] * m[2][2] - m[0][2] * m[2][1])
        + m[2][0] * (m[0][1] * m[1][2] - m[0][2] * m[1][1])
}

fn walk(
    node: &gltf::Node,
    parent: [[f32; 4]; 4],
    buffers: &[gltf::buffer::Data],
    out: &mut Scene,
) {
    let world = mul(parent, node.transform().matrix());

    if let Some(mesh) = node.mesh() {
        // A negative determinant means this node mirrors, which reverses
        // winding — the same trap as in the FBX path, and just as invisible.
        let flipped = det3(&world) < 0.0;
        for (i, prim) in mesh.primitives().enumerate() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                out.report.warnings.push(
                    "A primitive that was not triangles (points or lines) was left out.".into(),
                );
                continue;
            }
            let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let Some(positions) = reader.read_positions() else { continue };

            let mut sm = SubMesh {
                name: if i > 0 {
                    format!("{}_{i}", mesh.name().unwrap_or("mesh"))
                } else {
                    mesh.name().unwrap_or("mesh").to_string()
                },
                base_color: [1.0, 1.0, 1.0, 1.0],
                ..Default::default()
            };
            for p in positions {
                sm.positions.push(point(&world, p));
            }
            if let Some(ns) = reader.read_normals() {
                // **Transformed, never negated.** Reflecting a surface reflects
                // its outward normal too, so the transformed normal is already
                // right; it is the WINDING that a mirror inverts, and swapping
                // two corners below is the whole of the correction. Negating as
                // well undoes it, and leaves the normals disagreeing with the
                // faces they belong to.
                let nm = normal_matrix(&world);
                for n in ns {
                    sm.normals.push(dir(&nm, n));
                }
            }
            if let Some(uv) = reader.read_tex_coords(0) {
                // Already in glTF's convention — this is a repack, so it must
                // NOT get the flip the FBX path applies.
                sm.uvs = Some(uv.into_f32().collect());
            }
            if let Some(c) = reader.read_colors(0) {
                sm.colors = Some(c.into_rgba_u8().collect());
            }
            match reader.read_indices() {
                Some(idx) => sm.indices = idx.into_u32().collect(),
                None => sm.indices = (0..sm.positions.len() as u32).collect(),
            }
            if flipped {
                for t in sm.indices.as_chunks_mut::<3>().0 {
                    t.swap(1, 2);
                }
            }

            let mat = prim.material();
            let pbr = mat.pbr_metallic_roughness();
            sm.base_color = pbr.base_color_factor();
            sm.texture = pbr.base_color_texture().map(|t| t.texture().source().index());
            out.meshes.push(sm);
        }
    }

    for child in node.children() {
        walk(&child, world, buffers, out);
    }
}
