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
    /// Each slot is read from its file and resampled to the palette's layer
    /// size on a worker: from `.floptle/cache/palette/` when it has been read
    /// before, decoded otherwise. Read on the frame, it was most of the frame
    /// a world's terrain arrived in. The ground never waits for it in white:
    /// a palette of each slot's mean colour (cached beside its picture, the
    /// last one seen, or mid grey) is on the GPU in the frame the list
    /// changes, and the pictures land into it a few slots a frame
    /// ([`palette_slots_per_frame`]). A palette that changed but kept its
    /// size lands into the one already drawn, so an edit to one slot leaves
    /// the others as they were. `wait` takes every slot on this call, for a
    /// verb that draws one picture with no frames before it.
    pub(crate) fn sync_terrain_palette(&mut self, wait: bool) {
        let size = floptle_render::terrain_layer_size(self.project.terrain_layer_size);
        let read_before = self.terrain_palette_size;
        if read_before.is_some_and(|was| was != size) {
            self.terrain_textures_dirty = true;
        }
        if std::mem::take(&mut self.terrain_textures_dirty) {
            self.terrain_palette_size = Some(size);
            // Honour each texture's own filter while resizing — a bilinear
            // resize of pixel art destroys it before any sampler runs — and
            // size a slot's maps the way its colour is. A list changed again
            // before it landed starts again: the one in flight is dropped.
            let settings = &self.texture_settings;
            let root = self.project_root.clone();
            let resolve = |p: &str| (!p.is_empty()).then(|| crate::project::resolve_asset_path(&root, p));
            let slots: Vec<PaletteSlotFiles> = self
                .terrain_textures
                .iter()
                .take(floptle_render::TERRAIN_SLOTS as usize)
                .enumerate()
                .map(|(i, p)| {
                    let m = self.terrain_tex_maps.get(i).cloned().unwrap_or_default();
                    // A slot's own Material fills in what its palette line
                    // does not name: its picture, its normal map, and one
                    // ORM picture from its roughness, occlusion or metal map
                    // (read as glTF packs them, R occlusion, G roughness, B
                    // metalness).
                    let own = self.terrain_tex_materials.get(i).and_then(Option::as_ref);
                    let or_own = |line: &str, own: Option<&Option<String>>| -> String {
                        if line.is_empty() { own.and_then(|o| o.clone()).unwrap_or_default() } else { line.to_string() }
                    };
                    let colour = or_own(p, own.map(|o| &o.texture));
                    let orm_own = own.and_then(|o| o.roughness_map.clone().or_else(|| o.ao_map.clone()).or_else(|| o.metallic_map.clone()));
                    PaletteSlotFiles {
                        nearest: crate::assets::tex_setting(settings, &root, &colour).filter
                            == crate::assets::FilterMode::Pixelated,
                        colour: resolve(&colour),
                        maps: [
                            resolve(&or_own(&m.normal, own.map(|o| &o.normal_map))),
                            resolve(&or_own(&m.orm, Some(&orm_own))),
                            resolve(&m.height),
                        ],
                    }
                })
                .collect();
            // A pixel-art slot's own texel grid, for the pixel border, comes
            // with its picture from the worker (`PaletteRead`); until then the
            // layer's.
            self.terrain_slot_texels = vec![0; slots.len()];
            let surfaced = slots.iter().any(|s| s.maps.iter().any(Option::is_some));
            let bit = |i: usize, has: bool| if has { 1u32 << i } else { 0 };
            self.terrain_surface_bits = [
                slots.iter().enumerate().map(|(i, s)| bit(i, s.maps[0].is_some())).fold(0, |a, b| a | b),
                // The ORM-and-height layer: either map makes one.
                slots.iter().enumerate().map(|(i, s)| bit(i, s.maps[1].is_some() || s.maps[2].is_some())).fold(0, |a, b| a | b),
            ];
            // What the ground shows until each picture lands: its cached
            // mean, the mean it last had, or mid grey; nothing at all white.
            let means: Vec<[f32; 3]> = slots
                .iter()
                .enumerate()
                .map(|(i, s)| match &s.colour {
                    None => [1.0; 3],
                    Some(f) => crate::palette_cache::mean(&root, f, size, s.nearest)
                        .or_else(|| self.terrain_slot_means.get(i).copied())
                        .unwrap_or([0.214; 3]),
                })
                .collect();
            if let Some(gpu) = self.gpu.as_ref() {
                let layers = if surfaced { 3 * floptle_render::TERRAIN_SLOTS } else { floptle_render::TERRAIN_SLOTS };
                let live = self.raster.as_ref().map(|r| r.terrain_palette_texture());
                let keep = read_before == Some(size)
                    && live.is_some_and(|t| t.width() == size && t.depth_or_array_layers() == layers);
                if !keep {
                    let tex = floptle_render::terrain_palette::stand_in(gpu, size, &means, surfaced);
                    let mask = crate::terrain_edit::terrain_nearest_mask(
                        &self.terrain_textures,
                        &self.texture_settings,
                        &self.project_root,
                    );
                    if let Some(raymarch) = self.raymarch.as_mut() {
                        raymarch.set_terrain_palette_texture(gpu, tex.clone());
                    }
                    if let Some(raster) = self.raster.as_mut() {
                        raster.set_terrain_palette_texture(gpu, tex, mask);
                    }
                }
            }
            self.terrain_palette_landing = means;
            let (tx, rx) = std::sync::mpsc::channel();
            crate::worker::spawn("terrain-palette", move || {
                #[cfg(test)]
                crate::terrain_edit::made_on("palette");
                for (i, s) in slots.into_iter().enumerate() {
                    let read = |f: &Option<std::path::PathBuf>| {
                        f.as_ref().and_then(|f| crate::palette_cache::load(&root, f, size, s.nearest))
                    };
                    let colour = read(&s.colour);
                    let mean = colour.as_ref().map_or([1.0; 3], |c| c.1);
                    let [n, o, h] = &s.maps;
                    let pictures = floptle_render::SlotPictures::new(
                        // A slot with no picture is white, as it always was.
                        Some(colour.map_or_else(
                            || floptle_render::TextureData { pixels: vec![255; (size * size * 4) as usize], width: size, height: size },
                            |c| c.0,
                        )),
                        read(n).map(|t| t.0),
                        read(o).map(|t| t.0),
                        read(h).map(|t| t.0),
                        size,
                    );
                    // A newer list took over: stop reading for this one.
                    // A pixel-art picture's own width: the grid a pixel
                    // border steps on, not the layer it was resized into.
                    let texels = if s.nearest { s.colour.as_deref().and_then(png_width).unwrap_or(0) } else { 0 };
                    if tx.send((i, pictures, mean, texels)).is_err() {
                        return;
                    }
                }
            });
            self.terrain_palette_job = Some(rx);
        }
        let Some(rx) = self.terrain_palette_job.as_ref() else { return };
        let budget = if wait { usize::MAX } else { palette_slots_per_frame(size) };
        let mut landed = Vec::new();
        let mut done = false;
        while landed.len() < budget {
            let next = if wait { rx.recv().map_err(|_| std::sync::mpsc::TryRecvError::Disconnected) } else { rx.try_recv() };
            match next {
                Ok(slot) => landed.push(slot),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        self.terrain_palette_landed_last = landed.len();
        if let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_ref()) {
            let tex = raster.terrain_palette_texture();
            for (i, pictures, _, _) in &landed {
                floptle_render::terrain_palette::write_slot(gpu, tex, *i as u32, pictures);
            }
        }
        for (i, _, mean, texels) in landed {
            if let Some(m) = self.terrain_palette_landing.get_mut(i) {
                *m = mean;
            }
            if let Some(t) = self.terrain_slot_texels.get_mut(i) {
                *t = texels;
            }
        }
        if done {
            self.terrain_palette_job = None;
            let means = std::mem::take(&mut self.terrain_palette_landing);
            self.set_terrain_slot_means(means);
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

/// One palette slot's files, as the worker reads them.
struct PaletteSlotFiles {
    colour: Option<std::path::PathBuf>,
    nearest: bool,
    /// Normal, ORM, height.
    maps: [Option<std::path::PathBuf>; 3],
}

/// How many palette slots land on the GPU in one frame: about 1.4 MB of
/// writes a frame whatever the layer size, so a palette arriving never
/// costs a frame its smoothness.
pub(crate) fn palette_slots_per_frame(size: u32) -> usize {
    match size {
        0..=256 => 4,
        257..=512 => 2,
        _ => 1,
    }
}

/// A PNG's width from its header, or `None` for anything else: what a
/// pixel-art slot's texel grid is (`terrain_slot_texels`).
fn png_width(file: &std::path::Path) -> Option<u32> {
    if !file.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
        return None;
    }
    let head = floptle_vfs::read(file).ok()?;
    (head.len() >= 24 && &head[1..4] == b"PNG" && &head[12..16] == b"IHDR")
        .then(|| u32::from_be_bytes([head[16], head[17], head[18], head[19]]))
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
                floptle_script::TerrainOp { pos: [p.x as f64, p.y as f64, p.z as f64], radius: 2.0, strength: 0.6, mode: floptle_script::TerrainOpMode::Flatten, id: 0, normal: None, brush: None }
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

    /// **A border takes its slot's style, and height decides who shows
    /// (stage 2).** The dug pit's rim, red ground over blue subsoil, a 3 m
    /// blend, from above:
    ///  - soft (as before) mixes the two across the band;
    ///  - sharp is a crisp line, a small part of soft's mixed pixels;
    ///  - pixel steps on the texel grid: as crisp, and not the same picture;
    ///  - a bumpy height map on the red ground with height blending makes
    ///    the band pure red and blue patches rather than a fade;
    ///  - a bias up on the blue gives blue more of the picture.
    ///
    /// `FLOPTLE_BORDER_PNG=<dir>` writes the pictures.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_border_takes_its_style_and_height_decides_who_shows() {
        use crate::terrain_edit::SlotBorder;
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_border_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        for (name, c) in [("red", [200u8, 40, 40, 255]), ("blue", [40, 40, 200, 255])] {
            let tex = floptle_render::TextureData { pixels: c.repeat(16 * 16), width: 16, height: 16 };
            std::fs::write(dir.join(format!("textures/{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        }
        // Bumps: a checker of high and low, four squares a tile.
        let mut bumps = Vec::with_capacity(16 * 16 * 4);
        for i in 0..16 * 16 {
            let v = if ((i % 16) / 4 + (i / 16) / 4) % 2 == 0 { 255 } else { 0 };
            bumps.extend_from_slice(&[v, v, v, 255]);
        }
        let tex = floptle_render::TextureData { pixels: bumps, width: 16, height: 16 };
        std::fs::write(dir.join("textures/bumps.png"), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        ed.project_root = dir.clone();
        ed.terrain_textures = vec!["textures/red.png".into(), "textures/red.png".into(), "textures/blue.png".into()];
        ed.terrain_tex_maps = vec![
            crate::terrain_edit::SlotMaps { height: "textures/bumps.png".into(), ..Default::default() },
            crate::terrain_edit::SlotMaps { height: "textures/bumps.png".into(), ..Default::default() },
            crate::terrain_edit::SlotMaps::default(),
        ];
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
        // Both slots' borders the same; the subsoil's bias apart.
        let shoot = |ed: &mut crate::Editor, b: SlotBorder, blue_bias: f32| {
            ed.terrain_tex_blend = vec![3.0, 3.0, 3.0];
            ed.terrain_tex_rough = vec![0.5; 3];
            ed.terrain_tex_border = vec![b, b, SlotBorder { height_bias: blue_bias, ..b }];
            let at = ed.camera.position;
            ed.terrain_gpu_dirty = true;
            settle(ed);
            ed.settle_terrain_meshes(at, std::time::Duration::from_secs(60));
            let cam = ed.camera.render_camera();
            crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame")
        };
        let soft = shoot(&mut ed, SlotBorder::default(), 0.0);
        let sharp = shoot(&mut ed, SlotBorder { style: 1, ..Default::default() }, 0.0);
        let pixel = shoot(&mut ed, SlotBorder { style: 2, ..Default::default() }, 0.0);
        let height = shoot(&mut ed, SlotBorder { height_blend: 0.9, ..Default::default() }, 0.0);
        let biased = shoot(&mut ed, SlotBorder { height_blend: 0.9, ..Default::default() }, 0.6);
        // The same height blend with every height map's contrast at nothing:
        // what the bumps themselves add.
        let flat = shoot(&mut ed, SlotBorder { height_blend: 0.9, contrast: 0.0, ..Default::default() }, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
        if let Ok(out) = std::env::var("FLOPTLE_BORDER_PNG") {
            for (name, px) in [("soft", &soft), ("sharp", &sharp), ("pixel", &pixel), ("height", &height), ("biased", &biased)] {
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&out).join(format!("border_{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
        }
        // Neither the red ground nor the blue subsoil but a mix; and blue.
        let mixed = |px: &[u8]| {
            px.as_chunks::<4>().0.iter().filter(|p| {
                let (r, b) = (p[0] as f32, p[2] as f32);
                r.max(b) > 30.0 && r / b.max(1.0) > 0.45 && r / b.max(1.0) < 2.2
            }).count()
        };
        let blue = |px: &[u8]| px.as_chunks::<4>().0.iter().filter(|p| p[2] as f32 > 2.2 * p[0].max(1) as f32).count();
        let m = [mixed(&soft), mixed(&sharp), mixed(&pixel), mixed(&height), mixed(&biased)];
        assert!(m[0] > 1000, "the fixture: a soft 3 m border should mix a band ({m:?})");
        assert!(blue(&sharp) > 1000 && blue(&sharp) < (w * h) as usize - 1000, "the fixture: the sharp picture shows both slots");
        assert!(m[1] * 5 < m[0], "a sharp border mixes {} pixels against soft's {} ({m:?})", m[1], m[0]);
        assert!(m[2] * 5 < m[0], "a pixel border mixes {} pixels against soft's {} ({m:?})", m[2], m[0]);
        let differs = pixel.as_chunks::<4>().0.iter().zip(sharp.as_chunks::<4>().0).filter(|(a, b)| a[0].abs_diff(b[0]) > 60).count();
        assert!(differs > 50, "the pixel border draws the same picture as the sharp one ({differs} pixels differ)");
        assert!(m[3] * 2 < m[0], "height blending mixes {} pixels against the plain fade's {} ({m:?})", m[3], m[0]);
        assert!(blue(&biased) > blue(&height) + 200, "a bias up on the blue gave it {} pixels against {} without", blue(&biased), blue(&height));
        let bumps = height.as_chunks::<4>().0.iter().zip(flat.as_chunks::<4>().0).filter(|(a, b)| a[2].abs_diff(b[2]) > 60).count();
        assert!(bumps > 50, "the red ground's height map changed {bumps} pixels of the border: it is not being read");
    }

    /// **Up close, a blend between two slots shows only those two.** A
    /// triangle whose corners carry slots 1 and 3 interpolates its slot from
    /// one to the other, through 2; up close it changes so little across a
    /// pixel that the "whole slot" shortcut took the pixel where it passed 2
    /// for slot 2 alone, and a thin line of a slot painted nowhere near ran
    /// across the triangle (Ty's "blending lines", glowing in the dark when
    /// that slot glows). A pit dug through red ground into blue subsoil, its
    /// rim seen from 0.6 m, slot 2 green: not one green pixel.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn up_close_a_blend_shows_no_slot_between_its_two() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_between_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        for (name, c) in [("red", [200u8, 40, 40, 255]), ("green", [40, 200, 40, 255]), ("blue", [40, 40, 200, 255])] {
            let tex = floptle_render::TextureData { pixels: c.repeat(16 * 16), width: 16, height: 16 };
            std::fs::write(dir.join(format!("textures/{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        }
        ed.project_root = dir.clone();
        ed.terrain_textures = vec!["textures/red.png".into(), "textures/green.png".into(), "textures/blue.png".into()];
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
            relief: 0.0,
            cave_depth: 0.0,
            surface_a: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            surface_b: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            subsoil_depth: 0.0,
            ..Default::default()
        });
        let top = field.raycast(Vec3::new(0.0, 250.0, 0.0), Vec3::NEG_Y, 250.0).unwrap();
        // Slot 3 painted beside slot 1, the way Ty's walls were painted.
        field.paint_texture(top + Vec3::new(4.0, 0.0, 0.0), 4.0, 3);
        add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        let sun = ed.world.spawn();
        ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-1.2), ..Transform::IDENTITY });
        ed.world.insert(sun, floptle_core::Light { intensity: 1.0, shadows: false, ..Default::default() });
        let (w, h) = (320u32, 240u32);
        let green = |px: &[u8]| px.as_chunks::<4>().0.iter().filter(|p| p[1] > 90 && p[1] as f32 > 1.6 * p[0].max(p[2]) as f32).count();
        let mut seen = Vec::new();
        // Across the painted edge, a little over half a metre off the ground.
        for a in 0..12 {
            let ang = 0.0f32;
            let rim = top + Vec3::new(-1.5 + a as f32 * 0.5, 0.0, 0.0);
            let ground = field.raycast(rim + Vec3::Y * 10.0, Vec3::NEG_Y, 20.0).unwrap_or(rim);
            ed.camera.position = DVec3::new(ground.x as f64, ground.y as f64 + 0.6, ground.z as f64);
            ed.camera.pitch = -1.5;
            ed.camera.yaw = ang;
            let at = ed.camera.position;
            ed.settle_terrain_meshes(at, std::time::Duration::from_secs(60));
            let cam = ed.camera.render_camera();
            let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false).expect("a frame");
            if let Ok(out) = std::env::var("FLOPTLE_BETWEEN_PNG") {
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&out).join(format!("between_{a}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
            let red = px.as_chunks::<4>().0.iter().filter(|p| p[0] > 90 && p[0] as f32 > 1.6 * p[1].max(p[2]) as f32).count();
            let blue = px.as_chunks::<4>().0.iter().filter(|p| p[2] > 90 && p[2] as f32 > 1.6 * p[0].max(p[1]) as f32).count();
            seen.push((red, blue, green(&px)));
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.iter().any(|(r, b, _)| *r > 500 && *b > 500), "the fixture: a view should show both the red ground and the blue subsoil: {seen:?}");
        let lines: usize = seen.iter().map(|s| s.2).sum();
        assert_eq!(lines, 0, "slot 2 drew between slots 1 and 3 (red, blue, green px per view): {seen:?}");
    }

    /// **Where three slots meet, every border keeps its width.** Grass with
    /// a red and a blue square painted on it, each pair meeting along a
    /// straight line and all three at corners, every slot a Natural 1 m
    /// border. `FLOPTLE_THREE_PNG=<dir>` writes the pictures.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn where_three_slots_meet_every_border_keeps_its_width() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_three_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        for (name, c) in [("green", [40u8, 200, 40, 255]), ("red", [200, 40, 40, 255]), ("blue", [40, 40, 200, 255])] {
            let tex = floptle_render::TextureData { pixels: c.repeat(16 * 16), width: 16, height: 16 };
            std::fs::write(dir.join(format!("textures/{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        }
        ed.project_root = dir.clone();
        ed.terrain_textures = vec!["textures/green.png".into(), "textures/red.png".into(), "textures/blue.png".into()];
        // `FLOPTLE_THREE_TEX=<solar>` draws the pictures with Solar's soil,
        // slate and flagstone and their maps instead: what the borders look
        // like on real ground. The checks below are for the flat colours.
        let real = std::env::var("FLOPTLE_THREE_TEX").ok();
        if let Some(root) = &real {
            ed.project_root = root.into();
            let t = |n: &str| format!("textures/env/terrain/{n}");
            let maps = |m: &str| crate::terrain_edit::SlotMaps {
                normal: t(&format!("maps/{m}_normal.png")),
                orm: t(&format!("maps/{m}_orm.png")),
                height: t(&format!("maps/{m}_height.png")),
            };
            ed.terrain_textures = vec![t("02_soil.png"), t("09_slate.png"), t("03_flagstone.png")];
            ed.terrain_tex_maps = vec![maps("Ground039"), maps("Rock058"), maps("Rocks011")];
        }
        ed.terrain_textures_dirty = true;
        for _ in 0..20_000 {
            ed.sync_terrain_palette(false);
            if ed.terrain_palette_job.is_none() && !ed.terrain_textures_dirty {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut field = floptle_field::ChunkField::new(1.5);
        field.fill_with_rgba(Vec3::new(-60.0, -10.0, -60.0), Vec3::new(60.0, 10.0, 60.0), |p: Vec3| p.y - 0.2, |_| [255, 255, 255, 1]);
        let top = Vec3::new(0.0, 0.2, 0.0);
        // Red left of x = 0 and blue right of it, both north of z = 0: the
        // two meet along x = 0 and each meets the grass along z = 0.
        for (slot, x0, x1) in [(2u8, -12, 0), (3, 0, 12)] {
            for x in x0..x1 {
                for z in -14..0 {
                    let p = Vec3::new(x as f32 + 0.5, 0.0, z as f32 + 0.5);
                    let g = field.raycast(top + p + Vec3::Y * 10.0, Vec3::NEG_Y, 20.0).unwrap_or(top + p);
                    field.paint_texture(g, 0.75, slot);
                }
            }
        }
        add_body(&mut ed, 1, DVec3::ZERO, &field);
        let sun = ed.world.spawn();
        ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-1.2), ..Transform::IDENTITY });
        ed.world.insert(sun, floptle_core::Light { intensity: 1.0, shadows: false, ..Default::default() });
        let (w, h) = (480u32, 320u32);
        let views = [("low", Vec3::new(0.0, 2.5, 6.0), -0.5f32), ("above", Vec3::new(0.0, 9.0, -1.0), -1.5), ("corner", Vec3::new(0.0, 2.0, 0.5), -1.5)];
        let shoot = |ed: &mut crate::Editor, border: crate::terrain_edit::SlotBorder, blend: f32, tag: &str| -> Vec<Vec<u8>> {
            ed.terrain_tex_blend = vec![blend; 3];
            ed.terrain_tex_rough = vec![0.5; 3];
            ed.terrain_tex_border = vec![border; 3];
            ed.terrain_gpu_dirty = true;
            settle(ed);
            let mut shots = Vec::new();
            for (name, pos, pitch) in views {
                ed.camera.position = DVec3::new((top.x + pos.x) as f64, (top.y + pos.y) as f64, (top.z + pos.z) as f64);
                ed.camera.pitch = pitch;
                ed.camera.yaw = 0.0;
                let at = ed.camera.position;
                ed.settle_terrain_meshes(at, std::time::Duration::from_secs(60));
                let cam = ed.camera.render_camera();
                let px = crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame");
                if let Ok(out) = std::env::var("FLOPTLE_THREE_PNG") {
                    let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                    std::fs::write(std::path::Path::new(&out).join(format!("three_{tag}{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
                }
                shots.push(px);
            }
            shots
        };
        let mut natural = crate::terrain_edit::SlotBorder::default();
        let mut blend = 0.0;
        crate::terrain_edit::apply_border_default(floptle_scene::TerrainBorderDoc::Natural, &mut blend, &mut natural);
        shoot(&mut ed, natural, blend, "");
        // A plain soft fade a metre wide: on flat ground under an even sun
        // every border near the camera is a ramp across many pixels, so a
        // step between two neighbouring pixels is a border cut off. The low
        // view from its 100th row down: above that the squares' far edge is
        // twenty metres off, where a metre is under a pixel.
        let soft = shoot(&mut ed, crate::terrain_edit::SlotBorder::default(), 1.0, "soft_");
        let _ = std::fs::remove_dir_all(&dir);
        if real.is_some() {
            return;
        }
        let steps: Vec<(usize, u8)> = soft
            .iter()
            .zip([100u32, 0, 0])
            .map(|(px, from)| {
                let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..((y * w + x) * 4 + 3) as usize];
                let (mut cuts, mut most) = (0, 0u8);
                for y in from..h - 1 {
                    for x in 0..w - 1 {
                        let a = at(x, y);
                        for b in [at(x + 1, y), at(x, y + 1)] {
                            let d = (0..3).map(|k| a[k].abs_diff(b[k])).max().unwrap_or(0);
                            most = most.max(d);
                            cuts += usize::from(d > 48);
                        }
                    }
                }
                (cuts, most)
            })
            .collect();
        let colours = |px: &[u8]| {
            let n = |f: &dyn Fn(&[u8]) -> bool| px.as_chunks::<4>().0.iter().filter(|p| f(&p[..])).count();
            (n(&|p| p[0] > 120 && p[1] < 100), n(&|p| p[1] > 120 && p[0] < 100 && p[2] < 100), n(&|p| p[2] > 120 && p[0] < 100))
        };
        let (r, g, b) = colours(&soft[2]);
        assert!(r > 2000 && g > 2000 && b > 2000, "the fixture: the corner view shows all three ({r} red, {g} green, {b} blue)");
        assert!(steps.iter().all(|(cuts, _)| *cuts == 0), "a soft border was cut off where three slots meet: (steps over 48, largest step) per view {steps:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Anti-tiling hides a texture's repeat from far off (0626).** One slot
    /// of light noise with one dark disc per tile, so the picture's structure
    /// is the tile's own period, a grid of dots, seen straight down from 60 m. Plain, the picture correlates
    /// strongly with itself one tile across; hex-tiled, that peak is gone. A
    /// macro layer adds broad light and dark patches: the picture's block
    /// means spread further apart. `FLOPTLE_ANTITILE_PNG=<dir>` writes them.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn antitile_hides_the_repeat_and_macro_varies_the_ground() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_antitile_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        let mut seed = 0x2545_f491u32;
        let mut noise = Vec::with_capacity(32 * 32 * 4);
        for i in 0..32 * 32 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let (x, y) = ((i % 32) as f32 - 16.0, (i / 32) as f32 - 16.0);
            let v = if x * x + y * y < 36.0 { 20 } else { 150 + (seed >> 24) as u8 / 4 };
            noise.extend_from_slice(&[v, v, v, 255]);
        }
        let tex = floptle_render::TextureData { pixels: noise, width: 32, height: 32 };
        std::fs::write(dir.join("textures/noise.png"), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        ed.project_root = dir.clone();
        ed.terrain_textures = vec!["textures/noise.png".into()];
        ed.terrain_textures_dirty = true;
        for _ in 0..20_000 {
            ed.sync_terrain_palette(false);
            if ed.terrain_palette_job.is_none() && !ed.terrain_textures_dirty {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 3,
            radius: 400.0,
            voxel: 2.0,
            relief: 0.0,
            cave_depth: 0.0,
            craters: 0,
            surface_a: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            surface_b: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            subsoil_depth: 0.0,
            ..Default::default()
        });
        let top = field.raycast(Vec3::new(0.0, 600.0, 0.0), Vec3::NEG_Y, 600.0).unwrap();
        add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        let sun = ed.world.spawn();
        ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-1.5), ..Transform::IDENTITY });
        ed.world.insert(sun, floptle_core::Light { intensity: 1.0, shadows: false, ..Default::default() });
        ed.camera.position = DVec3::new(top.x as f64, top.y as f64 + 60.0, top.z as f64 + 0.01);
        ed.camera.pitch = -1.55;
        ed.camera.yaw = 0.0;
        let (w, h) = (320u32, 240u32);
        let shoot = |ed: &mut crate::Editor, tile: [f32; 3]| {
            ed.terrain_tex_tile = vec![tile];
            let at = ed.camera.position;
            ed.terrain_gpu_dirty = true;
            settle(ed);
            ed.settle_terrain_meshes(at, std::time::Duration::from_secs(60));
            let cam = ed.camera.render_camera();
            crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame")
        };
        let plain = shoot(&mut ed, [0.0; 3]);
        let hex = shoot(&mut ed, [1.0, 0.0, 0.0]);
        let mac = shoot(&mut ed, [0.0, 1.0, 10.0]);
        let _ = std::fs::remove_dir_all(&dir);
        if let Ok(out) = std::env::var("FLOPTLE_ANTITILE_PNG") {
            for (name, px) in [("plain", &plain), ("hex", &hex), ("macro", &mac)] {
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&out).join(format!("antitile_{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
        }
        // Luminance of the middle 120 x 120, inside the ground the fixture
        // meshes at full detail.
        const N: usize = 120;
        let lum = |px: &[u8]| -> Vec<f32> {
            let mut v = Vec::with_capacity(N * N);
            for y in 60..180u32 {
                for x in 100..220u32 {
                    let i = ((y * w + x) * 4) as usize;
                    v.push(px[i] as f32 * 0.3 + px[i + 1] as f32 * 0.59 + px[i + 2] as f32 * 0.11);
                }
            }
            v
        };
        // The strongest correlation of the picture with itself shifted by
        // `d` pixels along x, for shifts past the texture's own grain.
        let repeat = |px: &[u8]| -> (f32, usize) {
            let l = lum(px);
            let mean = l.iter().sum::<f32>() / l.len() as f32;
            let c: Vec<f32> = l.iter().map(|v| v - mean).collect();
            let var = c.iter().map(|v| v * v).sum::<f32>().max(1e-6);
            (8..60usize)
                .map(|d| {
                    let mut s = 0.0;
                    for y in 0..N {
                        for x in 0..N - d {
                            s += c[y * N + x] * c[y * N + x + d];
                        }
                    }
                    (s / var * N as f32 / (N - d) as f32, d)
                })
                .fold((f32::MIN, 0), |a, b| if b.0 > a.0 { b } else { a })
        };
        // How far apart the means of 30 x 30 blocks spread: broad patches.
        let patches = |px: &[u8]| -> f32 {
            let l = lum(px);
            let means: Vec<f32> = (0..16)
                .map(|b| {
                    let (bx, by) = ((b % 4) * 30, (b / 4) * 30);
                    (0..30).flat_map(|y| (0..30).map(move |x| (y, x))).map(|(y, x)| l[(by + y) * N + bx + x]).sum::<f32>() / 900.0
                })
                .collect();
            let m = means.iter().sum::<f32>() / 16.0;
            (means.iter().map(|v| (v - m) * (v - m)).sum::<f32>() / 16.0).sqrt()
        };
        let (p, pd) = repeat(&plain);
        let (x, xd) = repeat(&hex);
        assert!(p > 0.5, "the fixture: the plain tile barely repeats ({p:.2} at {pd} px), so this tests nothing");
        assert!(x < p * 0.5, "hex tiling left the repeat: {x:.2} at {xd} px against {p:.2} at {pd} px plain");
        let (sp, sm) = (patches(&plain), patches(&mac));
        // The render is deterministic, so without the layer this is 1.0 exactly;
        // at full strength the spread about doubles.
        assert!(sm > sp * 1.6, "a macro layer left the ground as even as before: block spread {sm:.2} against {sp:.2}");
    }

    /// Read the palette `ed.terrain_textures` names, waiting for it to land.
    fn land_palette(ed: &mut crate::Editor) {
        ed.terrain_textures_dirty = true;
        ed.sync_terrain_palette(true);
        assert!(ed.terrain_palette_job.is_none(), "the palette should have landed");
    }

    /// **Distant ground stops crawling.** The palette had one level, so a
    /// pixel 60 m from a texture tiled every 4.5 m covered a dozen texels and
    /// read one of them: which one changed with every small camera move, and
    /// the ground shimmered. With the mips it reads their average. The
    /// fixture is the worst case, black and white texel by texel at random.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn distant_ground_reads_its_mips_and_holds_still_as_the_camera_moves() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_palette_mips_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        let mut seed = 0x9e37_79b9u32;
        let mut noise = Vec::with_capacity(256 * 256 * 4);
        for _ in 0..256 * 256 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let v = if seed >> 31 == 0 { 0 } else { 255 };
            noise.extend_from_slice(&[v, v, v, 255]);
        }
        let tex = floptle_render::TextureData { pixels: noise, width: 256, height: 256 };
        std::fs::write(dir.join("textures/noise.png"), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        ed.project_root = dir.clone();
        ed.terrain_textures = vec!["textures/noise.png".into()];
        // What Solar's terrain slots ask for; a texture names no filter is pixel art.
        ed.texture_settings.insert(
            "textures/noise.png".into(),
            crate::assets::TexSetting { filter: crate::assets::FilterMode::SmoothMipmaps, ..Default::default() },
        );
        land_palette(&mut ed);
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 3,
            radius: 400.0,
            voxel: 2.0,
            relief: 0.0,
            cave_depth: 0.0,
            craters: 0,
            surface_a: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            surface_b: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            subsoil_depth: 0.0,
            ..Default::default()
        });
        let top = field.raycast(Vec3::new(0.0, 600.0, 0.0), Vec3::NEG_Y, 600.0).unwrap();
        add_body(&mut ed, 1, DVec3::ZERO, &field);
        settle(&mut ed);
        let sun = ed.world.spawn();
        ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-1.5), ..Transform::IDENTITY });
        ed.world.insert(sun, floptle_core::Light { intensity: 1.0, shadows: false, ..Default::default() });
        let (w, h) = (320u32, 240u32);
        // 60 m up looking down: about 0.2 m a pixel, a dozen texels.
        let shoot = |ed: &mut crate::Editor, dx: f64| {
            ed.camera.position = DVec3::new(top.x as f64 + dx, top.y as f64 + 60.0, top.z as f64 + 0.01);
            ed.camera.pitch = -1.55;
            ed.camera.yaw = 0.0;
            let at = ed.camera.position;
            ed.terrain_gpu_dirty = true;
            settle(ed);
            ed.settle_terrain_meshes(at, std::time::Duration::from_secs(60));
            let cam = ed.camera.render_camera();
            crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame")
        };
        // A third of a pixel over.
        let a = shoot(&mut ed, 0.0);
        let b = shoot(&mut ed, 0.07);
        // A slot left on `Smooth` draws its full-size picture at every
        // distance, as the palette always did.
        ed.texture_settings.insert(
            "textures/noise.png".into(),
            crate::assets::TexSetting { filter: crate::assets::FilterMode::Smooth, ..Default::default() },
        );
        let smooth = shoot(&mut ed, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
        if let Ok(out) = std::env::var("FLOPTLE_PALETTE_PNG") {
            for (name, px) in [("a", &a), ("b", &b)] {
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&out).join(format!("palette_mips_{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
        }
        let lum = |px: &[u8], i: usize| px[i] as f32 * 0.3 + px[i + 1] as f32 * 0.59 + px[i + 2] as f32 * 0.11;
        let (mut crawl, mut mean, mut spread, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for y in 60..180u32 {
            for x in 100..220u32 {
                let i = ((y * w + x) * 4) as usize;
                crawl += (lum(&a, i) - lum(&b, i)).abs();
                mean += lum(&a, i);
                n += 1.0;
            }
        }
        let (crawl, mean) = (crawl / n, mean / n);
        for y in 60..180u32 {
            for x in 100..220u32 {
                let i = ((y * w + x) * 4) as usize;
                spread += (lum(&a, i) - mean).powi(2);
            }
        }
        let spread = (spread / n).sqrt();
        let mut kept = 0.0f32;
        for y in 60..180u32 {
            for x in 100..220u32 {
                let i = ((y * w + x) * 4) as usize;
                kept += (lum(&smooth, i) - mean).powi(2);
            }
        }
        let kept = (kept / n).sqrt();
        assert!(mean > 20.0, "the fixture: the ground should be lit, mean {mean:.1}");
        assert!(kept > mean * 0.3, "a Smooth slot should still read single texels as it always did: spread {kept:.1} about {mean:.1}");
        // One level reads single texels: a spread near half the mean and a
        // crawl as large. The mips read their average.
        assert!(spread < mean * 0.1, "far ground still reads single texels: spread {spread:.1} about a mean of {mean:.1}");
        assert!(crawl < mean * 0.03, "far ground crawled {crawl:.2} a pixel (mean {mean:.1}) under a third-of-a-pixel move");
    }

    /// Flat ground on a planet, painted with one slot: `colour`, and its
    /// surface maps if given, hex-tiled by `antitile`, shaded Physical. The
    /// camera `height` m over it looking down; one `w`×`h` picture under
    /// each of `suns` (the way toward the sun, and its intensity).
    #[cfg(feature = "editor-ui")]
    #[allow(clippy::too_many_arguments)]
    fn terrain_photos(
        tag: &str,
        colour: floptle_render::TextureData,
        normal: Option<floptle_render::TextureData>,
        orm: Option<floptle_render::TextureData>,
        antitile: f32,
        height: f64,
        (w, h): (u32, u32),
        suns: &[([f32; 3], f32)],
    ) -> Vec<Vec<u8>> {
        terrain_photos_filtered(tag, colour, normal, orm, antitile, crate::assets::FilterMode::SmoothMipmaps, None, height, (w, h), suns)
    }

    /// [`terrain_photos`] with the slot's colour sampled by `filter`.
    #[cfg(feature = "editor-ui")]
    #[allow(clippy::too_many_arguments)]
    fn terrain_photos_filtered(
        tag: &str,
        colour: floptle_render::TextureData,
        normal: Option<floptle_render::TextureData>,
        orm: Option<floptle_render::TextureData>,
        antitile: f32,
        filter: crate::assets::FilterMode,
        material: Option<floptle_scene::MaterialDoc>,
        height: f64,
        (w, h): (u32, u32),
        suns: &[([f32; 3], f32)],
    ) -> Vec<Vec<u8>> {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return Vec::new() };
        let dir = std::env::temp_dir().join(format!("floptle_surface_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        let save = |name: &str, tex: &floptle_render::TextureData| {
            std::fs::write(dir.join("textures").join(name), floptle_assets::encode_png(tex).unwrap()).unwrap();
            format!("textures/{name}")
        };
        let colour = save("colour.png", &colour);
        let mut maps = crate::terrain_edit::SlotMaps::default();
        if let Some(t) = &normal {
            maps.normal = save("normal.png", t);
        }
        if let Some(t) = &orm {
            maps.orm = save("orm.png", t);
        }
        ed.project_root = dir.clone();
        ed.terrain_textures = vec![colour.clone()];
        ed.terrain_tex_maps = vec![maps];
        ed.terrain_tex_tile = vec![[antitile, 0.0, 0.0]];
        ed.terrain_tex_materials = vec![material];
        ed.texture_settings.insert(colour, crate::assets::TexSetting { filter, ..Default::default() });
        land_palette(&mut ed);
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 3,
            radius: 400.0,
            voxel: 2.0,
            relief: 0.0,
            cave_depth: 0.0,
            craters: 0,
            surface_a: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            surface_b: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
            subsoil_depth: 0.0,
            ..Default::default()
        });
        let top = field.raycast(Vec3::new(0.0, 600.0, 0.0), Vec3::NEG_Y, 600.0).unwrap();
        let body = add_body(&mut ed, 1, DVec3::ZERO, &field);
        ed.world.insert(
            body,
            floptle_core::Material { shading: floptle_core::material::Shading::Physical, roughness: 1.0, metallic: 0.0, ..Default::default() },
        );
        settle(&mut ed);
        let sun = ed.world.spawn();
        ed.camera.position = DVec3::new(top.x as f64, top.y as f64 + height, top.z as f64 + 0.01);
        ed.camera.pitch = -1.55;
        ed.camera.yaw = 0.0;
        let mut out = Vec::new();
        for (i, &(direction, intensity)) in suns.iter().enumerate() {
            ed.world.insert(sun, floptle_core::Light { direction, intensity, shadows: false, ..Default::default() });
            let at = ed.camera.position;
            ed.terrain_gpu_dirty = true;
            settle(&mut ed);
            ed.settle_terrain_meshes(at, std::time::Duration::from_secs(60));
            let cam = ed.camera.render_camera();
            let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false).expect("a frame");
            if let Ok(dir) = std::env::var("FLOPTLE_SURFACE_PNG") {
                let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
                std::fs::write(std::path::Path::new(&dir).join(format!("surface_{tag}_{i}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
            out.push(px);
        }
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// A 4×4 picture of one colour.
    #[cfg(feature = "editor-ui")]
    fn flat_tex(px: [u8; 4]) -> floptle_render::TextureData {
        floptle_render::TextureData { pixels: px.repeat(16), width: 4, height: 4 }
    }

    /// Plain grey ground with each surface map a single colour (`None`, no
    /// map): under each sun, the mean brightness of the middle of a 160×120
    /// picture from 60 m and its brightest pixel there.
    #[cfg(feature = "editor-ui")]
    fn surfaced_ground(tag: &str, normal: Option<[u8; 4]>, orm: Option<[u8; 4]>, suns: &[([f32; 3], f32)]) -> Vec<[f32; 2]> {
        let shots = terrain_photos(tag, flat_tex([180, 180, 180, 255]), normal.map(flat_tex), orm.map(flat_tex), 0.0, 60.0, (160, 120), suns);
        shots
            .iter()
            .map(|px| {
                let (mut sum, mut peak) = (0.0f32, 0.0f32);
                for y in 40..80u32 {
                    for x in 60..100u32 {
                        let i = ((y * 160 + x) * 4) as usize;
                        let l = px[i] as f32 * 0.3 + px[i + 1] as f32 * 0.59 + px[i + 2] as f32 * 0.11;
                        sum += l;
                        peak = peak.max(l);
                    }
                }
                [sum / 1600.0, peak]
            })
            .collect()
    }

    /// The way toward a sun 35° up in the sky, on the side `(x, z)` points to.
    #[cfg(feature = "editor-ui")]
    fn sun_from(x: f32, z: f32) -> [f32; 3] {
        let (c, s) = (35f32.to_radians().cos(), 35f32.to_radians().sin());
        [x * c, s, z * c]
    }

    /// **A slot's normal map turns the ground toward the light.** A map
    /// tilting every texel toward the picture's right lights the ground most
    /// from the +x side, and one tilting toward the picture's top from the
    /// −z side: the picture lies on the ground with its right toward +x and
    /// its top toward −z, as its colours do. A flat map lights both sides
    /// alike, and so does a slot with no map.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_slots_normal_map_turns_the_ground_toward_the_light() {
        let suns = [(sun_from(1.0, 0.0), 1.0), (sun_from(-1.0, 0.0), 1.0), (sun_from(0.0, 1.0), 1.0), (sun_from(0.0, -1.0), 1.0)];
        let mean = |v: Vec<[f32; 2]>| v.iter().map(|m| m[0]).collect::<Vec<f32>>();
        let plain = mean(surfaced_ground("plain", None, None, &suns));
        if plain.is_empty() {
            return;
        }
        let right = mean(surfaced_ground("right", Some([218, 128, 218, 255]), None, &suns));
        let up = mean(surfaced_ground("up", Some([128, 218, 218, 255]), None, &suns));
        let level = mean(surfaced_ground("level", Some([128, 128, 255, 255]), None, &suns));
        let (px, mx, pz, mz) = (0, 1, 2, 3);
        assert!((plain[px] - plain[mx]).abs() < plain[px] * 0.05, "the fixture: flat ground lit alike from +x and −x, {plain:?}");
        assert!((level[px] - plain[px]).abs() < plain[px] * 0.05, "a flat normal map should light as no map does: {level:?} against {plain:?}");
        assert!(right[px] > right[mx] * 1.3, "a map tilted to the picture's right should face +x: {right:?}");
        assert!(up[mz] > up[pz] * 1.3, "a map tilted to the picture's top should face −z: {up:?}");
    }

    /// **Hex tiling turns a normal map's texels back with its picture.**
    /// Each hexagon lays a turned copy of the texture, so a bump in it is
    /// turned too, and the normals read off it point the way the copy was
    /// turned unless they are turned back. The fixture is a round bump per
    /// tile, marked by a dark dot in the colour: lit from one side, every bump
    /// in every hexagon must be brightest on that side. Without the turn-back
    /// each hexagon's bumps are lit from a side of their own.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_hex_tiled_normal_map_is_lit_from_the_suns_side() {
        let side = 64u32;
        let mut dot = Vec::with_capacity((side * side * 4) as usize);
        let mut bump = Vec::with_capacity((side * side * 4) as usize);
        let c = side as f32 / 2.0;
        for y in 0..side {
            for x in 0..side {
                let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
                let r2 = dx * dx + dy * dy;
                let v = if r2 < 25.0 { 0 } else { 230 };
                dot.extend_from_slice(&[v, v, v, 255]);
                // A dome: tilting out from the centre, up the picture as +Y.
                let (tx, ty) = if r2 < 22.0 * 22.0 { (dx / 22.0 * 0.7, -dy / 22.0 * 0.7) } else { (0.0, 0.0) };
                let tz = (1.0 - tx * tx - ty * ty).max(0.0).sqrt();
                let enc = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
                bump.extend_from_slice(&[enc(tx), enc(ty), enc(tz), 255]);
            }
        }
        let tex = |p: Vec<u8>| floptle_render::TextureData { pixels: p, width: side, height: side };
        let (w, h) = (320u32, 240u32);
        let sun = [(sun_from(1.0, 0.0), 1.0)];
        let plain = terrain_photos("bumps", tex(dot.clone()), Some(tex(bump.clone())), None, 0.0, 30.0, (w, h), &sun);
        if plain.is_empty() {
            return;
        }
        let hex = terrain_photos("bumpshex", tex(dot), Some(tex(bump)), None, 1.0, 30.0, (w, h), &sun);
        // Each bump's arrow: its ring's brightness about the ring's mean,
        // weighted by offset. Agreement is how well the arrows line up.
        let agreement = |px: &[u8]| -> (f64, f64, usize) {
            let l: Vec<f32> = px.chunks(4).map(|p| p[0] as f32 * 0.3 + p[1] as f32 * 0.59 + p[2] as f32 * 0.11).collect();
            let (wi, hi) = (w as usize, h as usize);
            let mut seen = vec![false; wi * hi];
            let mut centres = Vec::new();
            for start in 0..wi * hi {
                if seen[start] || l[start] > 40.0 {
                    continue;
                }
                let (mut stack, mut sum, mut n) = (vec![start], (0.0f64, 0.0f64), 0usize);
                seen[start] = true;
                while let Some(i) = stack.pop() {
                    let (x, y) = (i % wi, i / wi);
                    sum = (sum.0 + x as f64, sum.1 + y as f64);
                    n += 1;
                    for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                        if nx < wi && ny < hi && !seen[ny * wi + nx] && l[ny * wi + nx] <= 40.0 {
                            seen[ny * wi + nx] = true;
                            stack.push(ny * wi + nx);
                        }
                    }
                }
                if n >= 2 {
                    centres.push((sum.0 / n as f64, sum.1 / n as f64));
                }
            }
            let (mut total, mut lengths, mut count) = ((0.0f64, 0.0f64), 0.0f64, 0usize);
            for &(cx, cy) in &centres {
                let ring: Vec<(f64, f64, f64)> = (-9i32..=9)
                    .flat_map(|dy| (-9i32..=9).map(move |dx| (dx, dy)))
                    .filter_map(|(dx, dy)| {
                        let r2 = (dx * dx + dy * dy) as f64;
                        let (x, y) = (cx.round() as i32 + dx, cy.round() as i32 + dy);
                        ((9.0..=64.0).contains(&r2) && x >= 0 && y >= 0 && (x as usize) < wi && (y as usize) < hi)
                            .then(|| (dx as f64, dy as f64, l[y as usize * wi + x as usize] as f64))
                    })
                    .collect();
                if ring.len() < 150 {
                    continue;
                }
                let mean = ring.iter().map(|r| r.2).sum::<f64>() / ring.len() as f64;
                let a = ring.iter().fold((0.0, 0.0), |a, r| (a.0 + r.0 * (r.2 - mean), a.1 + r.1 * (r.2 - mean)));
                total = (total.0 + a.0, total.1 + a.1);
                lengths += (a.0 * a.0 + a.1 * a.1).sqrt();
                count += 1;
            }
            let along = total.0 / lengths.max(1e-6);
            ((total.0 * total.0 + total.1 * total.1).sqrt() / lengths.max(1e-6), along, count)
        };
        let ((p, pa, pn), (x, xa, xn)) = (agreement(&plain[0]), agreement(&hex[0]));
        assert!(pn >= 20 && xn >= 20, "the fixture: too few bumps found ({pn} plain, {xn} hex)");
        // The camera looks down with +x to the right of the picture.
        assert!(p > 0.8 && pa > 0.7, "the fixture: one sun should light every plain bump from +x (agreement {p:.2}, along +x {pa:.2})");
        assert!(x > 0.8 && xa > 0.7, "hex tiling lit each hexagon's bumps from a side of its own: agreement {x:.2}, along +x {xa:.2}, over {xn} bumps");
    }

    /// **A pixel-art slot's normal map is as crisp as its pixels.** Its
    /// colour reads the nearest texel; its normal and ORM maps were read
    /// smoothly whatever the slot was, so each pixel's bump melted into the
    /// next. The fixture is a normal map tilting each texel the opposite way
    /// to its neighbours, seen close enough that a texel spans many pixels:
    /// read nearest, the light steps from texel to texel; read smoothly, it
    /// ramps. Twice: a small map, which the palette enlarges to its layer
    /// size (enlarged smoothly, each step was a ramp before any sampler read
    /// it), and a map at the layer size seen from 15 cm, where one
    /// texel spans many pixels and the sampler alone decides.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_pixel_art_slots_normal_map_is_as_crisp_as_its_pixels() {
        let checker = |side: u32| {
            let mut normal = Vec::new();
            for y in 0..side {
                for x in 0..side {
                    let t: u8 = if (x + y) % 2 == 0 { 218 } else { 38 };
                    normal.extend_from_slice(&[t, 128, 218, 255]);
                }
            }
            floptle_render::TextureData { pixels: normal, width: side, height: side }
        };
        let grey = |side: u32| floptle_render::TextureData { pixels: [180u8, 180, 180, 255].repeat((side * side) as usize), width: side, height: side };
        let sun = [(sun_from(1.0, 0.0), 1.0)];
        let (w, h) = (320u32, 240u32);
        for (side, height) in [(8u32, 6.0f64), (256, 0.15)] {
        let shoot = |tag: &str, filter| {
            terrain_photos_filtered(&format!("{tag}{side}"), grey(side), Some(checker(side)), None, 0.0, filter, None, height, (w, h), &sun)
        };
        let crisp = shoot("pixelnormal", crate::assets::FilterMode::Pixelated);
        if crisp.is_empty() {
            return;
        }
        let soft = shoot("smoothnormal", crate::assets::FilterMode::Smooth);
        // How many sharp edges the light has along the middle rows: single-
        // pixel steps across at least half the row's range, per row. A texel
        // edge read nearest is one such step, so a row crossing a dozen
        // texels has about a dozen; read smoothly, an edge is a ramp of small
        // steps and none counts. (A chunk seam may add one.)
        let steps = |px: &[u8]| -> f32 {
            let l = |x: u32, y: u32| {
                let i = ((y * w + x) * 4) as usize;
                px[i] as f32 * 0.3 + px[i + 1] as f32 * 0.59 + px[i + 2] as f32 * 0.11
            };
            let rows: Vec<f32> = (100..140)
                .step_by(4)
                .map(|y| {
                    let row: Vec<f32> = (40..w - 40).map(|x| l(x, y)).collect();
                    let range = row.iter().copied().fold(f32::MIN, f32::max) - row.iter().copied().fold(f32::MAX, f32::min);
                    row.windows(2).filter(|p| (p[1] - p[0]).abs() > range * 0.5).count() as f32
                })
                .collect();
            rows.iter().sum::<f32>() / rows.len() as f32
        };
        let (c, s) = (steps(&crisp[0]), steps(&soft[0]));
        assert!(s < 2.5, "the fixture ({side} texels): a smoothly read map should ramp between texels, {s:.1} sharp edges a row");
        assert!(c > 6.0, "a pixel-art slot's {side}-texel normal map was read smoothly: {c:.1} sharp edges a row (smooth slot {s:.1})");
        }
    }

    /// **A slot's ORM map shades it.** Its occlusion darkens the light from
    /// everywhere and not the sun's; its roughness, multiplied into the
    /// material's, makes a smooth slot catch the sun's highlight that a rough
    /// one spreads out.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_slots_orm_map_darkens_its_crevices_and_sets_its_roughness() {
        // Ambient only, then the sun nearly overhead behind the camera.
        let suns = [([0.0, 1.0, 0.0], 0.0), ([0.0, 1.0, 0.05], 1.0)];
        let open = surfaced_ground("open", None, Some([255, 255, 0, 255]), &suns);
        if open.is_empty() {
            return;
        }
        let shut = surfaced_ground("shut", None, Some([40, 255, 0, 255]), &suns);
        let glossy = surfaced_ground("glossy", None, Some([255, 25, 0, 255]), &suns);
        assert!(open[0][0] > 5.0, "the fixture: the ambient light should show, {open:?}");
        assert!(shut[0][0] < open[0][0] * 0.5, "occlusion 40/255 should darken the ambient: {shut:?} against {open:?}");
        // The sun's highlight: a small bright spot on smooth ground, spread
        // out to nothing on rough ground.
        assert!(glossy[1][1] > open[1][1] * 1.3, "roughness 0.1 should catch the sun's highlight that 1 spreads out: {glossy:?} against {open:?}");
    }

    /// PICTURES (run by hand): PBR terrain slots as an artist would set them
    /// up. Four slots on flat ground: a flagstone photo with a normal, ORM and
    /// height made from its own brightness; concrete and asphalt sets
    /// (colour, normal, ORM at 1024 texels); granite with a separate rock
    /// normal, hex-tiled. Low sun from each side, the maps on and off.
    /// `FLOPTLE_SHOWCASE_TEXTURES=<a textures folder> FLOPTLE_SURFACE_PNG=<dir>
    /// cargo test -p floptle-editor --lib pbr_terrain_showcase -- --ignored`;
    /// the folder holds `env/terrain/03_flagstone.png`, `env/terrain/14_granite.png`,
    /// `flora/rock_n.png` and `base/{concrete,asphalt}_{albedo,normal,orm}.png`.
    #[cfg(feature = "editor-ui")]
    #[test]
    #[ignore]
    fn pbr_terrain_showcase() {
        let (Ok(out), Ok(textures)) = (std::env::var("FLOPTLE_SURFACE_PNG"), std::env::var("FLOPTLE_SHOWCASE_TEXTURES")) else { return };
        let solar = std::path::Path::new(&textures);
        let load = |rel: &str| floptle_assets::load_texture(&solar.join(rel)).expect(rel);
        // A normal, ORM and height from a colour photo: brightness as height.
        let derive = |t: &floptle_render::TextureData, depth: f32| {
            let (w, h) = (t.width as i32, t.height as i32);
            let lum = |x: i32, y: i32| {
                let i = (((y.rem_euclid(h)) * w + x.rem_euclid(w)) * 4) as usize;
                (t.pixels[i] as f32 * 0.3 + t.pixels[i + 1] as f32 * 0.59 + t.pixels[i + 2] as f32 * 0.11) / 255.0
            };
            let blur = |x: i32, y: i32| (lum(x, y) * 4.0 + lum(x + 1, y) + lum(x - 1, y) + lum(x, y + 1) + lum(x, y - 1)) / 8.0;
            let (mut n, mut orm, mut ht) = (Vec::new(), Vec::new(), Vec::new());
            for y in 0..h {
                for x in 0..w {
                    let hx = (blur(x + 1, y) - blur(x - 1, y)) * depth;
                    let hy = (blur(x, y + 1) - blur(x, y - 1)) * depth;
                    let v = [-hx, hy, 1.0f32];
                    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                    let enc = |c: f32| ((c / l * 0.5 + 0.5) * 255.0).round() as u8;
                    n.extend_from_slice(&[enc(v[0]), enc(v[1]), enc(v[2]), 255]);
                    let b = blur(x, y);
                    let ao = ((0.35 + b * 1.3).min(1.0) * 255.0) as u8;
                    let rough = ((0.95 - b * 0.55).clamp(0.2, 1.0) * 255.0) as u8;
                    orm.extend_from_slice(&[ao, rough, 0, 255]);
                    let hb = (b * 255.0) as u8;
                    ht.extend_from_slice(&[hb, hb, hb, 255]);
                }
            }
            let tex = |p| floptle_render::TextureData { pixels: p, width: t.width, height: t.height };
            (tex(n), tex(orm), tex(ht))
        };
        let shoot = |tag: &str, maps_on: bool| {
            let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
            let dir = std::env::temp_dir().join(format!("floptle_showcase_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
            let save = |name: &str, t: &floptle_render::TextureData| {
                std::fs::write(dir.join("textures").join(name), floptle_assets::encode_png(t).unwrap()).unwrap();
                format!("textures/{name}")
            };
            let flag = load("env/terrain/03_flagstone.png");
            let (fn_, fo, fh) = derive(&flag, 3.0);
            let slots = [
                (save("flag.png", &flag), Some(save("flag_n.png", &fn_)), Some(save("flag_orm.png", &fo)), Some(save("flag_h.png", &fh)), 0.0, 1.0),
                (save("concrete.png", &load("base/concrete_albedo.png")), Some(save("concrete_n.png", &load("base/concrete_normal.png"))), Some(save("concrete_orm.png", &load("base/concrete_orm.png"))), None, 0.0, 0.6),
                (save("asphalt.png", &load("base/asphalt_albedo.png")), Some(save("asphalt_n.png", &load("base/asphalt_normal.png"))), Some(save("asphalt_orm.png", &load("base/asphalt_orm.png"))), None, 0.0, 0.8),
                (save("granite.png", &load("env/terrain/14_granite.png")), Some(save("rock_n.png", &load("flora/rock_n.png"))), None, None, 1.0, 1.0),
            ];
            ed.project_root = dir.clone();
            ed.project.terrain_layer_size = 1024;
            ed.terrain_textures = slots.iter().map(|s| s.0.clone()).collect();
            ed.terrain_tex_maps = slots
                .iter()
                .map(|s| {
                    if !maps_on {
                        return crate::terrain_edit::SlotMaps::default();
                    }
                    crate::terrain_edit::SlotMaps {
                        normal: s.1.clone().unwrap_or_default(),
                        orm: s.2.clone().unwrap_or_default(),
                        height: s.3.clone().unwrap_or_default(),
                    }
                })
                .collect();
            ed.terrain_tex_tile = slots.iter().map(|s| [s.4, if s.4 > 0.0 { 0.4 } else { 0.0 }, 40.0]).collect();
            ed.terrain_tex_scale = slots.iter().map(|s| s.5).collect();
            ed.terrain_tex_blend = vec![1.5; 4];
            ed.terrain_tex_rough = vec![0.5; 4];
            for s in &slots {
                ed.texture_settings.insert(
                    s.0.clone(),
                    crate::assets::TexSetting { filter: crate::assets::FilterMode::SmoothMipmaps, ..Default::default() },
                );
            }
            land_palette(&mut ed);
            let mut field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
                seed: 3,
                radius: 400.0,
                voxel: 1.0,
                relief: 0.0,
                cave_depth: 0.0,
                craters: 0,
                surface_a: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
                surface_b: floptle_field::procgen::LayerPaint { slot: 1, color: [1.0; 3] },
                subsoil_depth: 0.0,
                ..Default::default()
            });
            let top = field.raycast(Vec3::new(0.0, 600.0, 0.0), Vec3::NEG_Y, 600.0).unwrap();
            // Concrete to the left, asphalt to the right, granite beyond; flagstone the rest.
            for (x, z, slot) in [(-7.0f32, -4.0f32, 2u8), (7.0, -4.0, 3), (0.0, -16.0, 4)] {
                field.paint_texture(Vec3::new(top.x + x, top.y, top.z + z), 6.5, slot);
            }
            let body = add_body(&mut ed, 1, DVec3::ZERO, &field);
            ed.world.insert(
                body,
                floptle_core::Material { shading: floptle_core::material::Shading::Physical, roughness: 1.0, metallic: 0.0, ..Default::default() },
            );
            settle(&mut ed);
            let sun = ed.world.spawn();
            let (w, h) = (960u32, 540u32);
            let views: [(&str, DVec3, f32, [f32; 3]); 8] = [
                ("walk_sun_left", DVec3::new(0.0, 1.7, 5.0), -0.38, [-0.95, 0.3, -0.1]),
                ("walk_sun_right", DVec3::new(0.0, 1.7, 5.0), -0.38, [0.95, 0.3, -0.1]),
                ("overhead", DVec3::new(0.0, 16.0, 4.0), -1.25, [-0.8, 0.5, -0.3]),
                ("concrete_sun_left", DVec3::new(-7.0, 1.0, -1.0), -0.7, [-0.95, 0.28, 0.1]),
                ("concrete_sun_right", DVec3::new(-7.0, 1.0, -1.0), -0.7, [0.95, 0.28, 0.1]),
                ("asphalt", DVec3::new(7.0, 1.0, -1.0), -0.7, [-0.6, 0.35, -0.7]),
                ("flagstone", DVec3::new(0.0, 0.9, 4.0), -0.75, [-0.9, 0.35, 0.2]),
                ("granite_edge", DVec3::new(0.0, 1.2, -8.5), -0.7, [0.85, 0.4, -0.3]),
            ];
            for (name, at, pitch, sun_dir) in views {
                ed.world.insert(
                    sun,
                    floptle_core::Light { direction: sun_dir, intensity: 1.5, ambient: [0.3, 0.32, 0.38], shadows: true, ..Default::default() },
                );
                ed.camera.position = DVec3::new(top.x as f64, top.y as f64, top.z as f64) + at;
                ed.camera.pitch = pitch;
                ed.camera.yaw = 0.0;
                let pos = ed.camera.position;
                ed.terrain_gpu_dirty = true;
                settle(&mut ed);
                ed.settle_terrain_meshes(pos, std::time::Duration::from_secs(120));
                let cam = ed.camera.render_camera();
                let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false).expect("a frame");
                let tex = floptle_render::TextureData { pixels: px, width: w, height: h };
                std::fs::write(std::path::Path::new(&out).join(format!("{tag}_{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            }
            let _ = std::fs::remove_dir_all(&dir);
        };
        shoot("maps", true);
        shoot("nomaps", false);
    }

    /// The 1×1 level of palette layer `layer`: the slot's colour as the
    /// whole ground sees it from far off, right now.
    fn palette_colour(ed: &crate::Editor, layer: u32) -> [u8; 4] {
        let (gpu, raster) = (ed.gpu.as_ref().unwrap(), ed.raster.as_ref().unwrap());
        let tex = raster.terrain_palette_texture();
        let px = floptle_render::probe::readback_layer(gpu, tex, layer, tex.mip_level_count() - 1);
        [px[0], px[1], px[2], px[3]]
    }

    /// **The ground shows its own colours from the first frame, and a palette
    /// read once is never decoded again.** The palette was white until every
    /// slot had been decoded and resized on the worker: seconds of white
    /// ground under load at 512 texels. Now the frame the list arrives has a
    /// palette of each slot's mean colour on the GPU (mid grey the first time
    /// a picture is ever seen), the pictures land into it, and each one is
    /// kept under `.floptle/cache/palette/` with its mean, so a second load
    /// shows the right colours at once and decodes nothing.
    #[test]
    fn a_palette_shows_its_colours_from_the_first_frame_and_decodes_once() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_palette_first_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        let colours = [[200u8, 30, 30, 255], [30, 60, 210, 255]];
        let names = ["red.png", "blue.png"];
        for (c, n) in colours.iter().zip(names) {
            let tex = floptle_render::TextureData { pixels: c.repeat(64 * 64), width: 64, height: 64 };
            std::fs::write(dir.join("textures").join(n), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        }
        let files: Vec<std::path::PathBuf> = names.iter().map(|n| dir.join("textures").join(n)).collect();
        let decoded = || {
            crate::palette_cache::DECODED.lock().unwrap().iter().filter(|p| files.contains(p)).count()
        };
        let white = |c: [u8; 4]| c[0] > 245 && c[1] > 245 && c[2] > 245;
        let near = |c: [u8; 4], want: [u8; 4]| (0..3).all(|k| (c[k] as i32 - want[k] as i32).abs() <= 6);
        let setup = |ed: &mut crate::Editor| {
            ed.project_root = dir.clone();
            ed.terrain_textures = names.iter().map(|n| format!("textures/{n}")).collect();
            ed.terrain_textures_dirty = true;
        };
        // The first time: nothing cached, nothing white.
        setup(&mut ed);
        ed.sync_terrain_palette(false);
        for slot in 0..2 {
            let c = palette_colour(&ed, slot);
            assert!(!white(c), "slot {slot} was white on the first frame: {c:?}");
        }
        ed.sync_terrain_palette(true);
        assert!(ed.terrain_palette_job.is_none(), "the palette should have landed");
        assert_eq!(decoded(), 2, "each picture is decoded the first time");
        for (slot, want) in colours.iter().enumerate() {
            assert!(near(palette_colour(&ed, slot as u32), *want), "slot {slot} landed as {:?}", palette_colour(&ed, slot as u32));
        }
        // A second editor on the same project: its first frame is already
        // each slot's own colour, and nothing is decoded.
        drop(ed);
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        setup(&mut ed);
        ed.sync_terrain_palette(false);
        for (slot, want) in colours.iter().enumerate() {
            let c = palette_colour(&ed, slot as u32);
            assert!(near(c, *want), "slot {slot} showed {c:?} on the first frame of a second load, not its colour {want:?}");
        }
        ed.sync_terrain_palette(true);
        assert_eq!(decoded(), 2, "a second load decoded a picture the cache holds");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A palette lands a few slots a frame.** Writing every slot's picture
    /// and its mips in the frame the last one was read was up to 170 MB of
    /// writes at once at 1024 texels; a palette arriving now costs each
    /// frame a few slots, and all of them arrive.
    #[test]
    fn palette_slots_land_a_few_a_frame() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_palette_budget_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        let mut names = Vec::new();
        for i in 0..32u8 {
            let c = [i * 7, 255 - i * 7, 100, 255];
            let tex = floptle_render::TextureData { pixels: c.repeat(16), width: 4, height: 4 };
            let n = format!("textures/s{i}.png");
            std::fs::write(dir.join(&n), floptle_assets::encode_png(&tex).unwrap()).unwrap();
            names.push(n);
        }
        ed.project_root = dir.clone();
        ed.terrain_textures = names;
        ed.terrain_textures_dirty = true;
        let budget = super::palette_slots_per_frame(256);
        // Start the read, then give the worker time to read every slot, so
        // the whole palette is waiting when the frames come to land it.
        ed.sync_terrain_palette(false);
        let (mut frames, mut total) = (usize::from(ed.terrain_palette_landed_last > 0), ed.terrain_palette_landed_last);
        std::thread::sleep(std::time::Duration::from_millis(500));
        for _ in 0..20_000 {
            ed.sync_terrain_palette(false);
            let n = ed.terrain_palette_landed_last;
            assert!(n <= budget, "a frame landed {n} slots, over its {budget}");
            if n > 0 {
                frames += 1;
                total += n;
            }
            if ed.terrain_palette_job.is_none() && !ed.terrain_textures_dirty {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(total, 32, "every slot should land");
        assert!(frames >= 32 / budget, "32 slots landed over only {frames} frames");
        let last = palette_colour(&ed, 31);
        assert!((last[0] as i32 - 217).abs() <= 3 && (last[1] as i32 - 38).abs() <= 3, "the last slot landed as {last:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plain grey ground, a slot Material `material` (`None`: the terrain's
    /// own), a normal map `normal`: the mean and brightest pixel of the
    /// middle under each sun, and the mean red.
    #[cfg(feature = "editor-ui")]
    fn slot_material_ground(tag: &str, normal: Option<[u8; 4]>, material: Option<floptle_scene::MaterialDoc>, suns: &[([f32; 3], f32)]) -> Vec<[f32; 3]> {
        let shots = terrain_photos_filtered(
            tag,
            flat_tex([180, 180, 180, 255]),
            normal.map(flat_tex),
            None,
            0.0,
            crate::assets::FilterMode::SmoothMipmaps,
            material,
            60.0,
            (160, 120),
            suns,
        );
        shots
            .iter()
            .map(|px| {
                let (mut sum, mut peak, mut red) = (0.0f32, 0.0f32, 0.0f32);
                for y in 40..80u32 {
                    for x in 60..100u32 {
                        let i = ((y * 160 + x) * 4) as usize;
                        let l = px[i] as f32 * 0.3 + px[i + 1] as f32 * 0.59 + px[i + 2] as f32 * 0.11;
                        sum += l;
                        peak = peak.max(l);
                        red += px[i] as f32 - (px[i + 1] as f32 + px[i + 2] as f32) * 0.5;
                    }
                }
                [sum / 1600.0, peak, red / 1600.0]
            })
            .collect()
    }

    /// A slot Material: Physical, the rest as `f` sets it.
    #[cfg(feature = "editor-ui")]
    fn slot_material(f: impl FnOnce(&mut floptle_core::Material)) -> Option<floptle_scene::MaterialDoc> {
        let mut m = floptle_core::Material { shading: floptle_core::material::Shading::Physical, roughness: 1.0, metallic: 0.0, ..Default::default() };
        f(&mut m);
        Some(floptle_scene::MaterialDoc::from_material(&m))
    }

    /// **A palette slot shades with its own Material.** Wet cobbles beside
    /// dry dirt: a slot whose Material is smooth catches the sun's highlight
    /// on terrain whose own Material is rough. Before, one Material shaded
    /// every slot.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_slot_with_a_smooth_material_catches_the_highlight() {
        let sun = [([0.0, 1.0, 0.05], 1.0)];
        let dry = slot_material_ground("dry", None, None, &sun);
        if dry.is_empty() {
            return;
        }
        let wet = slot_material_ground("wet", None, slot_material(|m| m.roughness = 0.12), &sun);
        assert!(wet[0][1] > dry[0][1] * 1.3, "a smooth slot Material should catch the highlight: peak {:.1} against {:.1}", wet[0][1], dry[0][1]);
    }

    /// **A slot Material's normal strength scales its bumps, and a negative
    /// one turns them into dents.** Both signs probed: a map tilting to the
    /// picture's right faces +x at strength 2 and −x at strength −1.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_negative_normal_strength_turns_bumps_into_dents() {
        let suns = [(sun_from(1.0, 0.0), 1.0), (sun_from(-1.0, 0.0), 1.0)];
        let right = Some([218, 128, 218, 255]);
        let up = slot_material_ground("nstrong", right, slot_material(|m| m.normal_strength = 2.0), &suns);
        if up.is_empty() {
            return;
        }
        let flipped = slot_material_ground("nflip", right, slot_material(|m| m.normal_strength = -1.0), &suns);
        let flat = slot_material_ground("nzero", right, slot_material(|m| m.normal_strength = 0.0), &suns);
        assert!(up[0][0] > up[1][0] * 1.3, "strength 2 should face +x: {up:?}");
        assert!(flipped[1][0] > flipped[0][0] * 1.3, "strength -1 should face -x: {flipped:?}");
        assert!((flat[0][0] - flat[1][0]).abs() < flat[0][0] * 0.05, "strength 0 should light both sides alike: {flat:?}");
    }

    /// **A slot Material can glow, and can be unlit.** With no sun at all, a
    /// slot whose Material is emissive orange shows orange, and an unlit slot
    /// shows its own colour, where the terrain's own Material leaves the
    /// ground in the dark.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_slot_material_can_glow_and_be_unlit() {
        let dark = [([0.0, 1.0, 0.0], 0.0)];
        let plain = slot_material_ground("dark", None, None, &dark);
        if plain.is_empty() {
            return;
        }
        let glow = slot_material_ground("glow", None, slot_material(|m| {
            m.emissive = [1.0, 0.35, 0.0];
            m.emissive_strength = 2.0;
        }), &dark);
        let unlit = slot_material_ground("unlit", None, slot_material(|m| m.unlit = true), &dark);
        assert!(glow[0][2] > plain[0][2] + 40.0, "an emissive slot should glow orange in the dark: red {:.1} against {:.1}", glow[0][2], plain[0][2]);
        assert!(unlit[0][0] > plain[0][0] * 2.0 && unlit[0][0] > 120.0, "an unlit slot should show its colour in the dark: {:.1} against {:.1}", unlit[0][0], plain[0][0]);
    }

    /// **The palette is one array, at the size the project asks for.** The
    /// raymarch and the meshed terrain each built their own copy; and every
    /// layer was cut to 256 texels whatever the project wanted.
    #[test]
    fn the_palette_is_one_array_at_the_projects_layer_size() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let dir = std::env::temp_dir().join(format!("floptle_palette_size_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
        let tex = floptle_render::TextureData { pixels: vec![128; 1024 * 1024 * 4], width: 1024, height: 1024 };
        std::fs::write(dir.join("textures/grey.png"), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        ed.project_root = dir.clone();
        ed.terrain_textures = vec!["textures/grey.png".into()];
        let held = |ed: &crate::Editor| {
            let raster = ed.raster.as_ref().unwrap().terrain_palette_texture().clone();
            let raymarch = ed.raymarch.as_ref().unwrap().terrain_palette_texture().clone();
            assert!(raster == raymarch, "the raymarch and the raster hold separate palettes");
            (raster.width(), raster.mip_level_count())
        };
        land_palette(&mut ed);
        assert_eq!(held(&ed), (256, 9), "a project that names no size keeps 256 texels, with every mip");
        // Changing the setting alone reads the palette again.
        ed.project.terrain_layer_size = 512;
        ed.sync_terrain_palette(true);
        assert_eq!(held(&ed), (512, 10));
        ed.project.terrain_layer_size = 1024;
        ed.sync_terrain_palette(true);
        assert_eq!(held(&ed), (1024, 11));
        let _ = std::fs::remove_dir_all(&dir);
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
                floptle_script::TerrainOp { pos: [p.x as f64, p.y as f64, p.z as f64], radius: 3.0, strength: 1.0, mode, id: 0, normal: None, brush: None }
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
