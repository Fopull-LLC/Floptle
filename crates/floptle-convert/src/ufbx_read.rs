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
//! Handled by baking it into the mesh's vertices.
//!
//! **Winding.** Any of the above can involve a mirror, and mirroring inverts
//! triangle winding. A model with every face backwards is invisible from
//! outside and solid from within — which reads as "the converter lost my
//! model", not as a winding problem.
//!
//! # Flat for a prop, a tree for anything that moves
//!
//! A model with no skeleton and no animation has its node transforms **baked
//! into vertex positions**: a flat list of world-space meshes, which sidesteps
//! every FBX transform subtlety at once — pre/post-rotation, rotation orders,
//! inherit modes. Parts stay *separate and named*, one mesh per node per
//! material, so nothing a person would want to select is merged away.
//!
//! A model with a skin or with animation keeps its **node tree**, because that
//! is what moves. Every node becomes a glTF node at its local transform; a
//! skinned mesh is written in its bind pose with each bone's inverse bind
//! matrix; a rigid part hangs from the node that carries it; and every
//! animation stack becomes one clip, resampled by ufbx into plain
//! translation, rotation and scale keys so FBX's rotation orders and pivots
//! arrive already resolved.
//!
//! # Materials as the artist set them
//!
//! ufbx reads both families of FBX material — the PBR shaders (Stingray,
//! Arnold standard surface, 3ds Max physical, glTF) and the classic
//! Phong/Lambert ones — into one description, and also OBJ's `.mtl`. Each map
//! goes to the glTF slot it means; see [`material`] for the conversions a slot
//! needs on the way.

use floptle_assets::glb_write::{AlphaMode, AnimTarget, WriteAnimation, WriteChannel, WriteMaterial, WriteSkin};
use floptle_render::TextureData;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use crate::common::{Scene, SceneNode, SubMesh};
use crate::textures::{self, Channel, Finder};
use crate::{ConvertError, MapSlot, MapUse, MissingMap, Report};

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
        // FBX can tell a node not to inherit its parent's scale; glTF always
        // does. ufbx inserts a helper node where that matters, so a kept tree
        // composes the way glTF composes it.
        inherit_mode_handling: ufbx::InheritModeHandling::HelperNodes,
        // A file with a face that has no vertices is damaged, not unreadable.
        // Better to convert the other 20,000 faces and say so.
        allow_empty_faces: true,
        generate_missing_normals: true,
        // **An OBJ's materials are in another file**, and ufbx reads it only
        // when asked to. Without this every OBJ converted grey, with not one
        // of its `.mtl` maps. A `.mtl` that is named but missing is not a
        // reason to refuse the geometry; one beside the OBJ under its own
        // name is found even when `mtllib` names another.
        load_external_files: true,
        ignore_missing_external_files: true,
        obj_search_mtl_by_filename: true,
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
    let mut mats = Materials::new(src);

    // Which nodes the kept tree needs, if the model keeps one at all.
    let clips = animated_stacks(&scene);
    let skinned_any = scene.nodes.iter().any(|n| n.mesh.as_ref().is_some_and(|m| skin_of(m).is_some()));
    let keep_tree = skinned_any || !clips.is_empty();
    let mut node_index: HashMap<u32, usize> = HashMap::new();
    if keep_tree {
        build_tree(&scene, unit, &mut out, &mut node_index);
    }

    for node in scene.nodes.iter() {
        let Some(mesh) = node.mesh.as_ref() else { continue };
        if node.is_root {
            continue;
        }

        let skin = if keep_tree { skin_of(mesh) } else { None };
        // Where the vertices are written, and the matrix that puts them there:
        // world space for a flat prop and for a skinned mesh (in its bind
        // pose), the node's own space for a rigid part of a kept tree.
        let bind_world = skin
            .and_then(|s| s.clusters.iter().next().map(|c| c.geometry_to_world))
            .unwrap_or(node.geometry_to_world);
        let (place, world) = match (skin, keep_tree) {
            (Some(_), _) => (bind_world, bind_world),
            (None, true) => (node.geometry_to_node, node.geometry_to_world),
            (None, false) => (node.geometry_to_world, node.geometry_to_world),
        };
        let xform = scaled(&place, unit);
        let normals = normal_matrix(&xform);
        // A mirroring transform reverses winding. Detected from the transform
        // itself rather than trusted from a flag, because this has to stay
        // right whatever ufbx did upstream to get here. The world transform
        // decides even for a part written in its node's space: the engine
        // draws it through that node and does not turn it round itself.
        let flipped = ufbx::matrix_determinant(&world) < 0.0;

        let name = if node.element.name.is_empty() {
            "mesh".to_string()
        } else {
            node.element.name.to_string()
        };

        let rig = skin.map(|s| bind_skin(s, node, &xform, unit, &node_index, &mut out));
        let parent = if keep_tree && skin.is_none() { node_index.get(&node.element.typed_id).copied() } else { None };

        // **One SubMesh per material.** A glTF primitive has exactly one
        // material; an FBX mesh can have many, and merging them makes a model
        // that is one colour where it should be several.
        let parts: Vec<Option<&ufbx::MeshPart>> = if mesh.material_parts.is_empty() {
            vec![None]
        } else {
            mesh.material_parts.iter().map(Some).collect()
        };
        for part in parts {
            if part.is_some_and(|p| p.num_triangles == 0) {
                continue;
            }
            let material = part.and_then(|p| mesh.materials.get(p.index as usize)).map(|r| &**r);
            let part_name = match material {
                Some(m) if !m.element.name.is_empty() && parts_len(mesh) > 1 => {
                    format!("{name}_{}", m.element.name)
                }
                _ => name.clone(),
            };
            let look = material.map(|m| mats.get(m, &mut out));
            let geo = Geometry { mesh, part, xform: &xform, normals, flipped, rig: rig.as_ref(), uv: look.as_ref().and_then(|l| l.uv) };
            if let Some(mut sm) = build_part(&geo, &part_name) {
                if let Some(l) = look {
                    sm.base_color = l.material.base_color;
                    sm.material = Some(l.material);
                }
                sm.parent = parent;
                sm.skin = rig.as_ref().map(|r| r.skin);
                out.meshes.push(sm);
            }
        }
    }

    if keep_tree {
        write_clips(&scene, &clips, unit, &node_index, &mut out);
    }
    note_dropped(&scene, keep_tree, clips.len(), &mut out.report);
    out.report.materials = scene.materials.len();
    out.report.maps = std::mem::take(&mut mats.maps);
    out.report.missing = std::mem::take(&mut mats.missing);
    out.report.warnings.append(&mut mats.warnings);
    Ok(out)
}

