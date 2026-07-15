//! Custom `.flsl` shader materials in the editor (ADR-0007, Phase 2).
//!
//! The pipeline: a Material's `shader` names a project `.flsl` file; this
//! module compiles it (parse → check → transpile → naga against the REAL
//! raster+field sources), registers the pipeline with the raster pass, and
//! keeps one live group(3) binding (params UBO + texture slots) per entity.
//!
//! Hot reload is the house mtime pattern (texture registry, prefab cache):
//! shaders in use are re-stat'ed every frame and recompile on change. A broken
//! edit KEEPS the last good pipeline running and reports to the Console + the
//! IDE squiggle — a failed save never black-screens the scene.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use floptle_core::{Entity, Material};
use floptle_render::{FlslBindingId, FlslBlend, FlslShaderId, TexId};
use floptle_shader::CompiledFragment;

use crate::Editor;

/// One `.flsl` file's compile state, keyed by project-relative path.
pub(crate) struct FlslEntry {
    mtime: Option<SystemTime>,
    /// The last GOOD compile — kept while `error` reports a newer failure.
    pub(crate) compiled: Option<(CompiledFragment, FlslShaderId)>,
    /// The newest failure (compile or naga), already line-mapped for humans.
    pub(crate) error: Option<String>,
}

/// One entity's live material binding (group(3) UBO + textures), plus what it
/// was built from — so a frame with no changes does zero GPU work.
pub(crate) struct FlslMatBind {
    pub(crate) binding: FlslBindingId,
    shader: FlslShaderId,
    params: Vec<u8>,
    textures: Vec<Option<TexId>>,
}

impl Editor {
    /// Per-frame driver, called before any gather: hot-reload every `.flsl`
    /// the scene references, then create/refresh each shader-material
    /// entity's group(3) binding. Unchanged frames cost a few file stats.
    pub(crate) fn ensure_flsl_materials(&mut self) {
        if self.gpu.is_none() || self.raster.is_none() {
            return;
        }
        // Field Shape nodes carry SDF-stage shaders — those go through
        // `sync_field_shapes`, not the fragment-material path.
        let field_shape: Vec<Entity> = self
            .world
            .query::<floptle_core::Matter>()
            .filter(|(_, m)| matches!(m, floptle_core::Matter::FieldShape { .. }))
            .map(|(e, _)| e)
            .collect();
        let mats: Vec<(Entity, Material)> = self
            .world
            .query::<Material>()
            .filter(|(e, m)| m.shader.is_some() && !field_shape.contains(e))
            .map(|(e, m)| (e, m.clone()))
            .collect();

        let mut paths: Vec<String> =
            mats.iter().filter_map(|(_, m)| m.shader.clone()).collect();
        paths.sort();
        paths.dedup();
        for p in &paths {
            self.ensure_flsl_shader(p);
        }

        // Plan each entity's binding from the compile cache (immutable pass),
        // then resolve textures + touch the GPU (mutable pass).
        struct Plan {
            e: Entity,
            shader: FlslShaderId,
            params: Vec<u8>,
            slot_paths: Vec<Option<String>>,
        }
        let mut plans: Vec<Plan> = Vec::new();
        for (e, mat) in &mats {
            let rel = mat.shader.as_deref().unwrap_or_default();
            let Some((compiled, shader)) =
                self.flsl_cache.get(rel).and_then(|en| en.compiled.as_ref())
            else {
                continue; // never compiled — the node keeps its built-in look
            };
            let params = compiled.pack_params(
                &|name| mat.shader_params.get(name).copied(),
                &|slot| mat.shader_tiling.get(slot).map(tiling_pack),
            );
            let slot_paths = compiled
                .textures
                .iter()
                .map(|slot| mat.shader_textures.get(slot).cloned())
                .collect();
            plans.push(Plan { e: *e, shader: *shader, params, slot_paths });
        }

        let mut seen: HashSet<Entity> = HashSet::new();
        for plan in plans {
            seen.insert(plan.e);
            let textures: Vec<Option<TexId>> = plan
                .slot_paths
                .iter()
                .map(|p| p.as_deref().and_then(|p| self.ensure_texture(p)))
                .collect();
            let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                return;
            };
            match self.flsl_binds.get_mut(&plan.e) {
                Some(b) if b.shader == plan.shader && b.textures == textures => {
                    if b.params != plan.params {
                        raster.write_flsl_params(gpu, b.binding, &plan.params);
                        b.params = plan.params;
                    }
                }
                Some(b) => {
                    // Shader or texture set changed: rebuild the bind group in place.
                    b.binding = raster.set_flsl_binding(
                        gpu,
                        Some(b.binding),
                        plan.shader,
                        &plan.params,
                        &textures,
                    );
                    b.shader = plan.shader;
                    b.params = plan.params;
                    b.textures = textures;
                }
                None => {
                    let reuse = self.flsl_free.pop();
                    let binding =
                        raster.set_flsl_binding(gpu, reuse, plan.shader, &plan.params, &textures);
                    self.flsl_binds.insert(
                        plan.e,
                        FlslMatBind {
                            binding,
                            shader: plan.shader,
                            params: plan.params,
                            textures,
                        },
                    );
                }
            }
        }

