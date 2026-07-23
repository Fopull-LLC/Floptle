//! **Flow-rig**: give one object of a model a soft bone-chain + auto-weights so it
//! can bend and curve — hair, cloth, antennae, a tail. The chain runs top→bottom
//! down the object's Y extent; each vertex is weighted to the one or two nearest
//! bones by height (a normalized blend), so posing/keyframing the chain sweeps the
//! geometry smoothly. Everything is baked into a new `<stem>.rigged.glb` beside the
//! source via [`crate::glb_write`]; the rest of the model rides along unchanged.
//!
//! This is a *starter* rig: a straight vertical chain is the right default for
//! anything that hangs and flows. Re-pose the bones to shape it.

use std::path::{Path, PathBuf};

use floptle_core::math::Mat4;

use crate::gltf_import::ImportError;
use crate::glb_write::{write_glb, WriteNode, WriteSkin};

/// What the flow-rig pass produced.
#[derive(Debug, Default)]
pub struct RigReport {
    pub output: PathBuf,
    pub object: String,
    pub bones: usize,
}

/// Add a `bone_count`-segment flow-rig to `object` in `model_path`, writing
/// `<stem>.rigged.glb`. `bone_count` is clamped to at least 2.
pub fn add_flow_rig(
    model_path: &Path,
    object: &str,
    bone_count: usize,
) -> Result<RigReport, ImportError> {
    let (objs, textures) = crate::gltf_mirror::gather_objects(model_path)?;
    let n = bone_count.max(2);

    // Locate the target object's baked geometry + its Y range and horizontal centroid.
    let target = objs
        .iter()
        .find(|o| o.name == object)
        .ok_or(ImportError::NoGeometry)?;
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut cx, mut cz) = (0.0f32, 0.0f32);
    for p in &target.mesh.positions {
        min_y = min_y.min(p[1]);
        max_y = max_y.max(p[1]);
        cx += p[0];
        cz += p[2];
    }
    let vcount = target.mesh.positions.len().max(1) as f32;
    cx /= vcount;
    cz /= vcount;
    let range = (max_y - min_y).max(1e-5);
    let seg = range / (n - 1) as f32;

    // Bone rest positions: straight vertical chain from the top down.
    let bone_y = |i: usize| max_y - i as f32 * seg;

    // ---- weight the target's vertices to the 1–2 nearest bones by height ----
    let mut joints: Vec<[u16; 4]> = Vec::with_capacity(target.mesh.positions.len());
    let mut weights: Vec<[f32; 4]> = Vec::with_capacity(target.mesh.positions.len());
    for p in &target.mesh.positions {
        let t = ((max_y - p[1]) / range).clamp(0.0, 1.0); // 0 at root, 1 at tip
        let f = t * (n - 1) as f32;
        let lower = (f.floor() as usize).min(n - 1);
        let upper = (lower + 1).min(n - 1);
        let frac = (f - lower as f32).clamp(0.0, 1.0);
        if lower == upper {
            joints.push([lower as u16, 0, 0, 0]);
            weights.push([1.0, 0.0, 0.0, 0.0]);
        } else {
            joints.push([lower as u16, upper as u16, 0, 0]);
            weights.push([1.0 - frac, frac, 0.0, 0.0]);
        }
    }

    // ---- build the output node list ----
    let mut nodes: Vec<WriteNode> = Vec::new();
    // Every OTHER object rides along unchanged (rigid root node).
    for o in &objs {
        if o.name != object {
            nodes.push(WriteNode::mesh_node(o.name.clone(), o.mesh.clone()));
        }
    }
    // The bone chain (parent → child, straight down). Node index bookkeeping.
    let bone_base = nodes.len();
    for i in 0..n {
        let translation = if i == 0 { [cx, bone_y(0), cz] } else { [0.0, -seg, 0.0] };
        let parent = (i > 0).then_some(bone_base + i - 1);
        let name = if i == 0 { format!("{object}_root") } else { format!("{object}_{i}") };
        nodes.push(WriteNode {
            name,
            translation,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            parent,
            mesh: None,
            skin: None,
        });
    }
    // The skinned target mesh (a root node bound to the chain).
    let mut hair_mesh = target.mesh.clone();
    hair_mesh.joints = Some(joints);
    hair_mesh.weights = Some(weights);
    let mut hair_node = WriteNode::mesh_node(object.to_string(), hair_mesh);
    hair_node.skin = Some(0);
    nodes.push(hair_node);

    // Skin: palette slot i ↔ bone i; inverse-bind = inverse(bone rest world). The
    // chain is pure translation at rest, so the bind pose is the identity deform.
    let skin = WriteSkin {
        joints: (bone_base..bone_base + n).collect(),
        inverse_bind: (0..n)
            .map(|i| {
                Mat4::from_translation([-cx, -bone_y(i), -cz].into()).to_cols_array()
            })
            .collect(),
    };

    let bytes = write_glb(&nodes, &[skin], &textures);
    let output = model_path.with_extension("rigged.glb");
    std::fs::write(&output, bytes).map_err(|e| ImportError::Gltf(gltf::Error::Io(e)))?;
    Ok(RigReport { output, object: object.to_string(), bones: n })
}