/// A mesh's first skin, when it has one with any bones in it.
fn skin_of(mesh: &ufbx::Mesh) -> Option<&ufbx::SkinDeformer> {
    mesh.skin_deformers.iter().next().map(|s| &**s).filter(|s| s.clusters.iter().any(|c| c.bone_node.is_some()))
}

/// The animation stacks that move something worth moving: a mesh, a bone, or
/// a node either of them hangs from. A stack that only flies a camera has
/// nothing to say to a model and is reported as dropped instead.
fn animated_stacks(scene: &ufbx::Scene) -> Vec<(String, ufbx::BakedAnimRoot)> {
    let mut relevant: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for node in scene.nodes.iter() {
        let carries = node.mesh.is_some() || node.bone.is_some();
        if !carries {
            continue;
        }
        let mut cur: Option<&ufbx::Node> = Some(node);
        while let Some(n) = cur {
            if !relevant.insert(n.element.element_id) {
                break;
            }
            cur = n.parent.as_deref();
        }
    }
    for c in scene.skin_clusters.iter() {
        let mut cur = c.bone_node.as_deref();
        while let Some(n) = cur {
            if !relevant.insert(n.element.element_id) {
                break;
            }
            cur = n.parent.as_deref();
        }
    }
    let mut out = Vec::new();
    for stack in scene.anim_stacks.iter() {
        let opts = ufbx::BakeOpts { trim_start_time: true, ..Default::default() };
        let Ok(baked) = ufbx::bake_anim(scene, &stack.anim, opts) else { continue };
        let moves = baked.nodes.iter().any(|b| {
            let moving = !(b.constant_translation && b.constant_rotation && b.constant_scale);
            moving && relevant.contains(&b.element_id)
        });
        if moves {
            let name = if stack.element.name.is_empty() { format!("Clip{}", out.len() + 1) } else { stack.element.name.to_string() };
            out.push((name, baked));
        }
    }
    out
}

/// `m` as glTF TRS, translation into metres.
fn trs(m: &ufbx::Matrix, unit: f64) -> ([f32; 3], [f32; 4], [f32; 3]) {
    let t = ufbx::matrix_to_transform(m);
    (
        [(t.translation.x * unit) as f32, (t.translation.y * unit) as f32, (t.translation.z * unit) as f32],
        quat(t.rotation),
        [t.scale.x as f32, t.scale.y as f32, t.scale.z as f32],
    )
}

fn quat(q: ufbx::Quat) -> [f32; 4] {
    let l = (q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w).sqrt();
    if l < 1e-12 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    [(q.x / l) as f32, (q.y / l) as f32, (q.z / l) as f32, (q.w / l) as f32]
}

/// Every node of the file as a glTF node at its local transform, parents
/// before children. The file's own root is kept as the top node, so whatever
/// transform it carries still applies.
fn build_tree(scene: &ufbx::Scene, unit: f64, out: &mut Scene, index: &mut HashMap<u32, usize>) {
    let mut order: Vec<&ufbx::Node> = scene.nodes.iter().map(|n| &**n).collect();
    order.sort_by_key(|n| n.node_depth);
    for node in order {
        let parent = node.parent.as_ref().and_then(|p| index.get(&p.element.typed_id).copied());
        let local = if node.is_root { node.node_to_world } else { node.node_to_parent };
        let (translation, rotation, scale) = trs(&local, unit);
        let name = if node.is_root {
            "Root".to_string()
        } else if node.element.name.is_empty() {
            format!("Node{}", node.element.typed_id)
        } else {
            node.element.name.to_string()
        };
        index.insert(node.element.typed_id, out.nodes.len());
        out.nodes.push(SceneNode { name, parent, translation, rotation, scale });
    }
}

