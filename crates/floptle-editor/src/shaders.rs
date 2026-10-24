//! Custom `.flsl` shader materials in the editor.
//!
//! The pipeline: a Material's `shader` names a project `.flsl` file; this
//! module compiles it (parse → check → transpile → naga against the real
//! raster+field sources), registers the pipeline with the raster pass, and
//! keeps one live group(3) binding (params UBO + texture slots) per entity.
//!
//! Hot reload is the house mtime pattern (texture registry, prefab cache):
//! shaders in use are re-stat'ed every frame and recompile on change. A broken
//! edit keeps the last good pipeline running and reports to the Console + the
//! IDE squiggle — a failed save never black-screens the scene.

use std::collections::{HashMap, HashSet};
use floptle_core::time::SystemTime;

use floptle_core::{Entity, Material, ObjectMaterials};
use floptle_render::{FlslBindingId, FlslBlend, FlslShaderId, TexId};
use floptle_shader::CompiledFragment;

use crate::Editor;

/// One `.flsl` file's compile state, keyed by project-relative path.
pub(crate) struct FlslEntry {
    pub(crate) mtime: Option<SystemTime>,
    /// The last good compile — kept while `error` reports a newer failure.
    pub(crate) compiled: Option<(CompiledFragment, FlslShaderId)>,
    /// The newest failure (compile or naga), already line-mapped for humans.
    pub(crate) error: Option<String>,
}

/// What one live binding is made of: the shader, its packed params and the
/// image in each texture slot. Two materials that agree on all three draw the
/// same pixels, so they share one binding, and a draw is batched by its
/// binding.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BindContent {
    shader: u32,
    params: Vec<u8>,
    textures: Vec<Option<u32>>,
}

/// One material's place in the binding pool: which shared binding it draws
/// with, and the content that binding was chosen for — so a frame with no
/// changes does zero GPU work.
pub(crate) struct FlslMatBind {
    pub(crate) binding: FlslBindingId,
    shader: FlslShaderId,
    content: BindContent,
}

/// The live group(3) bindings, one per distinct [`BindContent`], each with
/// the number of materials drawing through it, plus the retired slots.
///
/// One binding per *content*, not per node, because the binding is part of a
/// draw's batch key. A binding per node made three thousand parts given one
/// look three thousand draws, where the same parts with no shader were a
/// handful.
#[derive(Default)]
pub(crate) struct FlslPool {
    live: HashMap<BindContent, (FlslBindingId, u32)>,
    free: Vec<FlslBindingId>,
}

impl FlslPool {
    /// How many distinct bindings are live.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live.len()
    }

    /// One material stopped drawing with `content`; the last one out retires
    /// the slot.
    fn release(&mut self, content: &BindContent) {
        if let Some((id, users)) = self.live.get_mut(content) {
            *users -= 1;
            if *users == 0 {
                let id = *id;
                self.live.remove(content);
                self.free.push(id);
            }
        }
    }

    /// The binding made of `content`: the one already live, or a new one.
    fn acquire(
        &mut self,
        gpu: &floptle_render::Gpu,
        raster: &mut floptle_render::Raster,
        content: &BindContent,
        shader: FlslShaderId,
        textures: &[Option<TexId>],
    ) -> FlslBindingId {
        if let Some((id, users)) = self.live.get_mut(content) {
            *users += 1;
            return *id;
        }
        let reuse = self.free.pop();
        let id = raster.set_flsl_binding(gpu, reuse, shader, &content.params, textures);
        self.live.insert(content.clone(), (id, 1));
        id
    }
}

/// When [`Editor::ensure_flsl_materials`] last walked the scene, and what it
/// depended on. The walk visits every material in the world, and in a scene
/// of thousands of parts that was milliseconds every frame for an answer that
/// had not changed.
#[derive(Default)]
pub(crate) struct FlslGate {
    /// The world's change log as of the last walk. `None`: walk next frame.
    cursor: Option<floptle_core::ChangeCursor>,
    /// Shader compiles so far, and the texture registry's fingerprint, as of
    /// the last walk.
    seen: (u64, u64),
    /// Every shader the last walk found in use, re-checked for edits each
    /// frame whether or not the walk runs.
    paths: Vec<String>,
    /// Bumped whenever a `.flsl` compiles: the param layout may have moved.
    compiles: u64,
    /// Walks run so far, whole or partial, and the materials they planned:
    /// what a test reads to see a quiet frame skip the walk and a one-node
    /// change plan one node.
    pub(crate) walks: u64,
    pub(crate) planned: u64,
    /// Pipelines were built since the pipeline cache was last written (and
    /// `false` before the first write, which keeps the passes' own).
    pub(crate) pipelines_unsaved: bool,
    pipelines_saved_once: bool,
}

/// A number that changes whenever any path in the texture registry moves to
/// a different image. Order-free, because the registry is a hash map.
fn registry_fingerprint(registry: &HashMap<String, TexId>) -> u64 {
    use std::hash::{Hash, Hasher};
    registry.iter().fold(registry.len() as u64, |acc, (k, v)| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        k.hash(&mut h);
        v.0.hash(&mut h);
        acc.wrapping_add(h.finish())
    })
}

