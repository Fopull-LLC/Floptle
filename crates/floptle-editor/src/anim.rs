//! Editor-side animation glue: asset registries, doc ↔ runtime binding,
//! clip extraction, and the per-frame animator advance.
//!
//! The pure runtime lives in `floptle-anim`; the serializable assets live in
//! `floptle-scene::anim`. This module connects them to the live editor world:
//!
//! - **Registries** — every `*.anim.ron` clip and `*.actl.ron` controller
//!   under `assets/`, keyed by project-relative path without extension.
//! - **Binding** — resolve a controller doc + its clips against a target
//!   "skeleton": a rigged Mesh's node hierarchy, or (for cutscene/scene
//!   animation) the node itself + its descendants matched by scene `Name`.
//! - **Instances** — one bound [`floptle_anim::Controller`] per animated
//!   entity, advanced scripts → animation → physics each Play frame; rigged
//!   Mesh poses land in [`AnimSystem::poses`] (read by the draw arm), scene
//!   poses write node `Transform`s directly.
//! - **Extraction** — bake a model's embedded glTF clips into standalone
//!   `.anim.ron` files under `assets/animations/<Model>/`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use floptle_anim::{
    Clip, Controller, Interp, Layer, NodeChannels, PropValue, PropertyTrack, SkelNode, Skeleton,
    State, Track, TransformTRS,
};
use floptle_core::math::{DMat4, Mat3, Mat4, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{AnimController, BoneAttach, Entity, Matter, Name, World};
use floptle_scene::{
    AnimChannelDoc, AnimClipDoc, AnimControllerDoc, AnimEventDoc, AnimPropTrackDoc, AnimPropValueDoc,
    AnimTrackDoc3, AnimTrackDoc4, ANIM_CLIP_EXT, ANIM_CTL_EXT,
};

use crate::MeshAsset;

/// A rigged model's shared animation data, kept on its [`MeshAsset`].
pub struct RigAsset {
    pub skeleton: Skeleton,
    /// The model's embedded clips, bound to `skeleton` by index.
    pub clips: Vec<Clip>,
    /// Skeleton node index for each registered mesh part (parallel to
    /// `MeshAsset::parts`).
    pub part_nodes: Vec<usize>,
    /// Rest-pose world matrices with `offset` pre-applied (drawn when nothing
    /// is animating).
    pub rest_world: Vec<Mat4>,
    /// Placement offset (recenters the authored rig on the node origin —
    /// kept OUT of the skeleton/clips so extracted clips stay portable).
    pub offset: Mat4,
    /// Per registered part (parallel to `part_nodes`): `Some` for a TRUE vertex-skinned
    /// part (its bind vertices + per-vertex joints/weights + bone palette inputs), which
    /// the draw path CPU-deforms each frame; `None` for a rigid-parented part (drawn at
    /// its node matrix, R6-style). This is what makes a skinned character actually move.
    pub skins: Vec<Option<SkinnedPart>>,
    /// Per SKELETON node (parallel to `skeleton.nodes`): `true` if the node renders
    /// geometry — an **object** / mesh sub-object (Sae's `Forearm`, a character's mesh
    /// part); `false` if it's a structural / skin-joint node — a **bone** of the rig
    /// (an armature joint, an empty). Objects and bones are both pose-able skeleton
    /// nodes keyframed the same way; this only tells the UI how to group + label them
    /// (the "Objects" vs "Bones" lists). A skinned mesh whose own node also deforms
    /// counts as an object (it has geometry).
    pub node_is_object: Vec<bool>,
}

/// CPU vertex-skinning data for one part: the bind-pose vertices plus everything needed
/// to deform them by the animated skeleton each frame (see [`cpu_skin_part`]).
pub struct SkinnedPart {
    /// Bind-pose vertices (skin space), CPU-skinned into a scratch buffer per frame.
    pub base: Vec<floptle_render::Vertex>,
    /// Per-vertex joint indices (into `joint_nodes`) + weights (parallel to `base`).
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
    /// Skeleton node index for each palette slot.
    pub joint_nodes: Vec<usize>,
    /// glTF inverse-bind matrix per palette slot.
    pub inverse_bind: Vec<Mat4>,
}

/// CPU vertex-skin one part: deform its bind vertices by the bone palette
/// (`node_world[joint_node] · inverse_bind`) into `out`. `node_world` is the skeleton's
/// per-node world matrices for this frame (`AnimSystem::poses[e]`, or the rig rest).
/// Zero-weight vertices fall back to the mesh node's own matrix, matching the importer's
/// zero-weight pad. Positions and normals are transformed; UVs pass through.
pub fn cpu_skin_part(
    part: &SkinnedPart,
    part_node: usize,
    node_world: &[Mat4],
    out: &mut Vec<floptle_render::Vertex>,
) {
    let palette: Vec<Mat4> = part
        .joint_nodes
        .iter()
        .zip(&part.inverse_bind)
        .map(|(&jn, ib)| node_world.get(jn).copied().unwrap_or(Mat4::IDENTITY) * *ib)
        .collect();
    let fallback = node_world.get(part_node).copied().unwrap_or(Mat4::IDENTITY);
    out.clear();
    out.reserve(part.base.len());
    for (vi, v) in part.base.iter().enumerate() {
        let j = part.joints[vi];
        let w = part.weights[vi];
        let wsum = w[0] + w[1] + w[2] + w[3];
        let m = if wsum > 1e-4 {
            let mut acc = Mat4::ZERO;
            for k in 0..4 {
                if w[k] > 0.0
                    && let Some(p) = palette.get(j[k] as usize)
                {
                    acc += *p * (w[k] / wsum);
                }
            }
            acc
        } else {
            fallback
        };
        let pos = m.transform_point3(Vec3::from(v.pos));
        let normal = (Mat3::from_mat4(m) * Vec3::from(v.normal)).normalize_or_zero();
        out.push(floptle_render::Vertex {
            pos: pos.to_array(),
            normal: normal.to_array(),
            uv: v.uv,
        });
    }
}

/// How a bound controller instance applies its pose.
pub enum AnimBinding {
    /// Poses a rigged Mesh's parts — render-side only (`AnimSystem::poses`).
    Rig,
    /// Drives scene node `Transform`s: an entity per skeleton node (`None` =
    /// no matching node). `covered` = nodes any clip animates (only those are
    /// written, so un-animated siblings stay script/physics-owned).
    Nodes { entities: Vec<Option<Entity>>, covered: Vec<usize> },
}

/// One live animated entity: its bound controller runtime + how to apply it.
pub struct AnimInstance {
    /// Controller asset key (`None` = embedded-clips fallback for a rigged
    /// Mesh without a controller component).
    pub asset: Option<String>,
    /// The Mesh asset path this was bound against (a runtime model swap must
    /// rebind so the pose targets the NEW skeleton).
    pub mesh_path: Option<String>,
    /// The `AnimSystem::revision` this was bound at (stale → rebind).
    pub revision: u64,
    pub ctl: Controller,
    pub binding: AnimBinding,
    /// Scratch: world matrices for the rig path.
    world: Vec<Mat4>,
}

/// Everything animation the editor owns. One field on `Editor`.
#[derive(Default)]
pub struct AnimSystem {
    /// `*.anim.ron` clip assets: (key, doc), sorted by key.
    pub clips: Vec<(String, AnimClipDoc)>,
    /// `*.actl.ron` controller assets: (key, doc), sorted by key.
    pub controllers: Vec<(String, AnimControllerDoc)>,
    /// Bumped on every save/rescan; instances rebind lazily when stale.
    pub revision: u64,
    /// Live runtimes per entity (play mode + Animating-tab preview).
    pub instances: HashMap<Entity, AnimInstance>,
    /// This frame's world-space part matrices for rigged Mesh nodes.
    pub poses: HashMap<Entity, Vec<Mat4>>,
    /// Transforms captured before a scene-binding preview wrote them (restored
    /// when the preview retargets/stops so editing never corrupts the scene).
    pub preview_restore: Vec<(Entity, Transform)>,
    /// Property-carrying components (UI/light/material) captured before a
    /// property-track preview overwrote them — the counterpart to
    /// `preview_restore` so scrubbing a cell/opacity lane is also non-destructive.
    pub preview_restore_props: Vec<(Entity, PropRestore)>,
    /// Pending warnings for the Console (e.g. a script playing an unknown state).
    pub warnings: Vec<String>,
    /// Warning keys already emitted this play session (one warning, not 60/s).
    warned: HashSet<String>,
}

impl AnimSystem {
    /// Re-scan `assets/` for animation clips + controllers.
    pub fn rescan(&mut self, project_root: &Path) {
        self.clips.clear();
        self.controllers.clear();
        let root = project_root.to_path_buf();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.starts_with('.') && name != "target" {
                        stack.push(p);
                    }
                    continue;
                }
                let Some(fname) = p.file_name().and_then(|s| s.to_str()) else { continue };
                if fname.ends_with(ANIM_CLIP_EXT) {
                    if let Ok(doc) = floptle_scene::load_anim_clip(&p) {
                        self.clips.push((asset_key(&p, &root, ANIM_CLIP_EXT), doc));
                    }
                } else if fname.ends_with(ANIM_CTL_EXT)
                    && let Ok(doc) = floptle_scene::load_anim_controller(&p) {
                        self.controllers.push((asset_key(&p, &root, ANIM_CTL_EXT), doc));
                    }
            }
        }
        self.clips.sort_by(|a, b| a.0.cmp(&b.0));
        self.controllers.sort_by(|a, b| a.0.cmp(&b.0));
        self.revision += 1;
    }

    /// Look up a clip by key, falling back to a unique file-stem match (so a
    /// moved clip file degrades gracefully instead of silently breaking).
    pub fn clip(&self, key: &str) -> Option<&AnimClipDoc> {
        self.resolve_clip_key(key).and_then(|k| {
            self.clips.iter().find(|(rk, _)| *rk == k).map(|(_, d)| d)
        })
    }

    /// The registry key `key` resolves to (exact, else unique stem match).
    pub fn resolve_clip_key(&self, key: &str) -> Option<String> {
        if self.clips.iter().any(|(k, _)| k == key) {
            return Some(key.to_string());
        }
        let stem = key.rsplit('/').next()?;
        let mut hits = self.clips.iter().filter(|(k, _)| k.rsplit('/').next() == Some(stem));
        let first = hits.next()?;
        if hits.next().is_some() {
            return None; // ambiguous — require the full key.
        }
        Some(first.0.clone())
    }

    pub fn controller(&self, key: &str) -> Option<&AnimControllerDoc> {
        if let Some((_, d)) = self.controllers.iter().find(|(k, _)| k == key) {
            return Some(d);
        }
        let stem = key.rsplit('/').next()?;
        let mut hits =
            self.controllers.iter().filter(|(k, _)| k.rsplit('/').next() == Some(stem));
        let first = hits.next()?;
        if hits.next().is_some() {
            return None;
        }
        Some(&first.1)
    }

    /// Refresh the in-memory clip registry entry + bump the revision (so bound
    /// animators rebind and previews reflect the edit) WITHOUT touching disk.
    /// Used for LIVE edits held under the pointer — a bone gizmo/inspector DRAG
    /// defers its disk save to pointer-up, but the preview must still update in
    /// real time as the bone moves; this is the cheap per-frame refresh that does it.
    pub fn register_clip(&mut self, key: &str, doc: &AnimClipDoc) {
        match self.clips.iter_mut().find(|(k, _)| k == key) {
            // Only bump the revision when the doc actually CHANGED. Otherwise a clip
            // stuck `dirty` (Record armed with nothing moving, a held drag, or a clip
            // whose file was deleted) would re-register identical content every frame,
            // bumping the revision each frame and forcing EVERY animator to fully rebind
            // each frame — the editor freeze. No change → no bump → no rebind storm.
            Some(slot) if slot.1 == *doc => {}
            Some(slot) => {
                slot.1 = doc.clone();
                self.revision += 1;
            }
            None => {
                self.clips.push((key.to_string(), doc.clone()));
                self.clips.sort_by(|a, b| a.0.cmp(&b.0));
                self.revision += 1;
            }
        }
    }

    /// Save a clip doc back to disk + refresh the registry entry in place.
    pub fn save_clip(&mut self, project_root: &Path, key: &str, doc: &AnimClipDoc) {
        let path = project_root.join(format!("{key}{ANIM_CLIP_EXT}"));
        if let Err(e) = floptle_scene::save_anim_clip(doc, &path) {
            eprintln!("  save clip {key} failed: {e}");
            return;
        }
        self.register_clip(key, doc);
    }

    /// Save a controller doc back to disk + refresh the registry entry.
    pub fn save_controller(&mut self, project_root: &Path, key: &str, doc: &AnimControllerDoc) {
        let path = project_root.join(format!("{key}{ANIM_CTL_EXT}"));
        if let Err(e) = floptle_scene::save_anim_controller(doc, &path) {
            eprintln!("  save controller {key} failed: {e}");
            return;
        }
        match self.controllers.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = doc.clone(),
            None => {
                self.controllers.push((key.to_string(), doc.clone()));
                self.controllers.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
        self.revision += 1;
    }

    /// Drop every live runtime (Play start/stop, scene load).
    pub fn clear_instances(&mut self) {
        self.instances.clear();
        self.poses.clear();
        self.warnings.clear();
        self.warned.clear();
        self.restore_preview_now();
    }

    /// Undo a scene-binding preview's transform + property writes.
    pub fn restore_preview(&mut self, world: &mut World) {
        for (e, tr) in self.preview_restore.drain(..) {
            if let Some(slot) = world.get_mut::<Transform>(e) {
                *slot = tr;
            }
        }
        for (e, snap) in std::mem::take(&mut self.preview_restore_props) {
            if let Some(el) = snap.element
                && let Some(slot) = world.get_mut::<floptle_ui::ElementSpec>(e)
            {
                *slot = el;
            }
            if let Some(m) = snap.matter
                && let Some(slot) = world.get_mut::<Matter>(e)
            {
                *slot = m;
            }
            if let Some(mat) = snap.material
                && let Some(slot) = world.get_mut::<floptle_core::Material>(e)
            {
                *slot = mat;
            }
        }
    }

    fn restore_preview_now(&mut self) {
        // (no world here — callers that can restore pass through
        // `restore_preview`; this just forgets, for teardown paths)
        self.preview_restore.clear();
        self.preview_restore_props.clear();
    }

    /// Drop the preview snapshots WITHOUT applying them. Used when ● Record
    /// stops: recording skips the per-frame restore (the world carries the
    /// previewed values), so the held snapshot is stale mid-record state —
    /// `stop_record_ui` restores the true pre-record scene instead.
    pub fn forget_preview(&mut self) {
        self.restore_preview_now();
    }
}