/// A skinned mesh's binding: which output skin it uses and, per mesh vertex,
/// its four strongest joints and their weights.
struct Rig {
    skin: usize,
    joints: Vec<[u16; 4]>,
    weights: Vec<[f32; 4]>,
}

/// Write `skin` as a glTF skin over the kept tree and work out each vertex's
/// joints. `place` is the matrix the mesh's vertices are written through (its
/// bind pose in world space, in metres).
///
/// Each joint's inverse bind matrix is the bone's `geometry_to_bone` taken
/// back through `place`, so a vertex written at its bind position and moved by
/// the bone lands where ufbx itself would skin it. A vertex no bone holds
/// follows the mesh's own node, through one extra joint, rather than being
/// pinned to the first bone.
fn bind_skin(
    skin: &ufbx::SkinDeformer,
    node: &ufbx::Node,
    place: &ufbx::Matrix,
    unit: f64,
    index: &HashMap<u32, usize>,
    out: &mut Scene,
) -> Rig {
    let back = ufbx::matrix_invert(place);
    let mut joints: Vec<usize> = Vec::new();
    let mut inverse_bind: Vec<[f32; 16]> = Vec::new();
    // Cluster index → joint slot.
    let mut slot: Vec<Option<u16>> = Vec::with_capacity(skin.clusters.len());
    for c in skin.clusters.iter() {
        let bone = c.bone_node.as_ref().and_then(|b| index.get(&b.element.typed_id).copied());
        match bone {
            Some(b) => {
                slot.push(Some(joints.len() as u16));
                joints.push(b);
                inverse_bind.push(cols(&ufbx::matrix_mul(&scaled(&c.geometry_to_bone, unit), &back)));
            }
            None => slot.push(None),
        }
    }
    let mut self_slot: Option<u16> = None;
    let mut vj = Vec::with_capacity(skin.vertices.len());
    let mut vw = Vec::with_capacity(skin.vertices.len());
    for v in skin.vertices.iter() {
        let mut w: Vec<(u16, f32)> = Vec::new();
        for k in v.weight_begin..v.weight_begin + v.num_weights {
            let Some(sw) = skin.weights.get(k as usize) else { continue };
            if let Some(Some(s)) = slot.get(sw.cluster_index as usize)
                && sw.weight > 0.0
            {
                w.push((*s, sw.weight as f32));
            }
        }
        w.sort_by(|a, b| b.1.total_cmp(&a.1));
        w.truncate(4);
        let total: f32 = w.iter().map(|x| x.1).sum();
        if total <= 1e-6 {
            let s = *self_slot.get_or_insert_with(|| {
                let s = joints.len() as u16;
                joints.push(index.get(&node.element.typed_id).copied().unwrap_or(0));
                inverse_bind.push(cols(&ufbx::matrix_mul(&scaled(&node.geometry_to_node, unit), &back)));
                s
            });
            w = vec![(s, 1.0)];
        }
        let total: f32 = w.iter().map(|x| x.1).sum();
        let (mut j, mut ws) = ([0u16; 4], [0f32; 4]);
        for (i, (s, x)) in w.iter().enumerate() {
            j[i] = *s;
            ws[i] = x / total;
        }
        vj.push(j);
        vw.push(ws);
    }
    if !matches!(skin.skinning_method, ufbx::SkinningMethod::Linear | ufbx::SkinningMethod::Rigid) {
        out.report.warnings.push(
            "A skin asks for dual-quaternion blending; it is converted to the linear blending glTF has, \
             which can pinch at twisted joints."
                .into(),
        );
    }
    out.skins.push(WriteSkin { joints, inverse_bind });
    Rig { skin: out.skins.len() - 1, joints: vj, weights: vw }
}

/// A ufbx matrix as glTF's column-major sixteen.
fn cols(m: &ufbx::Matrix) -> [f32; 16] {
    [
        m.m00 as f32, m.m10 as f32, m.m20 as f32, 0.0,
        m.m01 as f32, m.m11 as f32, m.m21 as f32, 0.0,
        m.m02 as f32, m.m12 as f32, m.m22 as f32, 0.0,
        m.m03 as f32, m.m13 as f32, m.m23 as f32, 1.0,
    ]
}

