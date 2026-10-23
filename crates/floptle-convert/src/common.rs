//! The shape every reader produces, and the one place it becomes a `.glb`.
//!
//! Each format is read into this and then written once. The alternative —
//! every reader building `WriteNode`s itself — is how a converter ends up with
//! five subtly different opinions about whether normals are optional and which
//! way a triangle winds.

use floptle_assets::glb_write::{
    write_glb_animated, WriteAnimation, WriteMaterial, WriteMesh, WriteNode, WriteSkin,
};
use floptle_render::{TexFilter, TextureData};

use crate::{ConvertError, Report};

/// One piece of geometry with one material.
///
/// Flat, indexed, triangles only — the glTF shape — because every reader has to
/// get there anyway and doing it at the edge keeps the conversion honest.
#[derive(Default, Clone)]
pub struct SubMesh {
    pub name: String,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Option<Vec<[f32; 2]>>,
    pub colors: Option<Vec<[u8; 4]>>,
    pub indices: Vec<u32>,
    pub base_color: [f32; 4],
    /// Index into [`Scene::textures`].
    pub texture: Option<usize>,
    /// The full material, when the source had more to say than a colour and
    /// one picture. Its texture indices are into [`Scene::textures`] too.
    pub material: Option<WriteMaterial>,
    /// The [`Scene::nodes`] entry this mesh hangs from, so it moves with an
    /// animated node. `None`: a root, with its vertices already in place.
    pub parent: Option<usize>,
    /// Index into [`Scene::skins`], for a mesh that bends with a skeleton,
    /// with its per-vertex joint slots and weights.
    pub skin: Option<usize>,
    pub joints: Option<Vec<[u16; 4]>>,
    pub weights: Option<Vec<[f32; 4]>>,
}

/// A node of the converted model's tree: a bone, or the transform an animated
/// part hangs from. Only a model with a skeleton or animation has any.
#[derive(Clone, Debug)]
pub struct SceneNode {
    pub name: String,
    pub parent: Option<usize>,
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl SubMesh {
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }

    /// Give it flat normals if it has none.
    ///
    /// **A glTF mesh without normals is legal and is shaded flat by the
    /// viewer**, which looks like a broken export rather than a missing
    /// attribute — and STL genuinely has no per-vertex normals at all. Computing
    /// them here means every path downstream can assume they exist.
    pub fn ensure_normals(&mut self) {
        if self.normals.len() == self.positions.len() && !self.normals.is_empty() {
            return;
        }
        let mut acc = vec![[0f32; 3]; self.positions.len()];
        for tri in self.indices.as_chunks::<3>().0 {
            let (a, b, c) =
                (tri[0] as usize, tri[1] as usize, tri[2] as usize);
            if a >= acc.len() || b >= acc.len() || c >= acc.len() {
                continue;
            }
            let (p, q, r) = (self.positions[a], self.positions[b], self.positions[c]);
            let u = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
            let v = [r[0] - p[0], r[1] - p[1], r[2] - p[2]];
            let n = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            for i in [a, b, c] {
                for k in 0..3 {
                    acc[i][k] += n[k];
                }
            }
        }
        for n in acc.iter_mut() {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len > 1e-12 {
                for c in n.iter_mut() {
                    *c /= len;
                }
            } else {
                // A degenerate or unreferenced vertex. Up is arbitrary and
                // wrong, but a zero normal is a black shading artefact.
                *n = [0.0, 1.0, 0.0];
            }
        }
        self.normals = acc;
    }

    /// Drop triangles that index past the end of the vertex list.
    ///
    /// Real files in the wild have these, and the glTF spec does not allow
    /// them: a viewer is entitled to reject the whole file, so one bad triangle
    /// in a scan can make the entire conversion unopenable. Counted, so the
    /// report can say it happened rather than quietly shipping fewer faces.
    pub fn drop_bad_indices(&mut self) -> usize {
        let n = self.positions.len() as u32;
        let before = self.indices.len() / 3;
        let mut kept = Vec::with_capacity(self.indices.len());
        for tri in self.indices.as_chunks::<3>().0 {
            if tri[0] < n && tri[1] < n && tri[2] < n {
                kept.extend_from_slice(tri);
            }
        }
        self.indices = kept;
        before - self.indices.len() / 3
    }
}

/// A whole converted model, before it becomes bytes.
#[derive(Default)]
pub struct Scene {
    pub meshes: Vec<SubMesh>,
    pub textures: Vec<TextureData>,
    /// How each of [`Self::textures`] is filtered, where the source said.
    pub filters: Vec<Option<TexFilter>>,
    pub nodes: Vec<SceneNode>,
    /// Joint indices are into [`Self::nodes`].
    pub skins: Vec<WriteSkin>,
    /// Channel node indices are into [`Self::nodes`].
    pub animations: Vec<WriteAnimation>,
    pub report: Report,
}

impl Scene {
    /// Tidy every mesh, then write one `.glb`.
    ///
    /// The tidy pass is here rather than in each reader on purpose: "does this
    /// have normals, are its indices in range, is it empty" are questions about
    /// the output format's requirements, and answering them per reader is how
    /// one format ships broken while the other four are fine.
    pub fn into_glb(mut self) -> Result<(Vec<u8>, Report), ConvertError> {
        // The tree first, so node indices in skins and animations stay what
        // the reader wrote; meshes are appended after it.
        let mut nodes: Vec<WriteNode> = self
            .nodes
            .iter()
            .map(|n| WriteNode {
                name: n.name.clone(),
                translation: n.translation,
                rotation: n.rotation,
                scale: n.scale,
                parent: n.parent,
                mesh: None,
                skin: None,
                material: None,
            })
            .collect();
        let tree = nodes.len();
        let mut dropped_tris = 0usize;

        for mut m in std::mem::take(&mut self.meshes) {
            dropped_tris += m.drop_bad_indices();
            if m.positions.is_empty() || m.indices.is_empty() {
                continue;
            }
            m.ensure_normals();
            self.report.triangles += m.triangles();
            let skinned = m.skin.is_some()
                && m.joints.as_ref().is_some_and(|j| j.len() == m.positions.len())
                && m.weights.as_ref().is_some_and(|w| w.len() == m.positions.len());
            let mut node = WriteNode::mesh_node(
                m.name.clone(),
                WriteMesh {
                    positions: m.positions,
                    normals: m.normals,
                    uvs: m.uvs,
                    colors: m.colors,
                    joints: if skinned { m.joints } else { None },
                    weights: if skinned { m.weights } else { None },
                    indices: m.indices,
                    base_color: m.base_color,
                    texture: m.texture,
                },
            );
            node.parent = m.parent.filter(|&p| p < tree);
            node.skin = if skinned { m.skin } else { None };
            node.material = m.material;
            nodes.push(node);
        }

        if dropped_tris > 0 {
            self.report.warnings.push(format!(
                "{dropped_tris} triangle(s) pointed at vertices that are not there and were \
                 left out — the file was already damaged in that spot."
            ));
        }

        // **An empty result is a failure, not a small file.** A .glb with no
        // meshes opens fine and shows nothing, which reads as a broken engine
        // rather than as a file that never had geometry in it.
        if nodes.len() == tree {
            return Err(ConvertError::NoGeometry);
        }

        self.report.meshes = nodes.len() - tree;
        self.report.textures = self.textures.len();
        self.report.skins = self.skins.len();
        // A source that names no filter gets none, and the engine's default
        // applies, as it did to the source.
        let bytes = write_glb_animated(&nodes, &self.skins, &self.textures, &self.filters, &self.animations);
        Ok((bytes, self.report))
    }
}