/// `path` → registry key: project-relative, forward slashes, extension off.
pub fn asset_key(path: &Path, project_root: &Path, ext: &str) -> String {
    let rel = path.strip_prefix(project_root).unwrap_or(path);
    let s = rel.to_string_lossy().replace('\\', "/");
    s.strip_suffix(ext).unwrap_or(&s).to_string()
}

// ---- doc ↔ runtime conversion ------------------------------------------------

/// Bind a clip doc to a skeleton: node names → indices (channel `""` = the
/// root/self node). Channels naming nodes the skeleton doesn't have are
/// dropped — that's the retarget contract.
pub fn clip_from_doc(doc: &AnimClipDoc, skeleton: &Skeleton) -> Clip {
    let mut channels = Vec::new();
    for ch in &doc.channels {
        let node = if ch.node.is_empty() {
            Some(0)
        } else {
            skeleton.index_of(&ch.node)
        };
        let Some(node) = node else { continue };
        channels.push(NodeChannels {
            node,
            translation: ch.translation.as_ref().map(track3_from_doc),
            rotation: ch.rotation.as_ref().map(track4_from_doc),
            scale: ch.scale.as_ref().map(track3_from_doc),
            properties: ch.properties.iter().map(prop_track_from_doc).collect(),
        });
    }
    channels.sort_by_key(|c| c.node);
    let mut events: Vec<floptle_anim::ClipEvent> = doc
        .events
        .iter()
        .map(|e| floptle_anim::ClipEvent { t: e.t, func: e.func.clone() })
        .collect();
    events.sort_by(|a, b| a.t.total_cmp(&b.t));
    Clip { name: doc.name.clone(), duration: doc.duration.max(1e-3), channels, events }
}

fn track3_from_doc(d: &AnimTrackDoc3) -> Track<Vec3> {
    Track {
        times: d.times.clone(),
        values: d.values.iter().map(|v| Vec3::from(*v)).collect(),
        interp: if d.step { Interp::Step } else { Interp::Linear },
    }
}

fn track4_from_doc(d: &AnimTrackDoc4) -> Track<Quat> {
    Track {
        times: d.times.clone(),
        values: d.values.iter().map(|v| Quat::from_array(*v).normalize()).collect(),
        interp: if d.step { Interp::Step } else { Interp::Linear },
    }
}

fn prop_value_from_doc(v: &AnimPropValueDoc) -> PropValue {
    match v {
        AnimPropValueDoc::Float(x) => PropValue::Float(*x),
        AnimPropValueDoc::Text(s) => PropValue::Text(s.clone()),
    }
}

fn prop_track_from_doc(d: &AnimPropTrackDoc) -> PropertyTrack {
    // String lanes never blend; force Step so a mis-set flag can't try to lerp.
    let is_text = d.values.iter().any(|v| matches!(v, AnimPropValueDoc::Text(_)));
    PropertyTrack {
        component: d.component.clone(),
        field: d.field.clone(),
        times: d.times.clone(),
        values: d.values.iter().map(prop_value_from_doc).collect(),
        interp: if d.step || is_text { Interp::Step } else { Interp::Linear },
    }
}

/// Bake a bound runtime clip back into a name-keyed doc (extraction).
pub fn bake_clip_doc(clip: &Clip, skeleton: &Skeleton, source_model: &str) -> AnimClipDoc {
    let channels = clip
        .channels
        .iter()
        .filter_map(|ch| {
            let node = skeleton.nodes.get(ch.node)?.name.clone();
            Some(AnimChannelDoc {
                node,
                translation: ch.translation.as_ref().map(track3_to_doc),
                rotation: ch.rotation.as_ref().map(track4_to_doc),
                scale: ch.scale.as_ref().map(track3_to_doc),
                properties: ch.properties.iter().map(prop_track_to_doc).collect(),
            })
        })
        .collect();
    AnimClipDoc {
        name: clip.name.clone(),
        duration: clip.duration,
        source_model: source_model.to_string(),
        channels,
        events: clip
            .events
            .iter()
            .map(|e| AnimEventDoc { t: e.t, func: e.func.clone() })
            .collect(),
    }
}