/// Every kept animation stack as a clip. A node's property is written when it
/// moves, or when it holds still somewhere other than its rest pose, which a
/// clip that leaves it out would put back.
fn write_clips(
    scene: &ufbx::Scene,
    clips: &[(String, ufbx::BakedAnimRoot)],
    unit: f64,
    index: &HashMap<u32, usize>,
    out: &mut Scene,
) {
    let by_element: HashMap<u32, &ufbx::Node> = scene.nodes.iter().map(|n| (n.element.element_id, &**n)).collect();
    for (name, baked) in clips {
        let mut channels = Vec::new();
        for b in baked.nodes.iter() {
            let Some(node) = by_element.get(&b.element_id) else { continue };
            let Some(&ni) = index.get(&node.element.typed_id) else { continue };
            let rest = &out.nodes[ni];
            let t: Vec<[f32; 4]> = b
                .translation_keys
                .iter()
                .map(|k| [(k.value.x * unit) as f32, (k.value.y * unit) as f32, (k.value.z * unit) as f32, 0.0])
                .collect();
            let r: Vec<[f32; 4]> = b.rotation_keys.iter().map(|k| quat(k.value)).collect();
            let s: Vec<[f32; 4]> =
                b.scale_keys.iter().map(|k| [k.value.x as f32, k.value.y as f32, k.value.z as f32, 0.0]).collect();
            let differs = |v: &[[f32; 4]], rest: &[f32]| {
                v.iter().any(|k| k.iter().zip(rest).any(|(a, b)| (a - b).abs() > 1e-5))
            };
            let mut rest_r = rest.rotation;
            // q and -q are the same rotation.
            if let Some(first) = r.first()
                && first.iter().zip(rest_r).map(|(a, b)| a * b).sum::<f32>() < 0.0
            {
                rest_r = rest_r.map(|c| -c);
            }
            let times = |keys: &mut dyn Iterator<Item = f64>| keys.map(|t| t as f32).collect::<Vec<f32>>();
            if !b.constant_translation || differs(&t, &rest.translation) {
                channels.push(WriteChannel {
                    node: ni,
                    target: AnimTarget::Translation,
                    times: times(&mut b.translation_keys.iter().map(|k| k.time)),
                    values: t,
                    step: false,
                });
            }
            if !b.constant_rotation || differs(&r, &rest_r) {
                channels.push(WriteChannel {
                    node: ni,
                    target: AnimTarget::Rotation,
                    times: times(&mut b.rotation_keys.iter().map(|k| k.time)),
                    values: r,
                    step: false,
                });
            }
            if !b.constant_scale || differs(&s, &rest.scale) {
                channels.push(WriteChannel {
                    node: ni,
                    target: AnimTarget::Scale,
                    times: times(&mut b.scale_keys.iter().map(|k| k.time)),
                    values: s,
                    step: false,
                });
            }
        }
        out.report.animations.push(name.clone());
        out.animations.push(WriteAnimation { name: name.clone(), channels });
    }
}

/// One material, converted, and the UV transform its base-colour map asks for
/// (baked into the mesh's coordinates, which every reader honours).
#[derive(Clone)]
struct Look {
    material: WriteMaterial,
    uv: Option<ufbx::Matrix>,
}

/// The materials of one file, each converted once, with the pictures they
/// use and what the report says about them.
struct Materials {
    finder: Finder,
    /// Material element id → its conversion.
    done: HashMap<u32, Look>,
    /// Texture element id → its decoded picture and the name to report it by.
    pictures: HashMap<u32, Option<(Rc<TextureData>, String)>>,
    /// Output texture key → its index in the scene's textures.
    written: HashMap<String, usize>,
    maps: Vec<MapUse>,
    missing: Vec<MissingMap>,
    warnings: Vec<String>,
}

impl Materials {
    fn new(src: &Path) -> Self {
        Self {
            finder: Finder::new(src),
            done: HashMap::new(),
            pictures: HashMap::new(),
            written: HashMap::new(),
            maps: Vec::new(),
            missing: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn get(&mut self, m: &ufbx::Material, out: &mut Scene) -> Look {
        if let Some(l) = self.done.get(&m.element.element_id) {
            return l.clone();
        }
        let l = material(m, self, out);
        self.done.insert(m.element.element_id, l.clone());
        l
    }

    /// The picture `t` shows: embedded in the file, or found on disk. Reported
    /// as missing, by name and with the material and slot that wanted it, when
    /// it is neither.
    fn picture(&mut self, t: &ufbx::Texture, material: &str, slot: MapSlot) -> Option<(Rc<TextureData>, String)> {
        // A layered texture is a stack of pictures blended in the source
        // application; its bottom layer is the one that is the material.
        let t = if t.type_ == ufbx::TextureType::Layered { t.file_textures.iter().next().map(|r| &**r).unwrap_or(t) } else { t };
        if let Some(hit) = self.pictures.get(&t.element.element_id) {
            if hit.is_none() && !self.missing.iter().any(|m| m.material == material && m.slot == slot) {
                self.missing.push(MissingMap { material: material.to_string(), slot, name: tex_name(t) });
            }
            return hit.clone();
        }
        let name = tex_name(t);
        let embedded: Option<Vec<u8>> = if !t.content.is_empty() {
            Some(t.content.to_vec())
        } else {
            t.video.as_ref().filter(|v| !v.content.is_empty()).map(|v| v.content.to_vec())
        };
        let (bytes, label) = match embedded {
            Some(b) => (Some(b), format!("{name} (embedded)")),
            None => {
                let names = [
                    t.relative_filename.as_ref(),
                    t.filename.as_ref(),
                    t.absolute_filename.as_ref(),
                    t.element.name.as_ref(),
                ];
                match self.finder.find(&names) {
                    Some(p) => (std::fs::read(&p).ok(), p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name.clone())),
                    None => (None, name.clone()),
                }
            }
        };
        let decoded = match bytes {
            None => {
                self.missing.push(MissingMap { material: material.to_string(), slot, name: name.clone() });
                None
            }
            Some(b) => match textures::decode(&b, &name) {
                Some(px) => Some((Rc::new(px), label)),
                None => {
                    self.warnings.push(format!(
                        "The texture `{name}` is in a format Floptle cannot read, so the {} map of {} is left out.",
                        slot.label(),
                        if material.is_empty() { "a material" } else { material }
                    ));
                    None
                }
            },
        };
        self.pictures.insert(t.element.element_id, decoded.clone());
        decoded
    }

    /// Put `px` in the model once under `key`; the index it has there.
    fn write(&mut self, key: String, px: impl FnOnce() -> TextureData, out: &mut Scene) -> usize {
        if let Some(&i) = self.written.get(&key) {
            return i;
        }
        let i = out.textures.len();
        out.textures.push(px());
        out.filters.push(None);
        self.written.insert(key, i);
        i
    }

    fn used(&mut self, material: &str, slot: MapSlot, texture: &str, note: Option<&'static str>) {
        self.maps.push(MapUse { material: material.to_string(), slot, texture: texture.to_string(), note });
    }
}

fn tex_name(t: &ufbx::Texture) -> String {
    for n in [&t.relative_filename, &t.filename, &t.absolute_filename] {
        if !n.is_empty() {
            return textures::short_name(n);
        }
    }
    t.element.name.to_string()
}

/// The texture on a map, if the map uses it.
fn map_tex(m: &ufbx::MaterialMap) -> Option<&ufbx::Texture> {
    m.texture.as_ref().filter(|_| m.texture_enabled).map(|t| &**t)
}

fn same(a: Option<&ufbx::Texture>, b: Option<&ufbx::Texture>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.element.element_id == b.element.element_id)
}