        // Entities that lost their shader material free their binding slot.
        let stale: Vec<Entity> =
            self.flsl_binds.keys().copied().filter(|e| !seen.contains(e)).collect();
        for e in stale {
            if let Some(b) = self.flsl_binds.remove(&e) {
                self.flsl_free.push(b.binding);
            }
        }
    }

    /// Compile (or hot-reload) one `.flsl` by its Material path (asset-tree or
    /// project-relative). Keeps the last good pipeline on failure;
    /// Console-reports each new error once.
    fn ensure_flsl_shader(&mut self, rel: &str) {
        let full = self.resolve_asset_path(rel);
        let mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
        if let Some(entry) = self.flsl_cache.get(rel)
            && entry.mtime == mtime
        {
            return; // unchanged (or still missing — already reported)
        }

        let report = |editor: &mut Editor, msg: String| {
            let changed =
                editor.flsl_cache.get(rel).is_none_or(|e| e.error.as_deref() != Some(&msg));
            if changed {
                editor.console.push(
                    floptle_script::LogLevel::Error,
                    format!("◈ {rel}: {msg}"),
                    None,
                );
            }
            msg
        };

        let src = match std::fs::read_to_string(&full) {
            Ok(s) => s,
            Err(e) => {
                let msg = report(self, format!("can't read shader ({e})"));
                let old = self.flsl_cache.remove(rel);
                self.flsl_cache.insert(
                    rel.to_string(),
                    FlslEntry {
                        mtime,
                        compiled: old.and_then(|o| o.compiled),
                        error: Some(msg),
                    },
                );
                return;
            }
        };

        let outcome = floptle_shader::compile_fragment(&src).and_then(|compiled| {
            // naga against the REAL pass sources — passing here means the
            // pipeline build below can't fail on the shader.
            floptle_shader::validate(floptle_render::pass_prelude(), &compiled.chunk).map_err(
                |d| match d.chunk_line.and_then(|l| compiled.flsl_span_of_chunk_line(l)) {
                    Some(span) => {
                        let (l, c) = floptle_shader::text::line_col(&src, span.start);
                        format!("{l}:{c}: {}", d.message)
                    }
                    None => d.message,
                },
            )?;
            Ok(compiled)
        });

        match outcome {
            Ok(compiled) => {
                let replace =
                    self.flsl_cache.get(rel).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id);
                let chunk_full =
                    format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
                let blend = match compiled.blend {
                    floptle_shader::Blend::Opaque => FlslBlend::Opaque,
                    floptle_shader::Blend::Alpha => FlslBlend::Alpha,
                    floptle_shader::Blend::Additive => FlslBlend::Additive,
                };
                let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                    return;
                };
                let id = raster.register_flsl_shader(
                    gpu,
                    &chunk_full,
                    compiled.textures.len(),
                    blend,
                    replace,
                );
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("◈ compiled {rel}"),
                    None,
                );
                // The pipeline (and possibly its group(3) layout) changed:
                // retire every live binding of this shader so the next
                // ensure pass rebuilds them against the fresh layout.
                let stale: Vec<Entity> = self
                    .flsl_binds
                    .iter()
                    .filter(|(_, b)| b.shader == id)
                    .map(|(e, _)| *e)
                    .collect();
                for e in stale {
                    if let Some(b) = self.flsl_binds.remove(&e) {
                        self.flsl_free.push(b.binding);
                    }
                }
                self.flsl_cache
                    .insert(rel.to_string(), FlslEntry { mtime, compiled: Some((compiled, id)), error: None });
            }
            Err(msg) => {
                let msg = report(self, msg);
                let old = self.flsl_cache.remove(rel);
                self.flsl_cache.insert(
                    rel.to_string(),
                    FlslEntry { mtime, compiled: old.and_then(|o| o.compiled), error: Some(msg) },
                );
            }
        }
    }

    /// Drop every flsl cache (project switch — the raster pass was rebuilt).
    pub(crate) fn clear_flsl_state(&mut self) {
        self.flsl_cache.clear();
        self.flsl_binds.clear();
        self.flsl_free.clear();
        self.sdf_cache.clear();
        self.flsl_field_key.clear();
        // The passes persist across projects — un-splice any Field Shape code.
        if !self.flsl_shape_slots.is_empty() {
            if let (Some(gpu), Some(raster), Some(raymarch)) =
                (self.gpu.as_ref(), self.raster.as_mut(), self.raymarch.as_mut())
            {
                raymarch.set_custom_field(gpu, None);
                raster.set_custom_field(gpu, None);
            }
            self.flsl_shape_slots.clear();
        }
    }

    /// The IDE's live `.flsl` diagnostic: (1-based line, message) for the
    /// current text of an open buffer — parse + type errors (stage-agnostic:
    /// fragment and sdf shaders both squiggle), no GPU involved.
    pub(crate) fn check_flsl_syntax(text: &str) -> Option<(usize, String)> {
        match floptle_shader::check_source(text) {
            Ok(()) => None,
            Err(msg) => {
                let first = msg.lines().next().unwrap_or(&msg);
                let line = first
                    .split(':')
                    .next()
                    .and_then(|l| l.parse::<usize>().ok())
                    .unwrap_or(1);
                Some((line, first.to_string()))
            }
        }
    }
}

