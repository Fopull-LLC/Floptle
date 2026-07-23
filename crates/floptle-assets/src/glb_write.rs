//! A minimal, dependency-light **glTF 2.0 binary (`.glb`) writer** — just enough
//! to bake models the engine produces (the Mirror-apply normalizer, the hair
//! auto-rig) back into a standard `.glb` that any tool re-imports. The `gltf`
//! crate is import-only, so this hand-assembles the JSON chunk (via `serde_json`)
//! and packs all geometry + embedded PNG textures + inverse-bind matrices into one
//! BIN chunk.
//!
//! Scope is deliberately narrow: triangle meshes with POSITION/NORMAL, optional
//! TEXCOORD_0 / COLOR_0 / JOINTS_0+WEIGHTS_0, a node tree with TRS + optional skin,
//! PBR base-color (factor + optional embedded texture), and skins with
//! inverse-bind matrices. That's the exact surface our importer round-trips.

use floptle_render::TextureData;
use serde_json::{json, Value};

/// One triangle mesh attached to a [`WriteNode`]. Streams are parallel and in the
/// node's LOCAL space.
#[derive(Clone, Default)]
pub struct WriteMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Option<Vec<[f32; 2]>>,
    pub colors: Option<Vec<[u8; 4]>>,
    /// Skinning: per-vertex joint slots + weights (both present ⇒ the node's mesh
    /// is skinned and the node must reference a [`WriteSkin`]).
    pub joints: Option<Vec<[u16; 4]>>,
    pub weights: Option<Vec<[f32; 4]>>,
    pub indices: Vec<u32>,
    /// PBR base-color factor (rgba).
    pub base_color: [f32; 4],
    /// Index into the `textures` slice passed to [`write_glb`] (base-color map).
    pub texture: Option<usize>,
}

/// One node of the output scene: a TRS transform, an optional mesh, an optional
/// skin binding, and an optional parent (index into the same node slice).
#[derive(Clone)]
pub struct WriteNode {
    pub name: String,
    pub translation: [f32; 3],
    pub rotation: [f32; 4], // xyzw
    pub scale: [f32; 3],
    pub parent: Option<usize>,
    pub mesh: Option<WriteMesh>,
    pub skin: Option<usize>,
}

impl WriteNode {
    /// A node at the origin (identity TRS) with the given name + mesh.
    pub fn mesh_node(name: impl Into<String>, mesh: WriteMesh) -> Self {
        Self {
            name: name.into(),
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            parent: None,
            mesh: Some(mesh),
            skin: None,
        }
    }
}

/// A skin: the joint node indices it drives + one inverse-bind matrix per joint
/// (column-major `[f32; 16]`, matching glTF).
#[derive(Clone)]
pub struct WriteSkin {
    pub joints: Vec<usize>,
    pub inverse_bind: Vec<[f32; 16]>,
}

// glTF component types.
const F32: u32 = 5126;
const U32: u32 = 5125;
const U16: u32 = 5123;
const U8: u32 = 5121;
// bufferView targets.
const ARRAY_BUFFER: u32 = 34962;
const ELEMENT_ARRAY_BUFFER: u32 = 34963;

/// Accumulates the single BIN buffer + its bufferViews.
#[derive(Default)]
struct BinBuf {
    data: Vec<u8>,
    views: Vec<Value>,
}

impl BinBuf {
    /// Append `bytes` as a new bufferView (4-byte aligned), returning its index.
    fn view(&mut self, bytes: &[u8], target: Option<u32>) -> usize {
        while !self.data.len().is_multiple_of(4) {
            self.data.push(0);
        }
        let offset = self.data.len();
        self.data.extend_from_slice(bytes);
        let mut v = json!({ "buffer": 0, "byteOffset": offset, "byteLength": bytes.len() });
        if let Some(t) = target {
            v["target"] = json!(t);
        }
        self.views.push(v);
        self.views.len() - 1
    }
}

