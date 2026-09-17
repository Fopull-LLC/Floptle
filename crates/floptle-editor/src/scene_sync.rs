//! Keeping GPU-side scene resources (terrain, sky, textures, VFX) in step with the World.

use floptle_core::Entity;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::Name;
use crate::Editor;


impl Editor {
    /// Per-frame GPU sync for SDF matter: upload structurally-changed terrain
    /// volumes + shadow-occluder bakes into the shared 3D atlas (or just the
    /// dabbed region on the fast sculpt path), and refresh the texture palette.
    pub(crate) fn sync_terrain_gpu(&mut self) {
        // Terrain volumes render per-volume, each at native resolution: moving a
        // terrain needs no GPU work at all — its f64 anchor is read fresh every frame
        // when the globals are built. Only structural changes (add/edit/delete/resize)
        // re-upload the volume set into the shared 3D atlas. Static collider meshes
        // join the same atlas as shadow-only occluder volumes (they cast, never draw).
        let occluders_changed = self.refresh_mesh_occluders();
        if self.terrain_gpu_dirty || occluders_changed {
            if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
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
                items.sort_by_key(|(id, _)| *id);
                let entities: Vec<Entity> = items.iter().map(|&(_, e)| e).collect();
                // Occluders upload after the terrains (stable order by asset + name,
                // so identical content always lays out identically).
                let mut occ_items: Vec<(String, Entity)> = self
                    .mesh_occluders
                    .iter()
                    .map(|(&e, (key, _))| {
                        let name =
                            self.world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default();
                        (format!("{}\u{1}{name}", key.0), e)
                    })
                    .collect();
                occ_items.sort_by(|a, b| a.0.cmp(&b.0));
                let occ_entities: Vec<Entity> = occ_items.iter().map(|(_, e)| *e).collect();
                let mut baked: Vec<&floptle_field::BakedSdf> =
                    entities.iter().map(|e| &self.terrains[e].shadow).collect();
                baked.extend(occ_entities.iter().map(|e| &*self.mesh_occluders[e].1));
                let accepted = raymarch.set_volumes(gpu, &baked);
                let total = entities.len() + occ_entities.len();
                if accepted < total {
                    // Never drop content silently: colliders still work, but say so.
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!(
                            "{} volume(s) (terrain / mesh shadow occluders) exceed the GPU volume budget and won't render or cast (collision is unaffected)",
                            total - accepted
                        ),
                        None,
                    );
                }
                let t_kept = accepted.min(entities.len());
                self.terrain_slots = entities[..t_kept].to_vec();
                self.occluder_slots = occ_entities[..accepted - t_kept].to_vec();
                self.terrain_gpu_dirty = false;
                self.terrain_region_dirty = None; // the full upload supersedes any region
                self.terrain_wire_world.clear(); // terrain changed → rebuild the wireframe
            }
        } else if let Some((e, mn, mx, geom)) = self.terrain_region_dirty.take() {
            // Fast paint/sculpt path: upload only the dabbed voxel box into this
            // terrain's atlas slot — its field maps 1:1 at native resolution.
            if let (Some(gpu), Some(raymarch), Some(t), Some(slot)) = (
                self.gpu.as_ref(),
                self.raymarch.as_mut(),
                self.terrains.get(&e),
                self.terrain_slots.iter().position(|&se| se == e),
            ) {
                raymarch.set_volume_region(gpu, slot, &t.shadow, mn, mx);
            }
            if geom {
                // Sculpt moved this terrain's surface — rebuild just its wireframe.
                self.terrain_wire_world.retain(|(we, ..)| *we != e);
            }
        }
        // (see terrain_nearest_mask for the per-slot filter bits)
        // Re-upload the terrain texture palette when it changes. Each slot resolves
        // to a 256² layer (empty / unreadable slots become white so indices align).
        if self.terrain_textures_dirty {
            // Every slot is resampled to the palette's 256². Honour the texture's own
            // filter setting while doing it — a bilinear resize of pixel art destroys
            // it here, before any sampler runs (this was half the "terrain textures are
            // always blurry" bug; the other half was the hardcoded Linear sampler).
            let settings = &self.texture_settings;
            let root = &self.project_root;
            let layers: Vec<floptle_render::TextureData> = self
                .terrain_textures
                .iter()
                .map(|p| {
                    let nearest = crate::assets::tex_setting(settings, root, p).filter
                        == crate::assets::FilterMode::Pixelated;
                    let file = crate::project::resolve_asset_path(root, p);
                    if !p.is_empty()
                        && let Some(t) =
                            floptle_assets::load_texture_sized_filtered(&file, 256, 256, nearest)
                    {
                        return t;
                    }
                    floptle_render::TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 }
                })
                .collect();
            let mask =
                crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root);
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
            self.terrain_textures_dirty = false;
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
        match floptle_shader::compile_sky(&src) {
            Ok(compiled) => {
                if let (Some(gpu), Some(raymarch)) = (self.gpu.as_ref(), self.raymarch.as_mut()) {
                    raymarch.set_sky_shader(
                        gpu,
                        Some((&compiled.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL)),
                    );
                }
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
    /// This used to live at the end of `apply_frame_commands`, which is the
    /// editor's UI pass. So it ran for the editor's own window and for nothing
    /// else: `floptle shot` and every other path that goes straight to
    /// `render_world_into` photographed a scene wearing the wrong textures, and
    /// said nothing about it. It belongs to the frame, and both paths call it.
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
        // texture is as much a texture as the node's.
        let mut tex_paths: Vec<String> = self
            .world
            .query::<Material>()
            .filter_map(|(_, m)| m.texture.clone())
            .filter(|p| !self.texture_registry.contains_key(p))
            .collect();
        tex_paths.extend(
            self.world
                .query::<floptle_core::ObjectMaterials>()
                .flat_map(|(_, om)| om.0.values().filter_map(|m| m.texture.clone()))
                .filter(|p| !self.texture_registry.contains_key(p)),
        );
        // The surface maps too — normal, roughness, metallic, occlusion. They go
        // through the same registry lookup as the base texture and had the same
        // silence on a miss: a material with a normal map it could not resolve
        // drew flat, and the only sign was that it looked like every other flat
        // surface.
        let maps = |m: &Material| m.maps().into_iter().flatten().cloned().collect::<Vec<_>>();
        let map_paths: Vec<String> = self
            .world
            .query::<Material>()
            .flat_map(|(_, m)| maps(m))
            .chain(
                self.world
                    .query::<floptle_core::ObjectMaterials>()
                    .flat_map(|(_, om)| om.0.values().flat_map(maps).collect::<Vec<_>>()),
            )
            .filter(|p| !self.texture_registry.contains_key(p))
            .collect();
        tex_paths.extend(map_paths);
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
