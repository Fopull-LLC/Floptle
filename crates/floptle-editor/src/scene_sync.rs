//! Keeping GPU-side scene resources (terrain, sky, textures, VFX) in step with the World.

use floptle_core::Entity;
use floptle_core::Material;
use floptle_core::Matter;
use crate::Editor;


/// An occluder bake's atlas key: its shape and the bake itself (a rebake of
/// the same shape is a new allocation), with the high bit set so it never
/// meets a terrain's key.
fn occluder_key(k: &crate::shading::OccKey, b: &std::sync::Arc<floptle_field::BakedSdf>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    k.hash(&mut h);
    (std::sync::Arc::as_ptr(b) as usize).hash(&mut h);
    h.finish() | 1 << 63
}

impl Editor {
    /// Per-frame GPU sync for SDF matter: upload structurally-changed terrain
    /// volumes + shadow-occluder bakes into the shared 3D atlas (or just the
    /// dabbed boxes on the fast sculpt path), and refresh the texture palette.
    ///
    /// Within [`TERRAIN_SHADOW_UPLOAD_CELLS`](crate::terrain_edit::TERRAIN_SHADOW_UPLOAD_CELLS)
    /// a frame: edits' boxes first, then whatever a re-laid atlas has to
    /// upload. A volume the atlas already holds moves on the GPU for free, so
    /// a body arriving uploads only itself, and over frames, while the atlas
    /// drawn stays as it was until the new one is whole.
    pub(crate) fn sync_terrain_gpu(&mut self) {
        // Terrain volumes render per-volume, each at native resolution: moving a
        // terrain needs no GPU work at all — its f64 anchor is read fresh every frame
        // when the globals are built. Only structural changes (add/edit/delete/resize)
        // re-lay the volume set in the shared 3D atlas. Static collider meshes
        // join the same atlas as shadow-only occluder volumes (they cast, never draw).
        // Each step named inside `terrainShadow`: one 627 ms frame of it on a
        // cold load said nothing of which.
        let mut t0 = floptle_core::profile::Span::new();
        let mut part = |ed: &Self, name: &'static str| {
            ed.script_host.profile().borrow_mut().record_part(name, t0.ms());
            t0 = floptle_core::profile::Span::new();
        };
        self.land_terrain_proxies();
        part(self, "terrainProxies");
        let occluders_changed = self.refresh_mesh_occluders();
        part(self, "terrainOccluders");
        let mut budget = crate::terrain_edit::TERRAIN_SHADOW_UPLOAD_CELLS;
        if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
            // Edits' boxes, nearest-first by nothing in particular but
            // deterministic: entity order. A box larger than what is left of
            // the budget is uploaded a few Z layers at a time.
            let mut ents: Vec<Entity> = self.terrain_region_dirty.keys().copied().collect();
            ents.sort_by_key(|e| e.index());
            for e in ents {
                let Some(t) = self.terrains.get(&e) else {
                    self.terrain_region_dirty.remove(&e);
                    continue;
                };
                let Some(d) = self.terrain_region_dirty.get_mut(&e) else { continue };
                if d.key != t.shadow_key {
                    // A proxy since replaced is uploaded whole by the re-lay.
                    self.terrain_region_dirty.remove(&e);
                    continue;
                }
                if std::mem::take(&mut d.geom) {
                    // Sculpt moved this terrain's surface — rebuild just its wireframe.
                    self.terrain_wire_world.retain(|(we, ..)| *we != e);
                }
                while budget > 0 && !d.boxes.is_empty() {
                    let (mn, mut mx) = d.boxes.swap_remove(0);
                    let area = (mx[0].saturating_sub(mn[0]) as u64 * mx[1].saturating_sub(mn[1]) as u64).max(1);
                    let layers = (budget / area).max(1) as u32;
                    if mx[2].saturating_sub(mn[2]) > layers {
                        // The rest of the box waits for the next frame.
                        d.boxes.push(([mn[0], mn[1], mn[2] + layers], mx));
                        mx[2] = mn[2] + layers;
                    }
                    let cells = raymarch.set_volume_region_keyed(gpu, t.shadow_key, &t.shadow, mn, mx);
                    budget = budget.saturating_sub(cells.max(1));
                }
                if d.boxes.is_empty() {
                    self.terrain_region_dirty.remove(&e);
                }
            }
        } else {
            // Nothing to upload into: what an atlas made later holds is the
            // proxies as they are by then.
            self.terrain_region_dirty.clear();
        }
        part(self, "terrainShadowEdits");
        let laying = self.raymarch.as_ref().is_some_and(|r| r.atlas_building());
        if (self.terrain_gpu_dirty || self.terrain_atlas_dirty || occluders_changed || laying)
            && let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut())
        {
            // Deterministic slot order (by Matter::Terrain id) so the globals'
            // per-frame fill always matches the atlas layout.
            let mut items: Vec<(u32, Entity)> = self
                .terrains
                .keys()
                // Far-body impostors leave the shadow/AO atlas entirely: their
                // SDF can't matter at this range, it frees volume budget, and
                // it can't speckle the impostor sphere with self-shadowing.
                .filter(|&&e| {
                    !self.terrain_render.get(&e).is_some_and(|r| r.impostor)
                })
                .map(|&e| {
                    let id = match self.world.get::<Matter>(e) {
                        Some(Matter::Terrain { id, .. }) => *id,
                        _ => 0,
                    };
                    (id, e)
                })
                .collect();
            items.sort_by_key(|(id, e)| (*id, e.index()));
            let entities: Vec<Entity> = items.iter().map(|&(_, e)| e).collect();
            // Occluders upload after the terrains, ONE bake per distinct
            // (model, rotation, scale): seven copies of one building share
            // one region of the atlas, each drawn at its own place. Sorted
            // by key, so identical content always lays out identically.
            let wanted = self.occluders_wanted.as_ref().unwrap_or(&self.mesh_occluders);
            let unique: std::collections::BTreeMap<&crate::shading::OccKey, &std::sync::Arc<floptle_field::BakedSdf>> =
                wanted.values().map(|(k, b)| (k, b)).collect();
            let mut volumes: Vec<floptle_render::AtlasVolume> = entities
                .iter()
                .map(|e| {
                    let t = &self.terrains[e];
                    floptle_render::AtlasVolume { baked: &t.shadow, key: t.shadow_key }
                })
                .collect();
            volumes.extend(unique.iter().map(|(k, b)| floptle_render::AtlasVolume { baked: b, key: occluder_key(k, b) }));
            let total = volumes.len();
            let done = raymarch.set_volumes_keyed(gpu, &volumes, budget);
            // Consumed: what is left is the build's, which goes on while
            // `atlas_building` says so (and the frame's full re-mesh,
            // which reads this flag, happens once).
            if self.terrain_gpu_dirty || self.terrain_atlas_dirty {
                self.terrain_gpu_dirty = false;
                self.terrain_atlas_dirty = false;
                self.terrain_wire_world.clear(); // terrain changed → rebuild the wireframe
            }
            if let Some(accepted) = done {
                if let Some(r) = raymarch.atlas_refused() {
                    // Out of video memory, not out of room: say which, and
                    // what it costs, so the fix (free the GPU) is the one tried.
                    if !self.atlas_oom_said {
                        self.atlas_oom_said = true;
                        self.console.push(
                            floptle_script::LogLevel::Warn,
                            format!(
                                "out of video memory: the shadow atlas needs {:.0} MB and the GPU refused it, so \
                                 {} terrain / collidable-mesh shape(s) cast no distance-field shadow or AO. The \
                                 world still draws and the sun's shadow map still works. Another program is \
                                 probably holding most of the GPU's memory: close it, then reload the scene.",
                                r.bytes as f64 / (1024.0 * 1024.0),
                                r.dropped
                            ),
                            None,
                        );
                    }
                } else if accepted < total {
                    // Never drop content silently: say what is missing, and
                    // that it is only the distance-field shadow.
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!(
                            "{} distinct terrain / collidable-mesh shape(s) don't fit the GPU's \
                             shadow atlas, so they cast no distance-field shadow or AO. They still \
                             draw and collide.",
                            total - accepted
                        ),
                        None,
                    );
                }
                let t_kept = accepted.min(entities.len());
                self.terrain_slots = entities[..t_kept]
                    .iter()
                    .map(|&e| {
                        let s = &self.terrains[&e].shadow;
                        crate::terrain_edit::TerrainSlot { e, center: s.center, half: s.half_extent }
                    })
                    .collect();
                self.occluder_regions = unique
                    .keys()
                    .enumerate()
                    .filter(|(j, _)| entities.len() + j < accepted)
                    .map(|(j, k)| ((*k).clone(), entities.len() + j))
                    .collect();
                if let Some(w) = self.occluders_wanted.take() {
                    self.mesh_occluders = w;
                }
                // Instances are chosen per frame, nearest the camera first (see
                // `fill_terrain_volumes`). When more want to cast than fit, the
                // far ones go without. That is the policy working, not content
                // going missing, so it is a note in the Console, not a warning.
                let casting = self
                    .mesh_occluders
                    .keys()
                    .filter(|&&e| self.world.get::<floptle_core::CastShadow>(e).is_none_or(|c| c.0))
                    .count();
                let room = (self.project.shadow_volumes as usize)
                    .min(floptle_render::MAX_VOLUMES)
                    .saturating_sub(t_kept);
                if casting > room && !self.occluder_overflow_said {
                    self.occluder_overflow_said = true;
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "{casting} collidable meshes cast distance-field shadows and {room} fit at \
                             once, so the {room} nearest the camera cast them. The rest still draw \
                             and collide. Set \"casts shadows\" off on small props to spend the budget \
                             on what matters, or raise shadow_volumes in project.ron (up to {}).",
                            floptle_render::MAX_VOLUMES
                        ),
                        None,
                    );
                }
            }
        }
        part(self, "terrainShadowAtlas");
    }

    /// The terrain texture palette, put on the GPU when it changes.
    ///
    /// Each slot is read from its file and resampled to the palette's 256² on
    /// a worker, and the palette drawn stays until all of them are in. Read on
    /// the frame, it was most of the frame a world's terrain arrived in, and
    /// of every scene load after a launch in Solar: 66 to 320 ms named as the
    /// shadow upload it sat beside. `wait` takes it on this call, for a verb
    /// that draws one picture with no frames before it.
    pub(crate) fn sync_terrain_palette(&mut self, wait: bool) {
        if std::mem::take(&mut self.terrain_textures_dirty) {
            // Every slot is resampled to the palette's 256². Honour the texture's own
            // filter setting while doing it — a bilinear resize of pixel art destroys
            // it here, before any sampler runs (this was half the "terrain textures are
            // always blurry" bug; the other half was the hardcoded Linear sampler).
            // Empty / unreadable slots become white so indices align. A list
            // changed again before it landed starts again: the one in flight is dropped.
            let settings = &self.texture_settings;
            let root = &self.project_root;
            let slots: Vec<Option<(std::path::PathBuf, bool)>> = self
                .terrain_textures
                .iter()
                .map(|p| {
                    let nearest = crate::assets::tex_setting(settings, root, p).filter
                        == crate::assets::FilterMode::Pixelated;
                    (!p.is_empty()).then(|| (crate::project::resolve_asset_path(root, p), nearest))
                })
                .collect();
            let (tx, rx) = std::sync::mpsc::channel();
            crate::worker::spawn("terrain-palette", move || {
                #[cfg(test)]
                crate::terrain_edit::made_on("palette");
                let layers: Vec<floptle_render::TextureData> = slots
                    .into_iter()
                    .map(|slot| {
                        slot.and_then(|(file, nearest)| floptle_assets::load_texture_sized_filtered(&file, 256, 256, nearest))
                            .unwrap_or_else(|| floptle_render::TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 })
                    })
                    .collect();
                let means = layers.iter().map(crate::terrain_edit::layer_mean).collect();
                let _ = tx.send((layers, means));
            });
            self.terrain_palette_job = Some(rx);
        }
        let Some(rx) = self.terrain_palette_job.as_ref() else { return };
        let layers = if wait {
            rx.recv().ok()
        } else {
            match rx.try_recv() {
                Ok(layers) => Some(layers),
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
            }
        };
        self.terrain_palette_job = None;
        let Some((layers, means)) = layers else { return };
        // The list the layers were read from: a change since would have
        // started another job above.
        let mask =
            crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root);
        self.set_terrain_slot_means(means);
        if let Some(gpu) = self.gpu.as_ref() {
            if let Some(raymarch) = self.raymarch.as_mut() {
                raymarch.set_terrain_textures(gpu, &layers);
            }
            // Meshed terrain (P2/P6) draws in the raster pass, so it needs its own copy
            // of the palette + the same per-slot nearest mask.
            if let Some(raster) = self.raster.as_mut() {
                raster.set_terrain_palette(gpu, &layers, mask);
            }
        }
    }

    /// (Re)upload the skybox equirect when the Skybox node's texture changes.
    pub(crate) fn sync_sky_texture(&mut self) {
        // Re-upload the skybox texture when the skybox node's texture path changes.
        let sky_tex_path = self.world.query::<Matter>().find_map(|(_, m)| match m {
            Matter::Skybox { texture, .. } => texture.clone(),
            _ => None,
        });
        if sky_tex_path != self.sky_texture_loaded {
            let data =
                sky_tex_path.as_ref().and_then(|p| floptle_assets::load_texture(&self.resolve_asset_path(p)));
            if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
                raymarch.set_sky_texture(gpu, data.as_ref());
            }
            self.sky_texture_loaded = sky_tex_path;
        }
    }

    /// Compile + splice the Skybox node's Sky-stage `.flsl` (a procedural sky). Recompiles
    /// only on path/mtime change; a compile error keeps the last-good shader and logs.
    /// `None` path clears back to the built-in sky.
    pub(crate) fn sync_sky_shader(&mut self) {
        let path = self.world.query::<Matter>().find_map(|(_, m)| match m {
            Matter::Skybox { shader, .. } => shader.clone(),
            _ => None,
        });
        let Some(path) = path else {
            // No sky shader: clear if one was active.
            if self.sky_shader.take().is_some()
                && let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut())
            {
                raymarch.set_sky_shader(gpu, None);
            }
            return;
        };
        // Asset-tree paths already carry the root ("assets/shaders/…") — joining
        // project_root onto them gave assets/assets/… enoent, so no picked sky
        // shader ever loaded (the Material-shader double-join bug, same fix).
        let abs = self.resolve_asset_path(&path);
        let mtime = floptle_vfs::modified(&abs)
            .and_then(|t| t.duration_since(floptle_core::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        // Unchanged (same path + mtime) → nothing to do.
        if self.sky_shader.as_ref().is_some_and(|(p, mt, _)| *p == path && *mt == mtime) {
            return;
        }
        let Ok(src) = floptle_vfs::read_to_string(&abs) else {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("◈ sky shader {path} — can't read file"),
                None,
            );
            return;
        };
        let compile = floptle_core::profile::Span::new();
        let compiled = floptle_shader::compile_sky(&src);
        self.note_asset(&path, "shaderCompile", compile.ms());
        match compiled {
            Ok(compiled) => {
                let build = floptle_core::profile::Span::new();
                if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
                    raymarch.set_sky_shader(
                        gpu,
                        Some((&compiled.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL)),
                    );
                }
                self.note_asset(&path, "pipelineCompile", build.ms());
                self.flsl_gate.pipelines_unsaved = true;
                self.sky_shader = Some((path.clone(), mtime, compiled.uniforms.clone()));
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("◈ sky shader `{}` compiled", compiled.name),
                    None,
                );
            }
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("◈ sky shader {path}: {e}"),
                    None,
                );
            }
        }
    }

    /// This frame's `sky_uniforms`: each declared sky-shader uniform resolved to the
    /// Skybox node's Inspector override (`shader_params`) or, absent one, the shader's
    /// own `.flsl` default. Read fresh every frame so a knob drag is instant (no
    /// recompile) — the mirror of how a Material packs its params.
    pub(crate) fn sky_uniform_values(&self) -> [[f32; 4]; 16] {
        let mut arr = [[0.0f32; 4]; 16];
        let Some((_, _, schema)) = &self.sky_shader else { return arr };
        let params = self.world.query::<Matter>().find_map(|(_, m)| match m {
            Matter::Skybox { shader_params, .. } => Some(shader_params),
            _ => None,
        });
        for (i, u) in schema.iter().take(16).enumerate() {
            arr[i] = params.and_then(|p| p.get(&u.name).copied()).unwrap_or(u.default);
        }
        arr
    }

    /// Put every node in `targets` on `layer`, as one undo step.
    ///
    /// One `record()` and one `rebuild_sim()` for the whole set, not per node:
    /// twenty crates re-layered is one thing somebody did and one Ctrl+Z, and
    /// rebuilding the sim twenty times to reach the same state is twenty times
    /// the cost of reaching it once.
    pub(crate) fn apply_layer(&mut self, targets: &[Entity], layer: &str) {
        if targets.is_empty() {
            return;
        }
        self.record();
        for &e in targets {
            // "Default" is the absence of the component, not a value of it —
            // so putting a node back on Default has to remove it, or the scene
            // file grows a layer entry that means nothing.
            if layer == floptle_core::layers::DEFAULT_LAYER {
                self.world.remove::<floptle_core::Layer>(e);
            } else {
                self.world.insert(e, floptle_core::Layer(layer.to_string()));
            }
        }
        self.scene_dirty = true;
        // Re-layer the live sim: bodies re-resolve via sync_dynamic_params,
        // but static colliders bake their bit at build — so rebuild.
        self.rebuild_sim();
    }

    /// **Register every texture this scene's materials name, before the gather
    /// asks for them.**
    ///
    /// A gather cannot do it itself — `gpu`/`raster` are borrowed there — so it
    /// resolves a material's texture by looking the path up in the registry, and
    /// a path that never got here comes back `None`. `None` does not draw
    /// nothing: it means "no override", so the mesh's own imported texture draws
    /// instead. A material whose texture was never registered therefore looks
    /// exactly like a material that was never applied — except that its colour,
    /// its emissive and its maps all work, which is the most confusing possible
    /// failure. Reported as "I override the material, my new material has a
    /// texture, but it is still showing the texture of the model — though if I
    /// change the emission I can see it get brighter."
    ///
    /// It belongs to the frame, not to the editor's UI pass, and both paths call
    /// it: `floptle shot` and every other path that goes straight to
    /// `render_world_into` would otherwise photograph a scene wearing the wrong
    /// textures, and say nothing about it.
    ///
    /// Idempotent and cheap: every entry is skipped once registered, so the
    /// steady-state cost is one hash lookup per material per frame.
    pub(crate) fn ensure_scene_textures(&mut self) {
        // Once per frame, however many views ask. `render_world_into` is called
        // six times for six cube faces during a GI bake or a reflection
        // capture, and this walks the whole world four times — with the same
        // answer on every face, plus a fresh disk-load attempt for every path
        // that does not resolve.
        if self.textures_warmed_frame == self.frame_no && self.frame_no != 0 {
            return;
        }
        self.textures_warmed_frame = self.frame_no;
        // Node Materials and per-object override materials — an override's
        // texture is as much a texture as the node's. The surface maps too —
        // normal, roughness, metallic, occlusion. They go through the same
        // registry lookup as the base texture and had the same silence on a
        // miss: a material with a normal map it could not resolve drew flat,
        // and the only sign was that it looked like every other flat surface.
        // And a script's patch over a model's own materials names images too.
        //
        // Every path is looked up by reference and only a miss is copied: this
        // runs every frame over every material in the scene, and copying each
        // path to ask whether it is already loaded was most of what it cost.
        let mut tex_paths: Vec<String> = Vec::new();
        {
            let reg = &self.texture_registry;
            let mut want = |p: Option<&String>| {
                if let Some(p) = p
                    && !reg.contains_key(p)
                    && !tex_paths.contains(p)
                {
                    tex_paths.push(p.clone());
                }
            };
            let mut images = |m: &Material| {
                want(m.texture.as_ref());
                for p in m.maps() {
                    want(p);
                }
            };
            for (_, m) in self.world.query::<Material>() {
                images(m);
            }
            for (_, om) in self.world.query::<floptle_core::ObjectMaterials>() {
                om.0.values().for_each(&mut images);
            }
            for (_, p) in self.world.query::<floptle_core::MaterialPatch>() {
                images(&p.mat);
            }
        }
        // …and every sheet of every tileset a tilemap in this scene uses.
        //
        // Nothing else warms these. A tileset sheet reached the GPU only if some
        // Material happened to name the same image, which is why a tileset had
        // to be paired with a material to draw at all — and why the extra sheets
        // added in v0.36.0 drew nothing unless a material pointed at them too.
        // `tilemap_draws` resolves a page by path against this registry, so a
        // path that never gets here is a page that silently renders as
        // untextured.
        let sheets: Vec<String> = self
            .world
            .query::<Matter>()
            .filter_map(|(_, m)| match m {
                Matter::Tilemap { tileset, .. } => self.tiles.get(tileset),
                _ => None,
            })
            .flat_map(|s| s.pages_iter().map(|(_, t, ..)| t.to_string()).collect::<Vec<_>>())
            .filter(|p| !p.trim().is_empty() && !self.texture_registry.contains_key(p))
            .collect();
        tex_paths.extend(sheets);
        for p in tex_paths {
            self.ensure_texture(&p);
        }
    }

    /// Register (GPU-upload) every texture and import every mesh the particle
    /// system references this frame: the effect open in the Particles tab (its
    /// live working doc — so a just-picked asset resolves next frame
    /// deterministically), every saved effect, every live play instance, and the
    /// tab preview. Idempotent. Called at the top of `render()`, before the gather
    /// resolves batch textures / mesh handles.
    pub(crate) fn ensure_vfx_assets(&mut self) {
        let mut tex: Vec<String> = Vec::new();
        let mut meshes: Vec<String> = Vec::new();
        let push = |v: &mut Vec<String>, p: &str| {
            if !p.is_empty() && !v.iter().any(|q| q == p) {
                v.push(p.to_string());
            }
        };
        // The open working doc first (it holds edits not yet in the registry).
        #[cfg(feature = "editor-ui")]
        if let Some(doc) = &self.vfx_ui.doc {
            for t in &doc.tracks {
                match &t.render {
                    floptle_scene::VfxRenderDoc::Billboard { texture: Some(p) } => push(&mut tex, p),
                    floptle_scene::VfxRenderDoc::Mesh { asset_path } => push(&mut meshes, asset_path),
                    _ => {}
                }
            }
        }
        for p in self.vfx.texture_paths() {
            push(&mut tex, &p);
        }
        for p in self.vfx.mesh_paths() {
            push(&mut meshes, &p);
        }
        for p in tex {
            if !self.texture_registry.contains_key(&p) {
                self.ensure_texture(&p);
            }
        }
        for p in meshes {
            if !self.mesh_registry.contains_key(&p) {
                self.import_model(&p);
            }
        }
    }
}