/// One Sdf-stage `.flsl` file's parse/check state (Field Shapes — proposal §7).
pub(crate) struct SdfEntry {
    mtime: Option<SystemTime>,
    /// Parse + check result: per-slot transpilation happens at splice time,
    /// and the Inspector reads the uniform schema from the IR.
    pub(crate) parsed: Option<(floptle_shader::ShaderIr, floptle_shader::ir::Checked)>,
    pub(crate) error: Option<String>,
    /// Bumped on every successful recompile — the splice key.
    generation: u64,
}

impl Editor {
    /// Per-frame driver for Field Shapes: hot-reload their sdf shaders, and
    /// when the (entity, shader, generation) set changes, transpile per slot
    /// and splice `custom_d`/`custom_col` into BOTH passes. Runs right after
    /// `ensure_flsl_materials` — same pattern, the field mirror's version.
    pub(crate) fn sync_field_shapes(&mut self) {
        if self.gpu.is_none() || self.raster.is_none() || self.raymarch.is_none() {
            return;
        }
        // Stable slot order: by entity index (creation order survives frames).
        let mut shapes: Vec<(Entity, String)> = self
            .world
            .query::<floptle_core::Matter>()
            .filter(|(_, m)| matches!(m, floptle_core::Matter::FieldShape { .. }))
            .filter_map(|(e, _)| {
                let shader = self.world.get::<Material>(e).and_then(|m| m.shader.clone())?;
                Some((e, shader))
            })
            .collect();
        shapes.sort_by_key(|(e, _)| e.index());
        let truncated = shapes.len() > floptle_render::MAX_FIELD_SHAPES;
        shapes.truncate(floptle_render::MAX_FIELD_SHAPES);

        let mut paths: Vec<String> = shapes.iter().map(|(_, p)| p.clone()).collect();
        paths.sort();
        paths.dedup();
        for p in &paths {
            self.ensure_sdf_shader(p);
        }

        let key: Vec<(Entity, String, u64)> = shapes
            .iter()
            .map(|(e, p)| {
                (*e, p.clone(), self.sdf_cache.get(p).map(|s| s.generation).unwrap_or(0))
            })
            .collect();
        if key == self.flsl_field_key {
            return;
        }
        self.flsl_field_key = key;
        if truncated {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "scene has more than {} Field Shapes — extras don't render",
                    floptle_render::MAX_FIELD_SHAPES
                ),
                None,
            );
        }

        // Transpile every shape for its slot; a shape whose shader is broken
        // (or missing) simply drops out until it compiles.
        let mut dist_fns = String::new();
        let mut col_fns = String::new();
        let mut slots: HashMap<Entity, usize> = HashMap::new();
        for (e, path) in &shapes {
            let Some((ir, ck)) =
                self.sdf_cache.get(path).and_then(|s| s.parsed.as_ref())
            else {
                continue;
            };
            let slot = slots.len();
            match floptle_shader::transpile_sdf(ir, ck, slot) {
                Ok(c) => {
                    dist_fns.push_str(&c.dist_fn);
                    col_fns.push_str(&c.col_fn);
                    slots.insert(*e, slot);
                }
                Err(err) => {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("◈ {path}: {}", err.message),
                        None,
                    );
                }
            }
        }

        if slots.is_empty() {
            // Nothing splice-able: restore the byte-identical baseline passes.
            if !self.flsl_shape_slots.is_empty() {
                let (Some(gpu), Some(raster), Some(raymarch)) =
                    (self.gpu.as_ref(), self.raster.as_mut(), self.raymarch.as_mut())
                else {
                    return;
                };
                raymarch.set_custom_field(gpu, None);
                raster.set_custom_field(gpu, None);
                self.flsl_shape_slots.clear();
            }
            return;
        }

        // The combined fold + nearest-shape picker over the live slots.
        let n = slots.len();
        let mut field_code = dist_fns;
        field_code.push_str("fn custom_d(p: vec3<f32>) -> f32 {\n    var d = 1e9;\n");
        for i in 0..n {
            field_code.push_str(&format!("    d = min(d, flsl_shape{i}_d(p));\n"));
        }
        field_code.push_str("    return d;\n}\n");
        let mut color_code = col_fns;
        color_code.push_str(
            "fn custom_col(p: vec3<f32>) -> Matter {\n    var m = Matter(1e9, vec3<f32>(0.0));\n",
        );
        for i in 0..n {
            color_code.push_str(&format!(
                "    let d{i} = flsl_shape{i}_d(p);\n    if (d{i} < m.d) {{ m = Matter(d{i}, flsl_shape{i}_col(p)); }}\n"
            ));
        }
        color_code.push_str("    return m;\n}\n");
        color_code.push_str(
            "fn nearest_shape(p: vec3<f32>) -> i32 {\n    var bi = 0;\n    var bd = 1e9;\n",
        );
        for i in 0..n {
            color_code.push_str(&format!(
                "    let s{i} = flsl_shape{i}_d(p);\n    if (s{i} < bd) {{ bd = s{i}; bi = {i}; }}\n"
            ));
        }
        color_code.push_str("    return bi;\n}\n");

        // naga-gate BOTH assembled modules before swapping any pipeline —
        // a bad splice must never panic the pass builders.
        let support = floptle_shader::stdlib::SUPPORT_WGSL;
        let rm_src = floptle_render::Raymarch::preview_custom_source(Some((
            &field_code,
            &color_code,
            support,
        )));
        let raster_src = floptle_render::raster_custom_source(Some((&field_code, support)));
        for (what, src) in [("raymarch", &rm_src), ("raster", &raster_src)] {
            if let Err(d) = floptle_shader::validate_module(src) {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("◈ field shapes: generated {what} module rejected: {}", d.message),
                    None,
                );
                return;
            }
        }
        let (Some(gpu), Some(raster), Some(raymarch)) =
            (self.gpu.as_ref(), self.raster.as_mut(), self.raymarch.as_mut())
        else {
            return;
        };
        raymarch.set_custom_field(gpu, Some((&field_code, &color_code, support)));
        raster.set_custom_field(gpu, Some((&field_code, support)));
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("◈ field: {n} shape{} spliced into the scene field", if n == 1 { "" } else { "s" }),
            None,
        );
        self.flsl_shape_slots = slots;
    }

    /// Compile (or hot-reload) one Sdf-stage `.flsl` by material path.
    fn ensure_sdf_shader(&mut self, rel: &str) {
        let full = self.resolve_asset_path(rel);
        let mtime = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
        if let Some(entry) = self.sdf_cache.get(rel)
            && entry.mtime == mtime
        {
            return;
        }
        let prev_gen = self.sdf_cache.get(rel).map(|s| s.generation).unwrap_or(0);
        let outcome = std::fs::read_to_string(&full)
            .map_err(|e| format!("can't read shader ({e})"))
            .and_then(|src| floptle_shader::check_sdf(&src));
        match outcome {
            Ok(parsed) => {
                self.sdf_cache.insert(
                    rel.to_string(),
                    SdfEntry {
                        mtime,
                        parsed: Some(parsed),
                        error: None,
                        generation: prev_gen + 1,
                    },
                );
            }
            Err(msg) => {
                let changed =
                    self.sdf_cache.get(rel).is_none_or(|e| e.error.as_deref() != Some(&msg));
                if changed {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("◈ {rel}: {msg}"),
                        None,
                    );
                }
                let old = self.sdf_cache.remove(rel);
                self.sdf_cache.insert(
                    rel.to_string(),
                    SdfEntry {
                        mtime,
                        parsed: old.and_then(|o| o.parsed),
                        error: Some(msg),
                        generation: prev_gen,
                    },
                );
            }
        }
    }

}

