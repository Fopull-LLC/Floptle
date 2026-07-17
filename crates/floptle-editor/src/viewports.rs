//! Offscreen views: the Inspector's asset/material preview, the camera
//! preview, the split Game viewport, and camera-node authority helpers.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::Name;
use floptle_core::math::Mat3;
use floptle_core::math::Mat4;
use floptle_core::math::Quat;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use floptle_render::Globals;
use floptle_render::Gpu;
use floptle_render::InstanceRaw;
use floptle_render::MaterialParams;
use floptle_render::MeshId;
use floptle_render::Projection;
use floptle_render::RenderCamera;
use floptle_render::TexId;
use floptle_render::instance_of;
use floptle_render::instance_of_mat;
use std::path::Path;
use crate::assets::{is_material, is_model, is_texture};
use crate::dock::{EditorTab, game_tab_active, scene_and_game_split};
use crate::shading::{material_params, post_process_uniforms};
use crate::{Editor, Egui, PreviewTarget, PreviewView, scene_hit};

/// Create a `w×h` offscreen color+depth target the scene renders into, and register its
/// color with egui so a tab/inspector can draw it as an `Image`.
///
/// The color texture is the sRGB **surface** format, so the raster/raymarch/post
/// pipelines (all built against `surface_format()`) render into it unchanged and the
/// render-target view stays sRGB. But egui is handed a NON-sRGB *view* of the same
/// texture: egui-wgpu treats a sampled native texture as already gamma-encoded and
/// decodes it once in its shader, so sampling through an sRGB-format view would decode a
/// SECOND time (hardware sRGB→linear) and display the offscreen view ~40% too dark
/// (`srgb_to_linear` applied twice). A linear view makes egui sample the stored bytes
/// verbatim, so the docked Game view / camera POV / asset preview match the surface. On a
/// non-sRGB surface `remove_srgb_suffix()` is a no-op, so this stays correct there too.
fn make_offscreen_target(
    gpu: &Gpu,
    egui: &mut Egui,
    w: u32,
    h: u32,
    label: &str,
    filter: wgpu::FilterMode,
) -> PreviewTarget {
    let (w, h) = (w.max(1), h.max(1));
    let srgb = gpu.surface_format();
    let linear = srgb.remove_srgb_suffix();
    let view_formats: &[wgpu::TextureFormat] = if linear != srgb { &[linear] } else { &[] };
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: srgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats,
    });
    // sRGB view = render target (pipeline unchanged); linear view = what egui samples.
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let egui_view = color.create_view(&wgpu::TextureViewDescriptor {
        format: Some(linear),
        ..Default::default()
    });
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        // TEXTURE_BINDING so a viewport's SSAO pass can sample this depth (harmless for
        // the previews that never do).
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let tex_id = egui.renderer.register_native_texture(&gpu.device, &egui_view, filter);
    PreviewTarget { color_view, depth_view, tex_id }
}

impl Editor {
    // ---- asset preview (Inspector) ------------------------------------------
    /// Lazily create the 320² offscreen target the asset preview renders into, and
    /// register its color view with egui so the Inspector can draw it as an image.
    pub(crate) fn ensure_preview_target(&mut self) {
        if self.preview.is_some() {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        // Linear: this small turntable preview is a downscale of a larger render.
        self.preview =
            Some(make_offscreen_target(gpu, egui, 320, 320, "preview", wgpu::FilterMode::Linear));
    }

    /// (Re)load a selected texture asset into an egui texture handle for preview.
    pub(crate) fn ensure_preview_image(&mut self, path: &str) {
        if self.preview_image.as_ref().is_some_and(|(p, _, _)| p == path) {
            return;
        }
        let Some(egui) = self.egui.as_ref() else { return };
        if let Some(img) = floptle_assets::load_texture(Path::new(path)) {
            // TRUE dimensions — shown as the "N×N px" label and used for aspect.
            let dims = [img.width as usize, img.height as usize];
            // A texture larger than the GPU's max 2D dimension (e.g. an 8400px-wide
            // sprite sheet) would PANIC egui's wgpu upload the instant it's selected.
            // A preview only ever displays at a few hundred px, so upload a
            // downscaled copy while keeping the true dims for the label.
            const PREVIEW_MAX: u32 = 2048;
            let upload = if img.width > PREVIEW_MAX || img.height > PREVIEW_MAX {
                let s = PREVIEW_MAX as f32 / img.width.max(img.height) as f32;
                let w = ((img.width as f32 * s).floor() as u32).max(1);
                let h = ((img.height as f32 * s).floor() as u32).max(1);
                floptle_assets::load_texture_sized(Path::new(path), w, h).unwrap_or(img)
            } else {
                img
            };
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [upload.width as usize, upload.height as usize],
                &upload.pixels,
            );
            let handle = egui.ctx.load_texture(
                format!("preview:{path}"),
                color,
                egui::TextureOptions::LINEAR,
            );
            self.preview_image = Some((path.to_string(), handle, dims));
        }
    }