fn track3_to_doc(t: &Track<Vec3>) -> AnimTrackDoc3 {
    AnimTrackDoc3 {
        times: t.times.clone(),
        values: t.values.iter().map(|v| v.to_array()).collect(),
        step: t.interp == Interp::Step,
    }
}

fn track4_to_doc(t: &Track<Quat>) -> AnimTrackDoc4 {
    AnimTrackDoc4 {
        times: t.times.clone(),
        values: t.values.iter().map(|v| v.to_array()).collect(),
        step: t.interp == Interp::Step,
    }
}

fn prop_value_to_doc(v: &PropValue) -> AnimPropValueDoc {
    match v {
        PropValue::Float(x) => AnimPropValueDoc::Float(*x),
        PropValue::Text(s) => AnimPropValueDoc::Text(s.clone()),
    }
}

fn prop_track_to_doc(t: &PropertyTrack) -> AnimPropTrackDoc {
    AnimPropTrackDoc {
        component: t.component.clone(),
        field: t.field.clone(),
        times: t.times.clone(),
        values: t.values.iter().map(prop_value_to_doc).collect(),
        step: t.interp == Interp::Step,
    }
}

/// Extract every embedded clip of `model_path` into
/// `assets/animations/<ModelStem>/<Clip>.anim.ron`. Returns the written keys.
pub fn extract_clips(
    system: &mut AnimSystem,
    project_root: &Path,
    model_path: &str,
) -> Result<Vec<String>, String> {
    let rigged = floptle_assets::import_rigged(Path::new(model_path))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "model has no animations".to_string())?;
    let stem = Path::new(model_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "model".into());
    let rel_model = Path::new(model_path)
        .strip_prefix(project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| model_path.to_string());
    let mut written: Vec<String> = Vec::new();
    for clip in &rigged.clips {
        let mut doc = bake_clip_doc(clip, &rigged.skeleton, &rel_model);
        let safe: String = clip
            .name
            .chars()
            .map(|c| if c == '/' || c == '\\' || c == ':' { '_' } else { c })
            .collect();
        // Distinct clip names can still collide after sanitization ("A/B" and
        // "A:B" both become "A_B") — never silently clobber within one run.
        let mut key = format!("animations/{stem}/{safe}");
        let mut i = 2;
        while written.contains(&key) {
            key = format!("animations/{stem}/{safe}#{i}");
            i += 1;
        }
        // Re-extraction preserves hand-authored events (glTF clips carry none,
        // so anything on the existing file was added in the Animating tab).
        if let Some((_, existing)) = system.clips.iter().find(|(k, _)| *k == key)
            && !existing.events.is_empty() {
                doc.events = existing.events.clone();
            }
        system.save_clip(project_root, &key, &doc);
        written.push(key);
    }
    Ok(written)
}

// ---- binding an entity -------------------------------------------------------

/// The skeleton for a **scene-node** binding: node 0 = the entity itself, then
/// every descendant (depth-first). Rest pose = current local transforms.
fn scene_skeleton(world: &World, root: Entity) -> (Skeleton, Vec<Entity>) {
    let mut nodes = Vec::new();
    let mut ents = Vec::new();
    // children map (entity graph walk without a scene doc)
    let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
    for (e, p) in world.query::<floptle_core::Parent>() {
        children.entry(p.0).or_default().push(e);
    }
    fn local_trs(world: &World, e: Entity) -> TransformTRS {
        match world.get::<Transform>(e) {
            Some(t) => TransformTRS {
                t: t.translation.as_vec3(),
                r: t.rotation,
                s: t.scale,
            },
            None => TransformTRS::IDENTITY,
        }
    }
    fn walk(
        world: &World,
        children: &HashMap<Entity, Vec<Entity>>,
        e: Entity,
        parent: Option<usize>,
        nodes: &mut Vec<SkelNode>,
        ents: &mut Vec<Entity>,
    ) {
        let idx = nodes.len();
        let name = world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default();
        nodes.push(SkelNode { name, parent, rest: local_trs(world, e), pivot: Vec3::ZERO });
        ents.push(e);
        if let Some(kids) = children.get(&e) {
            for &k in kids {
                walk(world, children, k, Some(idx), nodes, ents);
            }
        }
    }
    walk(world, &children, root, None, &mut nodes, &mut ents);
    // Duplicate names bind to the FIRST occurrence — make later ones unique so
    // the name map stays deterministic.
    let mut seen = HashSet::new();
    for n in nodes.iter_mut() {
        if !seen.insert(n.name.clone()) {
            let mut i = 2;
            while !seen.insert(format!("{}#{i}", n.name)) {
                i += 1;
            }
            n.name = format!("{}#{i}", n.name);
        }
    }
    (Skeleton::new(nodes), ents)
}

/// Build layers from a controller doc, binding each state's clip to `skeleton`.
fn layers_from_doc(
    system: &AnimSystem,
    doc: &AnimControllerDoc,
    skeleton: &Skeleton,
) -> Vec<Layer> {
    let mut layers = Vec::new();
    for ld in &doc.layers {
        let mut states = Vec::new();
        for sd in &ld.states {
            let clip = match system.clip(&sd.clip) {
                Some(cd) => clip_from_doc(cd, skeleton),
                None => Clip { name: sd.name.clone(), duration: 1e-3, ..Default::default() },
            };
            let mut st = State::new(sd.name.clone(), clip);
            st.speed = sd.speed;
            st.looped = sd.looped;
            st.fade_in = sd.fade_in;
            st.fps = sd.fps;
            states.push(st);
        }
        let default_state =
            ld.default_state.as_ref().and_then(|n| states.iter().position(|s| &s.name == n));
        let mut layer = Layer::new(ld.name.clone(), states, default_state);
        layer.weight = ld.weight.clamp(0.0, 1.0);
        for t in &ld.transitions {
            let (Some(f), Some(to)) = (layer.state_index(&t.from), layer.state_index(&t.to))
            else {
                continue;
            };
            layer.fades.insert((f, to), t.fade.max(0.0));
        }
        layers.push(layer);
    }
    layers
}

/// Build (or rebuild) the animation instance for `e`, if it should have one:
/// a controller component, or a rigged Mesh (embedded-clips fallback).
pub fn bind_entity(
    system: &AnimSystem,
    world: &World,
    mesh_registry: &HashMap<String, MeshAsset>,
    e: Entity,
) -> Option<AnimInstance> {
    let ctl_key = world.get::<AnimController>(e).map(|c| c.asset.clone());
    let mesh_path = match world.get::<Matter>(e) {
        Some(Matter::Mesh { asset_path }) => Some(asset_path.clone()),
        _ => None,
    };
    let rig = mesh_path
        .as_ref()
        .and_then(|p| mesh_registry.get(p))
        .and_then(|m| m.rig.as_ref());
    match (ctl_key, rig) {
        // Controller on a rigged mesh: pose the rig.
        (Some(key), Some(rig)) => {
            let doc = system.controller(&key)?.clone();
            let mut ctl = Controller::new(
                rig.skeleton.rest_pose(),
                layers_from_doc(system, &doc, &rig.skeleton),
                doc.default_fade,
            );
            ctl.sample_fps = doc.sample_fps;
            Some(AnimInstance {
                asset: Some(key),
                mesh_path,
                revision: system.revision,
                ctl,
                binding: AnimBinding::Rig,
                world: Vec::new(),
            })
        }
        // Controller on anything else: animate the node + its descendants.
        (Some(key), None) => {
            let doc = system.controller(&key)?.clone();
            let (skeleton, ents) = scene_skeleton(world, e);
            let layers = layers_from_doc(system, &doc, &skeleton);
            let covered: Vec<usize> = {
                let mut set = HashSet::new();
                for l in &layers {
                    for s in &l.states {
                        set.extend(s.clip.covered_nodes());
                    }
                }
                let mut v: Vec<usize> = set.into_iter().collect();
                v.sort_unstable();
                v
            };
            let mut ctl = Controller::new(skeleton.rest_pose(), layers, doc.default_fade);
            ctl.sample_fps = doc.sample_fps;
            Some(AnimInstance {
                asset: Some(key),
                mesh_path,
                revision: system.revision,
                ctl,
                binding: AnimBinding::Nodes {
                    entities: ents.into_iter().map(Some).collect(),
                    covered,
                },
                world: Vec::new(),
            })
        }
        // No controller, but a rigged mesh: embedded clips, "Idle" (or the
        // first clip) as the default — models animate out of the box. Once a
        // clip has been EXTRACTED, the `.anim.ron` of the same name takes over
        // (so timeline edits + events apply without requiring a controller).
        (None, Some(rig)) => {
            if rig.clips.is_empty() {
                return None;
            }
            let states: Vec<State> = rig
                .clips
                .iter()
                .map(|c| {
                    let clip = match system.clip(&c.name) {
                        Some(doc) => clip_from_doc(doc, &rig.skeleton),
                        None => c.clone(),
                    };
                    State::new(c.name.clone(), clip)
                })
                .collect();
            let default = states
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case("idle"))
                .or(Some(0));
            let layer = Layer::new("Base".into(), states, default);
            let ctl = Controller::new(rig.skeleton.rest_pose(), vec![layer], 0.25);
            Some(AnimInstance {
                asset: None,
                mesh_path,
                revision: system.revision,
                ctl,
                binding: AnimBinding::Rig,
                world: Vec::new(),
            })
        }
        (None, None) => None,
    }
}