fn accessor(
    accessors: &mut Vec<Value>,
    view: usize,
    comp: u32,
    count: usize,
    typ: &str,
    normalized: bool,
) -> usize {
    let mut a = json!({
        "bufferView": view,
        "componentType": comp,
        "count": count,
        "type": typ,
    });
    if normalized {
        a["normalized"] = json!(true);
    }
    accessors.push(a);
    accessors.len() - 1
}

fn f32_bytes<const N: usize>(rows: &[[f32; N]]) -> Vec<u8> {
    let mut b = Vec::with_capacity(rows.len() * N * 4);
    for r in rows {
        for v in r {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    b
}

/// Serialize a node tree into a `.glb` byte vector. `textures` are embedded as PNG.
pub fn write_glb(nodes: &[WriteNode], skins: &[WriteSkin], textures: &[TextureData]) -> Vec<u8> {
    let mut bin = BinBuf::default();
    let mut accessors: Vec<Value> = Vec::new();
    let mut meshes: Vec<Value> = Vec::new();
    let mut materials: Vec<Value> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    let mut gltf_textures: Vec<Value> = Vec::new();

    // ---- embedded textures (PNG in a bufferView) ----
    for tex in textures {
        let png = crate::texture::encode_png(tex).unwrap_or_default();
        let view = bin.view(&png, None);
        let img = images.len();
        images.push(json!({ "bufferView": view, "mimeType": "image/png" }));
        gltf_textures.push(json!({ "source": img, "sampler": 0 }));
    }

    // ---- one mesh + material per node that has geometry ----
    // node index → glTF mesh index.
    let mut node_mesh: Vec<Option<usize>> = vec![None; nodes.len()];
    for (ni, node) in nodes.iter().enumerate() {
        let Some(m) = &node.mesh else { continue };
        if m.positions.is_empty() || m.indices.is_empty() {
            continue;
        }
        // POSITION (with required min/max).
        let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
        for p in &m.positions {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let pos_view = bin.view(&f32_bytes(&m.positions), Some(ARRAY_BUFFER));
        let pos_acc = accessor(&mut accessors, pos_view, F32, m.positions.len(), "VEC3", false);
        accessors[pos_acc]["min"] = json!(lo);
        accessors[pos_acc]["max"] = json!(hi);

        let nrm_view = bin.view(&f32_bytes(&m.normals), Some(ARRAY_BUFFER));
        let nrm_acc = accessor(&mut accessors, nrm_view, F32, m.normals.len(), "VEC3", false);

        let mut attrs = json!({ "POSITION": pos_acc, "NORMAL": nrm_acc });
        let has_uv = m.uvs.as_ref().is_some_and(|u| u.len() == m.positions.len());
        if let Some(uvs) = &m.uvs
            && has_uv
        {
            let v = bin.view(&f32_bytes(uvs), Some(ARRAY_BUFFER));
            attrs["TEXCOORD_0"] = json!(accessor(&mut accessors, v, F32, uvs.len(), "VEC2", false));
        }
        if let Some(cols) = &m.colors
            && cols.len() == m.positions.len()
        {
            let bytes: Vec<u8> = cols.iter().flat_map(|c| c.iter().copied()).collect();
            let v = bin.view(&bytes, Some(ARRAY_BUFFER));
            attrs["COLOR_0"] = json!(accessor(&mut accessors, v, U8, cols.len(), "VEC4", true));
        }
        if let (Some(j), Some(w)) = (&m.joints, &m.weights)
            && j.len() == m.positions.len()
            && w.len() == m.positions.len()
        {
            let mut jb = Vec::with_capacity(j.len() * 8);
            for row in j {
                for v in row {
                    jb.extend_from_slice(&v.to_le_bytes());
                }
            }
            let jv = bin.view(&jb, Some(ARRAY_BUFFER));
            attrs["JOINTS_0"] = json!(accessor(&mut accessors, jv, U16, j.len(), "VEC4", false));
            let wv = bin.view(&f32_bytes(w), Some(ARRAY_BUFFER));
            attrs["WEIGHTS_0"] = json!(accessor(&mut accessors, wv, F32, w.len(), "VEC4", false));
        }

        // indices
        let mut ib = Vec::with_capacity(m.indices.len() * 4);
        for i in &m.indices {
            ib.extend_from_slice(&i.to_le_bytes());
        }
        let iv = bin.view(&ib, Some(ELEMENT_ARRAY_BUFFER));
        let iacc = accessor(&mut accessors, iv, U32, m.indices.len(), "SCALAR", false);

        // material
        let mut pbr = json!({
            "baseColorFactor": m.base_color,
            "metallicFactor": 0.0,
            "roughnessFactor": 1.0,
        });
        if let Some(t) = m.texture
            && has_uv
            && t < gltf_textures.len()
        {
            pbr["baseColorTexture"] = json!({ "index": t });
        }
        let mat = materials.len();
        materials.push(json!({
            "name": format!("{}_mat", node.name),
            "pbrMetallicRoughness": pbr,
            "doubleSided": true,
        }));

        let mesh_idx = meshes.len();
        meshes.push(json!({
            "name": node.name,
            "primitives": [ { "attributes": attrs, "indices": iacc, "material": mat, "mode": 4 } ],
        }));
        node_mesh[ni] = Some(mesh_idx);
    }

    // ---- skins (inverse-bind accessors reference node joints resolved below) ----
    let mut gltf_skins: Vec<Value> = Vec::new();
    for s in skins {
        let mut ibm = Vec::with_capacity(s.inverse_bind.len() * 64);
        for m in &s.inverse_bind {
            for v in m {
                ibm.extend_from_slice(&v.to_le_bytes());
            }
        }
        let view = bin.view(&ibm, None);
        let acc = accessor(&mut accessors, view, F32, s.inverse_bind.len(), "MAT4", false);
        gltf_skins.push(json!({
            "inverseBindMatrices": acc,
            "joints": s.joints,
        }));
    }

    // ---- nodes + scene ----
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        match n.parent {
            Some(p) => children[p].push(i),
            None => roots.push(i),
        }
    }
    let json_nodes: Vec<Value> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let mut v = json!({
                "name": n.name,
                "translation": n.translation,
                "rotation": n.rotation,
                "scale": n.scale,
            });
            if let Some(m) = node_mesh[i] {
                v["mesh"] = json!(m);
            }
            if let Some(sk) = n.skin {
                v["skin"] = json!(sk);
            }
            if !children[i].is_empty() {
                v["children"] = json!(children[i]);
            }
            v
        })
        .collect();

    let mut root = json!({
        "asset": { "version": "2.0", "generator": "Floptle glb_write" },
        "scene": 0,
        "scenes": [ { "nodes": roots } ],
        "nodes": json_nodes,
        "meshes": meshes,
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [ { "byteLength": bin.data.len() } ],
    });
    if !materials.is_empty() {
        root["materials"] = json!(materials);
    }
    if !images.is_empty() {
        root["images"] = json!(images);
        root["textures"] = json!(gltf_textures);
        root["samplers"] = json!([ { "magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497 } ]);
    }
    if !gltf_skins.is_empty() {
        root["skins"] = json!(gltf_skins);
    }

    container(&root, &bin.data)
}

/// Wrap the JSON + BIN into the GLB binary container.
fn container(root: &Value, bin: &[u8]) -> Vec<u8> {
    let mut json_bytes = serde_json::to_vec(root).unwrap_or_default();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let mut bin_bytes = bin.to_vec();
    while !bin_bytes.len().is_multiple_of(4) {
        bin_bytes.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin_bytes.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&0x46546C67u32.to_le_bytes()); // "glTF"
    out.extend_from_slice(&2u32.to_le_bytes()); // version
    out.extend_from_slice(&(total as u32).to_le_bytes());
    // JSON chunk
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F534Au32.to_le_bytes()); // "JSON"
    out.extend_from_slice(&json_bytes);
    // BIN chunk
    out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x004E4942u32.to_le_bytes()); // "BIN\0"
    out.extend_from_slice(&bin_bytes);
    out
}