/// Fill the raymarch globals' Field Shape arrays for this frame: camera-
/// relative transforms, bounding radii, shader uniform values and the node
/// Material's surface response. `only` parks every OTHER shape out of
/// existence (the selection-outline mask marches just one). A free function
/// over disjoint Editor fields so callers can hold the GPU stack borrowed.
pub(crate) fn apply_field_shapes(
    world: &floptle_core::World,
    slots: &HashMap<Entity, usize>,
    sdf: &SdfCache,
    g: &mut floptle_render::RaymarchGlobals,
    cam_pos: floptle_core::math::DVec3,
    only: Option<Entity>,
) {
    let count = slots.values().max().map(|m| m + 1).unwrap_or(0);
    g.shape_meta = [count as f32, 0.0, 0.0, 0.0];
    for (&e, &slot) in slots {
        if only.is_some_and(|o| o != e) || !world.is_alive(e) {
            // Parked: unreachable position + zero bound = never surfaces.
            g.shape_pos[slot] = [1e7, 1e7, 1e7, 1.0];
            g.shape_aux[slot] = [0.0; 4];
            continue;
        }
        {
            let t = floptle_core::world_transform(world, e);
            let rel = (t.translation - cam_pos).as_vec3();
            let scale = t.scale.x.max(1e-4);
            let radius = match world.get::<floptle_core::Matter>(e) {
                Some(floptle_core::Matter::FieldShape { radius }) => *radius,
                _ => 1.0,
            };
            let q = t.rotation.inverse();
            g.shape_pos[slot] = [rel.x, rel.y, rel.z, scale];
            g.shape_rot[slot] = [q.x, q.y, q.z, q.w];
            g.shape_aux[slot] = [radius * scale, 0.0, 0.0, 0.0];
            let Some(mat) = world.get::<Material>(e) else { continue };
            if let Some((ir, _)) = mat
                .shader
                .as_deref()
                .and_then(|p| sdf.get(p))
                .and_then(|s| s.parsed.as_ref())
            {
                for (i, u) in ir.uniforms.iter().enumerate().take(16) {
                    g.shape_uniforms[slot * 16 + i] =
                        mat.shader_params.get(&u.name).copied().unwrap_or(u.default);
                }
            }
            g.shape_tint[slot] = [mat.color[0], mat.color[1], mat.color[2], 0.0];
            g.shape_emissive[slot] = [
                mat.emissive[0],
                mat.emissive[1],
                mat.emissive[2],
                mat.emissive_strength,
            ];
            g.shape_specular[slot] = [
                mat.specular[0],
                mat.specular[1],
                mat.specular[2],
                mat.specular_strength,
            ];
            g.shape_params[slot] = [
                mat.shininess,
                mat.rim_strength,
                if mat.unlit { 1.0 } else { 0.0 },
                mat.ambient,
            ];
            g.shape_rim[slot] = [mat.rim[0], mat.rim[1], mat.rim[2], 0.0];
        }
    }
}

/// A core [`Tiling`](floptle_core::Tiling) as the transpiler's packed param
/// lanes (rotation converted to radians here — the GPU never sees degrees).
fn tiling_pack(t: &floptle_core::Tiling) -> floptle_shader::TilingPack {
    match *t {
        floptle_core::Tiling::Uv { count, offset, rotation } => floptle_shader::TilingPack {
            a: [count[0], count[1], offset[0], offset[1]],
            b: [1.0, rotation.to_radians(), 0.0, 0.0],
        },
        floptle_core::Tiling::Triplanar { scale, blend } => {
            floptle_shader::TilingPack { a: [0.0; 4], b: [2.0, 0.0, scale, blend] }
        }
    }
}

/// Editor-side registry fields, bundled for `Editor` (see main.rs).
pub(crate) type FlslCache = HashMap<String, FlslEntry>;
pub(crate) type FlslBinds = HashMap<Entity, FlslMatBind>;
pub(crate) type SdfCache = HashMap<String, SdfEntry>;