/// Which entities should have an animator right now.
fn animated_entities(world: &World, mesh_registry: &HashMap<String, MeshAsset>) -> Vec<Entity> {
    let mut out: Vec<Entity> = world.query::<AnimController>().map(|(e, _)| e).collect();
    for (e, m) in world.query::<Matter>() {
        if let Matter::Mesh { asset_path } = m
            && mesh_registry.get(asset_path).is_some_and(|a| a.rig.is_some())
                && !out.contains(&e)
            {
                out.push(e);
            }
    }
    out
}

/// True when `e`'s instance is missing or bound against stale data.
fn needs_bind(
    system: &AnimSystem,
    world: &World,
    mesh_registry: &HashMap<String, MeshAsset>,
    e: Entity,
) -> bool {
    let cur_key = world.get::<AnimController>(e).map(|c| c.asset.clone());
    let mesh_path = match world.get::<Matter>(e) {
        Some(Matter::Mesh { asset_path }) => Some(asset_path.clone()),
        _ => None,
    };
    match system.instances.get(&e) {
        None => true,
        Some(inst) => {
            if inst.revision != system.revision
                || inst.asset != cur_key
                || inst.mesh_path != mesh_path
            {
                return true;
            }
            // Rig-load race: a controller can bind the very first play frame,
            // BEFORE its rigged mesh finished importing — falling back to a
            // node-skeleton binding (a static bind-pose T-pose that never
            // animates, since the clips are keyed by BONE name). None of the
            // keys above change when the rig later lands in the registry, so
            // upgrade a Nodes binding to the real Rig binding the moment the
            // mesh's skeleton is available.
            if matches!(inst.binding, AnimBinding::Nodes { .. })
                && mesh_path
                    .as_ref()
                    .and_then(|p| mesh_registry.get(p))
                    .is_some_and(|a| a.rig.is_some())
            {
                return true;
            }
            false
        }
    }
}

/// Advance every animator by `dt` (Play mode), applying this frame's Lua
/// animator commands first. Binding happens BEFORE the commands so a command
/// issued in a script's `start()` on the very first play frame still lands.
/// Returns fired clip events as `(entity id, function name)`.
pub fn advance_animators(
    system: &mut AnimSystem,
    world: &mut World,
    mesh_registry: &HashMap<String, MeshAsset>,
    dt: f32,
    cmds: Vec<(u32, floptle_script::AnimCmd)>,
) -> Vec<(u32, String)> {
    let wanted = animated_entities(world, mesh_registry);
    // Drop stale instances (component removed / node deleted).
    system.instances.retain(|e, _| wanted.contains(e));
    system.poses.retain(|e, _| system.instances.contains_key(e));

    // Pass 1: (re)bind whatever is missing, stale, or re-keyed.
    for &e in &wanted {
        if needs_bind(system, world, mesh_registry, e) {
            match bind_entity(system, world, mesh_registry, e) {
                Some(inst) => {
                    system.instances.insert(e, inst);
                }
                None => {
                    system.instances.remove(&e);
                }
            }
        }
    }

    // Pass 2: this frame's Lua animator commands (intent lands this frame).
    apply_commands(system, world, cmds);

    // Pass 3: advance, collect events, apply poses.
    let mut fired = Vec::new();
    for e in wanted {
        let Some(inst) = system.instances.get_mut(&e) else {
            // A wanted entity with no instance never got bound — diagnose why.
            diagnose_anim(system, world, mesh_registry, e, DiagBind::Missing);
            continue;
        };
        let bind = match inst.binding {
            AnimBinding::Rig => DiagBind::Rig,
            AnimBinding::Nodes { .. } => DiagBind::Nodes,
        };
        inst.ctl.advance(dt);
        for func in inst.ctl.take_fired() {
            fired.push((e.index(), func));
        }
        apply_instance(system, world, mesh_registry, e);
        diagnose_anim(system, world, mesh_registry, e, bind);
    }
    fired
}

/// The binding a wanted entity resolved to, for `diagnose_anim` (a cheap
/// discriminant so we needn't clone the binding's node lists).
enum DiagBind {
    Missing,
    Rig,
    Nodes,
}

/// One-shot per-entity diagnostic for the astronaut T-pose class of bug: a mesh
/// that SHOULD animate but doesn't. Fires (once) to the Console when a wanted
/// animated entity ends up with no runtime instance, binds to Nodes despite its
/// mesh carrying a rig, or produces no pose after advancing — the three states
/// that render as a static bind pose. Costs nothing once each has warned.
fn diagnose_anim(
    system: &mut AnimSystem,
    world: &World,
    mesh_registry: &HashMap<String, MeshAsset>,
    e: Entity,
    binding: DiagBind,
) {
    let mesh_path = match world.get::<Matter>(e) {
        Some(Matter::Mesh { asset_path }) => Some(asset_path.clone()),
        _ => None,
    };
    let ctl_key = world.get::<AnimController>(e).map(|c| c.asset.clone());
    let has_rig = mesh_path
        .as_ref()
        .and_then(|p| mesh_registry.get(p))
        .is_some_and(|a| a.rig.is_some());
    let in_registry = mesh_path.as_ref().is_some_and(|p| mesh_registry.contains_key(p));
    let trouble = match binding {
        DiagBind::Missing => Some("no runtime animator instance was created"),
        DiagBind::Nodes if has_rig => {
            Some("bound to a node skeleton even though the mesh has a rig (bind-pose T-pose)")
        }
        DiagBind::Rig if !system.poses.contains_key(&e) => {
            Some("rig-bound but produced no pose this frame")
        }
        _ => None,
    };
    if let Some(why) = trouble {
        let key = format!("anim-diag:{}", e.index());
        if system.warned.insert(key) {
            let msg = format!(
                "animation: {} ({why}). controller={:?} mesh={:?} in_registry={in_registry} \
                 has_rig={has_rig}. If this is the on-foot character, that's the T-pose.",
                e.index(),
                ctl_key.as_deref().unwrap_or("<none>"),
                mesh_path.as_deref().unwrap_or("<none>"),
            );
            // Also to stdout so it's visible when launched from a terminal, not
            // only in the in-editor Console panel.
            eprintln!("[anim-diag] {msg}");
            system.warnings.push(msg);
        }
    }
}

/// Apply an instance's current pose (rig → poses map, nodes → Transforms).
pub fn apply_instance(
    system: &mut AnimSystem,
    world: &mut World,
    mesh_registry: &HashMap<String, MeshAsset>,
    e: Entity,
) {
    let Some(inst) = system.instances.get_mut(&e) else { return };
    match &inst.binding {
        AnimBinding::Rig => {
            let Some(Matter::Mesh { asset_path }) = world.get::<Matter>(e) else { return };
            let Some(rig) = mesh_registry.get(asset_path).and_then(|m| m.rig.as_ref()) else {
                return;
            };
            rig.skeleton.world_matrices(inst.ctl.pose(), &mut inst.world);
            // Same placement offset as rest_world so pose and rest agree.
            for m in inst.world.iter_mut() {
                *m = rig.offset * *m;
            }
            system.poses.insert(e, inst.world.clone());
        }
        AnimBinding::Nodes { entities, covered } => {
            let pose = inst.ctl.pose();
            for &n in covered {
                let Some(Some(ent)) = entities.get(n) else { continue };
                let Some(p) = pose.get(n) else { continue };
                if let Some(tr) = world.get_mut::<Transform>(*ent) {
                    tr.translation = p.t.as_dvec3();
                    tr.rotation = p.r;
                    tr.scale = p.s;
                }
            }
            // Generic property lanes (opacity, colors, image swaps…) applied
            // through the same live ECS setters Lua uses, so animation and
            // scripts poke fields identically. Cheap no-op for transform clips.
            for s in inst.ctl.sample_properties() {
                let Some(Some(ent)) = entities.get(s.node) else { continue };
                let ent = *ent;
                match s.value {
                    PropValue::Float(x) => {
                        floptle_script::apply_component_field(world, ent, &s.component, &s.field, x as f64)
                    }
                    PropValue::Text(t) => {
                        floptle_script::apply_component_field_str(world, ent, &s.component, &s.field, &t)
                    }
                }
            }
        }
    }
}

/// Make every `BoneAttach` node ride its target mesh's bone this frame. Writes the
/// child's LOCAL transform = `bone_local · offset` (both in the mesh's model space);
/// the ordinary [`floptle_core::world_transform`] parent-walk then re-applies the
/// mesh's f64 world, so the attachment follows the bone jitter-free far from the
/// origin and every consumer (render/physics/gizmo/particles) sees it through the one
/// choke point. Uses the current animated pose when there is one, else the rig's rest
/// pose (so it works at rest / with the anim tab closed). Cost = # of attachments.
///
/// MUST run AFTER animation AND physics (physics moves the mesh ROOT — the pose only
/// bends the bones), and before anything reads the attached node's world transform.
pub fn resolve_attachments(
    system: &AnimSystem,
    world: &mut World,
    mesh_registry: &HashMap<String, MeshAsset>,
) {
    // Collect the jobs first — can't hold the query borrow while mutating transforms.
    let jobs: Vec<(Entity, Entity, String, Transform)> = world
        .query::<BoneAttach>()
        .map(|(e, a)| (e, a.target, a.bone.clone(), a.offset))
        .collect();
    for (child, target, bone, offset) in jobs {
        let Some(Matter::Mesh { asset_path }) = world.get::<Matter>(target) else { continue };
        let Some(rig) = mesh_registry.get(asset_path).and_then(|m| m.rig.as_ref()) else { continue };
        let Some(idx) = rig.skeleton.index_of(&bone) else { continue }; // bone gone after re-import
        let bone_local = system
            .poses
            .get(&target)
            .and_then(|p| p.get(idx))
            .or_else(|| rig.rest_world.get(idx))
            .copied()
            .unwrap_or(Mat4::IDENTITY);
        // Model-space: bone matrix · bone-local offset. Decomposed to the child's LOCAL
        // transform; world_transform re-applies the mesh f64 world (Parent chain intact).
        let local = Transform::from_matrix(bone_local.as_dmat4() * offset.world_matrix());
        if let Some(t) = world.get_mut::<Transform>(child) {
            *t = local;
        }
    }
}