/// **One source material as a glTF one, every map in the slot it means.**
///
/// What each slot needs on the way:
///
/// * **Base colour** is the diffuse colour and map, times the diffuse factor.
/// * **Opacity** becomes the base map's alpha. When it is the base map's own
///   alpha it is used as it is; a separate opacity map is written into the
///   base map's alpha. Mostly-solid-or-empty alpha is a cut-out (MASK),
///   anything else blends.
/// * **Roughness and metallic** are packed into glTF's one picture, roughness
///   in green and metallic in blue, with occlusion in red when there is an
///   occlusion map. A glossiness or shininess map is roughness upside down
///   and is inverted; with neither, a specular map stands in for it, as
///   roughness is what a specular map was painted to control.
/// * **Normal** is the normal map. A bump slot holds either a normal map or a
///   height map depending on who exported it; the picture is looked at, and a
///   height map is turned into a normal map.
/// * **Emission** is the emissive colour and map, times the emissive factor.
fn material(m: &ufbx::Material, mats: &mut Materials, out: &mut Scene) -> Look {
    let name = m.element.name.to_string();
    let pbr = &m.pbr;
    let fbx = &m.fbx;
    let mut w = WriteMaterial { name: name.clone(), ..Default::default() };

    // ---- base colour ----
    let base_factor = if pbr.base_factor.has_value { pbr.base_factor.value_vec4.x as f32 } else { 1.0 };
    if pbr.base_color.has_value {
        let v = pbr.base_color.value_vec4;
        w.base_color = [v.x as f32 * base_factor, v.y as f32 * base_factor, v.z as f32 * base_factor, 1.0];
    }
    let base_tex = map_tex(&pbr.base_color).or_else(|| map_tex(&fbx.diffuse_color));
    let base_px = base_tex.and_then(|t| mats.picture(t, &name, MapSlot::BaseColor));
    // A map with a colour beside it is tinted by it in some sources and
    // replaced by it in others. FBX's classic materials replace — Maya and
    // Max both write the swatch colour they had before the map was plugged
    // in — so a tint there would darken the map by a colour nobody meant.
    if base_px.is_some() && matches!(m.shader_type, ufbx::ShaderType::FbxLambert | ufbx::ShaderType::FbxPhong | ufbx::ShaderType::BlenderPhong) {
        w.base_color = [base_factor, base_factor, base_factor, 1.0];
    }

    // ---- opacity ----
    let opacity_tex = map_tex(&pbr.opacity).or_else(|| map_tex(&fbx.transparency_color)).or_else(|| map_tex(&fbx.transparency_factor));
    let mut opacity = 1.0f32;
    if pbr.opacity.has_value {
        opacity = (pbr.opacity.value_vec4.x as f32).clamp(0.0, 1.0);
    } else if fbx.transparency_factor.has_value {
        // FBX's classic transparency is a factor times a colour, where black
        // is opaque — Maya writes a factor of one beside a black colour for a
        // material that is not see-through at all.
        let c = if fbx.transparency_color.has_value { fbx.transparency_color.value_vec4 } else { ufbx::Vec4::default() };
        let lum = 0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z;
        opacity = (1.0 - fbx.transparency_factor.value_vec4.x * lum).clamp(0.0, 1.0) as f32;
    }
    w.base_color[3] = opacity;

    let mut base_index = None;
    match (&base_px, opacity_tex) {
        (Some((px, label)), Some(ot)) if same(base_tex, Some(ot)) => {
            base_index = Some(mats.write(format!("t{}", base_tex.map_or(0, |t| t.element.element_id)), || (**px).clone(), out));
            mats.used(&name, MapSlot::BaseColor, label, None);
            mats.used(&name, MapSlot::Opacity, label, Some("the base map's alpha"));
            w.alpha = alpha_mode(px, opacity);
        }
        (base, Some(ot)) => {
            if let Some((op, olabel)) = mats.picture(ot, &name, MapSlot::Opacity) {
                let key = format!("a{}:{}", base_tex.map_or(0, |t| t.element.element_id), ot.element.element_id);
                let merged = textures::with_opacity(base.as_ref().map(|b| &*b.0), &op);
                w.alpha = alpha_mode(&merged, opacity);
                base_index = Some(mats.write(key, || merged, out));
                if let Some((_, label)) = base {
                    mats.used(&name, MapSlot::BaseColor, label, None);
                }
                mats.used(&name, MapSlot::Opacity, &olabel, Some("written into the base map's alpha"));
            } else if let Some((px, label)) = base {
                base_index = Some(mats.write(format!("t{}", base_tex.map_or(0, |t| t.element.element_id)), || (**px).clone(), out));
                mats.used(&name, MapSlot::BaseColor, label, None);
            }
        }
        (Some((px, label)), None) => {
            base_index = Some(mats.write(format!("t{}", base_tex.map_or(0, |t| t.element.element_id)), || (**px).clone(), out));
            mats.used(&name, MapSlot::BaseColor, label, None);
            // A base map's own alpha is only a cut-out when the material says
            // so; an RGBA albedo with junk in its alpha is common.
            if opacity < 1.0 {
                w.alpha = AlphaMode::Blend;
            }
        }
        (None, None) => {
            if opacity < 1.0 {
                w.alpha = AlphaMode::Blend;
            }
        }
    }
    w.base_texture = base_index;

    // ---- roughness, metallic, occlusion ----
    if pbr.metalness.has_value {
        w.metallic = (pbr.metalness.value_vec4.x as f32).clamp(0.0, 1.0);
    }
    if pbr.roughness.has_value {
        w.roughness = (pbr.roughness.value_vec4.x as f32).clamp(0.0, 1.0);
    }
    let gloss_flag = m.features.roughness_as_glossiness.enabled;
    let spec_exp = map_tex(&fbx.specular_exponent);
    let rough_src: Option<(&ufbx::Texture, bool, Option<&'static str>)> = match map_tex(&pbr.roughness) {
        // A roughness map that is really the shininess exponent's (classic FBX
        // and OBJ's `map_Ns`), or one the material marks as glossiness.
        Some(t) if gloss_flag || same(Some(t), spec_exp) => Some((t, true, Some("glossiness, inverted"))),
        Some(t) => Some((t, false, None)),
        None => match map_tex(&pbr.glossiness) {
            Some(t) => Some((t, true, Some("glossiness, inverted"))),
            None => map_tex(&fbx.specular_color)
                .or_else(|| map_tex(&fbx.specular_factor))
                .map(|t| (t, true, Some("from the specular map"))),
        },
    };
    let metal_tex = map_tex(&pbr.metalness);
    let ao_tex = map_tex(&pbr.ambient_occlusion);
    let rough_px = rough_src.and_then(|(t, inv, note)| mats.picture(t, &name, MapSlot::Roughness).map(|p| (p, t, inv, note)));
    let metal_px = metal_tex.and_then(|t| mats.picture(t, &name, MapSlot::Metallic).map(|p| (p, t)));
    let ao_px = ao_tex.and_then(|t| mats.picture(t, &name, MapSlot::Occlusion).map(|p| (p, t)));
    if rough_px.is_some() || metal_px.is_some() {
        if rough_px.is_some() {
            w.roughness = 1.0;
        }
        if metal_px.is_some() {
            w.metallic = 1.0;
        }
        // One picture already packed glTF's way (both slots naming it) is
        // used as it is; separate pictures are packed.
        let prepacked = matches!((&rough_px, &metal_px), (Some((_, r, false, _)), Some((_, mt))) if r.element.element_id == mt.element.element_id);
        let ao_fits = ao_px.as_ref().filter(|_| !prepacked);
        let key = format!(
            "orm:{}:{}:{}:{}",
            rough_px.as_ref().map_or(0, |r| r.1.element.element_id),
            rough_px.as_ref().is_some_and(|r| r.2),
            metal_px.as_ref().map_or(0, |m| m.1.element.element_id),
            ao_fits.map_or(0, |a| a.1.element.element_id),
        );
        let idx = if let (true, Some(((px, _), ..))) = (prepacked, &rough_px) {
            mats.write(key, || (**px).clone(), out)
        } else {
            let (r, mt, ao) = (rough_px.as_ref(), metal_px.as_ref(), ao_fits);
            mats.write(
                key,
                || {
                    textures::pack_orm(
                        ao.map(|a| Channel { texture: &a.0.0, invert: false }),
                        r.map(|r| Channel { texture: &r.0.0, invert: r.2 }),
                        mt.map(|m| Channel { texture: &m.0.0, invert: false }),
                    )
                },
                out,
            )
        };
        w.metallic_roughness = Some(idx);
        if let Some(((_, label), _, _, note)) = &rough_px {
            mats.used(&name, MapSlot::Roughness, label, Some(note.unwrap_or("packed with metallic")));
        }
        if let Some(((_, label), _)) = &metal_px {
            mats.used(&name, MapSlot::Metallic, label, Some("packed with roughness"));
        }
        if let Some(((_, label), _)) = ao_fits {
            w.occlusion = Some(idx);
            mats.used(&name, MapSlot::Occlusion, label, Some("packed with roughness and metallic"));
        }
    }
    if w.occlusion.is_none()
        && let Some(((px, label), t)) = &ao_px
    {
        w.occlusion = Some(mats.write(format!("t{}", t.element.element_id), || (**px).clone(), out));
        mats.used(&name, MapSlot::Occlusion, label, None);
    }

    // ---- normal ----
    let declared_normal = map_tex(&pbr.normal_map).or_else(|| map_tex(&fbx.normal_map));
    let bump = map_tex(&fbx.bump);
    if fbx.bump_factor.has_value && bump.is_some() {
        w.normal_scale = (fbx.bump_factor.value_vec4.x as f32).max(0.0);
    }
    // OBJ's bump slot and FBX's bump slot both hold height maps in some files
    // and normal maps in others, so what is in the picture decides.
    let is_obj = m.shader_type == ufbx::ShaderType::WavefrontMtl;
    let (normal_tex, trust) = match declared_normal {
        Some(t) => (Some(t), !is_obj),
        None => (bump, false),
    };
    if let Some(t) = normal_tex
        && let Some((px, label)) = mats.picture(t, &name, MapSlot::Normal)
    {
        if trust || textures::looks_like_normal_map(&px) {
            w.normal = Some(mats.write(format!("t{}", t.element.element_id), || (*px).clone(), out));
            mats.used(&name, MapSlot::Normal, &label, None);
        } else {
            let strength = w.normal_scale;
            w.normal = Some(mats.write(format!("h{}", t.element.element_id), || textures::height_to_normal(&px, strength), out));
            w.normal_scale = 1.0;
            mats.used(&name, MapSlot::Normal, &label, Some("from a height map"));
        }
    }

    // ---- emission ----
    let ef = if pbr.emission_factor.has_value { pbr.emission_factor.value_vec4.x as f32 } else { 1.0 };
    if pbr.emission_color.has_value {
        let v = pbr.emission_color.value_vec4;
        w.emissive = [v.x as f32 * ef, v.y as f32 * ef, v.z as f32 * ef];
    }
    if let Some(t) = map_tex(&pbr.emission_color).or_else(|| map_tex(&fbx.emission_color))
        && let Some((px, label)) = mats.picture(t, &name, MapSlot::Emissive)
    {
        w.emissive_texture = Some(mats.write(format!("t{}", t.element.element_id), || (*px).clone(), out));
        if w.emissive.iter().all(|&c| c <= 0.0) {
            w.emissive = [ef.clamp(0.0, 1.0); 3];
        }
        mats.used(&name, MapSlot::Emissive, &label, None);
    }

    // Drawn from both sides unless the material says otherwise. FBX rarely
    // records it, and Maya, Max and Blender all show both sides of a face
    // until somebody turns backface culling on; an open mesh (a leaf, a
    // cloth, a single wall) that vanishes from behind is not what was made.
    let ds = &m.features.double_sided;
    w.double_sided = !ds.is_explicit || ds.enabled;

    // Maps with no glTF slot are named, so nobody hunts for a displacement
    // that was never going to arrive.
    for (map, what) in [(&pbr.displacement_map, "displacement"), (&fbx.displacement, "displacement"), (&fbx.reflection_color, "reflection")] {
        if let Some(t) = map_tex(map) {
            let note = format!("{} ({what} map `{}`)", if name.is_empty() { "a material" } else { &name }, tex_name(t));
            if !out.report.dropped.contains(&note) {
                out.report.dropped.push(note);
            }
        }
    }

    let uv = base_tex.filter(|t| t.has_uv_transform).map(|t| t.uv_to_texture);
    Look { material: w, uv }
}

