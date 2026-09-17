//! Turning the World into raster instances: tint, materials, particles, and the depth prepass decision.

use floptle_core::Entity;
use floptle_core::math::Mat4;
use floptle_render::InstanceRaw;
use floptle_render::MaterialParams;
use floptle_render::MeshId;
use floptle_render::TexId;
use floptle_render::instance_of_mat;
use std::collections::HashMap;
use crate::{MeshAsset, anim};


/// **Which material one part of a model draws with.**
///
///   this object's override  ▸  the node's Material  ▸  the part as imported
///
/// The most specific one wins, whole — its colour, its texture, its maps, its
/// retro flags. Its own function because that sentence is the contract, and it
/// used to be three-quarters true: a node Material multiplied its colour into
/// each part's imported colour while its texture replaced outright, so a model
/// given a new material kept the old picture on it and only the emissive
/// appeared to work. A rule that applies half a material is not a rule anybody
/// can predict, and this is the place it is stated once.
pub(crate) enum PartLook<'a> {
    /// This sub-object's own override material, plus the exact key it is
    /// stored under in `ObjectMaterials` — the object name or the material
    /// name, whichever matched. A per-part `.flsl` shader binding is keyed the
    /// same way, so callers that need to find this override's shader (rather
    /// than the node's own) need this back, not just the `Material`.
    Override(&'a str, &'a floptle_core::Material),
    /// The node-level Material, over every part of the model.
    Node(&'a MaterialParams),
    /// Nothing supersedes: the part's imported base colour (and, at the draw,
    /// its imported texture).
    Imported([f32; 3]),
}

/// Gather one `Matter::Mesh`'s draw instances. Rigged meshes animate: each part
/// either rides its (possibly animated) node rigidly (R6-style), or — for a true
/// vertex-skinned part — is CPU-deformed by this frame's bone palette, its
/// vertices re-uploaded, and drawn at the mesh matrix. `pose` is the node's animated
/// world matrices (falls back to the rig rest pose). Static (unrigged) meshes just
/// draw every part at `model`.
///
/// Does this view need the opaque depth prepass to run?
///
/// **One function, called from both render paths.** Every feature that reads the
/// prepass silently does nothing without it — no error, no warning, just a
/// picture missing something — so a view that forgets one term is a view where
/// that feature quietly stops existing. The two paths had already drifted: the
/// window's condition was missing contact shadows, so in a scene made of meshes
/// with reflections and lamp shadows both off, contact shadows worked in a
/// docked Game panel and did nothing in the window beside it.
///
/// Adding a feature that reads the prepass means adding a parameter here, which
/// is a compile error at both call sites rather than a silent omission at one.
pub(crate) fn wants_prepass(flsl_wants_depth: bool, ssr: bool, point_shadows: bool, contact: bool) -> bool {
    flsl_wants_depth || ssr || point_shadows || contact
}

/// Run this view's opaque depth prepass **and bind it**, in one call.
///
/// One call because the two have to happen together and had twice been written
/// apart: first the bind was inside the `if rm_draw` arm, so every feature that
/// reads the prepass silently did nothing in a scene made of meshes; then it was
/// guarded on "was the target reallocated?", which is permanently false once a
/// frame draws two views, so the window drew with the docked Game panel's depth
/// buffer and stored picture and reflections came and went.
///
/// Both are the same mistake — a per-view resource bound less often than per
/// view — and neither errors. Running and binding cannot be separated here, so
/// they are not separable at the call site either.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepass_and_bind(
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    raymarch: &mut floptle_render::Raymarch,
    globals: floptle_render::Globals,
    instances: &[(MeshId, Option<TexId>, floptle_render::InstanceRaw)],
    flsl: &[floptle_render::FlslDraw],
    skins: &[floptle_render::SkinDraw],
    depth_tex: &wgpu::Texture,
    history: Option<(&wgpu::TextureView, &wgpu::Sampler)>,
) {
    raster.depth_prepass_with(gpu, globals, instances, flsl, skins, depth_tex);
    raymarch.bind_frame_targets(gpu, raster.prepass_view(), history);

}