/// World matrix of `bone` on rigged mesh `mesh` — i.e. `mesh_world · bone_model_local`,
/// the exact frame `resolve_attachments` places a `BoneAttach` into (uses the current
/// animated pose, else the rig rest pose, matching that function's bone lookup). Invert
/// it to turn a desired child WORLD transform into a `BoneAttach.offset` (bone-local) so
/// the move gizmo edits the attachment instead of a `Transform` the resolve would clobber.
/// `None` if `mesh` isn't a rigged mesh or the bone name is gone (e.g. after a re-import).
pub fn bone_world_matrix(
    system: &AnimSystem,
    world: &World,
    mesh_registry: &HashMap<String, MeshAsset>,
    mesh: Entity,
    bone: &str,
) -> Option<DMat4> {
    let Some(Matter::Mesh { asset_path }) = world.get::<Matter>(mesh) else { return None };
    let rig = mesh_registry.get(asset_path).and_then(|m| m.rig.as_ref())?;
    let idx = rig.skeleton.index_of(bone)?;
    let bone_local = system
        .poses
        .get(&mesh)
        .and_then(|p| p.get(idx))
        .or_else(|| rig.rest_world.get(idx))
        .copied()
        .unwrap_or(Mat4::IDENTITY);
    Some(floptle_core::world_transform(world, mesh).world_matrix() * bone_local.as_dmat4())
}

/// Preview (edit-mode) apply for ONE entity at an explicit time: bind if
/// needed, seek the base layer, advance(0), apply. Scene bindings snapshot
/// the transforms they touch so the preview can be undone.
/// The property-carrying components a preview may overwrite, captured so the
/// authored scene returns after each preview frame. Cloned only for entities a
/// property-animating clip actually touches — see [`AnimSystem::preview_restore_props`].
#[derive(Default)]
pub struct PropRestore {
    pub element: Option<floptle_ui::ElementSpec>,
    pub matter: Option<Matter>,
    pub material: Option<floptle_core::Material>,
}

pub fn preview_pose(
    system: &mut AnimSystem,
    world: &mut World,
    mesh_registry: &HashMap<String, MeshAsset>,
    e: Entity,
    state: &str,
    t: f32,
) {
    if needs_bind(system, world, mesh_registry, e) {
        match bind_entity(system, world, mesh_registry, e) {
            Some(inst) => {
                system.instances.insert(e, inst);
            }
            None => return,
        }
    }
    // Snapshot transforms a scene binding will write (once per target).
    if system.preview_restore.is_empty()
        && let Some(AnimInstance { binding: AnimBinding::Nodes { entities, covered }, .. }) =
            system.instances.get(&e)
        {
            for &n in covered {
                if let Some(Some(ent)) = entities.get(n)
                    && let Some(tr) = world.get::<Transform>(*ent) {
                        system.preview_restore.push((*ent, *tr));
                    }
            }
        }
    // ...and the property-carrying components a property lane will overwrite, so
    // scrubbing a cell/opacity/color track is non-destructive like transforms.
    // Only for a controller that actually animates properties (skips the common
    // transform-only rig — no clones, no cost).
    if system.preview_restore_props.is_empty()
        && let Some(AnimInstance { binding: AnimBinding::Nodes { entities, .. }, ctl, .. }) =
            system.instances.get(&e)
        && ctl.has_properties()
    {
        for ent in entities.iter().flatten() {
            let snap = PropRestore {
                element: world.get::<floptle_ui::ElementSpec>(*ent).cloned(),
                matter: world.get::<Matter>(*ent).cloned(),
                material: world.get::<floptle_core::Material>(*ent).cloned(),
            };
            if snap.element.is_some() || snap.matter.is_some() || snap.material.is_some() {
                system.preview_restore_props.push((*ent, snap));
            }
        }
    }
    if let Some(inst) = system.instances.get_mut(&e)
        && let Some((li, si)) = inst.ctl.find_state(state, None) {
            // Snap straight to the state (no fade) and seek.
            inst.ctl.restart(li, si, Some(0.0));
            inst.ctl.seek(li, t);
            inst.ctl.advance(0.0);
            let _ = inst.ctl.take_fired(); // previews don't fire gameplay events
        }
    apply_instance(system, world, mesh_registry, e);
}

/// How far a replicated animator's clock may drift from the authority before
/// the local copy hard-seeks (seconds). Local advance covers everything under
/// this — corrections should be rare (a hitch, a missed packet burst).
const NET_ANIM_DRIFT: f32 = 0.25;

/// The animator-carrying entities under `root` (root first, then descendants),
/// in a DETERMINISTIC order — children by entity index, which matches across
/// peers because both machines spawn the same scene doc in node order. This is
/// how a networked node addresses its subtree's animators on the wire (the
/// `sub` index): the standard avatar is a Networked capsule with the rigged
/// Model as a CHILD, and the child carries the controller.
pub fn anim_subtree(
    world: &World,
    mesh_registry: &HashMap<String, MeshAsset>,
    root: Entity,
) -> Vec<Entity> {
    anim_subtree_in(&children_map(world), world, mesh_registry, root)
}

/// The world's parent→children map (built once, walked many times — the
/// per-tick gather walks every networked node's subtree).
fn children_map(world: &World) -> HashMap<Entity, Vec<Entity>> {
    let mut children: HashMap<Entity, Vec<Entity>> = HashMap::new();
    for (e, p) in world.query::<floptle_core::Parent>() {
        children.entry(p.0).or_default().push(e);
    }
    for kids in children.values_mut() {
        // Index order = scene-doc node order, identical on every peer.
        kids.sort_unstable_by_key(|e| e.index());
    }
    children
}

fn anim_subtree_in(
    children: &HashMap<Entity, Vec<Entity>>,
    world: &World,
    mesh_registry: &HashMap<String, MeshAsset>,
    root: Entity,
) -> Vec<Entity> {
    let is_animated = |e: Entity| {
        world.get::<AnimController>(e).is_some()
            || matches!(world.get::<Matter>(e), Some(Matter::Mesh { asset_path })
                if mesh_registry.get(asset_path).is_some_and(|a| a.rig.is_some()))
    };
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        if is_animated(e) {
            out.push(e);
        }
        if let Some(kids) = children.get(&e) {
            // Reversed so the lowest-index child pops first (stack order).
            stack.extend(kids.iter().rev().copied());
        }
    }
    out
}

/// Gather every networked animator's replicable state for the session's
/// snapshot diffing (`docs/netcode-design.md`): each `Replicated.animator`
/// node contributes its own animator AND its subtree's (addressed by the
/// deterministic `sub` walk index). Cheap: (state index, time, weight) per
/// layer, no poses, no strings.
pub fn collect_net_states(
    world: &World,
    mesh_registry: &HashMap<String, MeshAsset>,
    system: &AnimSystem,
) -> floptle_net::AnimStates {
    let mut out = Vec::new();
    let children = children_map(world);
    for (e, rep) in world.query::<floptle_core::Replicated>() {
        if !rep.animator {
            continue; // client-sided animator: each machine drives its own
        }
        for (sub, ent) in
            anim_subtree_in(&children, world, mesh_registry, e).into_iter().enumerate()
        {
            if sub > u8::MAX as usize {
                break; // wire addressing is u8 — a 256-animator avatar is a bug
            }
            let Some(inst) = system.instances.get(&ent) else { continue };
            let ns = inst.ctl.net_state();
            out.push((
                e,
                sub as u8,
                ns.speed,
                ns.layers
                    .iter()
                    .map(|l| floptle_net::AnimSrcLayer {
                        state: l.state,
                        t: l.t,
                        weight: l.weight,
                        dur: l.dur,
                        looped: l.looped,
                        rate: l.rate,
                    })
                    .collect(),
            ));
        }
    }
    out
}

/// Apply received animator entries onto the local runtimes (a client's remote
/// proxies): resolve each entry's `sub` index through the same deterministic
/// subtree walk the sender used, lazy-bind like `advance_animators`, apply.
/// Unresolvable subs / entities with no controller here (asset skew, removed
/// components) are skipped, never panic.
pub fn apply_net_states(
    system: &mut AnimSystem,
    world: &mut World,
    mesh_registry: &HashMap<String, MeshAsset>,
    updates: Vec<(Entity, floptle_net::AnimEntry)>,
) {
    for (root, en) in updates {
        let Some(e) =
            anim_subtree(world, mesh_registry, root).get(en.sub as usize).copied()
        else {
            continue;
        };
        if needs_bind(system, world, mesh_registry, e) {
            match bind_entity(system, world, mesh_registry, e) {
                Some(inst) => {
                    system.instances.insert(e, inst);
                }
                None => continue,
            }
        }
        let Some(inst) = system.instances.get_mut(&e) else { continue };
        let ns = floptle_anim::NetAnimState {
            speed: en.speed_f(),
            layers: en
                .layers
                .iter()
                .map(|l| floptle_anim::NetLayerState {
                    state: l.state_opt(),
                    t: l.t_secs(),
                    weight: l.weight_f(),
                    // Send-side prediction fields — the receive side reads its
                    // own clips instead.
                    dur: 0.0,
                    looped: false,
                    rate: 0.0,
                })
                .collect(),
        };
        inst.ctl.apply_net_state(&ns, NET_ANIM_DRIFT);
    }
}

