//! Rigged glTF import — models with animations (ADR-0006, animation system).
//!
//! Unlike [`crate::gltf_import`] (which bakes each node's world transform into
//! the vertices and discards the tree), this path **keeps the node hierarchy**:
//! each mesh part stays in its node's local space and records which node drives
//! it, the whole tree becomes a [`floptle_anim::Skeleton`], and every glTF
//! animation becomes a sampled [`floptle_anim::Clip`] (bound by node index,
//! name-recoverable via the skeleton). That's everything the rigid/hierarchical
//! animation path needs — R6-style characters whose body parts are parented to
//! animated bones. `JOINTS_0`/`WEIGHTS_0` + inverse-bind matrices are captured
//! too (per-part [`SkinStream`]) so the future GPU vertex-skinning render path
//! has its data ready.
//!
//! Returns `None` when the file has no animations — callers fall back to the
//! static baked import.

use std::collections::HashMap;
use std::path::Path;

use floptle_anim::{Clip, Interp, NodeChannels, SkelNode, Skeleton, Track, TransformTRS};
use floptle_core::math::{Mat4, Quat, Vec3};
use floptle_render::{MeshData, TextureData, Vertex};

use crate::gltf_import::ImportError;

/// A model imported with its rig: per-material-per-node parts (in node-local
/// space), the node hierarchy, and its animation clips.
#[derive(Debug)]
pub struct RiggedModel {
    pub name: String,
    pub parts: Vec<RiggedPart>,
    /// Decoded base-color textures, indexed by `RiggedPart::texture`.
    pub textures: Vec<TextureData>,
    pub skeleton: Skeleton,
    pub clips: Vec<Clip>,
    /// Rough world size (rest pose), for framing/preview — same contract as
    /// the static import.
    pub size: f32,
    /// Rest-pose bounds center in authored space. Skeleton + clips are kept
    /// exactly as authored (so extracted clips are portable across re-exports);
    /// renderers subtract this as a placement offset to center the model,
    /// matching the static import's recentering.
    pub center: [f32; 3],
    /// Axis-aligned rest bounds, center-relative (`min == -max`-ish).
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// One node's worth of geometry for one material, in node-local space.
#[derive(Debug)]
pub struct RiggedPart {
    pub mesh: MeshData,
    pub base_color: [f32; 3],
    pub texture: Option<usize>,
    /// Index into `RiggedModel::skeleton` — the node whose animated world
    /// matrix places this part.
    pub node: usize,
    /// Present when the primitive is vertex-weighted (a deforming mesh):
    /// the data for the future GPU skinning path.
    pub skin: Option<SkinStream>,
}

/// Captured skinning attributes for one part.
#[derive(Debug)]
pub struct SkinStream {
    /// Per-vertex joint indices (into `joints`) + weights.
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
    /// Skeleton node index per joint slot.
    pub joint_nodes: Vec<usize>,
    pub inverse_bind: Vec<Mat4>,
}

/// Import a `.glb`/`.gltf` keeping its node structure. `None` = a single-object
/// static prop with no rig (use the flattening static import instead).
///
/// We keep the tree when the model is animated, skinned, OR made of more than one
/// mesh object (an N64-style character split into body-part meshes). In every
/// case each mesh stays in its node's local space, the whole node tree becomes a
/// [`Skeleton`], and the model's objects/bones are individually addressable,
/// pose-able and keyframe-able — the static bake would merge them per-material
/// and throw the identities away. Only a lone single-mesh prop (nothing to parent
/// or animate independently) takes the cheaper flattening path.
pub fn import_rigged(path: &Path) -> Result<Option<RiggedModel>, ImportError> {
    let (doc, buffers, images) = gltf::import(path).map_err(ImportError::Gltf)?;
    // Keep the structure if the file is animated OR skinned (a rig authored
    // elsewhere, clips to be keyed IN-ENGINE — the astronaut_male case) OR it has
    // two or more mesh objects (the multi-part unrigged character — Sae). A single
    // mesh with no rig has no sub-objects to expose, so it stays a plain baked prop.
    let mesh_objects = doc.nodes().filter(|n| n.mesh().is_some()).count();
    if doc.animations().len() == 0 && doc.skins().len() == 0 && mesh_objects < 2 {
        return Ok(None);
    }

    let textures: Vec<TextureData> = images.iter().map(crate::gltf_import::to_rgba8).collect();

    // ---- skeleton: every node in the default scene, topologically sorted ----
    let mut nodes: Vec<SkelNode> = Vec::new();
    let mut gltf_to_skel: HashMap<usize, usize> = HashMap::new();
    let scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .ok_or(ImportError::NoGeometry)?;
    fn walk(
        node: &gltf::Node,
        parent: Option<usize>,
        nodes: &mut Vec<SkelNode>,
        map: &mut HashMap<usize, usize>,
    ) {
        let (t, r, s) = node.transform().decomposed();
        let idx = nodes.len();
        let mut name = node
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("Node{}", node.index()));
        // Baked clips bind channels by node NAME, so names must be unique.
        // Blender exports often carry a bone and its mesh child with the same
        // name — dedupe deterministically (first keeps the plain name; the
        // animated bone comes first in the walk, so it wins the plain name).
        if nodes.iter().any(|n| n.name == name) {
            let mut i = 2;
            while nodes.iter().any(|n| n.name == format!("{name}#{i}")) {
                i += 1;
            }
            name = format!("{name}#{i}");
        }
        nodes.push(SkelNode {
            name,
            parent,
            rest: TransformTRS {
                t: Vec3::from(t),
                r: Quat::from_array(r).normalize(),
                s: Vec3::from(s),
            },
        });
        map.insert(node.index(), idx);
        for child in node.children() {
            walk(&child, Some(idx), nodes, map);
        }
    }
    for node in scene.nodes() {
        walk(&node, None, &mut nodes, &mut gltf_to_skel);
    }