/// Multiply a node's [`Tint`](floptle_core::Tint) into everything it pushed
/// into this frame.
///
/// A function rather than a loop written twice because the Scene view and
/// `render_world_into` both have to do it: a kind of drawing tinted on one path
/// and not the other is a model that is red while you edit it and plain in the
/// game, which is the drift `offscreen_draws_the_same_world` exists to catch.
///
/// `from` is where this node's instances start — everything after it belongs to
/// this node and nothing before it does.
pub(crate) fn apply_node_tint(
    tint: Option<&floptle_core::Tint>,
    from: (usize, usize, usize, usize),
    instances: &mut [(MeshId, Option<TexId>, InstanceRaw)],
    flsl_draws: &mut [floptle_render::FlslDraw],
    skin_draws: &mut [floptle_render::SkinDraw],
    // The 2D lighting G-buffer. A flat node on the lit path draws unlit in the
    // raster pass and is corrected by the light composite, which reads this
    // copy of the colour — so a tint applied only to the raster instance is
    // corrected back out again by a pass that never heard about it.
    flat2d: &mut [(MeshId, Option<TexId>, floptle_render::Light2dInstance)],
) {
    let Some(t) = tint.filter(|t| !t.is_identity()) else { return };
    // The one place that knows which lanes a tint's rim and ambient live in:
    // `params` is (shininess, rim strength, unlit, ambient) and `rim` is
    // (r, g, b, packed tiling flags) — so only `rim[..3]` may be written, and
    // `rim[3]` must survive, or a tinted node loses its texture tiling.
    fn tint_instance(t: &floptle_core::Tint, raw: &mut InstanceRaw) {
        t.apply(&mut raw.color);
        let (rim, strength) =
            t.rim_over([raw.rim[0], raw.rim[1], raw.rim[2]], raw.params[1]);
        raw.rim[..3].copy_from_slice(&rim);
        raw.params[1] = strength;
        raw.params[3] = t.ambient_over(raw.params[3]);
    }
    for (_, _, raw) in &mut instances[from.0..] {
        tint_instance(t, raw);
    }
    for (_, _, _, raw) in &mut flsl_draws[from.1..] {
        tint_instance(t, raw);
    }
    for d in &mut skin_draws[from.2..] {
        tint_instance(t, &mut d.instance);
    }
    for (_, _, lit) in &mut flat2d[from.3..] {
        t.apply(&mut lit.tint);
    }
}

/// How many draw calls this frame's meshes cost.
///
/// `Counts::draws` was a literal `0` — never computed, always answering the
/// question it exists for with a lie. `draw_scene_with` (`floptle-render`)
/// buckets by `(mesh, texture[, flsl binding])` and issues one instanced
/// `draw_indexed` per bucket, opaque and blended kept apart; this counts the
/// same groups without duplicating that bucketing GPU-side. It folds opaque
/// and blended together, so a group that has instances in both phases is one
/// real draw call counted as one here — a coarse number a game can act on
/// beats the `0` it replaces. Terrain chunks, particle batches and 2D/UI
/// batches are their own passes with their own counts already (`chunks`,
/// `particles`) and are not included here.
pub(crate) fn count_draw_batches(
    instances: &[(MeshId, Option<TexId>, InstanceRaw)],
    flsl: &[floptle_render::FlslDraw],
    skins: &[floptle_render::SkinDraw],
) -> usize {
    let mut mesh_tex: std::collections::HashSet<(u32, Option<u32>)> = std::collections::HashSet::new();
    for (mesh, tex, _) in instances {
        mesh_tex.insert((mesh.0, tex.map(|t| t.0)));
    }
    let mut flsl_groups: std::collections::HashSet<(u32, Option<u32>, u32)> =
        std::collections::HashSet::new();
    for (mesh, tex, bind, _) in flsl {
        flsl_groups.insert((mesh.0, tex.map(|t| t.0), bind.0));
    }
    let mut skin_groups: std::collections::HashSet<(u32, Option<u32>)> = std::collections::HashSet::new();
    for s in skins {
        skin_groups.insert((s.mesh.0, s.tex.map(|t| t.0)));
    }
    mesh_tex.len() + flsl_groups.len() + skin_groups.len()
}