/// Opaque, cut-out or blended, from the alpha a picture carries and the
/// material's own opacity.
fn alpha_mode(px: &TextureData, opacity: f32) -> AlphaMode {
    match textures::alpha_kind(px) {
        Some(true) if opacity >= 1.0 => AlphaMode::Mask(0.5),
        Some(_) => AlphaMode::Blend,
        None if opacity < 1.0 => AlphaMode::Blend,
        None => AlphaMode::Opaque,
    }
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
/// it is pinned by a test that converts the same model exported both ways and
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

/// The matrix a normal is transformed by: the inverse transpose of the upper
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
    // cofactor(a) / det, which is transpose(inverse(a)). Written out rather
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
fn note_dropped(scene: &ufbx::Scene, kept_tree: bool, clips: usize, report: &mut Report) {
    let stacks = scene.anim_stacks.len();
    if stacks > clips {
        let n = stacks - clips;
        report.dropped.push(format!(
            "{n} animation{} that move{} only cameras or lights",
            if n == 1 { "" } else { "s" },
            if n == 1 { "s" } else { "" }
        ));
    }
    if !kept_tree && !scene.skin_deformers.is_empty() {
        report.dropped.push("skinning (the mesh is converted in its bind pose)".to_string());
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

/// What one part is cut from and how it is placed.
struct Geometry<'a> {
    mesh: &'a ufbx::Mesh,
    /// The material part, or `None` for the whole mesh.
    part: Option<&'a ufbx::MeshPart>,
    xform: &'a ufbx::Matrix,
    normals: [[f64; 3]; 3],
    flipped: bool,
    rig: Option<&'a Rig>,
    /// The base-colour map's UV transform, baked into the coordinates.
    uv: Option<ufbx::Matrix>,
}

/// Turn one material part (or the whole mesh) into a [`SubMesh`].
fn build_part(g: &Geometry, name: &str) -> Option<SubMesh> {
    let mesh = g.mesh;
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
    if g.rig.is_some() {
        sm.joints = Some(Vec::new());
        sm.weights = Some(Vec::new());
    }

    // ufbx triangulates a face into at most `max_face_triangles` triangles.
    let mut tri = vec![0u32; mesh.max_face_triangles.max(1) * 3];

    // **De-indexed, one vertex per triangle corner.** FBX indexes position,
    // normal and UV separately, so a corner where two faces meet at a hard edge
    // is one position with two normals — a thing glTF cannot express. Splitting
    // every corner is correct for those; identical corners are welded back
    // afterwards so the common case does not pay for it.
    let faces: Vec<u32> = match g.part {
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
            let order = if g.flipped { [0usize, 2, 1] } else { [0usize, 1, 2] };
            for o in order {
                let ix = corners[o] as usize;
                let p = ufbx::transform_position(g.xform, mesh.vertex_position[ix]);
                sm.positions.push([p.x as f32, p.y as f32, p.z as f32]);

                if mesh.vertex_normal.exists {
                    // **Transformed, never negated.** Reflecting a surface
                    // reflects its outward normal too, so the transformed
                    // normal is already right; it is the winding a mirror
                    // inverts, and reordering the corners above is the whole of
                    // the correction. Negating as well undoes it and leaves
                    // every normal disagreeing with its own face.
                    let nv = normal_dir(g.normals, mesh.vertex_normal[ix]);
                    sm.normals.push([nv[0] as f32, nv[1] as f32, nv[2] as f32]);
                }
                if let Some(uvs) = sm.uvs.as_mut() {
                    let mut uv = mesh.vertex_uv[ix];
                    if let Some(m) = &g.uv {
                        let t = ufbx::transform_position(m, ufbx::Vec3 { x: uv.x, y: uv.y, z: 0.0 });
                        uv = ufbx::Vec2 { x: t.x, y: t.y };
                    }
                    // **glTF's V axis points the other way.** Every texture on
                    // every converted model is upside down without this, which
                    // reads as the texture being wrong rather than the importer.
                    uvs.push([uv.x as f32, 1.0 - uv.y as f32]);
                }
                if let Some(cols) = sm.colors.as_mut() {
                    let c = mesh.vertex_color[ix];
                    cols.push([to_u8(c.x), to_u8(c.y), to_u8(c.z), to_u8(c.w)]);
                }
                if let (Some(rig), Some(js), Some(ws)) = (g.rig, sm.joints.as_mut(), sm.weights.as_mut()) {
                    let v = mesh.vertex_indices.get(ix).copied().unwrap_or(0) as usize;
                    js.push(rig.joints.get(v).copied().unwrap_or([0; 4]));
                    ws.push(rig.weights.get(v).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]));
                }
            }
        }
    }

    if sm.positions.is_empty() {
        return None;
    }
    sm.indices = (0..sm.positions.len() as u32).collect();
    weld(&mut sm);
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
    let mut joints: Option<Vec<[u16; 4]>> = sm.joints.as_ref().map(|_| Vec::new());
    let mut weights: Option<Vec<[f32; 4]>> = sm.weights.as_ref().map(|_| Vec::new());
    let mut remap = Vec::with_capacity(sm.positions.len());

    for i in 0..sm.positions.len() {
        let mut key = Vec::with_capacity(64);
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
        // Two corners at one place bound to different bones are two vertices:
        // welding them would tear one off its bone.
        if let Some(j) = sm.joints.as_ref().and_then(|j| j.get(i)) {
            for v in j {
                key.extend_from_slice(&v.to_le_bytes());
            }
        }
        if let Some(w) = sm.weights.as_ref().and_then(|w| w.get(i)) {
            for v in w {
                key.extend_from_slice(&v.to_bits().to_le_bytes());
            }
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
            if let (Some(dst), Some(c)) = (cols.as_mut(), sm.colors.as_ref().and_then(|c| c.get(i))) {
                dst.push(*c);
            }
            if let (Some(dst), Some(j)) = (joints.as_mut(), sm.joints.as_ref().and_then(|j| j.get(i))) {
                dst.push(*j);
            }
            if let (Some(dst), Some(w)) = (weights.as_mut(), sm.weights.as_ref().and_then(|w| w.get(i))) {
                dst.push(*w);
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
    sm.joints = joints;
    sm.weights = weights;
}