    // ---- parts: one per (node, material), vertices left in node space ----
    let mut parts: Vec<RiggedPart> = Vec::new();
    for node in doc.nodes() {
        let Some(&skel_idx) = gltf_to_skel.get(&node.index()) else { continue };
        let Some(mesh) = node.mesh() else { continue };
        // Per-part skinning capture (deforming meshes). The joint list/IBMs are
        // per-skin; vertices carry JOINTS_0/WEIGHTS_0.
        let skin_info = node.skin().map(|skin| {
            let reader = skin.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let ibms: Vec<Mat4> = reader
                .read_inverse_bind_matrices()
                .map(|it| it.map(|m| Mat4::from_cols_array_2d(&m)).collect())
                .unwrap_or_else(|| vec![Mat4::IDENTITY; skin.joints().count()]);
            let joint_nodes: Vec<usize> = skin
                .joints()
                .map(|j| match gltf_to_skel.get(&j.index()) {
                    Some(&idx) => idx,
                    None => {
                        eprintln!(
                            "  [rig] skin joint node {} is outside the default scene — \
                             binding it to the root (deform may look wrong)",
                            j.index()
                        );
                        0
                    }
                })
                .collect();
            (joint_nodes, ibms)
        });
        let mut by_material: HashMap<i64, usize> = HashMap::new();
        for prim in mesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                continue;
            }
            let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let Some(pos_iter) = reader.read_positions() else { continue };
            let positions: Vec<[f32; 3]> = pos_iter.collect();
            if positions.is_empty() {
                continue;
            }
            let indices: Vec<u32> = match reader.read_indices() {
                Some(ri) => ri.into_u32().collect(),
                None => (0..positions.len() as u32).collect(),
            };
            let normals: Vec<[f32; 3]> = match reader.read_normals() {
                Some(it) => it.collect(),
                None => crate::gltf_import::compute_normals(&positions, &indices),
            };
            let uvs: Option<Vec<[f32; 2]>> =
                reader.read_tex_coords(0).map(|tc| tc.into_f32().collect());
            let vjoints: Option<Vec<[u16; 4]>> =
                reader.read_joints(0).map(|j| j.into_u16().collect());
            let vweights: Option<Vec<[f32; 4]>> =
                reader.read_weights(0).map(|w| w.into_f32().collect());
            // COLOR_0 — vertex paint authored in Blender (normalized to RGBA8).
            let vcolors: Option<Vec<[u8; 4]>> =
                reader.read_colors(0).map(|c| c.into_rgba_u8().collect());