/// Mirror each live animator's state to the script host (`anim:state()` etc.).
pub fn build_info(system: &AnimSystem) -> HashMap<u32, floptle_script::AnimInfo> {
    let mut out = HashMap::new();
    for (e, inst) in &system.instances {
        let layers = inst
            .ctl
            .layers
            .iter()
            .map(|l| {
                let (state, t, fin) = match l.current() {
                    Some((n, t, f)) => (Some(n.to_string()), t, f),
                    None => (None, 0.0, false),
                };
                (l.name.clone(), state, t, fin)
            })
            .collect();
        let states = inst
            .ctl
            .layers
            .iter()
            .flat_map(|l| l.states.iter().map(|s| s.name.clone()))
            .collect();
        out.insert(e.index(), floptle_script::AnimInfo { layers, states });
    }
    out
}

/// Apply this frame's queued Lua animator commands to the live runtimes.
/// Runs before `advance_animators`, so intent set this frame lands this frame.
pub fn apply_commands(
    system: &mut AnimSystem,
    world: &World,
    cmds: Vec<(u32, floptle_script::AnimCmd)>,
) {
    use floptle_script::AnimCmd;
    if cmds.is_empty() {
        return;
    }
    // entity index → live instance entity (commands arrive as raw indices).
    let by_index: HashMap<u32, Entity> =
        system.instances.keys().map(|e| (e.index(), *e)).collect();
    let mut warn: Vec<(String, String)> = Vec::new();
    for (eid, cmd) in cmds {
        let Some(&e) = by_index.get(&eid) else {
            // No instance yet (e.g. play() on the very first frame): it will
            // exist after this frame's advance; commands next frame will land.
            let _ = world;
            continue;
        };
        let Some(inst) = system.instances.get_mut(&e) else { continue };
        match cmd {
            AnimCmd::Play { state, layer, fade, restart } => {
                if let Some((li, si)) = inst.ctl.find_state(&state, layer.as_deref()) {
                    if restart {
                        inst.ctl.restart(li, si, fade);
                    } else {
                        inst.ctl.play(li, si, fade);
                    }
                } else {
                    // A typo'd / renamed state silently doing nothing is a
                    // brutal footgun ("Idle" vs an exported "Idle.001") — say
                    // so in the Console, once per name per play session.
                    let names: Vec<&str> = inst
                        .ctl
                        .layers
                        .iter()
                        .flat_map(|l| l.states.iter().map(|s| s.name.as_str()))
                        .collect();
                    warn.push((
                        format!("{eid}:{state}"),
                        format!(
                            "anim: no state \"{state}\" on this animator (states: {})",
                            names.join(", ")
                        ),
                    ));
                }
            }
            AnimCmd::Stop { layer, fade } => match layer {
                Some(l) => {
                    if let Some(li) = inst.ctl.layer_index(&l) {
                        inst.ctl.stop_layer(li, fade);
                    }
                }
                None => {
                    for li in 0..inst.ctl.layers.len() {
                        inst.ctl.stop_layer(li, fade);
                    }
                }
            },
            AnimCmd::SetSpeed(s) => inst.ctl.speed = s.max(0.0),
            AnimCmd::SetLayerWeight { layer, weight } => {
                if let Some(li) = inst.ctl.layer_index(&layer) {
                    inst.ctl.set_layer_weight(li, weight);
                }
            }
            AnimCmd::Seek { t, layer } => {
                let li = layer.as_deref().and_then(|l| inst.ctl.layer_index(l)).unwrap_or(0);
                inst.ctl.seek(li, t);
            }
        }
    }
    for (key, msg) in warn {
        if system.warned.insert(key) {
            system.warnings.push(msg);
        }
    }
}

/// Load a rigged model into a `RigAsset` + return its parts for registration.
/// Called from `import_model` when the static importer defers to the rig path.
pub fn rig_from_model(
    model: &floptle_assets::RiggedModel,
    overrides: &crate::rig_overrides::RigOverrides,
) -> RigAsset {
    // Apply any per-model re-parenting (the `.rig.ron` sidecar) up front: it
    // reorders + reindexes the skeleton, so every node reference we build below
    // (parts, skin joints, clip channels) is threaded through the old→new remap.
    let mut skeleton = model.skeleton.clone();
    let mut part_nodes: Vec<usize> = model.parts.iter().map(|p| p.node).collect();
    let remap = (!overrides.reparent.is_empty()).then(|| apply_reparent(&mut skeleton, &overrides.reparent));
    if let Some(rm) = &remap {
        for pn in part_nodes.iter_mut() {
            *pn = rm[*pn];
        }
    }

    // Rotation PIVOTS. Default each object node's pivot to its geometry centroid (in
    // node-local space) — far more useful than the model origin for a baked object —
    // then let the `.rig.ron` sidecar override per node by name. Bones (no geometry)
    // keep pivot ZERO (their origin already is the joint).
    let mut centroid_sum = vec![(Vec3::ZERO, 0usize); skeleton.nodes.len()];
    for (part, &node) in model.parts.iter().zip(&part_nodes) {
        if let Some(slot) = centroid_sum.get_mut(node) {
            for v in &part.mesh.vertices {
                slot.0 += Vec3::from(v.pos);
                slot.1 += 1;
            }
        }
    }
    for (i, node) in skeleton.nodes.iter_mut().enumerate() {
        if let Some(p) = overrides.pivot.get(&node.name) {
            node.pivot = Vec3::from(*p);
        } else if centroid_sum[i].1 > 0 {
            node.pivot = centroid_sum[i].0 / centroid_sum[i].1 as f32;
        }
    }

    let offset = Mat4::from_translation(-Vec3::from(model.center));
    let mut rest_world = Vec::new();
    skeleton.world_matrices(&skeleton.rest_pose(), &mut rest_world);
    for m in rest_world.iter_mut() {
        *m = offset * *m;
    }
    // Classify every skeleton node: a node that any render part lives on renders
    // geometry → it's an "object"; everything else (armature joints, empties) is a
    // "bone" of the rig. Both are keyframed identically — this only drives the UI's
    // Objects/Bones grouping.
    let mut node_is_object = vec![false; skeleton.nodes.len()];
    for &pn in &part_nodes {
        if let Some(flag) = node_is_object.get_mut(pn) {
            *flag = true;
        }
    }
    // Embedded clips bind channels by index — remap them onto the reordered skeleton.
    let mut clips = model.clips.clone();
    if let Some(rm) = &remap {
        for clip in clips.iter_mut() {
            for ch in clip.channels.iter_mut() {
                ch.node = rm[ch.node];
            }
            clip.channels.sort_by_key(|c| c.node);
        }
    }
    let skins = model
        .parts
        .iter()
        .map(|p| {
            p.skin.as_ref().map(|s| SkinnedPart {
                base: p.mesh.vertices.clone(),
                joints: s.joints.clone(),
                weights: s.weights.clone(),
                joint_nodes: match &remap {
                    Some(rm) => s.joint_nodes.iter().map(|&j| rm[j]).collect(),
                    None => s.joint_nodes.clone(),
                },
                inverse_bind: s.inverse_bind.clone(),
            })
        })
        .collect();
    RigAsset { skeleton, clips, part_nodes, rest_world, offset, node_is_object, skins }
}

