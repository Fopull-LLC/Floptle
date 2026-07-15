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
        let mats: Vec<(Entity, Material)> = self
            .world
            .query::<Material>()
            .filter(|(_, m)| m.shader.is_some())
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
            let params = compiled.pack_params(&|name| mat.shader_params.get(name).copied());
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

    /// Compile (or hot-reload) one `.flsl` by project-relative path. Keeps the
    /// last good pipeline on failure; Console-reports each new error once.
    fn ensure_flsl_shader(&mut self, rel: &str) {
        let full = self.project_root.join(rel);
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
    }

    /// The IDE's live `.flsl` diagnostic: (1-based line, message) for the
    /// current text of an open buffer — parse + type errors, no GPU involved.
    pub(crate) fn check_flsl_syntax(text: &str) -> Option<(usize, String)> {
        match floptle_shader::compile_fragment(text) {
            Ok(_) => None,
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

/// Editor-side registry fields, bundled for `Editor` (see main.rs).
pub(crate) type FlslCache = HashMap<String, FlslEntry>;
pub(crate) type FlslBinds = HashMap<Entity, FlslMatBind>;