            let mat = prim.material();
            let key = mat.index().map(|i| i as i64).unwrap_or(-1);
            let part_idx = *by_material.entry(key).or_insert_with(|| {
                let pbr = mat.pbr_metallic_roughness();
                let bcf = pbr.base_color_factor();
                parts.push(RiggedPart {
                    mesh: MeshData::default(),
                    base_color: [bcf[0], bcf[1], bcf[2]],
                    texture: pbr
                        .base_color_texture()
                        .map(|info| info.texture().source().index()),
                    node: skel_idx,
                    skin: skin_info.as_ref().map(|(jn, ibm)| SkinStream {
                        joints: Vec::new(),
                        weights: Vec::new(),
                        joint_nodes: jn.clone(),
                        inverse_bind: ibm.clone(),
                    }),
                });
                parts.len() - 1
            });
            let part = &mut parts[part_idx];
            let base = part.mesh.vertices.len() as u32;
            for i in 0..positions.len() {
                let uv = uvs.as_ref().map(|u| u[i]).unwrap_or([0.0, 0.0]);
                part.mesh.vertices.push(Vertex {
                    pos: positions[i],
                    normal: normals[i],
                    uv,
                });
            }
            for idx in indices {
                part.mesh.indices.push(base + idx);
            }
            // Keep the skin streams PARALLEL to the vertex list: a primitive
            // that lacks JOINTS_0/WEIGHTS_0 in an otherwise-skinned part pads
            // with zero weights (the shader then falls back to the node
            // transform) so slots never misalign.
            if let Some(stream) = part.skin.as_mut() {
                match (vjoints, vweights) {
                    (Some(j), Some(w)) if j.len() == positions.len() && w.len() == positions.len() => {
                        stream.joints.extend(j);
                        stream.weights.extend(w);
                    }
                    _ => {
                        stream.joints.extend(std::iter::repeat_n([0u16; 4], positions.len()));
                        stream
                            .weights
                            .extend(std::iter::repeat_n([0.0f32; 4], positions.len()));
                    }
                }
                debug_assert_eq!(stream.joints.len(), part.mesh.vertices.len());
            }
            // Same parallel-stream rule for vertex paint: a part mixing painted and
            // unpainted primitives back-fills white (the identity for the albedo
            // multiply), so paint never misaligns against geometry.
            const WHITE: [u8; 4] = [255; 4];
            if vcolors.is_some() || part.mesh.colors.is_some() {
                let c = part.mesh.colors.get_or_insert_with(Vec::new);
                c.resize(base as usize, WHITE);
                match &vcolors {
                    Some(src) => {
                        c.extend(src.iter().take(positions.len()).copied());
                        c.resize(base as usize + positions.len(), WHITE);
                    }
                    None => c.resize(base as usize + positions.len(), WHITE),
                }
                debug_assert_eq!(c.len(), part.mesh.vertices.len());
            }
        }
    }
    parts.retain(|p| !p.mesh.vertices.is_empty() && !p.mesh.indices.is_empty());
    if parts.is_empty() {
        return Err(ImportError::NoGeometry);
    }

    // ---- clips: every animation, channels bound to skeleton indices ----
    let mut clips: Vec<Clip> = Vec::new();
    for anim in doc.animations() {
        let mut name = anim
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("Clip{}", anim.index()));
        // Clip names must be unique — extraction keys files by name, and the
        // embedded-clips fallback resolves states by name (same dedup rule as
        // nodes: first keeps the plain name).
        if clips.iter().any(|c| c.name == name) {
            let mut i = 2;
            while clips.iter().any(|c| c.name == format!("{name}#{i}")) {
                i += 1;
            }
            name = format!("{name}#{i}");
        }
        let mut duration = 0.0f32;
        let mut by_node: HashMap<usize, NodeChannels> = HashMap::new();
        for chan in anim.channels() {
            let Some(&node) = gltf_to_skel.get(&chan.target().node().index()) else { continue };
            let reader = chan.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let Some(inputs) = reader.read_inputs() else { continue };
            let times: Vec<f32> = inputs.collect();
            if times.is_empty() {
                continue;
            }
            let raw_interp = chan.sampler().interpolation();
            let interp = match raw_interp {
                gltf::animation::Interpolation::Step => Interp::Step,
                _ => Interp::Linear, // CubicSpline de-tangented to Linear below
            };
            let cubic = raw_interp == gltf::animation::Interpolation::CubicSpline;
            // Only create the node's channel entry for track kinds we handle —
            // a morph-weights channel must not leave an empty NodeChannels
            // behind (it would wrongly mark the node as animation-covered).
            use gltf::animation::util::ReadOutputs;
            macro_rules! entry {
                () => {
                    by_node
                        .entry(node)
                        .or_insert_with(|| NodeChannels { node, ..NodeChannels::default() })
                };
            }
            match reader.read_outputs() {
                Some(ReadOutputs::Translations(it)) => {
                    let values = detangent(it.map(Vec3::from).collect(), cubic);
                    duration = duration.max(*times.last().unwrap());
                    entry!().translation = Some(Track { times: times.clone(), values, interp });
                }
                Some(ReadOutputs::Rotations(rot)) => {
                    let values = detangent(
                        rot.into_f32().map(|q| Quat::from_array(q).normalize()).collect(),
                        cubic,
                    );
                    duration = duration.max(*times.last().unwrap());
                    entry!().rotation = Some(Track { times: times.clone(), values, interp });
                }
                Some(ReadOutputs::Scales(it)) => {
                    let values = detangent(it.map(Vec3::from).collect(), cubic);
                    duration = duration.max(*times.last().unwrap());
                    entry!().scale = Some(Track { times: times.clone(), values, interp });
                }
                _ => {} // morph-target weights: deferred
            }
        }
        let mut channels: Vec<NodeChannels> = by_node.into_values().collect();
        channels.sort_by_key(|c| c.node);
        clips.push(Clip { name, duration: duration.max(1e-3), channels, events: Vec::new() });
    }

    // ---- measure the rest-pose bounds. The skeleton and clips are kept
    // exactly as authored — the bounds CENTER is returned as a placement
    // offset the renderer applies, so extracted clips never embed an
    // import-time framing artifact (re-exports and retargets stay stable). ----
    let skeleton = Skeleton::new(nodes);
    let mut world = Vec::new();
    skeleton.world_matrices(&skeleton.rest_pose(), &mut world);
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for part in &parts {
        let m = world[part.node];
        for v in &part.mesh.vertices {
            let p = m.transform_point3(Vec3::from(v.pos));
            min = min.min(p);
            max = max.max(p);
        }
    }
    if !min.is_finite() || !max.is_finite() {
        return Err(ImportError::NoGeometry);
    }
    let center = (min + max) * 0.5;

    let size = (max - min).max_element().max(1e-6);
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("model").to_string();
    Ok(Some(RiggedModel {
        name,
        parts,
        textures,
        skeleton,
        clips,
        size,
        center: center.to_array(),
        min: (min - center).to_array(),
        max: (max - center).to_array(),
    }))
}

/// CubicSpline outputs store (in-tangent, key, out-tangent) triplets; keep the
/// key values only (Linear approximation — Blender exports Linear for bones,
/// so this is the rare path).
fn detangent<T: Copy>(values: Vec<T>, cubic: bool) -> Vec<T> {
    if !cubic {
        return values;
    }
    values.chunks_exact(3).map(|c| c[1]).collect()
}

/// Just the animation names in a glTF file — a light probe for the Inspector's
/// asset panel (no image decode; buffers only). Deduped like the full import
/// so the listed names match the extracted files.
pub fn probe_animations(path: &Path) -> Vec<String> {
    let Ok(gltf::Gltf { document, .. }) = gltf::Gltf::open(path) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for a in document.animations() {
        let mut name = a
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("Clip{}", a.index()));
        if out.contains(&name) {
            let mut i = 2;
            while out.contains(&format!("{name}#{i}")) {
                i += 1;
            }
            name = format!("{name}#{i}");
        }
        out.push(name);
    }
    out
}