/// Re-parent nodes in `skel` per the overrides (child name → new parent name;
/// empty/unknown parent = model root), keeping every node visually in place
/// (recomputing its local rest as `inv(new_parent_world) · own_world`), then
/// re-topo-sorting so parent index < child index (the [`Skeleton`] invariant).
/// Returns the old→new index remap so callers can fix every node reference
/// (parts, skin joints, clip channels). Cycles and self-parents are skipped.
fn apply_reparent(skel: &mut Skeleton, reparent: &BTreeMap<String, String>) -> Vec<usize> {
    let n = skel.nodes.len();
    let mut world = Vec::new();
    skel.world_matrices(&skel.rest_pose(), &mut world);

    let mut parent: Vec<Option<usize>> = skel.nodes.iter().map(|nn| nn.parent).collect();
    // Is `node` a descendant of `ancestor` under the (in-progress) parent table?
    let is_desc = |parent: &[Option<usize>], mut node: usize, ancestor: usize| -> bool {
        let mut guard = 0;
        while let Some(p) = parent[node] {
            if p == ancestor {
                return true;
            }
            node = p;
            guard += 1;
            if guard > n {
                return true; // treat an existing cycle as "would loop" — refuse
            }
        }
        false
    };
    for (child, par) in reparent {
        let Some(ci) = skel.index_of(child) else { continue };
        let pi = if par.is_empty() { None } else { skel.index_of(par) };
        if let Some(p) = pi
            && (p == ci || is_desc(&parent, p, ci))
        {
            continue; // no self-parent, no cycle
        }
        parent[ci] = pi;
    }
    // Keep-in-place: recompute the local rest of every node whose parent changed.
    for i in 0..n {
        if parent[i] != skel.nodes[i].parent {
            let pw = parent[i].map(|p| world[p]).unwrap_or(Mat4::IDENTITY);
            let local = pw.inverse() * world[i];
            skel.nodes[i].rest = TransformTRS::from_matrix(local);
        }
    }
    for (node, &p) in skel.nodes.iter_mut().zip(&parent) {
        node.parent = p;
    }
    // Topo-sort: emit a node once its parent is already emitted (parent-first).
    let mut new_idx = vec![0usize; n];
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut placed = vec![false; n];
    while order.len() < n {
        let mut progressed = false;
        for i in 0..n {
            if placed[i] {
                continue;
            }
            let ready = skel.nodes[i].parent.is_none_or(|p| placed[p]);
            if ready {
                new_idx[i] = order.len();
                order.push(i);
                placed[i] = true;
                progressed = true;
            }
        }
        if !progressed {
            // Defensive: a residual cycle — flush the rest in original order.
            for i in 0..n {
                if !placed[i] {
                    new_idx[i] = order.len();
                    order.push(i);
                    placed[i] = true;
                }
            }
            break;
        }
    }
    let mut slots: Vec<Option<SkelNode>> = std::mem::take(&mut skel.nodes).into_iter().map(Some).collect();
    let mut new_nodes: Vec<SkelNode> = Vec::with_capacity(n);
    for &old in &order {
        let mut sn = slots[old].take().unwrap();
        sn.parent = sn.parent.map(|p| new_idx[p]);
        new_nodes.push(sn);
    }
    *skel = Skeleton::new(new_nodes);
    new_idx
}

/// A fresh controller asset key in `dir_rel` (project-relative), defaulting to
/// `animation_controllers/`. Existing files get a numeric suffix.
pub fn new_controller_key(project_root: &Path, dir_rel: Option<&str>, name: &str) -> String {
    let rel = match dir_rel {
        Some(d) if !d.is_empty() => d.trim_matches('/').replace('\\', "/"),
        _ => "animation_controllers".to_string(),
    };
    let _ = std::fs::create_dir_all(project_root.join(&rel));
    let mut key = format!("{rel}/{name}");
    let mut i = 2;
    while project_root.join(format!("{key}{ANIM_CTL_EXT}")).exists() {
        key = format!("{rel}/{name}{i}");
        i += 1;
    }
    key
}