    /// Each frame: build the Inspector preview for the selected asset. Models and
    /// material presets render as a turntable-spinning subject into the offscreen
    /// target; textures load as an egui image.
    pub(crate) fn update_asset_preview(&mut self, dt: f32) {
        let Some(path) = self.selected_asset.clone() else {
            self.preview_material = None;
            return;
        };
        if is_texture(&path) {
            self.ensure_preview_image(&path);
            return;
        }
        if !is_model(&path) && !is_material(&path) {
            return;
        }
        if self.preview_spinning {
            self.preview_spin += dt * 0.8;
        }

        // Resolve the subject into drawable parts + a bounding radius. Rigged
        // models supply a per-part rest matrix (their parts are node-local).
        let mut parts: Vec<(MeshId, Option<TexId>)> = Vec::new();
        let mut part_mats: Option<Vec<Mat4>> = None;
        let mut radius = 1.0f32;
        let mut mat = MaterialParams::flat([0.8, 0.8, 0.82]);
        let is_mat = is_material(&path);
        if is_model(&path) {
            if !self.import_model(&path) {
                return;
            }
            if let Some(a) = self.mesh_registry.get(&path) {
                radius = (a.size * 0.5).max(0.2);
                parts = a.parts.iter().map(|m| (*m, None)).collect();
                if let Some(rig) = a.rig.as_ref() {
                    part_mats = Some(
                        rig.part_nodes
                            .iter()
                            .map(|&n| rig.rest_world.get(n).copied().unwrap_or(Mat4::IDENTITY))
                            .collect(),
                    );
                }
            }
        } else {
            // Material preset: (re)load it from the loaded presets by file stem.
            if self.preview_material.as_ref().is_none_or(|(p, _)| p != &path) {
                let stem = Path::new(&path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Some((_, doc)) = self.materials.iter().find(|(n, _)| *n == stem) {
                    self.preview_material = Some((path.clone(), doc.to_material()));
                }
            }
            if let Some((_, material)) = self.preview_material.clone() {
                let tex = material.texture.as_ref().and_then(|t| self.ensure_texture(t));
                mat = material_params(&material);
                radius = 0.85;
                if let Some(s) = self.mesh_ids.get(1).copied() {
                    parts.push((s, tex));
                }
            }
        }
        if parts.is_empty() {
            return;
        }

        // Turntable camera: orbit the subject, looking at the origin (the subject is
        // drawn camera-relative since the view matrix carries no translation).
        let dist = (radius * 3.0 * self.preview_zoom).max(0.4);
        let a = self.preview_spin;
        let eye = Vec3::new(a.cos() * dist, radius * 0.55, a.sin() * dist);
        let fwd = (Vec3::ZERO - eye).normalize();
        let right = fwd.cross(Vec3::Y).normalize();
        let up = right.cross(fwd);
        let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
        let cam = RenderCamera::new(
            eye.as_dvec3(),
            rot,
            Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
        );
        let vp = cam.view_proj(1.0);
        let model = Mat4::from_translation(-eye); // obj at origin, camera-relative
        let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = parts
            .iter()
            .enumerate()
            .map(|(i, (m, t))| {
                let local = part_mats
                    .as_ref()
                    .and_then(|v| v.get(i))
                    .copied()
                    .unwrap_or(Mat4::IDENTITY);
                let raw = if is_mat {
                    instance_of_mat(model * local, &mat)
                } else {
                    instance_of(model * local, [1.0, 1.0, 1.0])
                };
                (*m, *t, raw)
            })
            .collect();
        let l = Vec3::new(0.5, 0.8, 0.6).normalize();
        let globals = Globals {
            view_proj: vp.to_cols_array_2d(),
            light_dir: [l.x, l.y, l.z, 0.0],
            light_color: [1.0, 0.98, 0.93, 0.0],
            ambient: [0.30, 0.32, 0.38, 0.0],
            ..Default::default()
        };

        self.ensure_preview_target();
        if let (Some(gpu), Some(raster), Some(preview)) =
            (self.gpu.as_ref(), self.raster.as_mut(), self.preview.as_ref())
        {
            raster.draw_scene(
                gpu,
                &preview.color_view,
                &preview.depth_view,
                globals,
                &instances,
                Some([0.07, 0.08, 0.10, 1.0]),
                None, // no field: previews don't receive scene shadows/AO
            );
        }
    }

    /// Lazily create the 16:9 offscreen target the selected-camera POV preview renders
    /// into, registering its color view with egui as a texture id for the Inspector.
    pub(crate) fn ensure_cam_preview_target(&mut self) {
        if self.cam_preview.is_some() {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        self.cam_preview =
            Some(make_offscreen_target(gpu, egui, 320, 180, "cam-preview", wgpu::FilterMode::Linear));
    }

    /// Each frame: if a single Camera node is selected, render the scene from its POV
    /// into the 16:9 offscreen target so the Inspector can show what it sees. Mirrors
    /// the main render path (raster meshes + raymarch blobs/terrain), camera-relative
    /// to the selected camera.
    pub(crate) fn update_camera_preview(&mut self, elapsed: f32) {
        let Some(e) = self.selection.last().copied() else { return };
        let fov_y = match self.world.get::<Matter>(e) {
            Some(Matter::Camera { fov_y, .. }) => *fov_y,
            _ => return,
        };
        let wt = floptle_core::world_transform(&self.world, e);
        let cam = RenderCamera::new(
            wt.translation,
            wt.rotation,
            Projection::Perspective { fov_y, near: 0.05, far: 4000.0 },
        );
        self.ensure_cam_preview_target();
        let Some((cv, dv)) =
            self.cam_preview.as_ref().map(|p| (p.color_view.clone(), p.depth_view.clone()))
        else {
            return;
        };
        self.render_world_into(&cv, &dv, &cam, 16.0 / 9.0, elapsed);
    }

    /// Lazily (re)create the Game viewport's offscreen target at `w`×`h` pixels, freeing
    /// the previous egui texture registration on resize.
    pub(crate) fn ensure_game_vp(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(16), h.max(16));
        if self.game_vp.is_some() && self.game_vp_dims == (w, h) {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        if let Some(old) = self.game_vp.take() {
            egui.renderer.free_texture(&old.tex_id);
        }
        // Nearest: the game view is rendered at ~1:1 with its on-screen rect, so a
        // Nearest blit stays pixel-crisp (a Linear blit softens hard-edged low-res /
        // pixel-art textures by a sub-pixel — the "blurry despite nearest filtering"
        // report). The main Scene viewport renders direct-to-surface and was already crisp.
        self.game_vp =
            Some(make_offscreen_target(gpu, egui, w, h, "game-vp", wgpu::FilterMode::Nearest));
        self.game_vp_dims = (w, h);
        // Create the viewport's own post chain lazily; its actual size + retro mode are
        // set by `configure` every frame in update_game_viewport (retro composites at the
        // internal res, not the panel res), so we don't resize it here — that would
        // reallocate all its targets twice on a resize frame.
        if self.game_post.is_none() {
            self.game_post = Some(floptle_render::PostStack::new(gpu, w, h));
        }
    }

    /// Render the active-camera "game" view into its own offscreen target sized to the
    /// Game tab's rect, whenever a docked (non-fullscreen) Game tab is front — single-view
    /// or split. The tab then blits this at its exact rect+aspect, so the game view is
    /// always framed to its panel and never spills the full-window render behind other
    /// tabs. (A FULLSCREEN Game tab renders straight to the surface — it fills the window.)
    pub(crate) fn update_game_viewport(&mut self, elapsed: f32) {
        let active = self.fullscreen_tab.is_none()
            && self.dock_state.as_ref().is_some_and(game_tab_active);
        if !active {
            return;
        }
        let ppp = self.egui.as_ref().map(|e| e.ctx.pixels_per_point()).unwrap_or(1.0);
        let (w, h) = match self.game_rect {
            Some(r) => ((r.width() * ppp).round() as u32, (r.height() * ppp).round() as u32),
            None => (640, 360),
        };
        self.ensure_game_vp(w, h);
        // The active gameplay camera, or the editor camera if the scene has none.
        let cam = {
            let active = self.world.query::<Matter>().find_map(|(e, m)| {
                matches!(m, Matter::Camera { active: true, .. }).then_some(e)
            });
            match active {
                Some(e) => {
                    let fov_y = match self.world.get::<Matter>(e) {
                        Some(Matter::Camera { fov_y, .. }) => *fov_y,
                        _ => 60f32.to_radians(),
                    };
                    let wt = floptle_core::world_transform(&self.world, e);
                    RenderCamera::new(
                        wt.translation,
                        wt.rotation,
                        Projection::Perspective { fov_y, near: 0.05, far: 4000.0 },
                    )
                }
                None => self.camera.render_camera(),
            }
        };
        let aspect = w.max(1) as f32 / h.max(1) as f32;
        let Some((cv, dv)) =
            self.game_vp.as_ref().map(|p| (p.color_view.clone(), p.depth_view.clone()))
        else {
            return;
        };
        let (post_settings, _) = post_process_uniforms(&self.world);
        let post_on = post_settings.any();
        let retro_on = self.project.retro;

        // Composited resolution: the retro internal res in retro mode (so post/AO/dither
        // land on the same chunky pixel grid as the fullscreen view, THEN upscale), else
        // the panel res. This mirrors the surface path so a docked/split Game tab looks
        // identical to fullscreen instead of rendering crisp + unprocessed.
        let (cw, ch) = if retro_on {
            let rh = self.project.retro_height.max(80);
            (((rh as f32 * aspect).round() as u32).max(1), rh)
        } else {
            (w, h)
        };
        if let Some(gpu) = self.gpu.as_ref() {
            // The game's own retro pass, sized to the PANEL aspect (the shared `retro` is
            // window-sized, and same-frame reuse would fight the surface render).
            if retro_on {
                match self.game_retro.as_mut() {
                    Some(r) if r.resolution() == (cw, ch) => {}
                    Some(r) => r.resize_to(gpu, cw, ch),
                    None => {
                        let mut r = floptle_render::Retro::new(gpu, ch);
                        r.resize_to(gpu, cw, ch);
                        self.game_retro = Some(r);
                    }
                }
            }
            if post_on && let Some(post) = self.game_post.as_mut() {
                post.configure(gpu, cw, ch, retro_on);
            }
        }

        // In retro mode the scene composites at retro res into the retro target (its own
        // color/depth); post — if any — runs there, then a nearest-neighbor blit upscales
        // into the egui-registered game_vp color. Non-retro composites straight at panel res.
        let retro_views =
            self.game_retro.as_ref().map(|r| (r.color_view().clone(), r.depth_view().clone()));
        let depth = if retro_on {
            retro_views.as_ref().map(|(_, d)| d.clone()).unwrap_or_else(|| dv.clone())
        } else {
            dv.clone()
        };
        let scene_target = if post_on {
            self.game_post.as_ref().map(|p| p.input_view().clone())
        } else if retro_on {
            retro_views.as_ref().map(|(c, _)| c.clone())
        } else {
            Some(cv.clone())
        };
        let Some(scene_target) = scene_target else { return };
        self.render_world_into(&scene_target, &depth, &cam, aspect, elapsed);
        // Post composites into the retro color (retro) or the game_vp color (non-retro).
        if post_on && let (Some(gpu), Some(post)) = (self.gpu.as_ref(), self.game_post.as_ref()) {
            let proj = cam.proj_matrix(aspect);
            let ssao_frame = floptle_render::SsaoFrame {
                depth: &depth,
                proj: proj.to_cols_array_2d(),
                inv_proj: proj.inverse().to_cols_array_2d(),
            };
            let out = if retro_on {
                retro_views.as_ref().map(|(c, _)| c.clone()).unwrap_or_else(|| cv.clone())
            } else {
                cv.clone()
            };
            post.run(gpu, &post_settings, Some(&ssao_frame), &out);
        }
        // Retro upscale: chunky nearest-neighbor blit of the retro color into game_vp.
        if retro_on && let (Some(gpu), Some(retro)) = (self.gpu.as_ref(), self.game_retro.as_ref()) {
            retro.blit_to(gpu, &cv);
        }
        // ---- game UI: the docked Game view shows exactly what a build shows ----
        let ui_layers = self.gather_game_ui([w.max(1) as f32, h.max(1) as f32]);
        if !ui_layers.is_empty()
            && let (Some(gpu), Some(raster), Some(uir)) =
                (self.gpu.as_ref(), self.raster.as_ref(), self.ui_render.as_mut())
        {
            let vp = [w.max(1) as f32, h.max(1) as f32];
            let mut ui_instances = Vec::new();
            let mut ui_batches = Vec::new();
            for (dl, scale) in &ui_layers {
                let reg = &self.texture_registry;
                let uic = &self.ui_flsl_cache;
                let uib = &self.ui_flsl_binds;
                uir.pack(
                    gpu,
                    dl,
                    [0.0, 0.0],
                    *scale,
                    &mut |p| reg.get(p).copied(),
                    &mut |p, owner| {
                        let shader = uic.get(p).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id)?;
                        Some((shader, uib.get(&owner)?.binding))
                    },
                    &mut ui_instances,
                    &mut ui_batches,
                );
            }
            uir.draw(gpu, &cv, vp, &ui_instances, &ui_batches, raster);
        }
    }

    /// What the Inspector should draw for the current selection's preview.
    pub(crate) fn preview_view(&self) -> Option<PreviewView> {
        let path = self.selected_asset.as_ref()?;
        if is_texture(path) {
            let (_, handle, dims) = self.preview_image.as_ref()?;
            Some(PreviewView::Image(handle.clone(), *dims))
        } else if is_model(path) || is_material(path) {
            Some(PreviewView::Rendered(self.preview.as_ref()?.tex_id))
        } else {
            None
        }
    }

    /// True when the Game viewport is the FOCUSED viewport — it renders the active-camera
    /// "as a build" view, so editor interactions (pick/select, sculpt, gizmos, editor
    /// keybinds + free-fly camera) are suppressed there; only the game's own inputs run.
    /// When the Scene and Game tabs are split (both visible), focus follows the pointer:
    /// the game is focused only while the mouse is over its viewport, so you can still
    /// edit in the Scene view and the game only gets input when you're in it.
    pub(crate) fn game_view(&self) -> bool {
        match self.fullscreen_tab {
            Some(EditorTab::Game) => return true,
            Some(_) => return false,
            None => {}
        }
        let Some(dock) = self.dock_state.as_ref() else { return false };
        if scene_and_game_split(dock) {
            return self
                .egui
                .as_ref()
                .is_some_and(|e| scene_hit(&e.ctx, self.cursor, self.game_rect));
        }
        game_tab_active(dock)
    }

    // ---- cameras -----------------------------------------------------------
    /// The camera node that currently holds play-mode authority (active = true).
    pub(crate) fn active_camera(&self) -> Option<Entity> {
        self.world
            .query::<Matter>()
            .find_map(|(e, m)| matches!(m, Matter::Camera { active: true, .. }).then_some(e))
    }

    /// Spawn a camera node at the current editor viewpoint (so "what you see is the
    /// shot"). The first camera in a scene becomes the active one.
    pub(crate) fn add_camera_node(&mut self, parent: Option<Entity>) {
        self.record();
        let cam = self.camera.render_camera();
        let active = self.active_camera().is_none();
        let e = self.world.spawn();
        self.world.insert(
            e,
            Transform {
                translation: cam.world_position,
                rotation: cam.rotation,
                scale: Vec3::ONE,
            },
        );
        let n = self.world.query::<Matter>().filter(|(_, m)| matches!(m, Matter::Camera { .. })).count() + 1;
        self.world.insert(e, Name(format!("Camera {n}")));
        self.world.insert(e, Matter::Camera { fov_y: 60f32.to_radians(), active });
        if let Some(p) = parent {
            self.world.insert(e, floptle_core::Parent(p));
        }
        self.select_single(e);
    }

    /// Give `e` play-mode authority, clearing it from every other camera.
    pub(crate) fn set_active_camera(&mut self, e: Entity) {
        self.record(); // undoable, like every other scene mutation
        let cams: Vec<Entity> = self
            .world
            .query::<Matter>()
            .filter_map(|(c, m)| matches!(m, Matter::Camera { .. }).then_some(c))
            .collect();
        for c in cams {
            if let Some(Matter::Camera { active, .. }) = self.world.get_mut::<Matter>(c) {
                *active = c == e;
            }
        }
        if !self.playing {
            self.scene_dirty = true;
        }
    }

    /// Move a camera node to the current editor viewpoint.
    pub(crate) fn camera_to_view(&mut self, e: Entity) {
        self.record();
        let cam = self.camera.render_camera();
        if let Some(t) = self.world.get_mut::<Transform>(e) {
            t.translation = cam.world_position;
            t.rotation = cam.rotation;
        }
    }
}