/// A plan for one live `.flsl` material binding — shared by whole-node
/// Materials and per-part `ObjectMaterials` overrides, which are compiled and
/// bound through the same machinery under different keys (`K = Entity` for a
/// node, `K = (Entity, String)` for one named part of it).
struct FlslPlan<K> {
    key: K,
    shader: FlslShaderId,
    params: Vec<u8>,
    slot_paths: Vec<Option<String>>,
}

/// Apply one resolved [`FlslPlan`] (textures already looked up) to a binding
/// map: keep the binding when nothing changed, move to the binding already
/// made of the new content when there is one, and otherwise rewrite this
/// material's binding in place when it is the only one drawing with it (a
/// knob dragged every frame stays a uniform write) or take a new one when it
/// is not (copy on write: the others keep their look). `K` is generic so this
/// serves both `FlslBinds` (whole-node) and `ObjFlslBinds` (per-part).
fn apply_flsl_bind<K: Eq + std::hash::Hash>(
    binds: &mut HashMap<K, FlslMatBind>,
    pool: &mut FlslPool,
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    // (key, shader, params, resolved textures) — bundled to stay under the
    // house arg-count limit rather than because they're conceptually one thing.
    resolved: (K, FlslShaderId, Vec<u8>, Vec<Option<TexId>>),
) {
    let (key, shader, params, textures) = resolved;
    let content =
        BindContent { shader: shader.0, params, textures: textures.iter().map(|t| t.map(|t| t.0)).collect() };
    let Some(b) = binds.get_mut(&key) else {
        let binding = pool.acquire(gpu, raster, &content, shader, &textures);
        binds.insert(key, FlslMatBind { binding, shader, content });
        return;
    };
    if b.content == content {
        return;
    }
    let alone = pool.live.get(&b.content).is_some_and(|&(_, users)| users == 1);
    if alone && !pool.live.contains_key(&content) {
        pool.live.remove(&b.content);
        if b.content.shader == content.shader && b.content.textures == content.textures {
            raster.write_flsl_params(gpu, b.binding, &content.params);
        } else {
            b.binding = raster.set_flsl_binding(gpu, Some(b.binding), shader, &content.params, &textures);
        }
        pool.live.insert(content.clone(), (b.binding, 1));
    } else {
        pool.release(&b.content);
        b.binding = pool.acquire(gpu, raster, &content, shader, &textures);
    }
    b.shader = shader;
    b.content = content;
}

/// Drop every binding in `binds` that `keep` refuses, handing its share back
/// to the pool.
fn retire_binds<K: Eq + std::hash::Hash>(
    binds: &mut HashMap<K, FlslMatBind>,
    pool: &mut FlslPool,
    mut keep: impl FnMut(&K, &FlslMatBind) -> bool,
) {
    binds.retain(|k, b| {
        let stays = keep(k, b);
        if !stays {
            pool.release(&b.content);
        }
        stays
    });
}