pub(crate) fn part_look_rule<'a>(
    obj_mats: Option<&'a floptle_core::ObjectMaterials>,
    override_key: Option<&str>,
    // The glTF material this part was imported with — the other name the same
    // part answers to. See below for why both.
    material_name: Option<&str>,
    node_material: Option<&'a MaterialParams>,
    imported_base: [f32; 3],
) -> PartLook<'a> {
    // **A part answers to its object name and to its material name.**
    //
    // The object name is the precise one — it addresses one sub-object — but it
    // is not the name anybody has. Import de-duplicates repeated node names, so
    // an avatar whose torso node is called `Torso` in Blender is keyed `Torso#2`
    // here, and an override written as `Torso` matched nothing at all and said
    // nothing about it.
    //
    // The material name is the one on the model's own materials list, the one a
    // glTF author chose, and usually the one that means something across the
    // parts: a character's `Clothing` covers the torso and both arms, which is
    // exactly the grouping a clothing system wants to address at once.
    //
    // Object first, so the precise name still wins where both exist.
    if let Some(om) = obj_mats {
        // `get_key_value` rather than `get`: the returned key has to outlive
        // this call (a per-part `.flsl` binding is looked up by it later), and
        // the map's own key — not the caller's `override_key`/`material_name`
        // argument — is the only one guaranteed to live that long.
        if let Some(k) = override_key
            && let Some((k, m)) = om.0.get_key_value(k)
        {
            return PartLook::Override(k, m);
        }
        if let Some(k) = material_name
            && let Some((k, m)) = om.0.get_key_value(k)
        {
            return PartLook::Override(k, m);
        }
    }
    match node_material {
        Some(m) => PartLook::Node(m),
        None => PartLook::Imported(imported_base),
    }
}

