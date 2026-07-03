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
use crate::{Editor, PreviewTarget, PreviewView, scene_hit};

impl Editor {
    // ---- asset preview (Inspector) ------------------------------------------
    /// Lazily create the 320² offscreen target the asset preview renders into, and
    /// register its color view with egui so the Inspector can draw it as an image.
    pub(crate) fn ensure_preview_target(&mut self) {
        if self.preview.is_some() {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        let size = 320u32;
        let make = |fmt: wgpu::TextureFormat, usage: wgpu::TextureUsages, label| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage,
                view_formats: &[],
            })
        };
        let color = make(
            gpu.surface_format(),
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "preview-color",
        );
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = make(Gpu::DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT, "preview-depth");
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let tex_id =
            egui.renderer.register_native_texture(&gpu.device, &color_view, wgpu::FilterMode::Linear);
        self.preview = Some(PreviewTarget { color_view, depth_view, tex_id });
    }

    /// (Re)load a selected texture asset into an egui texture handle for preview.
    pub(crate) fn ensure_preview_image(&mut self, path: &str) {
        if self.preview_image.as_ref().is_some_and(|(p, _, _)| p == path) {
            return;
        }
        let Some(egui) = self.egui.as_ref() else { return };
        if let Some(img) = floptle_assets::load_texture(Path::new(path)) {
            let dims = [img.width as usize, img.height as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(dims, &img.pixels);
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
        let (w, h) = (320u32, 180u32);
        let make = |fmt: wgpu::TextureFormat, usage: wgpu::TextureUsages, label| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage,
                view_formats: &[],
            })
        };
        let color = make(
            gpu.surface_format(),
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "cam-preview-color",
        );
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth = make(Gpu::DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT, "cam-preview-depth");
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let tex_id =
            egui.renderer.register_native_texture(&gpu.device, &color_view, wgpu::FilterMode::Linear);
        self.cam_preview = Some(PreviewTarget { color_view, depth_view, tex_id });
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
        let make = |fmt: wgpu::TextureFormat, usage: wgpu::TextureUsages, label| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage,
                view_formats: &[],
            })
        };
        let color = make(
            gpu.surface_format(),
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "game-vp-color",
        );
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        // TEXTURE_BINDING so the viewport's SSAO pass can sample its depth.
        let depth = make(
            Gpu::DEPTH_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            "game-vp-depth",
        );
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let tex_id =
            egui.renderer.register_native_texture(&gpu.device, &color_view, wgpu::FilterMode::Linear);
        self.game_vp = Some(PreviewTarget { color_view, depth_view, tex_id });
        self.game_vp_dims = (w, h);
        // The viewport's own post chain, sized to match.
        match self.game_post.as_mut() {
            Some(p) => p.resize(gpu, w, h),
            None => self.game_post = Some(floptle_render::PostStack::new(gpu, w, h)),
        }
    }

    /// When the Scene and Game tabs are both visible (split), render the active-camera
    /// "game" view into its own offscreen target so the two viewports show independent
    /// views instead of the same surface render. (In single-view, the surface path draws
    /// whichever one view is shown — this is skipped.)
    pub(crate) fn update_game_viewport(&mut self, elapsed: f32) {
        let split = self.fullscreen_tab.is_none()
            && self.dock_state.as_ref().is_some_and(scene_and_game_split);
        if !split {
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
        // The scene's PostProcess node applies here too: render into the viewport's
        // own PostStack input, then run the chain (SSAO reads this viewport's depth)
        // into the egui-registered color target.
        let (post_settings, _) = post_process_uniforms(&self.world);
        if post_settings.any() && self.game_post.is_some() {
            let input = self.game_post.as_ref().map(|p| p.input_view().clone()).unwrap();
            self.render_world_into(&input, &dv, &cam, aspect, elapsed);
            if let (Some(gpu), Some(post)) = (self.gpu.as_ref(), self.game_post.as_ref()) {
                let proj = cam.proj_matrix(aspect);
                let ssao_frame = floptle_render::SsaoFrame {
                    depth: &dv,
                    proj: proj.to_cols_array_2d(),
                    inv_proj: proj.inverse().to_cols_array_2d(),
                };
                post.run(gpu, &post_settings, Some(&ssao_frame), &cv);
            }
        } else {
            self.render_world_into(&cv, &dv, &cam, aspect, elapsed);
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
        self.scene_dirty = true;
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