impl Editor {
    /// Per-frame driver, called before any gather: hot-reload every `.flsl`
    /// the scene references, then create/refresh each shader-material
    /// entity's group(3) binding. Unchanged frames cost a few file stats.
    ///
    /// Two sources feed this, compiled and bound through the same machinery:
    /// a node's own whole-model `Material` (`flsl_binds`, keyed by entity) and
    /// each part of an `ObjectMaterials` override that names a shader of its
    /// own (`obj_flsl_binds`, keyed by entity + the part's override key). A
    /// part's override is a whole material superseding the node's — its
    /// shader-or-not is part of that whole — so it gets its own entry rather
    /// than inheriting whatever the node happens to have. Entries made of the
    /// same content share one binding ([`FlslPool`]).
    ///
    /// Only what may have changed is walked. A frame where no material, model
    /// override or matter changed and no node was made or destroyed walks
    /// nothing; one where a few nodes did walks those nodes; a recompiled
    /// shader, a changed texture registry, or a change log that cannot say
    /// what happened walks every material.
    pub(crate) fn ensure_flsl_materials(&mut self) {
        if self.gpu.is_none() || self.raster.is_none() {
            return;
        }
        // Pipelines built last frame go to the cache on disk now, once the
        // frame that needed them has been drawn.
        if self.flsl_gate.pipelines_unsaved || !self.flsl_gate.pipelines_saved_once {
            if let Some(gpu) = self.gpu.as_ref() {
                gpu.save_pipeline_cache();
            }
            self.flsl_gate.pipelines_unsaved = false;
            self.flsl_gate.pipelines_saved_once = true;
        }
        // Edits to a shader already in use land whether or not the walk runs.
        for p in self.flsl_gate.paths.clone() {
            self.ensure_flsl_shader(&p);
        }
        let seen = (self.flsl_gate.compiles, registry_fingerprint(&self.texture_registry));
        // `None`: walk everything. `Some`: the entity indices that may have
        // changed since the last walk.
        let only: Option<HashSet<u32>> = match self.flsl_gate.cursor {
            Some(cursor) if self.flsl_gate.seen == seen => self.world.changes_since(cursor).map(|changes| {
                use std::any::TypeId;
                let read = [TypeId::of::<Material>(), TypeId::of::<ObjectMaterials>(), TypeId::of::<floptle_core::Matter>()];
                changes
                    .iter()
                    .filter(|c| match c.kind {
                        floptle_core::ChangeKind::Lifetime => true,
                        floptle_core::ChangeKind::Component(t) => read.contains(&t),
                    })
                    .map(|c| c.index)
                    .collect()
            }),
            _ => None,
        };
        if only.as_ref().is_some_and(HashSet::is_empty) {
            self.flsl_gate.cursor = Some(self.world.change_cursor());
            return;
        }
        let wanted = |e: &Entity| only.as_ref().is_none_or(|o| o.contains(&e.index()));
        self.flsl_gate.walks += 1;
        // Field Shape nodes carry SDF-stage shaders — those go through
        // `sync_field_shapes`, not the fragment-material path.
        let field_shape: HashSet<Entity> = self
            .world
            .query::<floptle_core::Matter>()
            .filter(|(e, m)| wanted(e) && matches!(m, floptle_core::Matter::FieldShape { .. }))
            .map(|(e, _)| e)
            .collect();

        let mut paths: Vec<String> = {
            let mut set: HashSet<&str> = HashSet::new();
            for (e, m) in self.world.query::<Material>() {
                if let Some(s) = m.shader.as_deref()
                    && wanted(&e)
                    && !field_shape.contains(&e)
                {
                    set.insert(s);
                }
            }
            for (e, om) in self.world.query::<ObjectMaterials>() {
                if !wanted(&e) || field_shape.contains(&e) {
                    continue;
                }
                set.extend(om.0.values().filter_map(|m| m.shader.as_deref()));
            }
            // A partial walk adds to the shaders in use; only a whole one can
            // say which are no longer.
            if only.is_some() {
                set.extend(self.flsl_gate.paths.iter().map(String::as_str));
            }
            set.into_iter().map(str::to_string).collect()
        };
        paths.sort();
        for p in &paths {
            if !self.flsl_gate.paths.contains(p) {
                self.ensure_flsl_shader(p);
            }
        }
        self.flsl_gate.paths = paths;

        // Plan each binding from the compile cache (immutable pass), then
        // resolve textures + touch the GPU (mutable pass) — one round for the
        // node-level materials, one for the per-part overrides. The plans
        // borrow the world's materials rather than cloning them: in a scene of
        // thousands of parts the clones were most of the cost.
        let plan_of = |cache: &FlslCache, m: &Material| -> Option<(FlslShaderId, Vec<u8>, Vec<Option<String>>)> {
            let rel = m.shader.as_deref().unwrap_or_default();
            let (compiled, shader) = cache.get(rel).and_then(|en| en.compiled.as_ref())?;
            let params = compiled.pack_params(
                &|name| m.shader_params.get(name).copied(),
                &|slot| m.shader_tiling.get(slot).map(tiling_pack),
            );
            // The material's override wins; a slot it leaves empty falls back to
            // the image the shader itself declares, so a texture shader looks
            // right the moment it's assigned instead of sampling nothing.
            let slot_paths = compiled
                .textures
                .iter()
                .zip(&compiled.texture_defaults)
                .map(|(slot, dflt)| m.shader_textures.get(slot).cloned().or_else(|| dflt.clone()))
                .collect();
            Some((*shader, params, slot_paths))
        };
        let plans: Vec<FlslPlan<Entity>> = self
            .world
            .query::<Material>()
            .filter(|(e, m)| m.shader.is_some() && wanted(e) && !field_shape.contains(e))
            .filter_map(|(e, m)| {
                let (shader, params, slot_paths) = plan_of(&self.flsl_cache, m)?;
                Some(FlslPlan { key: e, shader, params, slot_paths })
            })
            .collect();
        let obj_plans: Vec<FlslPlan<(Entity, String)>> = self
            .world
            .query::<ObjectMaterials>()
            .filter(|(e, _)| wanted(e) && !field_shape.contains(e))
            .flat_map(|(e, om)| {
                om.0.iter().filter(|(_, m)| m.shader.is_some()).map(move |(k, m)| (e, k, m))
            })
            .filter_map(|(e, key, m)| {
                let (shader, params, slot_paths) = plan_of(&self.flsl_cache, m)?;
                Some(FlslPlan { key: (e, key.clone()), shader, params, slot_paths })
            })
            .collect();

        // A slot naming an image that is not there yet keeps the walk running
        // every frame, as it always did, so the image is picked up when it
        // appears.
        let mut missing = false;
        let mut resolve = |ed: &mut Editor, slot_paths: &[Option<String>]| -> Vec<Option<TexId>> {
            slot_paths
                .iter()
                .map(|p| {
                    let t = p.as_deref().and_then(|p| ed.ensure_texture(p));
                    missing |= p.is_some() && t.is_none();
                    t
                })
                .collect()
        };
        // The binding work, apart from the textures it waited for (those
        // name themselves): what `materialBind` reports.
        let mut bind_ms = 0.0f32;
        let mut seen: HashSet<Entity> = HashSet::new();
        for plan in plans {
            seen.insert(plan.key);
            let textures = resolve(self, &plan.slot_paths);
            let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                return;
            };
            let bind = floptle_core::profile::Span::new();
            apply_flsl_bind(
                &mut self.flsl_binds, &mut self.flsl_pool, gpu, raster,
                (plan.key, plan.shader, plan.params, textures),
            );
            bind_ms += bind.ms();
        }
        self.flsl_gate.planned += (seen.len()) as u64;
        // Entities that lost their shader material give their share back —
        // of those walked: one not walked has not changed.
        retire_binds(&mut self.flsl_binds, &mut self.flsl_pool, |e, _| !wanted(e) || seen.contains(e));