/// Shared by the main surface gather and the offscreen `render_world_into` so the
/// fullscreen, docked, split, and camera-preview views all animate identically —
/// previously the offscreen path drew every mesh rigidly at its root, so a character
/// looked frozen whenever the Game view wasn't the fullscreen/focused one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_mesh_instances(
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    asset: &MeshAsset,
    pose: Option<&[Mat4]>,
    model: Mat4,
    tex: Option<TexId>,
    // The node-level Material's params (None = the node has no Material — parts
    // fall back to their imported base-color factor, matching runtime builds).
    mp: Option<&MaterialParams>,
    // Per-sub-object material overrides (the `ObjectMaterials` component) + the
    // texture registry to resolve their texture paths (pre-warmed each frame).
    obj_mats: Option<&floptle_core::ObjectMaterials>,
    texture_registry: &HashMap<String, TexId>,
    // This node's own per-part paint bases (the brush's work). `None` → fall back to
    // whatever the mesh imported with, so Blender paint still shows on unpainted nodes.
    node_paint: Option<&[u32]>,
    // The drawing entity + the per-entity skinned-buffer cache: each entity bakes
    // its pose into its own clone of a skinned part's vertex buffer, so instances
    // of one model animate independently.
    entity: Entity,
    variants: &mut anim::SkinVariants,
    skin_scratch: &mut Vec<floptle_render::Vertex>,
    instances: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
    // Gpu-skinned parts land here instead of `instances`: same
    // mesh, same material, but drawn through the `vs_skin` pipelines with this
    // draw's bone palette. Several characters of one model stay one draw call,
    // which the CPU path could not manage — it had to give each entity a private
    // vertex buffer to bake its pose into.
    skins: &mut Vec<floptle_render::SkinDraw>,
    flsl: Option<floptle_render::FlslBindingId>,
    flsl_out: &mut Vec<floptle_render::FlslDraw>,
    // Per-part `.flsl` bindings — an `ObjectMaterials` override that names its
    // own shader, keyed by (this entity, the override's key). Looked up fresh
    // per part rather than threaded in like `flsl`, because unlike the node's
    // shader this can differ part to part.
    obj_flsl: &crate::shaders::ObjFlslBinds,
) {
    // A node's custom `.flsl` material routes every part through the shader's
    // pipeline instead of the built-in one — same instance data either way.
    // `part_flsl` overrides that per part: `None` inherits the node's `flsl`,
    // `Some(x)` is the override's own answer (its own binding, or `None` for
    // "built-in, and not the node's shader either" — seeing the override at
    // all already means the node's shader does not apply here).
    let mut push_for = |mid: MeshId,
                         ptex: Option<TexId>,
                         raw: InstanceRaw,
                         part_flsl: Option<Option<floptle_render::FlslBindingId>>| {
        match part_flsl.unwrap_or(flsl) {
            Some(b) => flsl_out.push((mid, ptex, b, raw)),
            None => instances.push((mid, ptex, raw)),
        }
    };
    // **A part's look, by one rule: the most specific material wins, whole.**
    //
    //   this object's override  ▸  the node's Material  ▸  the part as imported
    //
    // Whichever of those applies is the material, entire — its colour, its
    // texture, its maps, its retro flags, and (see `part_flsl` above) its
    // shader. A material is a statement of what a surface looks like, and
    // half-applying one is what made this confusing: the node Material used
    // to multiply its colour into each part's imported colour while its
    // texture replaced outright, so "I gave it a new material and it still
    // has the old picture on it, but the emissive works" was the exact and
    // correct description of what the engine did — and a shader that named
    // itself on an override but never took effect was the same bug in the
    // one property this rule didn't reach yet.
    //
    // A model that looks right therefore carries no node Material at all. One is
    // how you say "this whole model is made of this" — and per-object overrides
    // are how you say it about one part.
    let part_look = |raster: &mut floptle_render::Raster,
                         asset: &MeshAsset,
                         part: usize|
     -> (Option<TexId>, MaterialParams, Option<Option<floptle_render::FlslBindingId>>) {
        // The part's own imported base-colour factor — its share of the model's
        // built-in look, which is what draws when nothing supersedes it.
        let base = asset.part_meta.get(part).map(|pm| pm.base_color).unwrap_or([1.0; 3]);
        let mat_name = asset.part_meta.get(part).map(|pm| pm.material.as_str());
        match part_look_rule(obj_mats, asset.override_key(part), mat_name, mp, base) {
            // An override is a whole material, surface maps and retro flags
            // included — resolved the same way a node's own Material is, so
            // "give this one object a normal map" works.
            PartLook::Override(key, m) => {
                let (t, p) = crate::shading::material_draw(raster, gpu, m, texture_registry, None);
                // No shader named: built-in look, same as ever. A shader
                // named: this part's own binding if it has compiled yet, or
                // (for the frame or two before it has) the built-in look
                // rather than the node's shader — an override that hasn't
                // finished compiling is not the same as no override.
                let part_flsl = m
                    .shader
                    .is_some()
                    .then(|| obj_flsl.get(&(entity, key.to_string())).map(|b| b.binding));
                (Some(t.unwrap_or_else(|| raster.white_texture(gpu))), p, part_flsl)
            }
            // `tex` is this node Material's own texture. `None` there does not
            // mean "keep what the part had" — a bind of `None` is what makes the
            // Mesh's texture draw, which is the imported look this material is
            // superseding. An untextured material means untextured, so it says
            // so with white.
            PartLook::Node(m) => (Some(tex.unwrap_or_else(|| raster.white_texture(gpu))), *m, None),
            PartLook::Imported(base) => (tex, MaterialParams::flat(base), None),
        }
    };
    // Vertex paint is per-part: import splits a model per-material into parts with
    // their own vertex arrays, so each part owns its own paint block. Instances of a
    // part share its base — same block, same draw call.
    let painted = |raster: &floptle_render::Raster, mid: MeshId, part: usize, base: MaterialParams| {
        let mut m = base;
        let brush = node_paint.and_then(|p| p.get(part).copied()).filter(|&b| b != 0);
        // Brush paint modulates 2× (paint light and shadow); imported glTF COLOR_0 stays a
        // plain ×1 multiply, per the glTF convention (white = identity).
        m.paint_modulate = brush.is_some();
        m.paint_base = brush.unwrap_or_else(|| raster.mesh_paint_base(mid));
        m
    };
    let Some(rig) = asset.rig.as_ref() else {
        for (i, &mid) in asset.parts.iter().enumerate() {
            let (ptex, pmp, part_flsl) = part_look(raster, asset, i);
            push_for(mid, ptex, instance_of_mat(model, &painted(raster, mid, i, pmp)), part_flsl);
        }
        return;
    };
    let node_world = pose.unwrap_or(rig.rest_world.as_slice());
    for (i, &mid) in asset.parts.iter().enumerate() {
        let part_node = rig.part_nodes.get(i).copied().unwrap_or(0);
        let (ptex, pmp, part_flsl) = part_look(raster, asset, i);
        let this_flsl = part_flsl.unwrap_or(flsl);
        if let Some(Some(skin)) = rig.skins.get(i) {
            let raw = instance_of_mat(model, &painted(raster, mid, i, pmp));
            let skin_base = rig.skin_bases.get(i).copied().unwrap_or(0);
            // A custom `.flsl` material routes the part through its own pipeline,
            // which has no skinned variant — those parts keep the CPU deform.
            if skin_base != 0 && this_flsl.is_none() {
                // GPU skinning: hand the pose over and draw the shared bind-pose
                // buffer. `push_skin_pose` is the same arithmetic `cpu_skin_part`
                // applies per vertex, done once per draw instead of once per vertex.
                let palette: Vec<Mat4> = skin
                    .joint_nodes
                    .iter()
                    .zip(&skin.inverse_bind)
                    .map(|(&jn, ib)| node_world.get(jn).copied().unwrap_or(Mat4::IDENTITY) * *ib)
                    .collect();
                let fallback = node_world.get(part_node).copied().unwrap_or(Mat4::IDENTITY);
                let pose = raster.push_skin_pose(skin_base, fallback, &palette);
                skins.push(floptle_render::SkinDraw { mesh: mid, tex: ptex, instance: raw, pose });
            } else {
                // Fallback: the skinning store refused this part (it is bounded by
                // the instance lane that addresses it), or a custom shader owns the
                // draw. CPU-skin into this entity's private clone, as before —
                // paint lives in `vpaint`, keyed by vertex_index, so the re-upload
                // can't stomp it, and paint/texture lookups stay on `mid`.
                let draw_mid = variants.variant_for(gpu, raster, entity, i, mid);
                anim::cpu_skin_part(skin, part_node, node_world, skin_scratch);
                raster.update_mesh_vertices(gpu, draw_mid, skin_scratch);
                push_for(draw_mid, ptex, raw, part_flsl);
            }
        } else {
            let local = node_world.get(part_node).copied().unwrap_or(Mat4::IDENTITY);
            push_for(mid, ptex, instance_of_mat(model * local, &painted(raster, mid, i, pmp)), part_flsl);
        }
    }
}