/// Default location for a fresh hand-authored clip.
pub fn new_clip_key(project_root: &Path, name: &str) -> String {
    let dir = project_root.join("animations");
    let _ = std::fs::create_dir_all(&dir);
    let mut key = format!("animations/{name}");
    let mut i = 2;
    while project_root.join(format!("{key}{ANIM_CLIP_EXT}")).exists() {
        key = format!("animations/{name}{i}");
        i += 1;
    }
    key
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The standard avatar shape — a Networked CAPSULE whose CHILD Model
    /// carries the Animation Controller — must be addressable for animator
    /// replication: the subtree walk finds the child (Ty's LAN test failed
    /// exactly here — the gather only looked at the Replicated node itself),
    /// deterministically, root-first when the root also animates.
    #[test]
    fn anim_subtree_finds_child_animators_deterministically() {
        let mut w = World::new();
        let registry: HashMap<String, MeshAsset> = HashMap::new();
        let capsule = w.spawn();
        w.insert(capsule, Transform::default());
        let model = w.spawn();
        w.insert(model, Transform::default());
        w.insert(model, floptle_core::Parent(capsule));
        w.insert(model, AnimController { asset: "animation_controllers/PlayerR6".into() });
        let sibling = w.spawn(); // un-animated child: not in the walk
        w.insert(sibling, Transform::default());
        w.insert(sibling, floptle_core::Parent(capsule));

        assert_eq!(anim_subtree(&w, &registry, capsule), vec![model], "child controller found");
        // Root carrying its own controller comes FIRST (sub 0), child after.
        w.insert(capsule, AnimController { asset: "animation_controllers/Root".into() });
        assert_eq!(anim_subtree(&w, &registry, capsule), vec![capsule, model]);
    }

    /// CPU skinning: at the bind pose the deform is the identity (no garble), and moving
    /// a bone translates the vertices weighted to it while others stay put — a two-joint
    /// blend interpolates. This is the math that makes a vertex-skinned mesh (Ty) animate.
    #[test]
    fn cpu_skin_bind_is_identity_and_bones_deform() {
        let v = |p: [f32; 3]| floptle_render::Vertex { pos: p, normal: [0.0, 1.0, 0.0], uv: [0.0, 0.0] };
        // Two joints: joint0 at origin, joint1 translated +Y by 1. inverse_bind = inverse
        // of each joint's bind-world matrix (the glTF invariant jointBind·invBind = I).
        let bind0 = Mat4::IDENTITY;
        let bind1 = Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0));
        let part = SkinnedPart {
            base: vec![v([0.0, 0.0, 0.0]), v([0.0, 1.0, 0.0]), v([0.0, 0.5, 0.0])],
            joints: vec![[0, 0, 0, 0], [1, 0, 0, 0], [0, 1, 0, 0]],
            weights: vec![[1.0, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0], [0.5, 0.5, 0.0, 0.0]],
            joint_nodes: vec![0, 1],
            inverse_bind: vec![bind0.inverse(), bind1.inverse()],
        };
        let mut out = Vec::new();
        // Bind pose (node_world == bind world): vertices unchanged.
        cpu_skin_part(&part, 0, &[bind0, bind1], &mut out);
        assert!((Vec3::from(out[0].pos) - Vec3::new(0.0, 0.0, 0.0)).length() < 1e-5);
        assert!((Vec3::from(out[1].pos) - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-5);
        assert!((Vec3::from(out[2].pos) - Vec3::new(0.0, 0.5, 0.0)).length() < 1e-5);
        // Animate joint1 by +X 2: its weighted vertex[1] moves +X 2; vertex[0] (joint0)
        // stays; vertex[2] (50/50) moves +X 1.
        let anim1 = Mat4::from_translation(Vec3::new(2.0, 1.0, 0.0));
        cpu_skin_part(&part, 0, &[bind0, anim1], &mut out);
        assert!((Vec3::from(out[0].pos) - Vec3::new(0.0, 0.0, 0.0)).length() < 1e-5);
        assert!((Vec3::from(out[1].pos) - Vec3::new(2.0, 1.0, 0.0)).length() < 1e-5);
        assert!((Vec3::from(out[2].pos) - Vec3::new(1.0, 0.5, 0.0)).length() < 1e-5);
        // A zero-weight vertex falls back to the part-node matrix (importer's pad).
        let part2 = SkinnedPart {
            base: vec![v([1.0, 0.0, 0.0])],
            joints: vec![[0, 0, 0, 0]],
            weights: vec![[0.0, 0.0, 0.0, 0.0]],
            joint_nodes: vec![0],
            inverse_bind: vec![Mat4::IDENTITY],
        };
        cpu_skin_part(&part2, 0, &[Mat4::from_translation(Vec3::new(0.0, 5.0, 0.0))], &mut out);
        assert!((Vec3::from(out[0].pos) - Vec3::new(1.0, 5.0, 0.0)).length() < 1e-5);
    }

    /// End-to-end on the real test model: extraction (bake to name-keyed docs)
    /// then rebinding against the same skeleton must reproduce the pose.
    #[test]
    fn bake_and_rebind_reproduces_the_pose() {
        let model = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/models/_test/UVMappedR6.glb");
        if !model.exists() {
            return; // repo asset — skip on a bare checkout
        }
        let rigged = floptle_assets::import_rigged(&model).expect("import").expect("has clips");
        assert!(!rigged.clips.is_empty());
        for clip in &rigged.clips {
            let doc = bake_clip_doc(clip, &rigged.skeleton, "models/_test/UVMappedR6.glb");
            assert_eq!(doc.channels.len(), clip.channels.len(), "{}", clip.name);
            let re = clip_from_doc(&doc, &rigged.skeleton);
            assert_eq!(re.channels.len(), clip.channels.len(), "{}", clip.name);
            let mut a = rigged.skeleton.rest_pose();
            let mut b = rigged.skeleton.rest_pose();
            let t = clip.duration * 0.37;
            clip.sample_into(t, &mut a);
            re.sample_into(t, &mut b);
            for (x, y) in a.iter().zip(b.iter()) {
                assert!((x.t - y.t).length() < 1e-4, "{} translation drift", clip.name);
                assert!(x.r.dot(y.r).abs() > 0.9999, "{} rotation drift", clip.name);
            }
        }
    }

    /// A controller on an Empty node animates its descendants by scene Name —
    /// the cutscene/scene-animation path.
    #[test]
    fn scene_binding_drives_descendant_transforms() {
        let mut world = World::new();
        let root = world.spawn();
        world.insert(root, Name("Director".to_string()));
        world.insert(root, Transform::IDENTITY);
        world.insert(root, Matter::Empty);
        world.insert(
            root,
            AnimController { asset: "animation_controllers/Cut".to_string() },
        );
        let door = world.spawn();
        world.insert(door, Name("Door".to_string()));
        world.insert(door, Transform::IDENTITY);
        world.insert(door, Matter::Empty);
        world.insert(door, floptle_core::Parent(root));

        let mut system = AnimSystem::default();
        system.clips.push((
            "animations/DoorOpen".to_string(),
            floptle_scene::AnimClipDoc {
                name: "DoorOpen".to_string(),
                duration: 1.0,
                source_model: String::new(),
                channels: vec![floptle_scene::AnimChannelDoc {
                    node: "Door".to_string(),
                    translation: Some(floptle_scene::AnimTrackDoc3 {
                        times: vec![0.0, 1.0],
                        values: vec![[0.0, 0.0, 0.0], [0.0, 4.0, 0.0]],
                        step: false,
                    }),
                    rotation: None,
                    scale: None,
                    properties: Vec::new(),
                }],
                events: vec![floptle_scene::AnimEventDoc {
                    t: 0.5,
                    func: "onHalfOpen".to_string(),
                }],
            },
        ));
        system.controllers.push((
            "animation_controllers/Cut".to_string(),
            floptle_scene::AnimControllerDoc {
                default_fade: 0.0,
                sample_fps: None,
                layers: vec![floptle_scene::AnimLayerDoc {
                    name: "Base".to_string(),
                    weight: 1.0,
                    states: vec![floptle_scene::AnimStateDoc {
                        name: "Open".to_string(),
                        clip: "animations/DoorOpen".to_string(),
                        speed: 1.0,
                        looped: false,
                        fade_in: None,
                        fps: None,
                        pos: [0.0, 0.0],
                    }],
                    default_state: Some("Open".to_string()),
                    transitions: Vec::new(),
                }],
            },
        ));

        let registry: HashMap<String, crate::MeshAsset> = HashMap::new();
        let fired = advance_animators(&mut system, &mut world, &registry, 0.5, Vec::new());
        assert_eq!(fired, vec![(root.index(), "onHalfOpen".to_string())]);
        let tr = world.get::<Transform>(door).unwrap();
        assert!(
            (tr.translation.y - 2.0).abs() < 1e-4,
            "door should be halfway up, got {}",
            tr.translation.y
        );
        // The half-open event fires exactly once.
        let fired = advance_animators(&mut system, &mut world, &registry, 0.1, Vec::new());
        assert!(fired.is_empty());
    }

    /// Regression: a command issued on the very first play frame (a script's
    /// start()) must land — binding happens before commands apply.
    #[test]
    fn first_frame_commands_are_not_dropped() {
        let mut world = World::new();
        let root = world.spawn();
        world.insert(root, Name("Director".to_string()));
        world.insert(root, Transform::IDENTITY);
        world.insert(root, Matter::Empty);
        world.insert(
            root,
            AnimController { asset: "animation_controllers/Cut".to_string() },
        );
        let mut system = AnimSystem::default();
        system.clips.push((
            "animations/A".to_string(),
            floptle_scene::AnimClipDoc {
                name: "A".to_string(),
                duration: 1.0,
                source_model: String::new(),
                channels: Vec::new(),
                events: Vec::new(),
            },
        ));
        system.controllers.push((
            "animation_controllers/Cut".to_string(),
            floptle_scene::AnimControllerDoc {
                default_fade: 0.0,
                sample_fps: None,
                layers: vec![floptle_scene::AnimLayerDoc {
                    name: "Base".to_string(),
                    weight: 1.0,
                    states: vec![
                        floptle_scene::AnimStateDoc {
                            name: "Idle".to_string(),
                            clip: "animations/A".to_string(),
                            speed: 1.0,
                            looped: true,
                            fade_in: None,
                            fps: None,
                            pos: [0.0, 0.0],
                        },
                        floptle_scene::AnimStateDoc {
                            name: "Run".to_string(),
                            clip: "animations/A".to_string(),
                            speed: 1.0,
                            looped: true,
                            fade_in: None,
                            fps: None,
                            pos: [0.0, 0.0],
                        },
                    ],
                    default_state: Some("Idle".to_string()),
                    transitions: Vec::new(),
                }],
            },
        ));
        let registry: HashMap<String, crate::MeshAsset> = HashMap::new();
        // Instances are empty (fresh Play) and a play("Run") arrives at once.
        let cmds = vec![(
            root.index(),
            floptle_script::AnimCmd::Play {
                state: "Run".to_string(),
                layer: None,
                fade: None,
                restart: false,
            },
        )];
        advance_animators(&mut system, &mut world, &registry, 0.016, cmds);
        let inst = system.instances.get(&root).expect("bound on frame 1");
        let (name, _, _) = inst.ctl.layers[0].current().expect("state playing");
        assert_eq!(name, "Run", "the first-frame play() command must land");
    }

    /// REGRESSION (Ty's astronaut T-pose): binding the solar demo's real
    /// character controller through the registry path (`layers_from_doc` →
    /// `AnimSystem::clip`) must yield NON-EMPTY clips that actually move the
    /// rig. The prior offline check fed `clip_from_doc` directly and so never
    /// exercised the registry key resolution — which is exactly where an
    /// unresolved clip degrades to a 1 ms empty clip and the rig sits at its
    /// bind pose (the T-pose). Skips cleanly if the solar assets aren't present.
    #[test]
    fn solar_astronaut_controller_binds_to_real_motion() {
        use std::path::Path;
        let solar = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../solar");
        let glb = solar.join("models/characters/character_retro.glb");
        let ctl_file = solar.join("animation_controllers/character.actl.ron");
        if !glb.exists() || !ctl_file.exists() {
            eprintln!("solar assets absent — skipping astronaut bind regression");
            return;
        }
        let model = floptle_assets::import_rigged(&glb)
            .expect("import_rigged ok")
            .expect("character_retro.glb carries a rig + clips");
        let rig = rig_from_model(&model, &crate::rig_overrides::RigOverrides::default());

        let mut sys = AnimSystem::default();
        sys.rescan(&solar);
        let doc = sys
            .controller("animation_controllers/character")
            .expect("character controller registered by the whole-project rescan")
            .clone();

        // Bind exactly as bind_entity's (controller, rig) arm does.
        let layers = layers_from_doc(&sys, &doc, &rig.skeleton);
        for st in &layers[0].states {
            // Empty (unresolved) clips fall back to a 1 ms no-channel clip.
            assert!(
                st.clip.duration > 0.02,
                "controller state bound an EMPTY clip — its key did not resolve in the \
                 registry; this is the astronaut T-pose"
            );
        }

        let mut ctl =
            floptle_anim::Controller::new(rig.skeleton.rest_pose(), layers, doc.default_fade);
        ctl.advance(0.30); // advance auto-starts the default ("idle") state
        let mut posed = Vec::new();
        rig.skeleton.world_matrices(ctl.pose(), &mut posed);
        let mut rest = Vec::new();
        rig.skeleton.world_matrices(&rig.skeleton.rest_pose(), &mut rest);
        let moved = posed
            .iter()
            .zip(&rest)
            .map(|(a, b)| (*a - *b).to_cols_array().iter().map(|v| v.abs()).sum::<f32>())
            .fold(0.0_f32, f32::max);
        assert!(
            moved > 1e-3,
            "the bound controller produced NO rig motion (bind pose = the T-pose); max delta {moved}"
        );
    }

    /// Re-parenting an object within a model (the `.rig.ron` override) must keep it
    /// visually in place and preserve the `Skeleton` topo-order (parent < child), so
    /// posing the new parent carries the child without teleporting it. Uses the Sae
    /// multi-object model; skips if the solar assets aren't present.
    #[test]
    fn reparent_keeps_object_in_place_and_topo_sorted() {
        use std::path::Path;
        let solar = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../solar");
        let glb = solar.join("models/Sae.glb");
        if !glb.exists() {
            eprintln!("Sae model absent — skipping reparent regression");
            return;
        }
        let model = floptle_assets::import_rigged(&glb)
            .expect("import")
            .expect("multi-object model kept its structure");

        let base = rig_from_model(&model, &crate::rig_overrides::RigOverrides::default());
        let fi = base.skeleton.index_of("Forearm").expect("Forearm object");
        let before = base.rest_world[fi].to_scale_rotation_translation().2;

        let mut ov = crate::rig_overrides::RigOverrides::default();
        ov.reparent.insert("Forearm".to_string(), "Soulder".to_string());
        ov.reparent.insert("Hand".to_string(), "Forearm".to_string()); // a chain, to exercise the re-sort
        let re = rig_from_model(&model, &ov);

        let si = re.skeleton.index_of("Soulder").expect("Soulder");
        let fi2 = re.skeleton.index_of("Forearm").expect("Forearm");
        let hi = re.skeleton.index_of("Hand").expect("Hand");
        assert_eq!(re.skeleton.nodes[fi2].parent, Some(si), "Forearm now under Soulder");
        assert_eq!(re.skeleton.nodes[hi].parent, Some(fi2), "Hand now under Forearm");
        assert!(si < fi2 && fi2 < hi, "parents must precede children after the re-sort");

        let after = re.rest_world[fi2].to_scale_rotation_translation().2;
        assert!(
            (before - after).length() < 1e-3,
            "re-parented Forearm must not move: {before} -> {after}"
        );
        // The part that renders "Forearm" must still point at the Forearm node.
        let forearm_part = re
            .part_nodes
            .iter()
            .find(|&&pn| re.skeleton.nodes[pn].name == "Forearm");
        assert_eq!(forearm_part, Some(&fi2), "part→node remap survived the re-sort");
    }
}