        let mut obj_seen: HashSet<(Entity, String)> = HashSet::new();
        for plan in obj_plans {
            obj_seen.insert(plan.key.clone());
            let textures = resolve(self, &plan.slot_paths);
            let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                return;
            };
            let bind = floptle_core::profile::Span::new();
            apply_flsl_bind(
                &mut self.obj_flsl_binds, &mut self.flsl_pool, gpu, raster,
                (plan.key, plan.shader, plan.params, textures),
            );
            bind_ms += bind.ms();
        }
        if bind_ms > 0.0 {
            self.script_host.profile().borrow_mut().record_part("materialBind", bind_ms);
        }
        self.flsl_gate.planned += obj_seen.len() as u64;
        retire_binds(&mut self.obj_flsl_binds, &mut self.flsl_pool, |k, _| !wanted(&k.0) || obj_seen.contains(k));

        self.flsl_gate.seen = (self.flsl_gate.compiles, registry_fingerprint(&self.texture_registry));
        self.flsl_gate.cursor = (!missing).then(|| self.world.change_cursor());
    }

    /// Compile (or hot-reload) one `.flsl` by its Material path (asset-tree or
    /// project-relative). Keeps the last good pipeline on failure;
    /// Console-reports each new error once.
    pub(crate) fn ensure_flsl_shader(&mut self, rel: &str) {
        // A warm already building it is never built a second time beside
        // it: the frame draws without it until it lands.
        if self.adopt_warm_shader(rel) {
            return;
        }
        let full = self.resolve_asset_path(rel);
        let mtime = floptle_vfs::modified(&full);
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

        let src = match floptle_vfs::read_to_string(&full) {
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

        let compile = floptle_core::profile::Span::new();
        let outcome = compile_fragment_validated(&src);
        self.note_asset(rel, "shaderCompile", compile.ms());

        match outcome {
            Ok(compiled) => {
                // A shader this GPU cannot bind is said by name here, and
                // keeps the last build that worked: pipeline validation would
                // only say a count was over a limit.
                if let Some(gpu) = self.gpu.as_ref()
                    && let Err(why) = floptle_render::Raster::flsl_fits(gpu, compiled.textures.len())
                {
                    let msg = report(self, why);
                    let old = self.flsl_cache.remove(rel);
                    self.flsl_cache.insert(
                        rel.to_string(),
                        FlslEntry { mtime, compiled: old.and_then(|o| o.compiled), error: Some(msg) },
                    );
                    return;
                }
                self.install_flsl(rel, mtime, compiled, None);
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

    /// Put a compiled material shader in use under `rel`: its pipeline built
    /// here, or the one `built` on a worker installed, the bindings of the
    /// shader it replaces retired, and the compile recorded.
    pub(crate) fn install_flsl(
        &mut self,
        rel: &str,
        mtime: Option<SystemTime>,
        compiled: CompiledFragment,
        built: Option<floptle_render::BuiltFlsl>,
    ) {
        let replace = self.flsl_cache.get(rel).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id);
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return;
        };
        let build = floptle_core::profile::Span::new();
        let here = |raster: &mut floptle_render::Raster| {
            raster.register_flsl_shader(gpu, &flsl_chunk(&compiled), compiled.textures.len(), flsl_blend(compiled.blend), replace)
        };
        // A worker's build is installed as it is; one that raced a Field
        // Shape change is built again here.
        let (id, built_here) = match built {
            Some(b) => match raster.install_flsl_shader(b, replace) {
                Ok(id) => (id, false),
                Err(_) => (here(raster), true),
            },
            None => (here(raster), true),
        };
        if built_here {
            self.note_asset(rel, "pipelineCompile", build.ms());
        }
        self.flsl_gate.pipelines_unsaved = true;
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("◈ compiled {rel}"),
            None,
        );
        // The pipeline (and possibly its group(3) layout) changed:
        // retire every live binding of this shader so the next
        // ensure pass rebuilds them against the fresh layout.
        // Per-part overrides too: they bind the same shader through the
        // same layout.
        retire_binds(&mut self.flsl_binds, &mut self.flsl_pool, |_, b| b.shader != id);
        retire_binds(&mut self.obj_flsl_binds, &mut self.flsl_pool, |_, b| b.shader != id);
        self.flsl_gate.compiles += 1;
        self.flsl_cache
            .insert(rel.to_string(), FlslEntry { mtime, compiled: Some((compiled, id)), error: None });
    }

    /// Drop every flsl cache (project switch — the raster pass was rebuilt).
    pub(crate) fn clear_flsl_state(&mut self) {
        #[cfg(feature = "editor-ui")]
        {
            self.shader_graph = Default::default();
        }
        self.flsl_cache.clear();
        self.flsl_binds.clear();
        self.obj_flsl_binds.clear();
        self.flsl_pool = Default::default();
        self.flsl_gate = Default::default();
        self.ui_flsl_cache.clear();
        self.ui_flsl_binds.clear();
        self.ui_flsl_free.clear();
        self.post_flsl_cache.clear();
        if let Some(reg) = self.post_shaders.as_mut() {
            reg.clear();
        }
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

/// One SDF-stage `.flsl` file's parse/check state (Field Shapes — proposal §7).
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
    /// and splice `custom_d`/`custom_col` into both passes. Runs right after
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

        // naga-gate both assembled modules before swapping any pipeline —
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

    /// Compile (or hot-reload) one SDF-stage `.flsl` by material path.
    fn ensure_sdf_shader(&mut self, rel: &str) {
        let full = self.resolve_asset_path(rel);
        let mtime = floptle_vfs::modified(&full);
        if let Some(entry) = self.sdf_cache.get(rel)
            && entry.mtime == mtime
        {
            return;
        }
        let prev_gen = self.sdf_cache.get(rel).map(|s| s.generation).unwrap_or(0);
        let outcome = floptle_vfs::read_to_string(&full)
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
/// Material's surface response. `only` parks every other shape out of
/// existence (the selection-outline mask marches just one). A free function
/// over disjoint Editor fields so callers can hold the GPU stack borrowed.
pub(crate) fn apply_field_shapes(
    world: &floptle_core::World,
    slots: &HashMap<Entity, usize>,
    sdf: &SdfCache,
    g: &mut floptle_render::RaymarchGlobals,
    cam_pos: floptle_core::math::DVec3,
    only: Option<&[Entity]>,
) {
    let count = slots.values().max().map(|m| m + 1).unwrap_or(0);
    g.shape_meta = [count as f32, 0.0, 0.0, 0.0];
    for (&e, &slot) in slots {
        if only.is_some_and(|o| !o.contains(&e)) || !world.is_alive(e) {
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
/// One binding per named part of an `ObjectMaterials` override that names its
/// own shader — see [`Editor::ensure_flsl_materials`].
pub(crate) type ObjFlslBinds = HashMap<(Entity, String), FlslMatBind>;

/// One `stage ui` `.flsl` file's compile state, keyed by element shader path.
pub(crate) struct UiFlslEntry {
    pub(crate) mtime: Option<SystemTime>,
    /// The last good compile — kept while `error` reports a newer failure.
    pub(crate) compiled: Option<(floptle_shader::CompiledUi, floptle_render::UiShaderId)>,
    pub(crate) error: Option<String>,
}

/// One element's live UI-shader params binding + what built it.
pub(crate) struct UiFlslBind {
    pub(crate) binding: floptle_render::UiBindingId,
    shader: floptle_render::UiShaderId,
    params: Vec<u8>,
}

pub(crate) type UiFlslCache = HashMap<String, UiFlslEntry>;
/// Keyed by entity index — the id the UI draw list carries per quad.
pub(crate) type UiFlslBinds = HashMap<u32, UiFlslBind>;

impl Editor {
    /// Per-frame driver for `stage ui` element shaders: hot-reload every
    /// `.flsl` a UI element references, then keep one params UBO per element
    /// (uniform writes when knobs move, zero GPU work when nothing changed).
    pub(crate) fn ensure_ui_shaders(&mut self) {
        if self.gpu.is_none() || self.ui_render.is_none() {
            return;
        }
        let elems: Vec<(Entity, String, std::collections::BTreeMap<String, [f32; 4]>)> = self
            .world
            .query::<floptle_ui::ElementSpec>()
            .filter(|(_, s)| !s.shader.is_empty())
            .map(|(e, s)| (e, s.shader.clone(), s.shader_params.clone()))
            .collect();

        let mut paths: Vec<String> = elems.iter().map(|(_, p, _)| p.clone()).collect();
        paths.sort();
        paths.dedup();
        for p in &paths {
            self.ensure_ui_shader(p);
        }

        let mut seen: HashSet<u32> = HashSet::new();
        for (e, path, overrides) in &elems {
            let Some((compiled, shader)) =
                self.ui_flsl_cache.get(path).and_then(|en| en.compiled.as_ref())
            else {
                continue; // never compiled — the element draws its fallback quad
            };
            let params = compiled.pack_params(&|name| overrides.get(name).copied());
            let (shader, eid) = (*shader, e.index());
            seen.insert(eid);
            let (Some(gpu), Some(uir)) = (self.gpu.as_ref(), self.ui_render.as_mut()) else {
                return;
            };
            match self.ui_flsl_binds.get_mut(&eid) {
                Some(b) if b.shader == shader => {
                    if b.params != params {
                        uir.write_ui_shader_params(gpu, b.binding, &params);
                        b.params = params;
                    }
                }
                Some(b) => {
                    b.binding = uir.set_ui_shader_binding(gpu, &params, Some(b.binding));
                    b.shader = shader;
                    b.params = params;
                }
                None => {
                    let reuse = self.ui_flsl_free.pop();
                    let binding = uir.set_ui_shader_binding(gpu, &params, reuse);
                    self.ui_flsl_binds.insert(eid, UiFlslBind { binding, shader, params });
                }
            }
        }

        let stale: Vec<u32> =
            self.ui_flsl_binds.keys().copied().filter(|e| !seen.contains(e)).collect();
        for e in stale {
            if let Some(b) = self.ui_flsl_binds.remove(&e) {
                self.ui_flsl_free.push(b.binding);
            }
        }
    }

    /// Compile (or hot-reload) one `stage ui` `.flsl`. Keeps the last good
    /// pipeline on failure; Console-reports each new error once.
    fn ensure_ui_shader(&mut self, rel: &str) {
        // A warm already building it is never built a second time beside
        // it: the frame draws without it until it lands.
        if self.adopt_warm_shader(rel) {
            return;
        }
        let full = self.resolve_asset_path(rel);
        let mtime = floptle_vfs::modified(&full);
        if let Some(entry) = self.ui_flsl_cache.get(rel)
            && entry.mtime == mtime
        {
            return;
        }
        let report = |editor: &mut Editor, msg: String| {
            let changed =
                editor.ui_flsl_cache.get(rel).is_none_or(|e| e.error.as_deref() != Some(&msg));
            if changed {
                editor.console.push(
                    floptle_script::LogLevel::Error,
                    format!("◈ {rel}: {msg}"),
                    None,
                );
            }
            msg
        };
        // In the game, built on the warm's workers: a scene's HUD shaders
        // were built on the frame it loaded (Solar's navball 30 ms, its
        // tape 18), the element drawn without its shader until it lands.
        // One that fails there is compiled here, which says why.
        if self.playing && self.warm_shader_instead(rel) {
            return;
        }
        let src = match floptle_vfs::read_to_string(&full) {
            Ok(s) => s,
            Err(e) => {
                let msg = report(self, format!("can't read shader ({e})"));
                let old = self.ui_flsl_cache.remove(rel);
                self.ui_flsl_cache.insert(
                    rel.to_string(),
                    UiFlslEntry { mtime, compiled: old.and_then(|o| o.compiled), error: Some(msg) },
                );
                return;
            }
        };
        let compile = floptle_core::profile::Span::new();
        let outcome = compile_ui_validated(&src);
        self.note_asset(rel, "shaderCompile", compile.ms());
        match outcome {
            Ok(compiled) => {
                let Some(builder) = self.gpu.as_ref().zip(self.ui_render.as_ref()).map(|(g, u)| u.ui_shader_builder(g))
                else {
                    return;
                };
                let build = floptle_core::profile::Span::new();
                let built = builder.build(&ui_chunk(&compiled));
                self.note_asset(rel, "pipelineCompile", build.ms());
                self.install_ui_flsl(rel, mtime, compiled, built);
            }
            Err(msg) => {
                let msg = report(self, msg);
                let old = self.ui_flsl_cache.remove(rel);
                self.ui_flsl_cache.insert(
                    rel.to_string(),
                    UiFlslEntry { mtime, compiled: old.and_then(|o| o.compiled), error: Some(msg) },
                );
            }
        }
    }
}

impl Editor {
    /// Put a compiled `stage ui` shader in use under `rel`: its pipelines,
    /// built here or on a warm's worker, installed, and the bindings of the
    /// shader it replaces retired.
    pub(crate) fn install_ui_flsl(
        &mut self,
        rel: &str,
        mtime: Option<SystemTime>,
        compiled: floptle_shader::CompiledUi,
        built: floptle_render::BuiltUiShader,
    ) {
        let replace = self.ui_flsl_cache.get(rel).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id);
        let Some(uir) = self.ui_render.as_mut() else { return };
        let id = uir.install_ui_shader(built, replace);
        self.console.push(floptle_script::LogLevel::Debug, format!("◈ compiled {rel} (ui)"), None);
        // Param layout may have changed shape: retire live bindings of
        // this shader so the next ensure pass rebuilds them.
        let stale: Vec<u32> = self.ui_flsl_binds.iter().filter(|(_, b)| b.shader == id).map(|(e, _)| *e).collect();
        for e in stale {
            if let Some(b) = self.ui_flsl_binds.remove(&e) {
                self.ui_flsl_free.push(b.binding);
            }
        }
        self.ui_flsl_cache.insert(rel.to_string(), UiFlslEntry { mtime, compiled: Some((compiled, id)), error: None });
    }

    /// Put a compiled screen shader in use under `rel`: its pipeline, built
    /// here or on a warm's worker, installed in the scene's registry.
    pub(crate) fn install_post_flsl(
        &mut self,
        rel: &str,
        mtime: Option<SystemTime>,
        compiled: floptle_shader::CompiledPost,
        built: floptle_render::BuiltPostShader,
    ) {
        let replace = self.post_flsl_cache.get(rel).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id);
        let Some(reg) = self.post_shaders.as_mut() else { return };
        let id = reg.install(built, replace);
        self.console.push(floptle_script::LogLevel::Debug, format!("◈ compiled {rel} (screen)"), None);
        self.post_flsl_cache.insert(rel.to_string(), PostFlslEntry { mtime, compiled: Some((compiled, id)), error: None });
    }
}

pub(crate) type SdfCache = HashMap<String, SdfEntry>;

/// One `stage post` `.flsl` file's compile state, keyed by its path on the
/// PostProcess node.
pub(crate) struct PostFlslEntry {
    pub(crate) mtime: Option<SystemTime>,
    /// The last good compile — kept while `error` reports a newer failure, so a
    /// broken save leaves the effect running instead of dropping the frame back
    /// to un-processed mid-edit.
    pub(crate) compiled: Option<(floptle_shader::CompiledPost, floptle_render::PostShaderId)>,
    pub(crate) error: Option<String>,
}

pub(crate) type PostFlslCache = HashMap<String, PostFlslEntry>;

impl Editor {
    /// Per-frame driver for the scene's **screen shaders**: hot-reload every
    /// `stage post` `.flsl` the PostProcess node lists, then hand the renderer
    /// this frame's ordered pass list with each pass's knob values.
    ///
    /// Every listed shader is compiled, including the switched-off ones. A
    /// pipeline build is expensive enough to see, and a checkbox that stalls for
    /// a frame the first time you tick it reads as the effect being slow rather
    /// than as it being compiled.
    pub(crate) fn ensure_post_shaders(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else { return };
        if self.post_shaders.is_none() {
            self.post_shaders = Some(floptle_render::PostShaders::new(gpu));
        }

        // The same two rules `post_process_uniforms` follows, and they have to
        // be the same two: a switched-off node runs no chain, screen shaders
        // included, and a node on a disabled layer is not the scene's node at
        // all (so it is skipped rather than vetoing the layer that replaced it).
        let listed: Vec<floptle_core::ScreenShader> = self
            .world
            .query::<floptle_core::Matter>()
            .filter(|(e, _)| self.world.get::<floptle_core::Disabled>(*e).is_none())
            .find_map(|(_, m)| match m {
                floptle_core::Matter::PostProcess { screen_shaders, enabled, .. } => {
                    Some(if *enabled { screen_shaders.clone() } else { Vec::new() })
                }
                _ => None,
            })
            .unwrap_or_default();

        // Nothing listed and nothing left over: don't even touch the registry.
        if listed.is_empty() {
            if self.post_shaders.as_ref().is_some_and(|s| s.has_passes())
                && let (Some(gpu), Some(reg)) = (self.gpu.as_ref(), self.post_shaders.as_mut())
            {
                reg.set_passes(gpu, &[]);
            }
            return;
        }

        let mut paths: Vec<String> = listed.iter().map(|s| s.shader.clone()).collect();
        paths.sort();
        paths.dedup();
        for p in &paths {
            self.ensure_post_shader(p);
        }

        let passes: Vec<(floptle_render::PostShaderId, Vec<u8>)> = listed
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| {
                let (compiled, id) =
                    self.post_flsl_cache.get(&s.shader).and_then(|e| e.compiled.as_ref())?;
                Some((*id, compiled.pack_params(&|name| s.params.get(name).copied())))
            })
            .collect();
        let (Some(gpu), Some(reg)) = (self.gpu.as_ref(), self.post_shaders.as_mut()) else {
            return;
        };
        reg.set_passes(gpu, &passes);
    }

    /// Compile (or hot-reload) one `stage post` `.flsl`. Keeps the last good
    /// pipeline on failure; Console-reports each new error once.
    fn ensure_post_shader(&mut self, rel: &str) {
        // A warm already building it is never built a second time beside
        // it: the frame draws without it until it lands.
        if self.adopt_warm_shader(rel) {
            return;
        }
        let full = self.resolve_asset_path(rel);
        let mtime = floptle_vfs::modified(&full);
        if let Some(entry) = self.post_flsl_cache.get(rel)
            && entry.mtime == mtime
        {
            return;
        }
        let report = |editor: &mut Editor, msg: String| {
            let changed =
                editor.post_flsl_cache.get(rel).is_none_or(|e| e.error.as_deref() != Some(&msg));
            if changed {
                editor.console.push(floptle_script::LogLevel::Error, format!("◈ {rel}: {msg}"), None);
            }
            msg
        };
        let keep = |editor: &mut Editor, mtime, msg: String| {
            let old = editor.post_flsl_cache.remove(rel);
            editor.post_flsl_cache.insert(
                rel.to_string(),
                PostFlslEntry { mtime, compiled: old.and_then(|o| o.compiled), error: Some(msg) },
            );
        };

        let src = match floptle_vfs::read_to_string(&full) {
            Ok(s) => s,
            Err(e) => {
                let msg = report(self, format!("can't read shader ({e})"));
                keep(self, mtime, msg);
                return;
            }
        };
        let compile = floptle_core::profile::Span::new();
        let outcome = compile_post_validated(&src);
        self.note_asset(rel, "shaderCompile", compile.ms());
        match outcome {
            Ok(compiled) => {
                let Some(builder) = self.gpu.as_ref().zip(self.post_shaders.as_ref()).map(|(g, r)| r.builder(g)) else {
                    return;
                };
                let build = floptle_core::profile::Span::new();
                let built = builder.build(&post_module_source(&compiled.chunk));
                self.note_asset(rel, "pipelineCompile", build.ms());
                self.install_post_flsl(rel, mtime, compiled, built);
            }
            Err(msg) => {
                let msg = report(self, msg);
                keep(self, mtime, msg);
            }
        }
    }
}

/// What a post chunk is validated against: the language's own prelude plus the
/// field stand-ins the shared stdlib support needs (a full-screen pass has no
/// field bound).
/// A naga diagnostic, placed on the `.flsl` line that produced it when the
/// compiled chunk can say which one that is.
fn at_flsl_line(
    src: &str,
    d: floptle_shader::transpile::WgslDiag,
    span_of: impl Fn(u32) -> Option<floptle_shader::ir::Span>,
) -> String {
    match d.chunk_line.and_then(span_of) {
        Some(span) => {
            let (l, c) = floptle_shader::text::line_col(src, span.start);
            format!("{l}:{c}: {}", d.message)
        }
        None => d.message,
    }
}

/// A mesh material's shader, compiled and then validated by naga against the
/// real raster + field sources: passing here means the pipeline build cannot
/// fail on the shader. The editor's loader and `floptle check` both call this.
/// The WGSL a compiled material shader is built from: its chunk after the
/// stdlib support it calls.
pub(crate) fn flsl_chunk(compiled: &CompiledFragment) -> String {
    format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk)
}

/// The raster pass's name for a shader's blend.
pub(crate) fn flsl_blend(b: floptle_shader::Blend) -> FlslBlend {
    match b {
        floptle_shader::Blend::Opaque => FlslBlend::Opaque,
        floptle_shader::Blend::Alpha => FlslBlend::Alpha,
        floptle_shader::Blend::Additive => FlslBlend::Additive,
    }
}

pub(crate) fn compile_fragment_validated(src: &str) -> Result<floptle_shader::CompiledFragment, String> {
    let compiled = floptle_shader::compile_fragment(src)?;
    floptle_shader::validate(floptle_render::pass_prelude(), &compiled.chunk)
        .map_err(|d| at_flsl_line(src, d, |l| compiled.flsl_span_of_chunk_line(l)))?;
    Ok(compiled)
}

/// A UI element's shader, validated against the real UI pass source plus the
/// field shim the shared stdlib support needs.
pub(crate) fn compile_ui_validated(src: &str) -> Result<floptle_shader::transpile::CompiledUi, String> {
    let compiled = floptle_shader::compile_ui(src)?;
    let prelude = format!("{}\n{}", floptle_render::Ui::ui_prelude(), floptle_shader::transpile::UI_FIELD_SHIM);
    floptle_shader::validate(&prelude, &compiled.chunk)
        .map_err(|d| at_flsl_line(src, d, |l| compiled.flsl_span_of_chunk_line(l)))?;
    Ok(compiled)
}

/// A screen shader, validated against the module assembled exactly as the
/// pipeline will be.
pub(crate) fn compile_post_validated(src: &str) -> Result<floptle_shader::transpile::CompiledPost, String> {
    let compiled = floptle_shader::compile_post(src)?;
    floptle_shader::validate(&post_prelude(), &compiled.chunk)
        .map_err(|d| at_flsl_line(src, d, |l| compiled.flsl_span_of_chunk_line(l)))?;
    Ok(compiled)
}

/// Compile a `.flsl` source for the stage it declares, through the same path
/// that stage's loader takes. `floptle check` asks this of every shader in a
/// project, so a broken one is found without opening the game to look at it.
#[cfg(feature = "editor-ui")]
pub(crate) fn compile_any_stage(src: &str) -> Result<floptle_shader::Stage, String> {
    use floptle_shader::Stage;
    let stage = floptle_shader::parse(src).ok().and_then(|ir| ir.stage).unwrap_or(Stage::Fragment);
    match stage {
        Stage::Fragment => compile_fragment_validated(src).map(|_| ()),
        Stage::Ui => compile_ui_validated(src).map(|_| ()),
        Stage::Post => compile_post_validated(src).map(|_| ()),
        Stage::Sky => floptle_shader::compile_sky(src).map(|_| ()),
        Stage::Sdf => floptle_shader::check_sdf(src).map(|_| ()),
    }
    .map(|()| stage)
}

fn post_prelude() -> String {
    format!(
        "{}\n{}",
        floptle_shader::transpile::POST_PRELUDE,
        floptle_shader::transpile::POST_FIELD_SHIM
    )
}

/// A `stage ui` shader's chunk as the UI pass builds it: the field shim and
/// the stdlib support ahead of the transpiled code.
pub(crate) fn ui_chunk(compiled: &floptle_shader::CompiledUi) -> String {
    format!("{}\n{}\n{}", floptle_shader::transpile::UI_FIELD_SHIM, floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk)
}

/// The complete WGSL module for one screen shader — the same text `validate`
/// saw, in the same order, which is the point: the thing that was checked is the
/// thing that gets built.
pub(crate) fn post_module_source(chunk: &str) -> String {
    format!("{}\n{}\n{}", post_prelude(), floptle_shader::stdlib::SUPPORT_WGSL, chunk)
}