/// Resolve mesh-particle draws to raster instances (camera-relative model matrix
/// plus alpha-aware tinted material) and append them to `instances`. Free function
/// so callers pass just `&mesh_registry`, a disjoint field borrow, while `gpu` and
/// `raster` are held by the main render's destructure.
pub(crate) fn resolve_mesh_particles(
    mesh_registry: &HashMap<String, MeshAsset>,
    draws: &[floptle_vfx::MeshDraw],
    instances: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) {
    for md in draws {
        let Some(asset) = mesh_registry.get(&md.asset_path) else { continue };
        for (model, color) in &md.instances {
            let mut mp = MaterialParams::flat([color[0], color[1], color[2]]);
            mp.alpha = color[3];
            let raw = instance_of_mat(*model, &mp);
            for &mid in &asset.parts {
                instances.push((mid, None, raw));
            }
        }
    }
}

#[cfg(test)]
mod tint_tests {
    use super::apply_node_tint;
    use floptle_core::Tint;
    use floptle_core::math::Mat4;
    use floptle_render::{MaterialParams, MeshId, SkinDraw};

    /// A part as a model imports one: textured, no rim, the room's own ambient.
    fn imported() -> MaterialParams {
        MaterialParams {
            color: [1.0, 1.0, 1.0],
            emissive: [0.0; 3],
            emissive_strength: 0.0,
            specular: [1.0; 3],
            shininess: 15.0,
            specular_strength: 0.0,
            rim: [0.0; 3],
            rim_strength: 0.0,
            unlit: false,
            ambient: 1.0,
            alpha: 1.0,
            tile_mode: 0,
            tile: [0.0; 4],
            tile_rotation: 0.0,
            paint_base: 0,
            terrain_paint_base: 0,
            paint_modulate: false,
            terrain_splat: false,
            ext_index: 0,
        }
    }