#[cfg(test)]
mod shadow_atlas_tests {
    use crate::terrain_edit::{EditorTerrain, TERRAIN_SHADOW_UPLOAD_CELLS};
    use floptle_core::math::{DVec3, Vec3};
    use floptle_core::{Entity, Matter, Transform};

    /// One layer of the largest proxy: a frame may finish the layer it began.
    const LAYER: u64 = 192 * 192;

    fn rock() -> floptle_field::ChunkField {
        let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 120.0, voxel: 1.5, relief: 6.0, cave_depth: 0.0, ..Default::default() };
        floptle_field::procgen::generate_planet(&fill)
    }

    /// A terrain arriving as a streamed-in body does: resident, and the atlas
    /// told to re-lay.
    fn add_body(ed: &mut crate::Editor, id: u32, at: DVec3, field: &floptle_field::ChunkField) -> Entity {
        let e = ed.world.spawn();
        ed.world.insert(e, Transform { translation: at, ..Transform::IDENTITY });
        ed.world.insert(e, Matter::Terrain { id, collision: Default::default() });
        ed.terrains.insert(e, EditorTerrain::new(field.clone()));
        ed.terrain_gpu_dirty = true;
        e
    }

    fn uploaded(ed: &crate::Editor) -> u64 {
        ed.raymarch.as_ref().map_or(0, |r| r.uploaded_cells())
    }

    /// One frame's shadow sync and its end (submit, and let the GPU catch
    /// up): the cells it uploaded.
    fn frame(ed: &mut crate::Editor) -> u64 {
        let before = uploaded(ed);
        ed.sync_terrain_gpu();
        let g = ed.gpu.as_ref().unwrap();
        g.queue.submit(None);
        let _ = g.device.poll(wgpu::PollType::wait_indefinitely());
        uploaded(ed) - before
    }

    /// Frames until nothing is left to upload: each frame's cells.
    fn settle(ed: &mut crate::Editor) -> Vec<u64> {
        let mut cells = Vec::new();
        for _ in 0..20_000 {
            cells.push(frame(ed));
            let r = ed.raymarch.as_ref().unwrap();
            // Occluder bakes are made off the frame, and land when they land.
            if !r.atlas_building() && !ed.terrain_gpu_dirty && ed.terrain_region_dirty.is_empty() && ed.occluder_bake_jobs.is_empty() {
                return cells;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        panic!("the shadow atlas never settled");
    }

    /// Every drawn terrain slot holds its terrain's proxy as it is now.
    fn atlas_is_current(ed: &crate::Editor) -> bool {
        let (gpu, r) = (ed.gpu.as_ref().unwrap(), ed.raymarch.as_ref().unwrap());
        ed.terrain_slots.iter().enumerate().all(|(i, s)| r.slot_holds(gpu, i, &ed.terrains[&s.e].shadow))
    }

    /// **The first atlas after a world lands is made off the frame, even
    /// short of video memory.** On a cold start the atlas is a placeholder,
    /// so the first world to land has a new one made, room for half as much
    /// again, on a worker. When the GPU refused that, the frame made one
    /// itself: the exact size, then half the volumes, each an allocation of
    /// up to hundreds of megabytes and its zeroing, on the frame. A 627 ms
    /// `terrainShadow` frame on a cold load had that shape. The worker
    /// falls back now, and the frame makes none. Counted, with the memory
    /// capped as a full GPU would: room for the world but not the roomy
    /// atlas, it is drawn and nothing was made on the frame; room for one of
    /// three worlds landing at once, one is drawn, the rest are reported
    /// refused, and nothing was made on the frame; room for none, the same.
    #[test]
    fn the_first_atlas_after_a_world_lands_is_made_off_the_frame() {
        let field = rock();
        let cells = |ed: &crate::Editor| ed.terrains.values().map(|t| t.shadow.distance.len() as u64).max().unwrap_or(0);
        for (worlds, cap_of, drawn) in [(1usize, 1.2f64, 1usize), (3, 1.5, 1), (1, 0.5, 0)] {
            let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
            for k in 0..worlds {
                add_body(&mut ed, 1 + k as u32, DVec3::new(900.0 * k as f64, 0.0, 0.0), &field);
            }
            // Six bytes a cell: an f16 distance and an RGBA8 colour.
            let one = cells(&ed) * 6;
            ed.raymarch.as_mut().unwrap().set_atlas_memory_cap(Some((one as f64 * cap_of) as u64));
            let before = ed.raymarch.as_ref().unwrap().atlas_allocs_on_frame();
            let per_frame = settle(&mut ed);
            let r = ed.raymarch.as_ref().unwrap();
            let made = r.atlas_allocs_on_frame() - before;
            assert_eq!(made, 0, "{worlds} world(s) landing in {:.1} worlds of memory made {made} atlas(es) on the frame", cap_of);
            assert_eq!(ed.terrain_slots.len(), drawn, "{worlds} world(s) in {cap_of:.1} worlds of memory");
            assert_eq!(r.atlas_refused().is_some(), drawn < worlds, "{worlds} world(s), {drawn} drawn: refused said {:?}", r.atlas_refused());
            let worst = per_frame.iter().copied().max().unwrap_or(0);
            assert!(worst <= TERRAIN_SHADOW_UPLOAD_CELLS + LAYER, "a frame uploaded {worst} cells, over the budget");
            if drawn > 0 {
                assert!(atlas_is_current(&ed), "a slot does not hold its terrain's proxy");
            }
        }
    }

    /// **Bodies arriving one after another upload only themselves, a budget
    /// a frame, and the shadows drawn stay until the new atlas is whole.**
    /// Each terrain that landed re-uploaded every terrain's proxy in its
    /// frame: Solar's belt rocks, streaming in near the camera one after
    /// another, were 100 to 490 ms of `terrainShadow` a frame for 15 s.
    /// Counted in cells: three 192³ bodies landing on consecutive frames
    /// beside one already drawn upload three bodies' cells in all, never more
    /// than the budget (and the layer it began) in a frame; the slots drawn
    /// change only on the frame the atlas is whole, the full re-mesh the
    /// dirty flag asks for is asked once, and each slot then holds its proxy
    /// bit for bit.
    #[test]
    fn bodies_arriving_upload_only_themselves_within_the_budget() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let field = rock();
        add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        let one = ed.terrains.values().next().unwrap().shadow.distance.len() as u64;
        assert!(one > 4 * TERRAIN_SHADOW_UPLOAD_CELLS, "the fixture's proxy fits in a frame");
        let drawn_before = ed.terrain_slots.clone();
        let mut cells = Vec::new();
        for k in 0..3u32 {
            add_body(&mut ed, 2 + k, DVec3::new(900.0 * (k + 1) as f64, 0.0, 0.0), &field);
            cells.push(frame(&mut ed));
            assert!(!ed.terrain_gpu_dirty, "the dirty flag outlived its frame: every frame would re-mesh every terrain");
        }
        let mut drawn_changed_at = None;
        for _ in 0..20_000 {
            if ed.terrain_slots != drawn_before {
                drawn_changed_at.get_or_insert(cells.len());
            }
            if !ed.raymarch.as_ref().unwrap().atlas_building() {
                break;
            }
            assert!(drawn_changed_at.is_none(), "the drawn slots changed before the atlas was whole");
            cells.push(frame(&mut ed));
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        let worst = cells.iter().copied().max().unwrap_or(0);
        assert!(worst <= TERRAIN_SHADOW_UPLOAD_CELLS + LAYER, "a frame uploaded {worst} cells, over the budget");
        let total: u64 = cells.iter().sum();
        assert_eq!(total, 3 * one, "{total} cells uploaded for three bodies of {one}: something was uploaded again");
        assert_eq!(ed.terrain_slots.len(), 4);
        assert!(atlas_is_current(&ed), "a slot does not hold its terrain's proxy");
    }

    /// **A thousand flattens at two far sites upload their own boxes, within
    /// the budget.** The boxes of a frame's edits were merged into one: two
    /// edits on opposite sides of a world uploaded the whole proxy between
    /// them, every frame they landed. Each terrain keeps its boxes apart now,
    /// merged only with boxes they nearly overlap. Counted in cells: no frame
    /// over the budget or near a whole proxy, and after, the atlas holds the
    /// proxy the edits left, so no box was dropped.
    #[test]
    fn a_thousand_flattens_at_two_far_sites_upload_their_own_boxes() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let field = rock();
        let e = add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        let proxy = ed.terrains[&e].shadow.distance.len() as u64;
        let site = |d: Vec3| field.raycast(d * 250.0, -d, 250.0).unwrap();
        let (a, b) = (site(Vec3::ONE.normalize()), site(-Vec3::ONE.normalize()));
        let ops: Vec<floptle_script::TerrainOp> = (0..1000)
            .map(|i| {
                let (ang, r) = (i as f32 * 2.399, 1.0 + (i % 20) as f32 * 0.4);
                let p = if i % 2 == 0 { a } else { b } + Vec3::new(ang.cos() * r, 0.0, ang.sin() * r);
                floptle_script::TerrainOp { pos: [p.x as f64, p.y as f64, p.z as f64], radius: 2.0, strength: 0.6, mode: floptle_script::TerrainOpMode::Flatten, id: 0, normal: None }
            })
            .collect();
        let mut first = Some(ops);
        let mut cells = Vec::new();
        for _ in 0..20_000 {
            ed.pump_collision_meshes();
            ed.apply_script_terrain_ops(first.take().unwrap_or_default());
            cells.push(frame(&mut ed));
            if ed.terrain_op_queue.is_empty() && ed.terrain_op_job.is_none() && ed.terrain_region_dirty.is_empty() {
                break;
            }
        }
        assert!(ed.terrain_region_dirty.is_empty(), "the edits never all landed");
        let worst = cells.iter().copied().max().unwrap_or(0);
        let total: u64 = cells.iter().sum();
        assert!(worst <= TERRAIN_SHADOW_UPLOAD_CELLS + LAYER, "a frame uploaded {worst} cells, over the budget");
        // Two sites' boxes, a few thousand cells each: one box spanning both
        // is most of the proxy.
        assert!(worst * 32 < proxy, "a frame uploaded {worst} cells of a {proxy}-cell proxy ({total} in {} frames): a box spanning the sites", cells.len());
        assert!(atlas_is_current(&ed), "the atlas lost an edit's box");
    }

    /// **A static mesh whose bake changes keeps casting its old bake until
    /// the atlas holding the new one is whole.** The occluder set drawn and
    /// the atlas regions it draws from change together: a new bake whose
    /// region is not uploaded yet would leave the mesh casting nothing for
    /// the frames the atlas takes. Here it settles at a new size while two
    /// bodies are arriving, so the atlas takes many frames: on every one
    /// the mesh casts, from a region that holds a bake of it.
    #[test]
    fn a_rebaked_mesh_casts_its_old_bake_until_the_new_one_is_in() {
        use floptle_assets::glb_write::{WriteMesh, WriteNode, write_glb};
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let root = std::env::temp_dir().join(format!("floptle_occ_atlas_{}", std::process::id()));
        std::fs::create_dir_all(root.join("models")).unwrap();
        let mesh = WriteMesh {
            positions: vec![[-1.0, 0.0, -1.0], [1.0, 0.0, -1.0], [0.0, 2.0, 1.0], [0.0, 0.0, 1.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 1, 2, 1, 3, 2, 3, 0, 2, 0, 3, 1],
            base_color: [1.0; 4],
            ..Default::default()
        };
        std::fs::write(root.join("models/mound.glb"), write_glb(&[WriteNode::mesh_node("m", mesh)], &[], &[], &[])).unwrap();
        ed.project_root = root.clone();
        let m = ed.world.spawn();
        ed.world.insert(m, Transform::from_translation(DVec3::new(0.0, 200.0, 0.0)));
        ed.world.insert(m, Matter::Mesh { asset_path: "models/mound.glb".into() });
        ed.world.insert(m, floptle_core::Collidable);
        // Flat slabs: 192 x 13 x 192 proxies, a frame or two of upload each,
        // and little video memory on a GPU other programs share.
        let mut field = floptle_field::ChunkField::new(1.5);
        field.fill_slab(Vec3::new(-300.0, -20.0, -300.0), Vec3::new(300.0, 0.0, 300.0), 0.0, [0.4, 0.5, 0.3]);
        add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        // The occluder instance the frame draws, if any: its box.
        let casts = |ed: &crate::Editor| {
            let mut g = floptle_render::RaymarchGlobals::default();
            crate::Editor::fill_terrain_volumes(&ed.terrains, &ed.terrain_slots, &ed.mesh_occluders, &ed.occluder_regions, 32, &ed.world, &mut g, DVec3::ZERO);
            (0..floptle_render::MAX_VOLUMES).find(|&i| g.vol_center[i][3] == 2.0).map(|i| g.vol_half[i])
        };
        let before = casts(&ed).expect("the mesh casts no shadow to begin with");
        // Resized, and held there: two frames before it is baked again,
        // bodies start arriving, so the new bake joins an atlas under way.
        ed.world.get_mut::<Transform>(m).unwrap().scale = Vec3::splat(1.5);
        while ed.occluder_settling.get(&m).is_none_or(|(_, n)| *n + 2 < crate::terrain_edit::OCCLUDER_SETTLE_FRAMES) {
            frame(&mut ed);
            assert!(casts(&ed).is_some(), "the mesh stopped casting while it settled");
        }
        for k in 0..6u32 {
            add_body(&mut ed, 2 + k, DVec3::new(900.0 * (k + 1) as f64, 0.0, 0.0), &field);
        }
        let mut frames = 0;
        let mut rebaked_while_building = false;
        loop {
            frame(&mut ed);
            frames += 1;
            let building = ed.raymarch.as_ref().unwrap().atlas_building();
            rebaked_while_building |= building && ed.occluders_wanted.is_some();
            assert!(casts(&ed).is_some(), "frame {frames}: the mesh cast nothing while the atlas was re-laid");
            if !building && ed.occluder_bakes >= 2 {
                break;
            }
            assert!(frames < 20_000, "the atlas never settled");
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(rebaked_while_building, "the new bake landed without waiting on the atlas: the fixture tests nothing");
        let after = casts(&ed).unwrap();
        assert!(after[0] > before[0] * 1.3, "the mesh still casts its old bake ({before:?} then {after:?})");
    }

    /// **A dug pit's rim blends over the width the palette gives.** A pit dug
    /// through red ground into a blue subsoil, seen from above: with no
    /// `|blend=` the two meet across one triangle, as they always have; with
    /// a width of 3 m far more of the picture is a mix of the two, and with
    /// 0.5 m less than that. `FLOPTLE_BLEND_PNG=<dir>` writes the pictures.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_dug_pits_rim_blends_over_the_palettes_width() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_blend_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        for (name, c) in [("red", [200u8, 40, 40, 255]), ("blue", [40, 40, 200, 255])] {
            let tex = floptle_render::TextureData { pixels: c.repeat(16 * 16), width: 16, height: 16 };
            std::fs::write(dir.join(format!("textures/{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        }
        ed.project_root = dir.clone();
        ed.terrain_textures = vec!["textures/red.png".into(), "textures/red.png".into(), "textures/blue.png".into()];
        ed.terrain_textures_dirty = true;
        for _ in 0..20_000 {
            ed.sync_terrain_palette(false);
            if ed.terrain_palette_job.is_none() && !ed.terrain_textures_dirty {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 8,
            radius: 120.0,
            voxel: 1.5,
            relief: 2.0,
            cave_depth: 0.0,
            subsoil_depth: 1.5,
            subsoil: floptle_field::procgen::LayerPaint { slot: 3, color: [1.0; 3] },
            ..Default::default()
        });
        let top = field.raycast(Vec3::new(0.0, 250.0, 0.0), Vec3::NEG_Y, 250.0).unwrap();
        for k in 0..4 {
            field.sculpt(floptle_field::Brush::Lower, top - Vec3::Y * (k as f32 * 0.8), 5.0, 1.0, Default::default());
        }
        add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        let sun = ed.world.spawn();
        ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-1.2), ..Transform::IDENTITY });
        ed.world.insert(sun, floptle_core::Light { intensity: 1.0, shadows: false, ..Default::default() });
        ed.camera.position = DVec3::new(top.x as f64, top.y as f64 + 22.0, top.z as f64 + 0.5);
        ed.camera.pitch = -1.5;
        ed.camera.yaw = 0.0;
        let (w, h) = (320u32, 240u32);
        let shoot = |ed: &mut crate::Editor, blend: f32| {
            ed.terrain_tex_blend = vec![0.0, 0.0, blend];
            ed.terrain_tex_rough = vec![0.5; 3];
            let at = ed.camera.position;
            ed.terrain_gpu_dirty = true;
            settle(ed);
            ed.settle_terrain_meshes(at, std::time::Duration::from_secs(60));
            let cam = ed.camera.render_camera();
            crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame")
        };
        let shots: Vec<(f32, Vec<u8>)> = [0.0f32, 0.5, 3.0].into_iter().map(|b| (b, shoot(&mut ed, b))).collect();
        let _ = std::fs::remove_dir_all(&dir);
        // A pixel that is neither the red ground nor the blue subsoil but a mix.
        let mixed = |px: &[u8]| {
            px.as_chunks::<4>().0.iter().filter(|p| {
                let (r, b) = (p[0] as f32, p[2] as f32);
                r.max(b) > 30.0 && r / b.max(1.0) > 0.45 && r / b.max(1.0) < 2.2
            }).count()
        };
        let counts: Vec<(f32, usize)> = shots.iter().map(|(b, px)| (*b, mixed(px))).collect();
        if let Ok(out) = std::env::var("FLOPTLE_BLEND_PNG") {
            for (b, px) in &shots {
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&out).join(format!("blend_{b}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
        }
        let (none, narrow, wide) = (counts[0].1, counts[1].1, counts[2].1);
        assert!(wide > none * 2 + 50, "a 3 m blend mixes {wide} pixels against {none} with none: {counts:?}");
        // With no width the edge is one triangle wide, about a voxel (1.5 m
        // here), so half a metre is crisper than that and three is wider.
        assert!(narrow * 3 < none * 2, "a 0.5 m blend mixes {narrow} pixels against {none} with none: {counts:?}");
        assert!(wide > narrow, "a 3 m blend mixes no more than a 0.5 m one: {counts:?}");
    }

    /// PICTURES (run by hand): a dug world's shadows drawn from the atlas as
    /// the budgeted path leaves it (boxes and a re-laid build), then from the
    /// same proxies uploaded whole the old way. The two must be the same
    /// picture. `FLOPTLE_SHADOW_PNG=<dir>` writes both.
    #[cfg(feature = "editor-ui")]
    #[test]
    #[ignore = "writes pictures to look at; needs a GPU"]
    fn shadow_atlas_pictures() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let field = rock();
        let e = add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        let top = field.raycast(Vec3::new(0.0, 250.0, 0.0), Vec3::NEG_Y, 250.0).unwrap();
        // A trench and a mound, landed through the frame loop's path.
        let ops: Vec<floptle_script::TerrainOp> = (0..120)
            .map(|i| {
                let p = top + Vec3::new(-24.0 + i as f32 * 0.4, if i % 2 == 0 { 0.0 } else { 6.0 }, (i % 2) as f32 * 10.0);
                let mode = if i % 2 == 0 { floptle_script::TerrainOpMode::Lower } else { floptle_script::TerrainOpMode::Raise };
                floptle_script::TerrainOp { pos: [p.x as f64, p.y as f64, p.z as f64], radius: 3.0, strength: 1.0, mode, id: 0, normal: None }
            })
            .collect();
        let mut first = Some(ops);
        for _ in 0..20_000 {
            ed.pump_collision_meshes();
            ed.apply_script_terrain_ops(first.take().unwrap_or_default());
            frame(&mut ed);
            if ed.terrain_op_queue.is_empty() && ed.terrain_op_job.is_none() && ed.terrain_region_dirty.is_empty() {
                break;
            }
        }
        // A second body arriving re-lays the atlas, so the first is copied.
        add_body(&mut ed, 2, DVec3::new(0.0, 0.0, -600.0), &field);
        settle(&mut ed);
        let sun = ed.world.spawn();
        ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-0.35) * floptle_core::math::Quat::from_rotation_y(0.9), ..Transform::IDENTITY });
        ed.world.insert(sun, floptle_core::Light { intensity: 1.2, ..Default::default() });
        ed.camera.position = DVec3::new(top.x as f64, top.y as f64 + 30.0, top.z as f64 + 45.0);
        ed.camera.pitch = -0.55;
        ed.camera.yaw = 0.0;
        let cam_world = ed.camera.position;
        ed.settle_terrain_meshes(cam_world, std::time::Duration::from_secs(60));
        let (w, h) = (480u32, 300u32);
        let cam = ed.camera.render_camera();
        let budgeted = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false).expect("a frame");
        // The old way: every proxy uploaded whole.
        {
            let (gpu, rm) = (ed.gpu.as_ref().unwrap(), ed.raymarch.as_mut().unwrap());
            let bakes: Vec<&floptle_field::BakedSdf> = ed.terrain_slots.iter().map(|s| &ed.terrains[&s.e].shadow).collect();
            assert_eq!(rm.set_volumes(gpu, &bakes), bakes.len());
        }
        let whole = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false).expect("a frame");
        if let Ok(dir) = std::env::var("FLOPTLE_SHADOW_PNG") {
            for (name, px) in [("budgeted", &budgeted), ("whole", &whole)] {
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&dir).join(format!("{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
        }
        let differ = budgeted.chunks(4).zip(whole.chunks(4)).filter(|(a, b)| a != b).count();
        let _ = e;
        assert_eq!(differ, 0, "{differ} pixels differ between the budgeted atlas and the whole upload");
    }

    /// **A terrain whose proxy is made again keeps casting its old one until
    /// the new one is in.** A world generated over a terrain, or loaded over
    /// it, has a proxy of another size and place; the atlas writes it over
    /// frames, and until it is whole the slot drawn is the old proxy with
    /// the old box. Drawn with the new box over the old texels, the shadow
    /// would be the old shape stretched into the new place.
    #[test]
    fn a_remade_proxy_casts_the_old_one_until_the_new_one_is_in() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let e = add_body(&mut ed, 1, DVec3::ZERO, &rock());
        settle(&mut ed);
        let mut g = floptle_render::RaymarchGlobals::default();
        let fill = |ed: &crate::Editor, g: &mut floptle_render::RaymarchGlobals| {
            crate::Editor::fill_terrain_volumes(&ed.terrains, &ed.terrain_slots, &ed.mesh_occluders, &ed.occluder_regions, 32, &ed.world, g, DVec3::ZERO);
            (g.vol_center[0], g.vol_half[0])
        };
        let old = fill(&ed, &mut g);
        let mut slab = floptle_field::ChunkField::new(1.5);
        slab.fill_slab(Vec3::new(-300.0, -40.0, -300.0), Vec3::new(300.0, 0.0, 300.0), 0.0, [0.4, 0.5, 0.3]);
        ed.terrains.insert(e, EditorTerrain::new(slab));
        ed.terrain_gpu_dirty = true;
        let new_half = ed.terrains[&e].shadow.half_extent;
        assert_ne!(old.1[..3], new_half[..], "the fixture's new proxy has the old box");
        let mut frames = 0;
        while ed.raymarch.as_ref().unwrap().atlas_building() || frames == 0 {
            frame(&mut ed);
            frames += 1;
            if ed.raymarch.as_ref().unwrap().atlas_building() {
                assert_eq!(fill(&ed, &mut g), old, "the slot was drawn with the new box before the new proxy was in");
            }
            assert!(frames < 20_000, "the new proxy never landed");
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        assert!(frames > 1, "the new proxy landed in one frame: no budget");
        assert_eq!(fill(&ed, &mut g).1[..3], new_half[..], "the slot kept the old box");
        assert!(atlas_is_current(&ed));
    }
}
