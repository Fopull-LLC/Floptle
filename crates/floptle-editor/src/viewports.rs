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
        let (fov_y, mask, ortho, oh) = match self.world.get::<Matter>(e) {
            Some(Matter::Camera { fov_y, cull_mask, ortho, ortho_height, .. }) => {
                (*fov_y, *cull_mask, *ortho, *ortho_height)
            }
            _ => return,
        };
        let wt = floptle_core::world_transform(&self.world, e);
        let cam = RenderCamera::new(
            wt.translation,
            wt.rotation,
            Projection::of_camera(fov_y, ortho, oh, 0.05, 300000.0),
        );
        self.ensure_cam_preview_target();
        let Some((cv, dv)) =
            self.cam_preview.as_ref().map(|p| (p.color_view.clone(), p.depth_view.clone()))
        else {
            return;
        };
        self.render_world_into(&cv, &dv, &cam, 16.0 / 9.0, elapsed, mask, None, (320, 180));
    }

    /// A1 render targets: every camera with a non-empty `target` name renders
    /// the world into its live `rt:<name>` texture — BEFORE any pass that
    /// might sample it (the main surface, the game viewport, previews).
    /// Runs in edit mode too, so a cockpit screen shows its feed while you
    /// place it.
    ///
    /// Each target carries its own size and refresh rate (`floptle/0078`), and
    /// the two things this cannot serve — more targets than
    /// [`Matter::TARGET_LIMIT`], and two cameras claiming one name — are
    /// reported once each rather than dropped quietly. See
    /// [`crate::render_targets`] for the decision itself.
    pub(crate) fn update_render_targets(&mut self, elapsed: f32) {
        let reqs = crate::render_targets::target_requests(&self.world);
        if reqs.is_empty() {
            return;
        }
        let plan = crate::render_targets::plan_render_targets(
            reqs,
            elapsed,
            &self.render_target_last,
        );
        for name in plan.dropped.iter().chain(plan.duplicates.iter()) {
            // Warned once per name: a per-frame log line would bury everything
            // else in the Console, and this is a scene-authoring mistake that
            // does not change until the scene does.
            if self.render_target_warned.insert(name.clone()) {
                if plan.dropped.contains(name) {
                    log::warn!(
                        "render target \"{name}\" is not being drawn: a scene may hold {} live \
                         targets and this one is past the limit. Lower a camera's refresh rate \
                         and share one target, or clear the `target` on a camera that no longer \
                         needs one.",
                        Matter::TARGET_LIMIT
                    );
                } else {
                    log::warn!(
                        "render target \"{name}\" is claimed by more than one camera — only the \
                         first draws. Two cameras writing one texture would flicker between two \
                         viewpoints; give each its own target name."
                    );
                }
            }
        }
        for r in plan.draw {
            // Allocate on first use, and RE-allocate when the camera asks for a
            // different size — otherwise a script that resizes its minimap gets
            // the old texture and nothing says why.
            let stale = self
                .render_targets
                .get(&r.name)
                .is_none_or(|t| (t.w, t.h) != (r.w, r.h));
            if stale {
                let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                    return;
                };
                let (tex, color, depth) = raster.register_render_target(gpu, r.w, r.h);
                // The registry key materials/UI use. Stale entries from renamed
                // targets keep their last frame — harmless, and re-rendered the
                // moment a camera claims the name again.
                self.texture_registry.insert(format!("rt:{}", r.name), tex);
                self.render_targets.insert(
                    r.name.clone(),
                    crate::render_targets::RenderTarget { tex, color, depth, w: r.w, h: r.h },
                );
            }
            let (tex, cv, dv) = {
                let s = &self.render_targets[&r.name];
                (s.tex, s.color.clone(), s.depth.clone())
            };
            let wt = floptle_core::world_transform(&self.world, r.e);
            let cam = RenderCamera::new(
                wt.translation,
                wt.rotation,
                Projection::of_camera(r.fov_y, r.ortho, r.ortho_height, 0.05, 300000.0),
            );
            // skip_tex = its own target: a camera can film another camera's
            // screen, never its own mid-pass (wgpu forbids it).
            self.render_world_into(
                &cv,
                &dv,
                &cam,
                r.w as f32 / r.h as f32,
                elapsed,
                r.mask,
                Some(tex),
                (r.w, r.h),
            );
            self.render_target_last.insert(r.name, elapsed);
        }
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
        if !self.game_offscreen() {
            return;
        }
        let (tab_org, tab_px) =
            self.game_tab_px().unwrap_or(([0.0, 0.0], [640.0, 360.0]));
        let (w, h) = (tab_px[0] as u32, tab_px[1] as u32);
        self.ensure_game_vp(w, h);
        // The active gameplay camera, or the editor camera if the scene has none.
        let mut cull_mask = u32::MAX;
        let cam = {
            let active = self.world.query::<Matter>().find_map(|(e, m)| {
                matches!(m, Matter::Camera { active: true, .. }).then_some(e)
            });
            match active {
                Some(e) => {
                    let (fov_y, ortho, oh) = match self.world.get::<Matter>(e) {
                        Some(Matter::Camera { fov_y, cull_mask: cm, ortho, ortho_height, .. }) => {
                            cull_mask = *cm;
                            (*fov_y, *ortho, *ortho_height)
                        }
                        _ => (60f32.to_radians(), false, Matter::ORTHO_HEIGHT),
                    };
                    let wt = floptle_core::world_transform(&self.world, e);
                    RenderCamera::new(
                        wt.translation,
                        wt.rotation,
                        Projection::of_camera(fov_y, ortho, oh, 0.05, 300000.0),
                    )
                }
                None => self.camera.render_camera(),
            }
        };
        let aspect = w.max(1) as f32 / h.max(1) as f32;
        // Feed the map's world→screen picker for the DOCKED game tab: its rect in
        // FULL-WINDOW physical pixels, matching the cursor space `input.mouse()`
        // reports. Fullscreen feeds from render_frame. Only once the tab has
        // actually reported a rect — publishing the placeholder size would have
        // `camera.exists()` answer yes with numbers off by the whole layout.
        if self.game_rect.is_some() {
            // Screen-space `draw.rect` arrives in this same cursor space.
            self.game_view_origin = [tab_org[0], tab_org[1]];
            let vp = cam.view_proj(aspect);
            self.script_host.set_view(floptle_script::ViewInfo {
                view_proj: vp.to_cols_array(),
                cam_world: [cam.world_position.x, cam.world_position.y, cam.world_position.z],
                vp_x: tab_org[0],
                vp_y: tab_org[1],
                vp_w: tab_px[0],
                vp_h: tab_px[1],
                fov_y: cam.projection.fov_y(),
                valid: true,
            });
        }
        // Script `gizmo.*` shapes for the DOCKED game tab, projected through the
        // gameplay camera into that tab's own rect (the Scene view's set is projected
        // for a different camera entirely and would land nowhere near).
        self.game_gizmo_lines.clear();
        if self.game_gizmos && self.gizmo_filter.script && !self.script_gizmos.is_empty() {
            crate::viz::project_script_gizmos(
                &self.script_gizmos,
                cam.world_position,
                cam.view_proj(aspect),
                floptle_core::math::Vec2::new(tab_org[0], tab_org[1]),
                floptle_core::math::Vec2::new(tab_px[0], tab_px[1]),
                &mut self.game_gizmo_lines,
            );
        }
        let Some((cv, dv)) =
            self.game_vp.as_ref().map(|p| (p.color_view.clone(), p.depth_view.clone()))
        else {
            return;
        };
        let (mut post_settings, _) = post_process_uniforms(&self.world);
        // The Game panel gets the player's colour-vision filter too — the whole
        // point of it being a player setting is that it applies wherever the game
        // is shown (`floptle/0079`).
        post_settings.color_filter = self.access.color_filter.lane();
        post_settings.color_filter_strength = self.access.color_filter_strength;
        post_settings.simulate_deficiency = self.access.simulate_deficiency;
        let post_on = post_settings.any();
        let retro_on = self.project.retro;

        // Composited resolution: the retro internal res in retro mode (so post/AO/dither
        // land on the same chunky pixel grid as the fullscreen view, THEN upscale), else
        // the panel res. This mirrors the surface path so a docked/split Game tab looks
        // identical to fullscreen instead of rendering crisp + unprocessed.
        let (cw, ch) = if retro_on { self.project.retro_size(aspect) } else { (w, h) };
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
        self.render_world_into(&scene_target, &depth, &cam, aspect, elapsed, cull_mask, None, (cw, ch));
        // World canvases: real geometry, so they draw into the scene target with
        // its depth, before post. `include_screen: false` — this tab shows a
        // BUILD, so screen-space layers belong in the flat overlay below, not
        // hanging in the world as authoring holograms. Without this the docked
        // tab drew no diegetic UI at all while still happily hit-testing it.
        let canvases = self.gather_ui_world(aspect, false);
        if !canvases.is_empty()
            && let (Some(gpu), Some(raster), Some(uir)) =
                (self.gpu.as_ref(), self.raster.as_ref(), self.ui_render.as_mut())
        {
            crate::ui_game::draw_ui_world(
                gpu,
                raster,
                uir,
                &self.texture_registry,
                (&self.ui_flsl_cache, &self.ui_flsl_binds),
                &scene_target,
                &depth,
                cam.world_position,
                cam.view_proj(aspect),
                &canvases,
            );
        }
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
            let dest = [w.max(1) as f32, h.max(1) as f32];
            if self.project.retro_integer_scale {
                retro.blit_integer(gpu, &cv, dest);
            } else {
                retro.blit_to(gpu, &cv);
            }
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
                                &|id| raster.texture_size(id),
                    &mut |p, owner| {
                        let shader = uic.get(p).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id)?;
                        Some((shader, uib.get(&owner)?.binding))
                    },
                    &mut ui_instances,
                    &mut ui_batches,
                );
            }
            // Capture the composited scene (now in `cv`, before the UI draws on
            // top) into the backdrop, so `backdrop()` UI shaders can frost it.
            {
                let mut enc = gpu
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ui-backdrop") });
                uir.capture_backdrop(gpu, &mut enc, &cv, w.max(1), h.max(1));
                gpu.queue.submit(Some(enc.finish()));
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

    /// True when the game is drawn into the DOCKED Game tab's own rect (via an
    /// offscreen target) rather than over the whole window.
    ///
    /// This is "where are the pixels", which is a different question from
    /// [`Self::game_view`]'s "who owns the keyboard" — and the one every
    /// pointer→viewport conversion wants. Confusing the two is why clicking in
    /// a docked Game tab used to hit-test against whole-window coordinates: the
    /// tab was focused, so `game_view()` said true, so the pointer was never
    /// moved into the tab's rect. Single source of truth for the render path
    /// and the input path alike; if they disagree, clicks land somewhere else.
    pub(crate) fn game_offscreen(&self) -> bool {
        !self.player_mode
            && self.fullscreen_tab.is_none()
            && self.dock_state.as_ref().is_some_and(game_tab_active)
    }

    /// True when the game owns the WHOLE window: the fullscreen Game tab, or
    /// the player. The complement of [`Self::game_offscreen`] for the two cases
    /// where the game is on screen at all.
    pub(crate) fn game_fullscreen(&self) -> bool {
        self.player_mode || self.fullscreen_tab == Some(EditorTab::Game)
    }

    /// True when the Scene (authoring) view is on the window surface this
    /// frame. The surface's own 3D render is only worth decorating — world
    /// canvases, element outlines — when someone can see it.
    pub(crate) fn scene_visible(&self) -> bool {
        match self.fullscreen_tab {
            Some(t) => t == EditorTab::Scene,
            None => self
                .dock_state
                .as_ref()
                .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Scene)),
        }
    }

    /// Where the game's screen-space UI is drawn this frame, in physical
    /// pixels: (top-left in window space, size). `None` when the game is not on
    /// screen at all — a docked layout with some other tab in front.
    ///
    /// Every pointer conversion goes through this, so "is the cursor over the
    /// game" and "where is the game" are one answer instead of two.
    pub(crate) fn game_surface_px(&self) -> Option<([f32; 2], [f32; 2])> {
        if self.game_fullscreen() {
            let gpu = self.gpu.as_ref()?;
            return Some((
                [0.0, 0.0],
                [gpu.config.width as f32, gpu.config.height.max(1) as f32],
            ));
        }
        self.game_offscreen().then(|| self.game_tab_px()).flatten()
    }

    /// The docked Game tab's drawing surface in PHYSICAL pixels: its top-left
    /// in window space, and its size.
    ///
    /// One `pixels_per_point`, one rounding, one place. The render target, the
    /// script view info, the gizmo projection and the UI hit test all size and
    /// offset from this — if any of them did its own `rect * ppp` they could
    /// disagree, and a disagreement here is a cursor that points at the wrong
    /// thing.
    pub(crate) fn game_tab_px(&self) -> Option<([f32; 2], [f32; 2])> {
        let r = self.game_rect?;
        let ppp = self.egui.as_ref().map(|e| e.ctx.pixels_per_point()).unwrap_or(1.0);
        Some((
            [r.min.x * ppp, r.min.y * ppp],
            [(r.width() * ppp).round().max(1.0), (r.height() * ppp).round().max(1.0)],
        ))
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
        self.world.insert(
            e,
            Matter::Camera {
                fov_y: 60f32.to_radians(),
                active,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: Matter::TARGET_W,
                target_h: Matter::TARGET_H,
                target_hz: 0.0,
                ortho: false,
                ortho_height: Matter::ORTHO_HEIGHT,
            },
        );
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

// ---------------------------------------------------------------------------
// The ◫ UI tab's canvas
// ---------------------------------------------------------------------------

impl Editor {
    /// Lazily (re)create the UI tab's offscreen canvas at `w`×`h` physical px.
    fn ensure_ui_design_vp(&mut self, w: u32, h: u32) {
        let (w, h) = (w.clamp(16, 8192), h.clamp(16, 8192));
        if self.ui_design_vp.is_some() && self.ui_design_vp_dims == (w, h) {
            return;
        }
        let (Some(gpu), Some(egui)) = (self.gpu.as_ref(), self.egui.as_mut()) else { return };
        if let Some(old) = self.ui_design_vp.take() {
            egui.renderer.free_texture(&old.tex_id);
        }
        // Nearest: the canvas is rendered AT its on-screen size (zoom multiplies
        // the render, it doesn't stretch a smaller image), so a linear blit
        // would only soften pixel-art UI for nothing.
        self.ui_design_vp =
            Some(make_offscreen_target(gpu, egui, w, h, "ui-design", wgpu::FilterMode::Nearest));
        self.ui_design_vp_dims = (w, h);
    }

    /// Which layer the UI tab is editing: the chosen one if it still exists,
    /// else the first in the scene.
    pub(crate) fn ui_design_layer(&self) -> Option<(Entity, floptle_ui::UiLayer)> {
        let layers: Vec<(Entity, floptle_ui::UiLayer)> =
            self.world.query::<floptle_ui::UiLayer>().map(|(e, l)| (e, *l)).collect();
        self.ui_design
            .layer
            .and_then(|idx| layers.iter().find(|(e, _)| e.index() == idx).copied())
            .or_else(|| layers.first().copied())
    }

    /// Load / save the UI tab's guides as the open scene changes.
    ///
    /// Guides follow the SCENE, and are keyed inside it by layer name — an
    /// entity index is a runtime accident, and a guide that silently reattached
    /// to a different layer after a reload would be worse than no guide.
    pub(crate) fn sync_ui_design_guides(&mut self) {
        let scene = self.scene_name.clone();
        if self.ui_design_guides_scene.as_deref() != Some(scene.as_str()) {
            if let Some(prev) = self.ui_design_guides_scene.clone() {
                crate::ui_design::save_guides(
                    &self.project_root,
                    &prev,
                    &self.world,
                    &self.ui_design.guides,
                );
            }
            self.ui_design.guides =
                crate::ui_design::load_guides(&self.project_root, &scene, &self.world);
            self.ui_design.guides_dirty = false;
            self.ui_design_guides_scene = Some(scene);
            return;
        }
        if std::mem::take(&mut self.ui_design.guides_dirty) {
            crate::ui_design::save_guides(
                &self.project_root,
                &scene,
                &self.world,
                &self.ui_design.guides,
            );
        }
    }

    /// Render the ◫ UI tab's canvas: the selected layer, solved at the preview
    /// resolution and drawn by the REAL UI pipeline into an offscreen target.
    ///
    /// Everything the tab draws on top (outlines, handles, guides) is chrome
    /// over this image — the image itself is the shipping renderer, so what the
    /// canvas shows is what the game shows. Also stashes the solved rects,
    /// which is what makes picking and snapping possible at all.
    pub(crate) fn update_ui_design_view(&mut self) {
        // Consumed here so a hidden tab stops costing a render.
        let visible = std::mem::take(&mut self.ui_design.tab_visible);
        if !visible {
            self.ui_design.rendered_layer = None;
            self.ui_design.placed.clear();
            return;
        }
        let Some((layer_ent, layer)) = self.ui_design_layer() else {
            self.ui_design.rendered_layer = None;
            self.ui_design.placed.clear();
            return;
        };
        self.ui_design.layer = Some(layer_ent.index());
        let preview_px = self.ui_design.preview_px(&layer);
        // The runtime's own scaler: switching the preview resolution shows what
        // the canvas scaler actually does, rather than a naive rescale.
        let layer_scale = layer.scale_for(preview_px);
        let design_vp = [preview_px[0] / layer_scale, preview_px[1] / layer_scale];
        let zoom = self.ui_design.zoom.clamp(0.05, 8.0);
        // Cap the target so a 4× zoom on an ultrawide can't ask for a texture
        // the device won't allocate; the canvas just stops getting crisper.
        let max_dim = 8192.0f32;
        let fit = (max_dim / (preview_px[0] * zoom)).min(max_dim / (preview_px[1] * zoom)).min(1.0);
        let render_scale = layer_scale * zoom * fit;
        let tw = (design_vp[0] * render_scale).round().max(16.0) as u32;
        let th = (design_vp[1] * render_scale).round().max(16.0) as u32;
        self.ensure_ui_design_vp(tw, th);

        self.ensure_ui_fonts();
        let mut roots = self.ui_layer_tree(layer_ent);
        // State preview: forced on the SELECTION, on the tree copies. A state is
        // a property of one element under one pointer, so "show me hover" means
        // "show me hover on this button" — and doing it on the copies is why it
        // can never reach the saved scene.
        let sel: Vec<u32> = self.selection.iter().map(|e| e.index()).collect();
        let forced = self.ui_design.state;
        let mut input = floptle_ui::StateInput::default();
        if let (Some(state), Some(&id)) = (forced, sel.first()) {
            match state {
                floptle_ui::UiState::Hover => input.hovered = Some(id),
                floptle_ui::UiState::Pressed => input.pressed = Some(id),
                floptle_ui::UiState::Focus => input.focused = Some(id),
                floptle_ui::UiState::Disabled | floptle_ui::UiState::Selected => {
                    fn mark(ns: &mut [floptle_ui::Node], id: u32, disabled: bool) {
                        for n in ns.iter_mut() {
                            if n.id == id {
                                if disabled {
                                    n.spec.disabled = true;
                                } else {
                                    n.spec.selected = true;
                                }
                            }
                            mark(&mut n.children, id, disabled);
                        }
                    }
                    mark(&mut roots, id, state == floptle_ui::UiState::Disabled);
                }
                floptle_ui::UiState::Base => {}
            }
        }
        if !self.ui_styles.styles.is_empty() {
            let (sheet, tokens) = (&self.ui_styles, &self.ui_tokens);
            let dt = self.ui_design_dt;
            floptle_ui::apply_styles(
                &mut roots,
                sheet,
                tokens,
                &input,
                &mut self.ui_design_rt,
                dt,
            );
        }
        let (placed, dl) = {
            let Some(uir) = self.ui_render.as_ref() else { return };
            let measure = |t: &floptle_ui::TextSpec| uir.measure_spec(t);
            let mut placed = floptle_ui::solve(&roots, design_vp, &measure);
            floptle_ui::place_scrollbars(&roots, &mut placed, &self.ui_layer_scrollbars(&roots));
            let masks = self.ui_layer_masks(&roots);
            let dl = floptle_ui::draw_list(&roots, &placed, &masks).for_layer(&layer);
            (placed, dl)
        };
        for q in &dl.quads {
            if !q.texture.is_empty() {
                let _ = self.ensure_texture(&q.texture);
            }
        }

        let Some(target) = self.ui_design_vp.as_ref() else { return };
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_ref()) else { return };
        // Clear to the chosen backdrop. wgpu clear colours are LINEAR and the
        // target is sRGB, so the picked colour is encoded on the way in —
        // without this the canvas background reads several stops too light.
        let lin = |c: f32| {
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        let bg = self.ui_design.backdrop;
        {
            let mut enc = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ui-design-clear") });
            enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-design-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: lin(bg[0]) as f64,
                            g: lin(bg[1]) as f64,
                            b: lin(bg[2]) as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            gpu.queue.submit(Some(enc.finish()));
        }
        let vp = [self.ui_design_vp_dims.0 as f32, self.ui_design_vp_dims.1 as f32];
        let mut instances = Vec::new();
        let mut batches = Vec::new();
        {
            let reg = &self.texture_registry;
            let uic = &self.ui_flsl_cache;
            let uib = &self.ui_flsl_binds;
            let Some(uir) = self.ui_render.as_mut() else { return };
            uir.pack(
                gpu,
                &dl,
                [0.0, 0.0],
                render_scale,
                &mut |p| reg.get(p).copied(),
                &|id| raster.texture_size(id),
                &mut |p, owner| {
                    let shader = uic.get(p).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id)?;
                    Some((shader, uib.get(&owner)?.binding))
                },
                &mut instances,
                &mut batches,
            );
            // A `backdrop()` UI shader has nothing behind it here — the canvas
            // shows the layer, not the 3D scene. Clear rather than leave the
            // last game frame stuck in the sampler.
            uir.clear_backdrop();
            uir.draw(gpu, &target.color_view, vp, &instances, &batches, raster);
        }
        self.ui_design.tex = Some(target.tex_id);
        self.ui_design.design_vp = design_vp;
        self.ui_design.render_scale = render_scale;
        self.ui_design.placed = placed;
        self.ui_design.rendered_layer = Some(layer_ent.index());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Editor;

    /// A dock with the Game tab pulled to the front of the central leaf.
    fn dock_showing_game() -> egui_dock::DockState<EditorTab> {
        let mut dock = crate::dock::default_dock();
        for node in dock.main_surface_mut().iter_mut() {
            if let Some(leaf) = node.get_leaf_mut()
                && let Some(at) = leaf.tabs.iter().position(|t| *t == EditorTab::Game)
            {
                leaf.active = egui_dock::TabIndex(at);
            }
        }
        dock
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
    }

    /// The bug Ty hit: a docked Game tab is FOCUSED, so the old code asked
    /// `game_view()`, got true, and measured the pointer against the whole
    /// window. It has to measure against the tab — offset and all.
    #[test]
    fn a_docked_game_tab_is_measured_from_its_own_corner() {
        let ed = Editor {
            dock_state: Some(dock_showing_game()),
            game_rect: Some(rect(300.0, 120.0, 640.0, 360.0)),
            ..Default::default()
        };
        assert!(ed.game_offscreen(), "a docked, front Game tab draws into its own rect");
        assert!(!ed.game_fullscreen(), "…and does not own the window");
        let (origin, size) = ed.game_surface_px().expect("a visible tab is a surface");
        assert_eq!(origin, [300.0, 120.0]);
        assert_eq!(size, [640.0, 360.0]);
    }

    /// Scene tab in front: the game is nowhere on screen, so nothing is over
    /// it. Returning a surface here would arm clicks on invisible buttons.
    #[test]
    fn a_hidden_game_tab_is_not_a_surface() {
        let ed = Editor {
            dock_state: Some(crate::dock::default_dock()), // Scene is front
            game_rect: Some(rect(300.0, 120.0, 640.0, 360.0)),
            ..Default::default()
        };
        assert!(!ed.game_offscreen());
        assert!(ed.game_surface_px().is_none());
    }

    /// Fullscreening some OTHER tab also takes the game off screen, even
    /// though the Game tab is still the front tab of its leaf.
    #[test]
    fn fullscreening_another_tab_takes_the_game_off_screen() {
        let ed = Editor {
            dock_state: Some(dock_showing_game()),
            fullscreen_tab: Some(EditorTab::Inspector),
            game_rect: Some(rect(300.0, 120.0, 640.0, 360.0)),
            ..Default::default()
        };
        assert!(!ed.game_offscreen());
        assert!(!ed.game_fullscreen());
        assert!(ed.game_surface_px().is_none());
    }

    /// The fullscreen Game tab and the player own the window. Without a GPU
    /// there is no window size to report, but the PREDICATE must still say so
    /// — it is what routes the overlay draw and the pointer.
    #[test]
    fn the_fullscreen_game_tab_and_the_player_own_the_window() {
        let ed = Editor {
            dock_state: Some(dock_showing_game()),
            fullscreen_tab: Some(EditorTab::Game),
            ..Default::default()
        };
        assert!(ed.game_fullscreen());
        assert!(!ed.game_offscreen(), "fullscreen is not the docked offscreen path");

        let player = Editor {
            player_mode: true,
            dock_state: Some(dock_showing_game()),
            ..Default::default()
        };
        assert!(player.game_fullscreen());
        assert!(!player.game_offscreen(), "a built game has no dock to draw into");
    }

    /// `pixels_per_point` is applied once and the size is rounded to whole
    /// pixels, matching the offscreen target exactly — a half-pixel
    /// disagreement between the render size and the hit-test size is a
    /// half-pixel of drift at the far corner.
    #[test]
    fn the_tab_rect_is_scaled_and_rounded_like_the_render_target() {
        let ed = Editor {
            dock_state: Some(dock_showing_game()),
            game_rect: Some(rect(10.5, 20.25, 300.3, 170.7)),
            ..Default::default()
        };
        // No egui context in a test, so ppp falls back to 1.0 — the rounding
        // is what this pins.
        let (_, size) = ed.game_surface_px().unwrap();
        assert_eq!(size, [300.0, 171.0]);
    }
    /// The Scene view is what the window surface draws; the Game tab draws its
    /// own. Getting this backwards costs a wasted solve of every UI layer per
    /// frame, or a Scene view missing its world canvases.
    #[test]
    fn only_the_visible_view_decorates_the_surface() {
        let scene_front = Editor {
            dock_state: Some(crate::dock::default_dock()),
            ..Default::default()
        };
        assert!(scene_front.scene_visible());

        let game_front =
            Editor { dock_state: Some(dock_showing_game()), ..Default::default() };
        assert!(!game_front.scene_visible(), "the surface is hidden behind the dock");

        let elsewhere = Editor {
            dock_state: Some(crate::dock::default_dock()),
            fullscreen_tab: Some(EditorTab::Inspector),
            ..Default::default()
        };
        assert!(!elsewhere.scene_visible());
    }
}