    /// **A tint's rim and ambient reach the lanes the shader reads.**
    ///
    /// `raster.wgsl` reads the rim strength out of `params.y` and the ambient
    /// multiplier out of `params.w`, and the rim's colour out of `rim.xyz`. Those
    /// four lanes are the whole contract between this function and the GPU, and
    /// nothing else in the engine asserts it — a tint that wrote the wrong index
    /// would simply have no effect, which is exactly how it would be reported
    /// ("the rim does nothing") and exactly what a picture cannot tell you.
    #[test]
    fn a_tints_rim_and_ambient_reach_the_instance_lanes() {
        let mat = imported();
        let mut inst = vec![(
            MeshId(1),
            None,
            floptle_render::instance_of_mat(Mat4::IDENTITY, &mat),
        )];
        // The fighters are GPU-skinned, so this is the path that actually
        // matters for a character — and it is its own list.
        let mut skins = vec![SkinDraw {
            mesh: MeshId(1),
            tex: None,
            instance: floptle_render::instance_of_mat(Mat4::IDENTITY, &mat),
            pose: 0,
        }];
        let tiling_before = inst[0].2.rim[3];

        let t = Tint {
            color: [0.92, 0.13, 0.15],
            alpha: 1.0,
            rim: [1.0, 0.14, 0.16],
            rim_strength: 1.3,
            ambient: 1.6,
        };
        apply_node_tint(
            Some(&t),
            (0, 0, 0, 0),
            &mut inst,
            &mut [],
            &mut skins,
            &mut [],
        );

        for (what, raw) in [("unskinned", &inst[0].2), ("skinned", &skins[0].instance)] {
            assert_eq!(&raw.rim[..3], &[1.0, 0.14, 0.16], "{what}: the rim's colour");
            assert_eq!(raw.params[1], 1.3, "{what}: params.y is the rim STRENGTH");
            assert_eq!(raw.params[3], 1.6, "{what}: params.w is the ambient multiplier");
            assert_eq!(raw.color[0], 0.92, "{what}: and the colour still multiplies");
            // params.z packs unlit + the paint base; params.x is shininess.
            assert_eq!(raw.params[0], 15.0, "{what}: shininess is not a tint's business");
        }
        assert_eq!(
            inst[0].2.rim[3], tiling_before,
            "rim.w is the packed TILING flags, not part of the rim — a tinted node \
             must not lose its texture tiling"
        );
    }
}
