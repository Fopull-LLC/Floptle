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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use floptle_anim::{Clip, Controller, Interp, Layer, NodeChannels, SkelNode, Skeleton, State, Track, TransformTRS};
use floptle_core::math::{Mat3, Mat4, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{AnimController, BoneAttach, Entity, Matter, Name, World};
use floptle_scene::{
    AnimChannelDoc, AnimClipDoc, AnimControllerDoc, AnimEventDoc, AnimTrackDoc3, AnimTrackDoc4,
    ANIM_CLIP_EXT, ANIM_CTL_EXT,
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

    /// Save a clip doc back to disk + refresh the registry entry in place.
    pub fn save_clip(&mut self, project_root: &Path, key: &str, doc: &AnimClipDoc) {
        let path = project_root.join(format!("{key}{ANIM_CLIP_EXT}"));
        if let Err(e) = floptle_scene::save_anim_clip(doc, &path) {
            eprintln!("  save clip {key} failed: {e}");
            return;
        }
        match self.clips.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = doc.clone(),
            None => {
                self.clips.push((key.to_string(), doc.clone()));
                self.clips.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
        self.revision += 1;
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

    /// Undo a scene-binding preview's transform writes.
    pub fn restore_preview(&mut self, world: &mut World) {
        for (e, tr) in self.preview_restore.drain(..) {
            if let Some(slot) = world.get_mut::<Transform>(e) {
                *slot = tr;
            }
        }
    }

    fn restore_preview_now(&mut self) {
        // (no world here — callers that can restore pass through
        // `restore_preview`; this just forgets, for teardown paths)
        self.preview_restore.clear();
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
        nodes.push(SkelNode { name, parent, rest: local_trs(world, e) });
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
            inst.revision != system.revision
                || inst.asset != cur_key
                || inst.mesh_path != mesh_path
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
        if needs_bind(system, world, e) {
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
        let Some(inst) = system.instances.get_mut(&e) else { continue };
        inst.ctl.advance(dt);
        for func in inst.ctl.take_fired() {
            fired.push((e.index(), func));
        }
        apply_instance(system, world, mesh_registry, e);
    }
    fired
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

/// Preview (edit-mode) apply for ONE entity at an explicit time: bind if
/// needed, seek the base layer, advance(0), apply. Scene bindings snapshot
/// the transforms they touch so the preview can be undone.
pub fn preview_pose(
    system: &mut AnimSystem,
    world: &mut World,
    mesh_registry: &HashMap<String, MeshAsset>,
    e: Entity,
    state: &str,
    t: f32,
) {
    if needs_bind(system, world, e) {
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
pub fn rig_from_model(model: &floptle_assets::RiggedModel) -> RigAsset {
    let offset = Mat4::from_translation(-Vec3::from(model.center));
    let mut rest_world = Vec::new();
    model.skeleton.world_matrices(&model.skeleton.rest_pose(), &mut rest_world);
    for m in rest_world.iter_mut() {
        *m = offset * *m;
    }
    RigAsset {
        skeleton: model.skeleton.clone(),
        clips: model.clips.clone(),
        part_nodes: model.parts.iter().map(|p| p.node).collect(),
        rest_world,
        offset,
        skins: model
            .parts
            .iter()
            .map(|p| {
                p.skin.as_ref().map(|s| SkinnedPart {
                    base: p.mesh.vertices.clone(),
                    joints: s.joints.clone(),
                    weights: s.weights.clone(),
                    joint_nodes: s.joint_nodes.clone(),
                    inverse_bind: s.inverse_bind.clone(),
                })
            })
            .collect(),
    }
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
}
