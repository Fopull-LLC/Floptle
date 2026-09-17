//! The gather: one pass over the World that produces everything the frame's
//! UI and draw phases consume — camera, uniforms, raster instances, 2D draws,
//! particles and the selection mask. `Editor::render` calls it once per frame
//! before it takes its own borrows of the renderer.

use floptle_core::Entity;
use floptle_core::Light;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::math::DVec3;
use floptle_core::math::Mat4;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use floptle_render::Globals;
use floptle_render::InstanceRaw;
use floptle_render::MaterialParams;
use floptle_render::MeshId;
use floptle_render::Projection;
use floptle_render::RaymarchGlobals;
use floptle_render::RenderCamera;
use floptle_render::TexId;
use floptle_render::instance_of;
use crate::assets::{AssetPayload, is_model};
#[cfg(feature = "editor-ui")]
use crate::dock::EditorTab;
#[cfg(feature = "editor-ui")]
use crate::gizmo::{build_gizmo, Tool};
use crate::shading::{blob_default_material, blob_mat_arrays, collect_shadow_proxies, material_params, post_process_uniforms, shadow_uniforms, skybox_uniforms, vol_fog_uniforms};
use crate::viz::{CameraGizmo, EmitterViz, ForceViz, box_lines, camera_frustum_lines, cursor_ground, gravity_volume_lines, light_dir_lines, mesh_collider_wire_local, oriented_box_lines, particle_gizmo_lines, point_light_lines, project, rigidbody_lines, terrain_collider_wire};
use crate::{Editor, anim};
#[cfg(feature = "editor-ui")]
use crate::scene_hit;
use crate::mesh_instances::{apply_node_tint, count_draw_batches, push_mesh_instances, resolve_mesh_particles};
use crate::draw_2d::{primitive_draw, water_draw, lit_2d_ranks, light2d_uniform};
use crate::render_frame::light_cap_warning;
use std::cell::RefCell;
use std::rc::Rc;

/// Everything one gather of the World hands to the frame's UI and draw phases.
pub(crate) struct FrameGather {
    pub(crate) aspect: f32,
    pub(crate) cam: RenderCamera,
    pub(crate) clear: [f32; 4],
    pub(crate) contact: [f32; 4],
    pub(crate) flat2d: Vec<(MeshId, Option<TexId>, floptle_render::Light2dInstance)>,
    pub(crate) flsl_draws: Vec<floptle_render::FlslDraw>,
    pub(crate) fog_color: [f32; 4],
    pub(crate) game_view: bool,
    pub(crate) gizmo_tool: Tool,
    pub(crate) globals: Globals,
    pub(crate) instances: Vec<(MeshId, Option<TexId>, InstanceRaw)>,
    pub(crate) light_node: Light,
    pub(crate) lights_2d: floptle_render::Light2dUniform,
    pub(crate) mask_blob: Option<RaymarchGlobals>,
    pub(crate) mask_mesh: Vec<(MeshId, InstanceRaw)>,
    pub(crate) mask_skins: Vec<floptle_render::SkinDraw>,
    pub(crate) particle_fog: [f32; 4],
    pub(crate) point_shadows: bool,
    pub(crate) post_settings: floptle_render::PostSettings,
    pub(crate) rm: RaymarchGlobals,
    pub(crate) rm_draw: bool,
    pub(crate) skin_draws: Vec<floptle_render::SkinDraw>,
    pub(crate) vfx_batches: Vec<floptle_render::ParticleBatch>,
    pub(crate) vfx_instances: Vec<floptle_render::ParticleInstance>,
    pub(crate) view_proj: Mat4,
}

/// This frame's lighting, read from the scene once and shared by the raster
/// globals, the raymarch globals and the particle pass.
pub(crate) struct FrameLighting {
    pub(crate) light_node: floptle_core::Light,
    pub(crate) sun: [f32; 4],
    pub(crate) li: f32,
    pub(crate) flat_camera: bool,
    pub(crate) lights_split: crate::shading::SplitLights,
    pub(crate) pl_count: [f32; 4],
    pub(crate) pl_pos: [[f32; 4]; 16],
    pub(crate) pl_col: [[f32; 4]; 16],
    pub(crate) pl_shape: [[f32; 4]; 16],
    pub(crate) pl_rot: [[f32; 4]; 16],
    pub(crate) pl_cone: [[f32; 4]; 16],
    pub(crate) sh_params: [f32; 4],
    pub(crate) sh_tint: [f32; 4],
    pub(crate) sh_extra: [f32; 4],
    pub(crate) contact: [f32; 4],
    pub(crate) point_shadows: bool,
    pub(crate) ssr: [f32; 4],
    pub(crate) ssr_prev_vp: [[f32; 4]; 4],
    pub(crate) probe_meta: [f32; 4],
    pub(crate) probe_pos: [[f32; 4]; 4],
    pub(crate) probe_half: [[f32; 4]; 4],
    pub(crate) fog_color: [f32; 4],
    pub(crate) fog_params: [f32; 4],
    pub(crate) fog_extra: [f32; 4],
    pub(crate) particle_fog: [f32; 4],
    pub(crate) atmo_meta: [f32; 4],
    pub(crate) atmo_color: [[f32; 4]; 4],
    pub(crate) atmo_body: [[f32; 4]; 4],
    pub(crate) atmo_params: [[f32; 4]; 4],
    pub(crate) star_meta: [f32; 4],
    pub(crate) star_pos: [[f32; 4]; 4],
    pub(crate) star_color: [[f32; 4]; 4],
    pub(crate) prox_count: [f32; 4],
    pub(crate) prox_a: [[f32; 4]; 32],
    pub(crate) prox_b: [[f32; 4]; 32],
    pub(crate) prox_rot: [[f32; 4]; 32],
    pub(crate) globals: Globals,
}

/// The scene turned into draws: every raster instance, the 2D light pass's
/// list, skinned and custom-shader draws, the blobs the raymarch takes, and
/// the per-frame tables the draw was gathered against.
pub(crate) struct FrameInstances {
    pub(crate) terrain_nearest_mask: u32,
    pub(crate) sort_z: std::collections::HashMap<Entity, DVec3>,
    pub(crate) lights_2d: floptle_render::Light2dUniform,
    pub(crate) instances: Vec<(MeshId, Option<TexId>, InstanceRaw)>,
    pub(crate) flat2d: Vec<(MeshId, Option<TexId>, floptle_render::Light2dInstance)>,
    pub(crate) skin_draws: Vec<floptle_render::SkinDraw>,
    pub(crate) flsl_draws: Vec<floptle_render::FlslDraw>,
    pub(crate) blobs: Vec<(DVec3, f32, MaterialParams)>,
}

/// The frame's view, as every gizmo pass reads it: the camera, its
/// view-projection, the target's aspect and size, and which kinds are on.
#[derive(Clone, Copy)]
struct GizmoView<'a> {
    cam: &'a RenderCamera,
    view_proj: Mat4,
    aspect: f32,
    gw: f32,
    gh: f32,
    filter: crate::GizmoFilter,
}

impl Editor {
    /// Gather the World for this frame. `None` when the renderer has not been
    /// created yet, in which case there is nothing to draw.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn gather_frame(
        &mut self,
        chunk_now: Option<f32>,
        elapsed: f32,
        profile: &Rc<RefCell<floptle_core::profile::FrameProfile>>,
        sky_active: bool,
        sky_uniform_vals: [[f32; 4]; 16],
        terrain_base_mat: MaterialParams,
    ) -> Option<FrameGather> {
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return None;
        };
        // One pose table per frame, not per pass: a frame gathers the scene
        // several times over and each pass reads pose indices an earlier gather
        // handed out. The project's era artefacts are set here for the same
        // reason — before any gather, so every view has the same look.
        raster.begin_skin_frame();
        raster.set_retro_defaults(self.project.retro_artefacts());
        // ---- gather the scene from the World ----
        let surface_aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        // The camera projects at the aspect of the target the scene composites
        // into, which is the surface's unless a retro width is pinned — see
        // `ProjectConfigDoc::render_aspect`.
        let aspect = self.project.render_aspect(surface_aspect);
        // The Game dock tab being front = render from the active camera node; otherwise
        // (Scene tab) use the editor's free-fly camera. Works whether or not we're
        // playing, so you can frame the active camera's shot without entering play.
        // (Inlined — self methods can't be called while gpu/egui are borrowed.) A
        // fullscreened tab overrides which view is front. A docked (non-fullscreen)
        // Game tab renders through its own offscreen target sized to the tab rect
        // (update_game_viewport + the tab's Image blit), so the surface renders the
        // editor view underneath — this keeps the game framed to its panel instead of
        // spilling the full-window render behind the other tabs. (Cost: a docked Game
        // tab draws the scene once for the offscreen game view and once for the hidden
        // editor surface; double-click the Game tab to fullscreen it for a single
        // full-window render.) Only a fullscreen Game tab renders the active camera
        // straight to the surface (it fills the whole window, so that framing is right).
        let game_view = matches!(self.fullscreen_tab, Some(EditorTab::Game));
        // The active camera's layer cull mask applies to the fullscreen game
        // view only — the editor Scene view always shows everything.
        let mut game_cull_mask = u32::MAX;
        let cam = {
            let active = if game_view { floptle_core::active_camera(&self.world) } else { None };
            match active {
                Some(e) => {
                    let (fov_y, ortho, oh) = match self.world.get::<Matter>(e) {
                        Some(Matter::Camera { fov_y, cull_mask, ortho, ortho_height, .. }) => {
                            game_cull_mask = *cull_mask;
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
        let view_proj = cam.view_proj(aspect);
        // Feed the map's world→screen picker (`camera.worldToScreen`) when the
        // Fullscreen game view owns the whole surface — its rect matches the
        // full-window cursor space `input.mouse()` reports. A docked game tab
        // feeds its own sub-rect from update_game_viewport instead.
        if game_view {
            self.game_view_origin = [0.0, 0.0]; // fullscreen play: cursor space IS viewport space
            self.script_host.set_view(floptle_script::ViewInfo {
                view_proj: view_proj.to_cols_array(),
                cam_world: [cam.world_position.x, cam.world_position.y, cam.world_position.z],
                vp_x: 0.0,
                vp_y: 0.0,
                vp_w: gpu.config.width as f32,
                vp_h: gpu.config.height as f32,
                fov_y: cam.projection.fov_y(),
                ortho_height: cam.projection.ortho_height().unwrap_or(0.0),
                valid: true,
            });
        }

        // Camera frustum + point-light gizmos so they're visible/placeable (hidden in
        // the game view, where you're seeing the game, not the editor overlays).
        self.camera_gizmos.clear();
        self.light_gizmos.clear();
        self.volume_gizmos.clear();
        self.rig_gizmos.clear();
        self.gi_probe_dots.clear();
        self.body_gizmos.clear();
        self.contact_gizmos.clear();
        self.terrain_wire_gizmo.clear();
        // Emptied here, outside the `show_gizmos` guard, and not where they are
        // filled. Inside it, turning gizmos off left the last frame's navmesh
        // on screen — projected, so frozen in place while the camera moved
        // around it — until something else happened to make the block run
        // again. A drawing that outlives the switch that draws it is worse than
        // no drawing: it is a picture of a level that is no longer there.
        self.nav_gizmo.clear();
        self.nav_surface.clear();
        self.mesh_wire_gizmo.clear();
        self.particle_gizmo.clear();
        // Script debug gizmos (`gizmo.*` from Lua), projected for the surface camera and
        // painted in the Scene view. The game view gets its own set (`game_gizmo_lines`)
        // off its own camera, behind the "Also in Game view" toggle — it's off by
        // default so the game view still shows what the player sees.
        self.script_gizmo_lines.clear();
        if self.show_gizmos && self.gizmo_filter.script && !self.script_gizmos.is_empty() {
            let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
            crate::viz::project_script_gizmos(
                &self.script_gizmos,
                cam.world_position,
                view_proj,
                floptle_core::math::Vec2::ZERO,
                floptle_core::math::Vec2::new(gw, gh),
                &mut self.script_gizmo_lines,
            );
        }
        // Fullscreen Game tab: `cam` above already is the active gameplay camera and the
        // viewport is the whole surface, so the same projection serves. The docked game
        // tab fills this from `update_game_viewport`, which has its own camera + rect.
        if game_view {
            self.game_gizmo_lines.clear();
            if self.game_gizmos && self.gizmo_filter.script && !self.script_gizmos.is_empty() {
                let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
                crate::viz::project_script_gizmos(
                    &self.script_gizmos,
                    cam.world_position,
                    view_proj,
                    floptle_core::math::Vec2::ZERO,
                    floptle_core::math::Vec2::new(gw, gh),
                    &mut self.game_gizmo_lines,
                );
            }
        }
        // What the packages queued with `handles.*`, projected for the Scene
        // view. Disjoint field borrows on purpose: the GPU state above is still
        // held, and this touches neither.
        {
            let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
            let painted = &mut self.ext_painted;
            painted.clear();
            crate::ext::handles::project(
                &self.ext.handles(),
                cam.world_position,
                view_proj,
                gw,
                gh,
                painted,
            );
        }
        // The GI probes, drawn where they actually are. Not behind `show_gizmos`:
        // this is a switch on the Light Probes node itself, and somebody who
        // ticks "show the probes" has asked for exactly this.
        if !game_view
            && self.gi_show_probes
            && let (Some(baked), Some((e, floptle_core::Matter::LightProbes { leak, .. }))) =
                (self.gi_baked.as_ref(), crate::gi_bake::gi_node(&self.world))
        {
            let center = floptle_core::world_transform(&self.world, e).translation;
            let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
            crate::viz::probe_dots(
                baked,
                center,
                leak,
                cam.world_position,
                view_proj,
                gw,
                gh,
                &mut self.gi_probe_dots,
            );
        }
        if !game_view {
            let gpu_size = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
            self.gather_gizmos(&cam, view_proj, aspect, gpu_size);
        }
        let gpu = self.gpu.as_ref()?;

        // Rebuild the overlay gizmo for the selected object (projects + hit-tests).
        // The Rect tool needs the object's local bounds (None = unsupported matter,
        // e.g. a UI element — those get 2D handles in the Scene tab instead).
        let rect_half = self
            .selection
            .last()
            .copied()
            .and_then(|e| crate::selection::rect_base_half(&self.world, &self.mesh_registry, e));
        // A selected armature bone drives the gizmo off its world transform (bones
        // aren't ECS entities); otherwise the selected entity does. Inlined with
        // disjoint field borrows (not the &self helper) to co-exist with the field
        // borrows live in this render scope.
        let bone_xf = self.bone_selection.and_then(|(mesh, idx)| {
            let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh) else {
                return None;
            };
            let rig = self.mesh_registry.get(asset_path)?.rig.as_ref()?;
            let bone_local = self
                .anim
                .poses
                .get(&mesh)
                .and_then(|p| p.get(idx))
                .or_else(|| rig.rest_world.get(idx))
                .copied()
                .unwrap_or(Mat4::IDENTITY);
            // Place the gizmo at the object's pivot (its joint), matching
            // `bone_gizmo_target` — for a baked object the node origin is at the feet.
            let pivot = rig.skeleton.nodes.get(idx).map(|n| n.pivot).unwrap_or(Vec3::ZERO);
            let world_m = floptle_core::world_transform(&self.world, mesh).world_matrix()
                * (bone_local * Mat4::from_translation(pivot)).as_dmat4();
            Some(floptle_core::transform::Transform::from_matrix(world_m))
        });
        // Map tool: the gizmo sits on the sub-object selection's centroid (a
        // Move-style gizmo; no selection = no gizmo). Reuses the bone-override
        // slot — both are "gizmo on a non-entity target". Cached by the frame
        // driver (this scope holds a mutable gpu borrow, so no &self calls).
        let map_xf = self.map_gizmo;
        // The map tool's gizmo is whatever its own transform mode says (move /
        // rotate / scale) — see `Editor::gizmo_tool`.
        // (inlined `gizmo_tool()`: this scope holds a mutable `gpu` borrow, so
        // whole-`self` method calls are out — disjoint field reads are fine)
        let gizmo_tool =
            if self.tool == Tool::MapEdit { self.map_xform.tool() } else { self.tool };
        // No sub-object selection = no map gizmo, whatever the transform mode
        // (the node's own gizmo would be a lie in map mode).
        self.gizmo = if self.tool == Tool::MapEdit && map_xf.is_none() {
            None
        } else {
            build_gizmo(
            gizmo_tool,
            self.selection.last().copied(),
            &self.world,
            self.cursor,
            cam.world_position,
            view_proj,
            gpu.config.width as f32,
            gpu.config.height.max(1) as f32,
            rect_half,
            map_xf.or(bone_xf),
            )
        };

        // Lighting comes from the scene's mandatory Lighting node (a Light
        // component). `spawn_into` makes exactly one, and `spawn_additive`
        // brings no second — so `next()` is *the* Lighting node
        // rather than the first of several, and `find("Lighting")` from a script
        // reaches the same one this reads.
        //
        // If something made a second anyway, say so once: a script writing "the"
        // 2D base light and this reading "the" 2D base light would then be
        // whichever the ECS happened to yield first.
        let lighting = self.gather_lighting(&cam, view_proj);
        let FrameLighting {
            light_node,
            sun,
            li,
            flat_camera,
            lights_split,
            pl_count,
            pl_pos,
            pl_col,
            pl_shape,
            pl_rot,
            pl_cone,
            sh_params,
            sh_tint,
            sh_extra,
            contact,
            point_shadows,
            ssr,
            ssr_prev_vp,
            probe_meta,
            probe_pos,
            probe_half,
            fog_color,
            fog_params,
            fog_extra,
            particle_fog,
            atmo_meta,
            atmo_color,
            atmo_body,
            atmo_params,
            star_meta,
            star_pos,
            star_color,
            prox_count,
            prox_a,
            prox_b,
            prox_rot,
            globals,
        } = lighting;

        let FrameInstances {
            terrain_nearest_mask,
            sort_z,
            lights_2d,
            mut instances,
            flat2d,
            skin_draws,
            flsl_draws,
            blobs,
        } = self.gather_instances(&cam, view_proj, game_cull_mask, &lights_split, flat_camera, profile, chunk_now, terrain_base_mat)?;

        // Live particle effects (play mode): pack every instance's billboards for
        // this frame. Owned data — drawn after the grid, before post, so particles
        // depth-test against the scene and inherit retro/post like everything else.
        // The tab's preview draws only while the Particles tab is actually up
        // (front of its dock leaf) and we're not in Play.
        let vfx_preview_on = !self.playing
            && self
                .dock_state
                .as_ref()
                .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Particles));
        let mut vfx_instances: Vec<floptle_render::ParticleInstance> = Vec::new();
        let mut vfx_batches: Vec<floptle_render::ParticleBatch> = Vec::new();
        self.vfx.collect(
            &self.world,
            &cam,
            &self.texture_registry,
            vfx_preview_on,
            &mut vfx_instances,
            &mut vfx_batches,
        );
        // Mesh-render particle tracks ride the raster instance list (lit + shadowed
        // like scene meshes), so append them to `instances` built above.
        let vfx_mesh_draws = self.vfx.collect_mesh_draws(&self.world, &cam, vfx_preview_on);
        resolve_mesh_particles(&self.mesh_registry, &vfx_mesh_draws, &mut instances);

        // Skybox: a Skybox node drives the environment background — a solid color, or an
        // equirect texture × tint, rotated by the node so a script can spin the sky.
        let (sky_params, sky_tint, sky_rot, sky_solid) = skybox_uniforms(&self.world);
        let clear = [sky_solid[0], sky_solid[1], sky_solid[2], 1.0];
        // The terrain's surface Material (active terrain's, or any terrain that has one)
        // so terrain shades like the rest of the scene. Neutral default = plain matte.
        // (Inlined via disjoint field access — a `&self` method can't be called here
        // while gpu/raster/etc. are mutably borrowed for the render.)
        let terrain_mat = {
            let pick = self
                .active_terrain
                .filter(|e| self.world.get::<Material>(*e).is_some())
                .or_else(|| {
                    self.terrains
                        .keys()
                        .copied()
                        .find(|&e| self.world.get::<Material>(e).is_some())
                });
            pick.and_then(|e| self.world.get::<Material>(e))
                .map(material_params)
                .unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]))
        };
        // The scene's PostProcess node drives the whole post chain (per scene, not
        // per project): PostStack settings + the raymarch SDF-ao params.
        let (mut post_settings, rm_ao_params) = post_process_uniforms(&self.world);
        // The player's colour-vision filter rides on top of the scene's chain,
        // and survives a scene whose PostProcess node is disabled:
        // a scene must not be able to veto an accessibility
        // setting the player turned on.
        post_settings.color_filter = self.access.color_filter.lane();
        post_settings.color_filter_strength = self.access.color_filter_strength;
        post_settings.simulate_deficiency = self.access.simulate_deficiency;
        // Film grain needs a clock or it is a dirty lens, not film. Reduced
        // motion is not applied here: grain is texture, not
        // movement, and freezing it makes it more of a fixed pattern to look at.
        post_settings.time = self.fog_time;
        // Sky shader: when active, `sky_meta.x = 1` makes the raymarch's `sky_color` call the
        // spliced `flsl_sky`, and its uniforms (Inspector knobs over `.flsl` defaults) drive
        // `sky_uniforms`. (Captured before the closure — it can't borrow `self`.)
        let (sky_meta, sky_uniforms): ([f32; 4], [[f32; 4]; 16]) = if sky_active {
            ([1.0, 0.0, 0.0, 0.0], sky_uniform_vals)
        } else {
            ([0.0; 4], [[0.0; 4]; 16])
        };
        // Build raymarch globals for a set of blobs (all of them, or just one for the
        // selection mask). Up to 16 blobs are folded together in one march.
        let (vol_fog_a, vol_fog_b, vol_fog_c) =
            vol_fog_uniforms(&light_node, self.fog_time, cam.world_position.y as f32);
        let terrain_scale = crate::terrain_edit::terrain_scale_lanes(&self.terrain_tex_scale);
        let make_rm = |set: &[(DVec3, f32, MaterialParams)]| -> RaymarchGlobals {
            let mut arr = [[0.0f32; 4]; 16];
            let n = set.len().min(16);
            for (i, (center, scale, _)) in set.iter().take(16).enumerate() {
                let c = (*center - cam.world_position).as_vec3();
                arr[i] = [c.x, c.y, c.z, scale.max(0.05)];
            }
            let (blob_tint, blob_emissive, blob_specular, blob_params, blob_rim) = blob_mat_arrays(set);
            let tm = &terrain_mat;
            RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                light_dir: sun,
                light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
                ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
                bg: [clear[0], clear[1], clear[2], 1.0],
                center: [0.0; 4],
                params: [elapsed, n as f32, 0.0, 0.0],
                vol_center: [[0.0; 4]; 16],
                vol_half: [[1.0, 1.0, 1.0, 0.5]; 16],
                vol_atlas: [[0.0; 4]; 16],
                vol_dims: [[1.0, 1.0, 1.0, 0.0]; 16],
                // .w = per-slot nearest mask (bit i = slot i is Pixelated). The palette
                // is one texture_2d_array with one sampler, so the shader can't pick a
                // sampler per slot — it reads this mask and selects the result instead.
                terrain_tint: [tm.color[0], tm.color[1], tm.color[2], terrain_nearest_mask as f32],
                terrain_emissive: [tm.emissive[0], tm.emissive[1], tm.emissive[2], tm.emissive_strength],
                terrain_specular: [tm.specular[0], tm.specular[1], tm.specular[2], tm.specular_strength],
                terrain_params: [tm.shininess, tm.rim_strength, if tm.unlit { 1.0 } else { 0.0 }, tm.ambient],
                terrain_rim: [tm.rim[0], tm.rim[1], tm.rim[2], 0.0],
                blobs: arr,
                point_count: pl_count,
                point_pos: pl_pos,
                point_color: pl_col,
                point_shape: pl_shape,
                point_rot: pl_rot,
                point_cone: pl_cone,
                blob_tint,
                blob_emissive,
                blob_specular,
                blob_params,
                blob_rim,
                sky_params,
                sky_tint,
                sky_rot,
                ao_params: rm_ao_params,
                shadow_params: sh_params,
                shadow_tint: sh_tint,
                shadow_extra: sh_extra,
                prox_count,
                prox_a,
                prox_b,
                prox_rot,
                fog_color,
                fog_params,
                fog_extra,
                terrain_scale,
                vol_fog_a,
                vol_fog_b,
                vol_fog_c,
                contact,
                ssr,
                ssr_prev_vp,
                probe_meta,
                probe_pos,
                probe_half,
                sky_meta,
                sky_uniforms,
                atmo_meta,
                atmo_color,
                atmo_body,
                atmo_params,
                star_meta,
                star_pos,
                star_color,
                // vol_tight_* are renderer-patched at draw time from the uploaded
                // volumes; the default is "unbounded" (behaves like the full brick).
                ..Default::default()
            }
        };

        // Selection outline source: every selected object's silhouette into the
        // mask — mesh instances, plus (for blobs/field shapes) a raymarch whose
        // outline hugs only the selected SDF surfaces. All selected entities get
        // an outline, not just the primary.
        let (mask_mesh, mask_skins, mask_blob) = if game_view {
            (Vec::new(), Vec::new(), None)
        } else {
            self.gather_masks(&cam, &sort_z, &make_rm)
        };
        let raymarch = self.raymarch.as_ref()?;

        // The raymarch pass renders the blob matter (gated by the SDF-matter toggle)
        // and/or the combined terrain volume — and it's also what draws a textured
        // skybox (rays that miss every bound sample the sky, zero march steps), so a
        // scene with no terrain/blobs still runs it when the sky has a texture; a
        // solid-color sky is just the raster clear. The globals are built either way
        // — on frames with nothing to raymarch they're still uploaded (not drawn) so
        // the raster pass's field bind group has this frame's shadow/proxy data.
        let show_blobs = self.project.matter && !blobs.is_empty();
        let rm_draw = show_blobs
            || !self.terrains.is_empty()
            || sky_params[0] >= 0.5
            || self.sky_shader.is_some() // a procedural sky shader must run the raymarch (sky pass)
            || !self.flsl_shape_slots.is_empty();
        let rm = {
            let mut g = make_rm(if show_blobs { &blobs } else { &[] });
            Self::fill_terrain_volumes(&self.terrains, &self.terrain_slots, &self.mesh_occluders, &self.occluder_slots, &self.world, &mut g, cam.world_position);
            crate::shaders::apply_field_shapes(&self.world, &self.flsl_shape_slots, &self.sdf_cache, &mut g, cam.world_position, None);
            // Baked GI. The renderer owns the probe texture; these four lanes
            // are only where the volume is, and they have to be stamped per
            // view because the field is camera-relative.
            raymarch.gi().apply(&mut g, cam.world_position.into());
            g
        };

        Some(FrameGather {
            aspect,
            cam,
            clear,
            contact,
            flat2d,
            flsl_draws,
            fog_color,
            game_view,
            gizmo_tool,
            globals,
            instances,
            light_node,
            lights_2d,
            mask_blob,
            mask_mesh,
            mask_skins,
            particle_fog,
            point_shadows,
            post_settings,
            rm,
            rm_draw,
            skin_draws,
            vfx_batches,
            vfx_instances,
            view_proj,
        })
    }

    /// The Scene view's gizmos for this frame — cameras, lights, colliders,
    /// emitters, the navmesh, the selection's handles — written into the
    /// editor's overlay lists for the draw. `gpu_size` is the surface in
    /// pixels, which the screen-space handles are sized against.
    fn gather_gizmos(&mut self, cam: &RenderCamera, view_proj: Mat4, aspect: f32, gpu_size: (f32, f32)) {
        if !self.show_gizmos {
            return;
        }
        let (gw, gh) = gpu_size;
        let v = GizmoView { cam, view_proj, aspect, gw, gh, filter: self.gizmo_filter };
        self.node_gizmos(v);
        self.rig_gizmos(v);
        self.sun_gizmo(v);
        self.body_gizmos(v);
        self.terrain_collider_gizmo(v);
        self.navmesh_gizmo(v);
        self.collider_gizmos(v);
        self.particle_gizmo(v);
    }

    /// Cameras, lights, gravity and other volumes, audio reach, and nav links — every
    /// node whose kind has a gizmo.
    fn node_gizmos(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, aspect, gw, gh, filter, .. } = v;
        // Only cameras and point lights get gizmos — gather the few Copy fields we
        // need (no per-frame Matter clone over the whole world).
        enum Giz {
            Cam(f32, bool, Option<f32>),
            Light(f32, floptle_core::LightShape, f32),
            Gravity(bool, f32), // radial?, radius
            /// A box whose size decides where something applies, and an
            /// optional inner box for the part that fades.
            Volume([f32; 3], Option<f32>),
            /// Full volume out to the first, silent by the second.
            Audio(f32, f32),
            /// A nav link: the far end in the node's own space, and whether
            /// it can be crossed both ways.
            Link([f32; 3], bool),
        }
        let gizmos: Vec<(Entity, Giz)> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Camera { fov_y, active, ortho, ortho_height, .. }
                    if filter.cameras =>
                {
                    Some((e, Giz::Cam(*fov_y, *active, ortho.then_some(*ortho_height))))
                }
                Matter::PointLight { range, shape, spot_angle, .. } if filter.lights => {
                    Some((e, Giz::Light(*range, *shape, *spot_angle)))
                }
                Matter::GravityVolume { mode, radius, .. } if filter.lights => {
                    Some((e, Giz::Gravity(*mode == floptle_core::GravityMode::Radial, *radius)))
                }
                // The three boxes you would otherwise size by typing a
                // number and reloading to see whether it reached.
                Matter::ReflectionProbe { half_extents, fade, .. } if filter.volumes => {
                    Some((e, Giz::Volume(*half_extents, Some(*fade))))
                }
                // A plain box, however it is used — one arm, so the three
                // cannot drift apart on screen.
                Matter::LightProbes { half_extents, .. }
                | Matter::NavMesh { half_extents, .. }
                | Matter::NavArea { half_extents, .. }
                    if filter.volumes =>
                {
                    Some((e, Giz::Volume(*half_extents, None)))
                }
                Matter::NavLink { to, bidirectional, .. } if filter.volumes => {
                    Some((e, Giz::Link(*to, *bidirectional)))
                }
                _ => None,
            })
            .collect();
        // Audio sources carry their reach as two numbers on a component
        // rather than as a `Matter` variant, so they are gathered
        // separately — the query above is over `Matter` and would never see
        // one.
        let gizmos: Vec<(Entity, Giz)> = gizmos
            .into_iter()
            // `Flat` ignores position entirely, so it has no reach to draw
            // — a ring around a music track would be a lie.
            .chain(
                self.world
                    .query::<floptle_audio::AudioSource>()
                    .filter(|(_, a)| {
                        filter.audio
                            && a.params.mode != floptle_audio::SpatialMode::Flat
                            && a.params.max_distance > 0.0
                    })
                    .map(|(e, a)| {
                        (e, Giz::Audio(a.params.min_distance, a.params.max_distance))
                    }),
            )
            .collect();
        for (e, g) in gizmos {
            let wt = floptle_core::world_transform(&self.world, e);
            match g {
                Giz::Cam(fov_y, active, ortho_height) => {
                    let lines = camera_frustum_lines(
                        wt.translation, wt.rotation, fov_y, aspect, cam.world_position, view_proj, gw, gh,
                        ortho_height,
                    );
                    if !lines.is_empty() {
                        self.camera_gizmos.push(CameraGizmo { lines, active });
                    }
                }
                Giz::Light(range, shape, spot_angle) => {
                    let lines = point_light_lines(
                        wt.translation, wt.rotation, wt.scale, range, shape, spot_angle,
                        cam.world_position, view_proj, gw, gh,
                    );
                    if !lines.is_empty() {
                        self.light_gizmos.push(lines);
                    }
                }
                Giz::Gravity(radial, radius) => {
                    let lines = gravity_volume_lines(
                        wt.translation, radial, radius, cam.world_position, view_proj, gw, gh,
                    );
                    if !lines.is_empty() {
                        self.light_gizmos.push(lines);
                    }
                }
                Giz::Volume(half, fade) => {
                    // The node's transform positions and scales the box, so
                    // the drawn outline has to be scaled the same way or it
                    // would describe a volume nothing uses.
                    let half = floptle_core::math::Vec3::from(half) * wt.scale;
                    let lines = box_lines(
                        wt.translation, half, cam.world_position, view_proj, gw, gh,
                    );
                    if !lines.is_empty() {
                        self.volume_gizmos.push(lines);
                    }
                    // The inner box is where the effect is at full strength;
                    // between the two it blends out. Drawn only when it is
                    // actually inside, so a fade wider than the box does not
                    // draw a second outline on top of the first.
                    if let Some(f) = fade
                        && f > 0.0
                    {
                        let inner = half - floptle_core::math::Vec3::splat(f);
                        if inner.min_element() > 0.05 {
                            let lines = box_lines(
                                wt.translation, inner, cam.world_position, view_proj, gw, gh,
                            );
                            if !lines.is_empty() {
                                self.volume_gizmos.push(lines);
                            }
                        }
                    }
                }
                Giz::Link(to, both) => {
                    // The far end is in the node's own space, so it turns
                    // and scales with whatever the link is parented to —
                    // which is what lets a ladder live in a prefab.
                    let far = wt.mul_transform(&floptle_core::Transform::from_translation(
                        DVec3::new(to[0] as f64, to[1] as f64, to[2] as f64),
                    ));
                    let lines = crate::viz::link_lines(
                        wt.translation, far.translation, both, cam.world_position, view_proj,
                        gw, gh,
                    );
                    if !lines.is_empty() {
                        self.volume_gizmos.push(lines);
                    }
                }
                Giz::Audio(min_d, max_d) => {
                    // Two rings: full volume inside the first, silent at the
                    // second. Both, because the gap between them is the
                    // fade, and one ring cannot show a gap.
                    for r in [min_d, max_d] {
                        let lines = crate::viz::radius_rings(
                            wt.translation, r, cam.world_position, view_proj, gw, gh,
                        );
                        if !lines.is_empty() {
                            self.volume_gizmos.push(lines);
                        }
                    }
                }
            }
        }
    }

    /// The rig of a selected mesh: the sticks you click to pose it.
    fn rig_gizmos(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, gw, gh, filter, .. } = v;
        // The rig of a selected mesh — the sticks you click to pose it.
        //
        // Only for a mesh that is selected, or whose bone is: every rig in
        // the scene at once buries the picture in white sticks, and the one
        // being posed would be the hardest of all to find.
        if filter.bones {
            let bone_sel = self.bone_selection;
            let mut rigged: Vec<Entity> = Vec::new();
            for e in self.selection.iter().copied().chain(bone_sel.map(|(m, _)| m)) {
                if !rigged.contains(&e) {
                    rigged.push(e);
                }
            }
            for e in rigged {
                let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(e) else {
                    continue;
                };
                let Some(rig) = self.mesh_registry.get(asset_path).and_then(|m| m.rig.as_ref())
                else {
                    continue;
                };
                let viz = crate::viz::rig_viz(
                    e,
                    rig,
                    self.anim.poses.get(&e).map(|p| p.as_slice()),
                    floptle_core::world_transform(&self.world, e).world_matrix(),
                    bone_sel.filter(|(m, _)| *m == e).map(|(_, i)| i),
                    cam.world_position,
                    view_proj,
                    gw,
                    gh,
                );
                if !viz.joints.is_empty() {
                    self.rig_gizmos.push(viz);
                }
            }
        }
    }

    /// The directional sun's direction, anchored at the star or in front of the camera.
    fn sun_gizmo(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, gw, gh, filter, .. } = v;
        // The directional "sun" Light has no world position, so its direction gizmo
        // only shows when the Lighting node is selected — anchored in front of the
        // editor camera so it's always framed, pointing along the light direction.
        // A positional star instead anchors at the star and points at the camera
        // (any direction is "toward something" for a point source).
        if filter.lights
            && self.selection.iter().any(|&e| self.world.get::<Light>(e).is_some())
        {
            let l = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
            // Stars mode: anchor at the brightest star body (if any).
            let star_anchor = if l.stars {
                let (meta, pos, _) =
                    crate::shading::star_uniforms(&self.world, &l, cam.world_position);
                (meta[0] > 0.0).then(|| {
                    cam.world_position
                        + DVec3::new(pos[0][0] as f64, pos[0][1] as f64, pos[0][2] as f64)
                })
            } else {
                None
            };
            let (anchor, dir) = if let Some(star) = star_anchor {
                let toward = (cam.world_position - star).normalize_or_zero().as_vec3();
                (star, if toward == Vec3::ZERO { Vec3::Y } else { toward })
            } else {
                let fwd = (self.camera.rotation() * Vec3::NEG_Z).as_dvec3();
                (cam.world_position + fwd * 6.0, Vec3::from(l.direction))
            };
            let lines = light_dir_lines(anchor, dir, cam.world_position, view_proj, gw, gh);
            if !lines.is_empty() {
                self.light_gizmos.push(lines);
            }
        }
    }

    /// Rigidbody collider outlines, live during Play, and the contact crosses.
    fn body_gizmos(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, gw, gh, filter, .. } = v;
        // Rigidbody collider outlines, so physics bodies are visible/placeable.
        let bodies: Vec<(Entity, floptle_core::RigidBody)> = if filter.physics {
            self.world.query::<floptle_core::RigidBody>().map(|(e, rb)| (e, *rb)).collect()
        } else {
            Vec::new()
        };
        // During Play the live body, not the authored component: a script
        // that set `node.height` (a controller's stand height, a crouch)
        // changed the capsule and moved its centre to keep the feet
        // planted, and an outline drawn from the component then sat a
        // hand's width above where the body actually met the ground —
        // an instrument that lied about the one thing it was for.
        let live: std::collections::HashMap<Entity, (DVec3, f32)> = self
            .sim
            .as_ref()
            .map(|sim| sim.body_states().map(|b| (b.entity, (b.pos, b.height))).collect())
            .unwrap_or_default();
        for (e, rb) in bodies {
            let wt = floptle_core::world_transform(&self.world, e);
            let (p, height) = match live.get(&e) {
                Some(&(pos, h)) => (pos, h),
                None => (wt.translation, rb.height),
            };
            let lines = if rb.kind == floptle_core::BodyKind::Box {
                let s = wt.scale;
                let half = Vec3::new(
                    rb.half_extents[0] * s.x,
                    rb.half_extents[1] * s.y,
                    rb.half_extents[2] * s.z,
                );
                box_lines(p, half, cam.world_position, view_proj, gw, gh)
            } else {
                rigidbody_lines(
                    p,
                    rb.kind == floptle_core::BodyKind::Capsule,
                    rb.radius,
                    height,
                    cam.world_position,
                    view_proj,
                    gw,
                    gh,
                )
            };
            if !lines.is_empty() {
                self.body_gizmos.push(lines);
            }
        }
        // Collision telegraph: a small cross at each contact resolved this step.
        // (Contacts are sim-frame — origin-relative — so convert to world here.)
        if let Some(sim) = self.sim.as_ref().filter(|_| filter.physics) {
            let cs = 0.15;
            for c in &sim.world.contacts {
                let cp = sim.world.origin
                    + DVec3::new(c.point.x as f64, c.point.y as f64, c.point.z as f64);
                for off in [DVec3::X, DVec3::Y, DVec3::Z] {
                    if let (Some(a), Some(b)) = (
                        project(cp - off * cs, cam.world_position, view_proj, gw, gh),
                        project(cp + off * cs, cam.world_position, view_proj, gw, gh),
                    ) {
                        self.contact_gizmos.push((a, b));
                    }
                }
            }
        }
    }

    /// The terrain collider wireframe: the surface physics collides with.
    fn terrain_collider_gizmo(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, gw, gh, filter, .. } = v;
        // Terrain collider wireframes: the surface physics actually collides
        // with. For a terrain set to collide with the drawn surface (the
        // default) that is every drawn triangle, from the same extraction
        // the collider runs — so this wireframe lies on the picture or the
        // collider does not, and a screenshot settles it. For one set to
        // the field it is the field's own zero crossing, coarsely — never
        // the shadow proxy, which is a surface nothing collides with.
        // Cached per terrain in node-local coords, rebuilt when that
        // terrain's shape changes; posed here through the node's full
        // transform, so a moved, turned or scaled terrain's wireframe
        // follows for free.
        if self.show_terrain_collider && filter.colliders {
            for (&e, t) in &self.terrains {
                let drawn = !matches!(
                    self.world.get::<Matter>(e),
                    Some(Matter::Terrain { collision: floptle_core::TerrainCollision::Field, .. })
                );
                // Rebuilt when the terrain's choice of surface changes too.
                self.terrain_wire_world.retain(|(we, d, _)| *we != e || *d == drawn);
                if !self.terrain_wire_world.iter().any(|(we, ..)| *we == e) {
                    let segs = if drawn {
                        crate::viz::terrain_collision_wire(&t.field)
                    } else {
                        let stride =
                            (t.shadow.dims.into_iter().max().unwrap_or(64) / 48).max(2);
                        terrain_collider_wire(&t.shadow, stride)
                    };
                    self.terrain_wire_world.push((e, drawn, segs));
                }
            }
            self.terrain_wire_world.retain(|(we, ..)| self.terrains.contains_key(we));
            for (e, _, segs) in &self.terrain_wire_world {
                let wt = floptle_core::world_transform(&self.world, *e);
                let (anchor, rot, scale) =
                    (wt.translation, wt.rotation.normalize(), wt.scale.x.max(1e-6));
                let place = |p: Vec3| {
                    let q = rot * (p * scale);
                    anchor + DVec3::new(q.x as f64, q.y as f64, q.z as f64)
                };
                for &(a, b) in segs {
                    let wa = place(a);
                    let wb = place(b);
                    if let (Some(pa), Some(pb)) = (
                        project(wa, cam.world_position, view_proj, gw, gh),
                        project(wb, cam.world_position, view_proj, gw, gh),
                    ) {
                        self.terrain_wire_gizmo.push((pa, pb));
                    }
                }
            }
        }
    }

    /// The baked navmesh as a surface, coloured per region, while its node is selected
    /// or a game walks it.
    fn navmesh_gizmo(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, gw, gh, filter, .. } = v;
        // The baked navmesh. Drawn when its node is selected — the same rule
        // the collider wireframes use, so verifying the thing you are
        // editing costs nothing — or whenever the View toggle is on.
        //
        // What is drawn is a **surface**, not a field of rectangles. The
        // bake cuts the walkable ground into rectangles because that is the
        // shape to search; outlining each of them turned one floor into
        // scattered boxes and could not answer the only question the picture
        // is for — *are these two pieces of ground joined?*
        //
        // `Overlay` (floptle-nav) decides that from the links, so the
        // outline is drawn only where the walkable surface actually ends and
        // the seams of the cut are invisible. `⊞ Cells` puts the old
        // per-rectangle wireframe back when the bake's working is the
        // question.
        // How solid the walkable surface is drawn. Low enough that the
        // level under it stays legible — the overlay is drawn over
        // everything, so an opaque one would hide the geometry it is
        // describing — and high enough to read as a surface rather than a
        // tint. A step's ribbon is stronger because it is the answer to a
        // question somebody is asking.
        const NAV_FILL_ALPHA: f32 = 0.22;
        const NAV_STEP_ALPHA: f32 = 0.40;
        // While a game is running, the mesh it is walking on is the bake
        // with this session's `nav.obstacle` holes cut into it. Drawing the
        // bake instead would show a clear corridor beside a unit that just
        // went round one — a tool lying about the thing it exists to
        // explain. The rev counter is compared rather than the polygons, so
        // a frame with nothing carved costs one integer.
        if self.playing {
            let rev = self.script_host.nav_obstacle_rev();
            if rev != self.nav_carved_rev {
                self.nav_carved_rev = rev;
                self.nav_carved = (rev > 0).then(|| self.script_host.nav_mesh_snapshot()).flatten();
                self.nav_overlay = None;
            }
        } else if self.nav_carved.is_some() {
            // Stop gives the level back, and that includes the picture.
            self.nav_carved = None;
            self.nav_carved_rev = 0;
            self.nav_overlay = None;
        }
        if let Some(mesh) = self.nav_carved.as_ref().or(self.nav_baked.as_ref()) {
            let selected = crate::nav_bake::nav_node(&self.world)
                .is_some_and(|(e, _)| self.selection.contains(&e));
            if (self.show_navmesh || selected) && filter.colliders {
                let anchor = DVec3::from_array(mesh.anchor);
                // A hair above the floor: drawn exactly on it, the overlay
                // fights the ground it describes.
                let lift = mesh.settings.cell_size * 0.5;
                let overlay = self.nav_overlay.get_or_insert_with(|| {
                    std::rc::Rc::new(floptle_nav::Overlay::build(mesh, lift))
                });
                // A distinct hue per island, spun by the golden ratio so
                // neighbouring numbers never land on neighbouring colours.
                //
                // Per island rather than per region, which is what this was.
                // The one question the picture exists to answer is *are
                // these two pieces of ground joined?*, and a region is the
                // bake's own grouping before any link is counted — so a
                // balcony and the floor its drop lands on came out two
                // colours while a character walks between them freely. A
                // level with five hundred ledges in it read as five hundred
                // colours and answered nothing.
                let hue = |island: u32| crate::viz::hue_rgb((island as f32 * 0.618_034).fract());
                let world = |p: [f32; 3]| {
                    anchor + DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64)
                };
                // A big level's overlay is hundreds of thousands of
                // edges, and all of them were being pushed and uploaded
                // every frame however little of the level was on screen —
                // which is why the picture got heavier the closer you
                // looked at it, exactly backwards. A segment with both ends
                // outside the viewport cannot cross it, so it is dropped
                // before it reaches the line buffer. The margin is generous
                // enough that a line grazing the edge still draws.
                const OFFSCREEN_MARGIN: f32 = 64.0;
                let onscreen = |p: floptle_core::math::Vec2| {
                    p.x > -OFFSCREEN_MARGIN
                        && p.y > -OFFSCREEN_MARGIN
                        && p.x < gw + OFFSCREEN_MARGIN
                        && p.y < gh + OFFSCREEN_MARGIN
                };
                let mut line = |a: [f32; 3], b: [f32; 3], col: [f32; 3]| {
                    if let (Some(pa), Some(pb)) = (
                        project(world(a), cam.world_position, view_proj, gw, gh),
                        project(world(b), cam.world_position, view_proj, gw, gh),
                    ) && (onscreen(pa) || onscreen(pb))
                    {
                        self.nav_gizmo.push((pa, pb, col));
                    }
                };

                if self.nav_cells {
                    // Every rectangle, faintly — the bake's working.
                    for e in &overlay.cells {
                        let c = hue(e.island);
                        line(e.a, e.b, [c[0] * 0.45, c[1] * 0.45, c[2] * 0.45]);
                    }
                }
                // The edge of the walkable surface, bright.
                for e in &overlay.boundary {
                    line(e.a, e.b, hue(e.island));
                }
                // Where two heights are genuinely joined — the picture of
                // what `max slope` and `step height` just did.
                for s in &overlay.steps {
                    let c = hue(s.island);
                    for (a, b) in [
                        (s.low[0], s.high[0]),
                        (s.low[1], s.high[1]),
                        (s.low[0], s.low[1]),
                        (s.high[0], s.high[1]),
                    ] {
                        line(a, b, c);
                    }
                }

                // The filled surface, in real world space so it sits on the
                // ground rather than being painted over the window.
                let cam_rel = |p: [f32; 3]| {
                    let w = world(p) - cam.world_position;
                    [w.x as f32, w.y as f32, w.z as f32]
                };
                let fill = |c: [f32; 3]| [c[0], c[1], c[2], NAV_FILL_ALPHA];
                let strip = |c: [f32; 3]| [c[0], c[1], c[2], NAV_STEP_ALPHA];
                // The same cull the lines get, done conservatively: a
                // triangle is dropped only when all three corners fall off
                // the same side of the viewport, which is the one case where
                // no part of it can cross the screen. A corner the camera is
                // behind projects to nothing, and anything with one of those
                // is kept — a wrong answer here would delete floor from the
                // middle of the picture, which is far worse than uploading a
                // triangle nobody sees.
                let offscreen_tri = |a: [f32; 3], b: [f32; 3], c: [f32; 3]| {
                    let ps = [world(a), world(b), world(c)]
                        .map(|w| project(w, cam.world_position, view_proj, gw, gh));
                    let Some(ps) = ps.iter().copied().collect::<Option<Vec<_>>>() else {
                        return false;
                    };
                    ps.iter().all(|p| p.x < -OFFSCREEN_MARGIN)
                        || ps.iter().all(|p| p.x > gw + OFFSCREEN_MARGIN)
                        || ps.iter().all(|p| p.y < -OFFSCREEN_MARGIN)
                        || ps.iter().all(|p| p.y > gh + OFFSCREEN_MARGIN)
                };
                for t in &overlay.tris {
                    if offscreen_tri(t.a, t.b, t.c) {
                        continue;
                    }
                    // Painted ground reads as painted: its own hue, and
                    // brighter, because a volume that did nothing and a
                    // volume that worked have to be tellable apart at a
                    // glance rather than by baking again and squinting.
                    let col = if t.area == floptle_nav::WALKABLE {
                        fill(hue(t.island))
                    } else {
                        let c = crate::viz::hue_rgb(
                            (0.12 + t.area as f32 * 0.17).fract(),
                        );
                        [c[0], c[1], c[2], NAV_FILL_ALPHA * 1.9]
                    };
                    for p in [t.a, t.b, t.c] {
                        self.nav_surface
                            .push(floptle_render::TriVertex { pos: cam_rel(p), color: col });
                    }
                }
                // The links, as the bake resolved them — not as they were
                // placed. An end that missed the floor is drawn in red, and
                // that is the whole point: the node's own gizmo can only
                // show where you put it, which is the thing that was wrong.
                //
                // Drawn as an ARC rather than a straight line, and the shape
                // of the arc is the kind of crossing: a jump bows up over
                // its gap, a drop leaves the ledge flat and falls away. A
                // level with a few hundred of these has to be readable at a
                // glance, and a field of identical straight segments is not
                // — a drop and a ladder looked the same, and a link that
                // went the wrong way looked like one that went the right
                // way.
                for l in &overlay.links {
                    let col = if !l.resolved {
                        [1.0, 0.35, 0.3]
                    } else if !l.enabled {
                        [0.45, 0.45, 0.5]
                    } else {
                        match l.kind {
                            floptle_nav::LinkKind::Drop => [1.0, 0.72, 0.25],
                            floptle_nav::LinkKind::Jump => [0.45, 1.0, 0.55],
                            floptle_nav::LinkKind::Placed => [0.45, 0.95, 1.0],
                        }
                    };
                    // The curve itself comes from `floptle-nav`, so the
                    // Scene view and the render probe that checks it are
                    // drawing the same shape rather than two of them.
                    let steps = floptle_nav::overlay::ARC_STEPS;
                    let mut prev = l.from;
                    for k in 1..=steps {
                        let next = l.point_at(k as f32 / steps as f32);
                        line(prev, next, col);
                        prev = next;
                    }
                    // A tick at each end you can enter from, so a one-way
                    // drop and a two-way ladder are not the same picture.
                    let rise = mesh.settings.step_height.max(0.25);
                    for (end, draw_it) in [(l.to, true), (l.from, l.bidirectional)] {
                        if draw_it {
                            line(end, [end[0], end[1] + rise, end[2]], col);
                        }
                    }
                }
                // A step's ribbon is filled too, and more strongly: it is
                // the answer to a question somebody is actively asking.
                for s in &overlay.steps {
                    if offscreen_tri(s.low[0], s.low[1], s.high[1]) {
                        continue;
                    }
                    let col = strip(hue(s.island));
                    for p in [
                        s.low[0], s.low[1], s.high[1], s.low[0], s.high[1], s.high[0],
                    ] {
                        self.nav_surface
                            .push(floptle_render::TriVertex { pos: cam_rel(p), color: col });
                    }
                }
            }
        }
    }

    /// Mesh and primitive collider wireframes: every Collidable when the toggle is on,
    /// plus the selected one.
    fn collider_gizmos(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, gw, gh, filter, .. } = v;
        // Mesh collider wireframes. Every Mesh node flagged Collidable or (legacy)
        // MeshCollider when the global toggle is on, plus the selected one always (so
        // you can verify it). Both markers build a static triangle-mesh collider, so
        // both must draw the wireframe (union; dedup a node flagged both).
        let mut collider_ents: Vec<Entity> =
            self.world.query::<floptle_core::Collidable>().map(|(e, _)| e).collect();
        for (e, _) in self.world.query::<floptle_core::MeshCollider>() {
            if !collider_ents.contains(&e) {
                collider_ents.push(e);
            }
        }
        let mesh_colliders: Vec<(Entity, String)> = collider_ents
            .into_iter()
            .filter_map(|e| match self.world.get::<Matter>(e) {
                Some(Matter::Mesh { asset_path }) => Some((e, asset_path.clone())),
                _ => None,
            })
            .collect();
        for (e, path) in mesh_colliders {
            if !filter.colliders
                || (!self.show_mesh_colliders && !self.selection.contains(&e))
            {
                continue;
            }
            if !self.mesh_wire_cache.contains_key(&path) {
                let file = crate::project::resolve_asset_path(&self.project_root, &path);
                let edges = floptle_assets::gltf_import::import(&file)
                    .map(|m| mesh_collider_wire_local(&m))
                    .unwrap_or_default();
                self.mesh_wire_cache.insert(path.clone(), edges);
            }
            let edges = &self.mesh_wire_cache[&path];
            let wt = floptle_core::world_transform(&self.world, e);
            let m = Mat4::from_scale_rotation_translation(wt.scale, wt.rotation, wt.translation.as_vec3());
            for &(a, b) in edges {
                let wa = m.transform_point3(a).as_dvec3();
                let wb = m.transform_point3(b).as_dvec3();
                if let (Some(pa), Some(pb)) = (
                    project(wa, cam.world_position, view_proj, gw, gh),
                    project(wb, cam.world_position, view_proj, gw, gh),
                ) {
                    self.mesh_wire_gizmo.push((pa, pb));
                }
            }
        }
        // Static primitive collider wireframes (the "Collidable" switch on a Cube /
        // Sphere / Capsule) — drawn with the same toggle as mesh colliders, plus the
        // selected one always. Each matches the static collider built at Play.
        let shape_colliders: Vec<(Entity, floptle_core::Shape)> = self
            .world
            .query::<floptle_core::Collidable>()
            .filter_map(|(e, _)| match self.world.get::<Matter>(e) {
                Some(Matter::Primitive { shape, .. }) => Some((e, *shape)),
                _ => None,
            })
            .collect();
        for (e, shape) in shape_colliders {
            if !filter.colliders
                || (!self.show_mesh_colliders && !self.selection.contains(&e))
            {
                continue;
            }
            let wt = floptle_core::world_transform(&self.world, e);
            let s = wt.scale;
            let lines = match shape {
                floptle_core::Shape::Cube => {
                    let m = Mat4::from_scale_rotation_translation(s, wt.rotation, wt.translation.as_vec3());
                    oriented_box_lines(m, 0.7, cam.world_position, view_proj, gw, gh)
                }
                floptle_core::Shape::Plane => {
                    // Flat in Z: outline the thin-box collider proxy.
                    let thin = Vec3::new(s.x, s.y, 0.02 * s.z.max(1.0));
                    let m = Mat4::from_scale_rotation_translation(thin, wt.rotation, wt.translation.as_vec3());
                    oriented_box_lines(m, 0.7, cam.world_position, view_proj, gw, gh)
                }
                floptle_core::Shape::Sphere => rigidbody_lines(
                    wt.translation, false, 0.85 * s.max_element(), 0.0,
                    cam.world_position, view_proj, gw, gh,
                ),
                floptle_core::Shape::Capsule => {
                    let r = 0.5 * s.x.max(s.z);
                    rigidbody_lines(
                        wt.translation, true, r, s.y + 2.0 * r,
                        cam.world_position, view_proj, gw, gh,
                    )
                }
            };
            self.mesh_wire_gizmo.extend(lines);
        }
    }

    /// The selected particle track's emitter: birth shape, emit direction, and forces.
    fn particle_gizmo(&mut self, v: GizmoView) {
        let GizmoView { cam, view_proj, gw, gh, filter, .. } = v;
        // Selected particle track: draw its emitter birth shape + emit direction +
        // force arrows, so authoring a VFX has spatial feedback. The node is the
        // Particles-tab preview anchor, or a selected ParticleSystem node; the edited
        // effect is `vfx_ui.doc`. sel_track only (less clutter) else every track.
        let particle_node = self
            .vfx
            .preview
            .as_ref()
            .and_then(|p| p.anchor)
            .or_else(|| {
                self.selection
                    .last()
                    .copied()
                    .filter(|&e| self.world.get::<floptle_core::ParticleSystem>(e).is_some())
            });
        if let (Some(node), Some(doc)) =
            (particle_node.filter(|_| filter.particles), self.vfx_ui.doc.as_ref())
        {
            use floptle_scene::{VfxForceDoc, VfxShapeDoc, VfxSpaceDoc};
            let wt = floptle_core::world_transform(&self.world, node);
            let m_shape = Mat4::from_scale_rotation_translation(
                wt.scale,
                wt.rotation,
                wt.translation.as_vec3(),
            );
            let m_anchor = Mat4::from_translation(wt.translation.as_vec3());
            let tracks: Vec<usize> = match self.vfx_ui.sel_track {
                Some(i) if i < doc.tracks.len() => vec![i],
                _ => (0..doc.tracks.len()).collect(),
            };
            for ti in tracks {
                let t = &doc.tracks[ti];
                let shape = match t.shape {
                    VfxShapeDoc::Point => EmitterViz::Point,
                    VfxShapeDoc::Cone { angle, radius } => EmitterViz::Cone { angle, radius },
                    VfxShapeDoc::Sphere { radius, .. } => EmitterViz::Sphere { radius },
                    VfxShapeDoc::Edge { length } => EmitterViz::Edge { length },
                    VfxShapeDoc::Ring { radius } => EmitterViz::Ring { radius },
                };
                let forces: Vec<ForceViz> = t
                    .forces
                    .iter()
                    .filter_map(|f| match *f {
                        VfxForceDoc::Directional { dir, .. } => {
                            Some(ForceViz::Directional { dir: Vec3::from(dir) })
                        }
                        VfxForceDoc::Point { center, strength } => Some(ForceViz::Point {
                            center: Vec3::from(center),
                            attract: strength >= 0.0,
                        }),
                        VfxForceDoc::Vortex { center, axis, .. } => Some(ForceViz::Vortex {
                            center: Vec3::from(center),
                            axis: Vec3::from(axis),
                        }),
                        VfxForceDoc::Turbulence { .. } => None,
                    })
                    .collect();
                // World-space forces act in world/anchor space (translation only);
                // Local-space forces (and every birth shape) ride the emitter frame.
                let m_force =
                    if t.space == VfxSpaceDoc::World { m_anchor } else { m_shape };
                self.particle_gizmo.extend(particle_gizmo_lines(
                    &shape, &forces, m_shape, m_force, cam.world_position, view_proj, gw, gh,
                ));
            }
        }
    }

    /// The Lighting node's uniforms for this frame: sun, lamps, shadows,
    /// reflections, fog, atmosphere, stars and the shadow proxies.
    fn gather_lighting(&mut self, cam: &RenderCamera, view_proj: Mat4) -> FrameLighting {
        let lighting_nodes = self.world.query::<Light>().count();
        if lighting_nodes > 1 && lighting_nodes != self.lighting_nodes_warned {
            self.lighting_nodes_warned = lighting_nodes;
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "💡 {lighting_nodes} Lighting nodes in this scene — a scene has one \
                     environment, and which of them lights it (and which one a script's \
                     getcomponent(\"Light\") reaches) is whichever the ECS yields first. \
                     Delete the spares."
                ),
                None,
            );
        }
        let light_node = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
        let sun = crate::shading::sun_vec(&self.world, &light_node, cam.world_position);
        let li = light_node.intensity;
        // Whether `Auto` reads as 2D in this scene, asked once rather than per node.
        let flat_camera = floptle_core::active_camera(&self.world).is_some_and(|ce| {
            matches!(self.world.get::<Matter>(ce), Some(Matter::Camera { ortho: true, .. }))
        });
        // One split serves both: the 3D slots the raster globals want, and the
        // count of what the sixteen-slot cap refused. Asked here
        // rather than beside the counts below so the scene's lights are walked
        // once a frame instead of twice.
        let lights_split = crate::shading::split_point_lights(
            &self.world,
            cam.world_position,
            &self.project.sorting_order(),
            flat_camera,
        );
        let lit3 = lights_split.three_d;
        let (pl_count, pl_pos, pl_col, pl_shape, pl_rot, pl_cone) = (
            [lit3.count as f32, 0.0, 0.0, 0.0],
            lit3.pos,
            lit3.color,
            lit3.shape,
            lit3.rot,
            lit3.cone,
        );
        self.light_counts =
            (lights_split.three_d.count + lights_split.two_d.count, lights_split.dropped);
        // `self.warn_lights_dropped(...)` would borrow all of `self`, which
        // conflicts with `gpu` above (a live `self.gpu.as_mut()` borrow through
        // most of this function) even though the two touch disjoint fields —
        // so the frame-guard is inlined here, but the actual decision is the
        // same `light_cap_warning` `render_world_into`'s copy calls.
        if self.lights_dropped_checked_frame != self.frame_no {
            self.lights_dropped_checked_frame = self.frame_no;
            let (warned, msg) = light_cap_warning(lights_split.dropped, self.lights_dropped_warned);
            self.lights_dropped_warned = warned;
            if let Some(msg) = msg {
                self.console.push(floptle_script::LogLevel::Warn, msg, None);
            }
        }
        // Sun shadows (Lighting node knobs) + the collider-proxy occluders that let
        // raster meshes cast — both ride the raymarch globals, which the raster pass
        // reads too through the shared field bind group.
        let (sh_params, sh_tint, sh_extra) = shadow_uniforms(&light_node);
        let contact = crate::shading::contact_uniform(&light_node);
        // Does any lamp in this frame cast? Local shadows march the depth
        // prepass, so if none does there is nothing here to pay for — and if one
        // does, the prepass has to run, which is decided far below. Reading the
        // flag off the packed lanes (rather than the World a second time) keeps
        // the answer tied to the sixteen lights that actually reached the
        // shader: a lamp ranked out of the slots casts nothing, so it must not
        // be able to switch a whole pass on either.
        let point_shadows = lit3.shape[..lit3.count.min(16)]
            .iter()
            .any(|s| (s[3] as u32) & 2 != 0);
        // Screen-space reflections read last frame's picture, so what the shader
        // is told here depends on whether one was ever taken — see `ssr_uniform`.
        // The matrix comes from the history itself, because only it knows which
        // camera the stored frame belongs to.
        let ssr = crate::shading::ssr_uniform(
            &light_node,
            self.scene_history.as_ref().is_some_and(|h| h.is_primed()),
        );
        let ssr_prev_vp = self.scene_history
            .as_ref()
            .and_then(|h| h.prev_view_proj(cam.world_position))
            .unwrap_or(floptle_core::math::Mat4::IDENTITY)
            .to_cols_array_2d();
        // What a reflective surface sees when the screen-space march finds
        // nothing: the room it is standing in, or the sky if it is not in one.
        let (probe_meta, probe_pos, probe_half) = crate::reflect_capture::probe_uniforms(
            &self.world,
            &self.probe_slots,
            self.capturing_probes,
            cam.world_position,
            crate::shading::reflection_clamp(&light_node),
        );
        let ((fog_color, fog_params, fog_extra), particle_fog) =
            crate::shading::fog_uniforms_and_particles_at(&light_node, &self.world, cam.world_position);
        let (atmo_meta, atmo_color, atmo_body, atmo_params) =
            crate::shading::atmo_uniforms(&self.world, cam.world_position);
        let (star_meta, star_pos, star_color) =
            crate::shading::star_uniforms(&self.world, &light_node, cam.world_position);
        // Proxies are what lets a raster mesh cast at all, and a lamp marches the
        // same list — so the sun's switch alone cannot decide whether they are
        // collected. A scene with the sun's shadows off and a torch
        // casting would otherwise hand the shader an empty proxy list, and the
        // torch would shine through every crate in the room.
        let (prox_count, prox_a, prox_b, prox_rot) = collect_shadow_proxies(
            &self.world,
            cam.world_position,
            light_node.shadows || point_shadows,
        );
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: sun,
            light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
            ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
            point_count: pl_count,
            point_pos: pl_pos,
            point_color: pl_col,
            point_shape: pl_shape,
            point_rot: pl_rot,
            point_cone: pl_cone,
            // Meshed terrain reads the triplanar scale + the per-slot nearest /
            // Glow bitmasks here (bitmasks as u32 — bit-exact at 32 slots).
            terrain_mask: [0.0, 0.22, 0.0, 0.0],
            terrain_bits: [
                crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root),
                self.terrain_glow_mask,
                0,
                0,
            ],
        };
        FrameLighting {
            light_node,
            sun,
            li,
            flat_camera,
            lights_split,
            pl_count,
            pl_pos,
            pl_col,
            pl_shape,
            pl_rot,
            pl_cone,
            sh_params,
            sh_tint,
            sh_extra,
            contact,
            point_shadows,
            ssr,
            ssr_prev_vp,
            probe_meta,
            probe_pos,
            probe_half,
            fog_color,
            fog_params,
            fog_extra,
            particle_fog,
            atmo_meta,
            atmo_color,
            atmo_body,
            atmo_params,
            star_meta,
            star_pos,
            star_color,
            prox_count,
            prox_a,
            prox_b,
            prox_rot,
            globals,
        }
    }

    /// Walk the World and turn every drawable node into instances for this
    /// frame's passes. Runs the animation preview and the drag ghost too, since
    /// both change what is drawn.
    #[allow(clippy::too_many_arguments)]
    fn gather_instances(
        &mut self,
        cam: &RenderCamera,
        view_proj: Mat4,
        game_cull_mask: u32,
        lights_split: &crate::shading::SplitLights,
        flat_camera: bool,
        profile: &Rc<RefCell<floptle_core::profile::FrameProfile>>,
        chunk_now: Option<f32>,
        terrain_base_mat: MaterialParams,
    ) -> Option<FrameInstances> {
        let (Some(gpu), Some(raster), Some(egui)) =
            (self.gpu.as_mut(), self.raster.as_mut(), self.egui.as_ref())
        else {
            return None;
        };
        // A model being dragged from Assets shows a live ghost at the cursor's
        // ground point, so you see it follow the cursor and land where you drop.
        // Only while the cursor is actually over the viewport (not over an opaque
        // panel), matching where the drop is accepted.
        let ghost_over_scene = scene_hit(&egui.ctx, self.cursor, self.scene_rect);
        let drag_ghost: Option<(String, DVec3)> = egui::DragAndDrop::payload::<AssetPayload>(&egui.ctx)
            .filter(|p| is_model(&p.path) && ghost_over_scene)
            .map(
                |p| {
                    let pos = cursor_ground(
                        cam.world_position,
                        cam.rotation,
                        view_proj.inverse(),
                        gpu.config.width as f32,
                        gpu.config.height.max(1) as f32,
                        self.cursor,
                    );
                    (p.path.clone(), pos)
                },
            );

        // Bone attachments follow their mesh's bones while authoring too (uses the
        // preview pose if the Animating tab is scrubbing, else the rig's rest pose).
        anim::resolve_attachments(&self.anim, &mut self.world, &self.mesh_registry);

        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        // Resolved up front for the same reason as paint_bases: the draw loop holds a
        // mutable borrow and can't call &self helpers.
        let terrain_nearest_mask =
            crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root);
        // Per-node vertex-paint bases, resolved before the draw loop (which borrows
        // `raster` mutably, so it can't call &self helpers). Empty for unpainted scenes.
        // Every node's sorting-layer Z, resolved before the draw loop borrows
        // `raster` mutably. Empty for a scene that uses no sorting layers, which
        // is every scene until one opts in.
        let sort_z = crate::sprite2d::draw_offsets(&self.world, &self.project, cam.world_position);

        // This frame's 2D lights, built once. The pass below is handed this very
        // value, so what the gather filtered by and what the shader accumulates
        // cannot be two different answers.
        let lights_2d = light2d_uniform(&self.world, &lights_split.two_d, view_proj);
        let reach_2d = lights_2d.reach();
        // Which flat nodes take part in 2D lighting, and at which sorting rank —
        // resolved here for the same reason `sort_z` is: the draw loop below
        // borrows `raster` mutably and cannot call an `&self` helper.
        let lit2d = lit_2d_ranks(&self.world, &self.project, flat_camera, reach_2d);
        let paint_bases: std::collections::HashMap<Entity, Vec<u32>> = self
            .world
            .query::<floptle_core::VertexPaint>()
            .filter_map(|(e, vp)| {
                let b = self.paint_data.get(&vp.id)?;
                Some((e, b.parts.iter().map(|&(base, _)| base).collect()))
            })
            .collect();
        // Render, first half: turning the scene into instances.
        // The submission itself is timed separately below and lands in the same
        // bucket — a game asking "what does rendering cost" wants one number, and
        // the two halves are not separable from Lua anyway.
        let gather_t = floptle_core::profile::Span::new();
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        // The 2D lighting G-buffer's draw list, built in this loop from the very
        // instances the raster pass gets (`Light2dInstance::from_raster`). That
        // is the whole mitigation for deferred's second draw path: there is no
        // second walk of the world to keep in step.
        let mut flat2d: Vec<(MeshId, Option<TexId>, floptle_render::Light2dInstance)> = Vec::new();
        // GPU-skinned parts, gathered alongside the plain ones and
        // drawn through the skinned pipelines in the same passes.
        let mut skin_draws: Vec<floptle_render::SkinDraw> = Vec::new();
        // Custom-shader draws (a Material with a compiled `.flsl`): same
        // instance data, drawn through the shader's own pipeline + group(3).
        let mut flsl_draws: Vec<floptle_render::FlslDraw> = Vec::new();
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        // Reused scratch for CPU vertex skinning (deformed vertices, re-uploaded per part).
        let mut skin_scratch: Vec<floptle_render::Vertex> = Vec::new();
        // Recycle skinned-buffer clones of deleted entities, then borrow the cache
        // for the draw loop (disjoint field from mesh_registry/raster).
        self.skin_variants.prune(&self.world);
        let skin_variants = &mut self.skin_variants;
        if let Some((path, pos)) = &drag_ghost
            && let Some(asset) = self.mesh_registry.get(path) {
                let ghost = Transform { translation: *pos, ..Transform::default() };
                let model = ghost.render_matrix(cam.world_position);
                for (i, &mid) in asset.parts.iter().enumerate() {
                    let local = asset
                        .rig
                        .as_ref()
                        .and_then(|r| r.rest_world.get(*r.part_nodes.get(i)?).copied())
                        .unwrap_or(Mat4::IDENTITY);
                    instances.push((mid, None, instance_of(model * local, [0.7, 0.85, 1.0])));
                }
            }
        // Fullscreen-game cull mask: resolve layer names to bits only when it
        // actually culls (the editor Scene view renders with MAX = no table).
        let game_layer_table =
            (game_cull_mask != u32::MAX).then(|| self.project.build_layers());
        // Frustum cull. Until this existed, terrain chunks were
        // the only thing in the engine that asked whether it was on screen —
        // every mesh, map mesh, tilemap, batch and primitive became an instance
        // every frame, and roughly half of any scene is behind the camera.
        //
        // Built from the same camera-relative `view_proj` the instance matrices
        // are, so a position that is right for a draw is right for the test.
        // The rejection sits at the top of the loop, before the match, so every
        // arm benefits from one test rather than eight.
        let frustum = floptle_render::Frustum::from_view_proj(view_proj);
        // How much was skipped, reported in the window title beside the fps.
        let mut culled_nodes = 0usize;
        // Scatter props submitted this frame.
        let mut scatter_props = 0usize;
        for (e, matter) in &ents {
            // Hidden nodes (Visible(false)) don't draw their geometry (a script or the
            // Inspector can toggle this); they still keep transforms, physics, children.
            if matches!(self.world.get::<floptle_core::Visible>(*e), Some(floptle_core::Visible(false))) {
                continue;
            }
            // Switched off (the Hierarchy/Inspector toggle) — this node or an ancestor.
            // Unlike `Visible`, this also takes the node out of physics and stops its
            // scripts; see `floptle_core::Disabled`.
            if floptle_core::is_disabled(&self.world, *e) {
                continue;
            }
            // The active camera's layer cull mask (fullscreen game view only).
            if let Some(lt) = &game_layer_table
                && (game_cull_mask >> lt.index_for(&self.world, *e)) & 1 == 0
            {
                continue;
            }
            // World transform (composes any parent chain) — a parent carries children.
            let mut t = floptle_core::world_transform(&self.world, *e);
            // A sorting layer is a Z nudge on the drawn transform, so ordering a
            // flat scene never moves anything the physics or a script can see.
            // Resolved before the loop (`raster` is borrowed mutably in here).
            t.translation += sort_z.get(e).copied().unwrap_or_default();
            // Off screen? Skip the whole node — the material lookups, the matrix,
            // every arm below. Answers false for anything whose
            // extent the scene does not know, and for the Blob, which is an SDF
            // primitive that shadows things it is not itself beside.
            // A pixels-per-unit sprite is drawn at its texture's size, not at
            // its `size` field, so culling has to know the texture — otherwise a
            // sixteen-unit sprite is culled on a radius of half a unit and pops
            // out of existence at the edge of the screen.
            let sprite_px = matches!(matter, Matter::Sprite { .. })
                .then(|| {
                    let p = self.world.get::<Material>(*e)?.texture.clone()?;
                    let id = self.texture_registry.get(&p).copied()?;
                    raster.texture_size(id)
                })
                .flatten();
            if crate::node_bounds::node_is_off_screen(
                &self.world, &self.mesh_registry, &self.anim.poses,
                *e, matter, &t, cam.world_position, &frustum, sprite_px,
            ) {
                culled_nodes += 1;
                continue;
            }
            // A node's Material (if any) overrides the look; else fall back to the
            // primitive's color (meshes default to white = untinted texture). A
            // material texture (resolved to a registered handle) re-textures the shape.
            let mat = self.world.get::<Material>(*e).cloned();
            // A texture-painted node also draws its paint overlay: the per-triangle atlas
            // mesh, coplanar over the base, alpha-blended in the transparent pass. The base
            // renders normally below — texture paint never changes how the node looks,
            // it only draws over it.
            if self.world.get::<floptle_core::TexturePaint>(*e).is_some() {
                let model = t.render_matrix(cam.world_position);
                let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                crate::paint_tex::push_painted_node(&self.world, &self.paint_tex, *e, model, &mp, &mut instances);
            }
            // The node's texture and its surface extras index, both resolved by
            // the renderer: a material with normal/roughness/metallic/occlusion
            // maps comes back as one combined `TexId`, so every arm below (and
            // everything downstream of them) keeps handling a single texture.
            let (tex, node_ext) = match mat.as_ref() {
                Some(m) => {
                    let (t, p) =
                        crate::shading::material_draw(raster, gpu, m, &self.texture_registry, None);
                    (t, p.ext_index)
                }
                None => (None, 0),
            };
            // Where this node's instances start. The extras index is stamped onto
            // every one of them after the match rather than threaded through the
            // eight arms and the helpers they call — those build their own
            // `MaterialParams` and would each need the renderer passed down to
            // ask for an index. One stamp at the end cannot miss an arm.
            let ext_from = (instances.len(), flsl_draws.len(), skin_draws.len(), flat2d.len());
            let flsl = self.flsl_binds.get(e).map(|b| b.binding);
            match matter {
                Matter::Primitive { shape, color } => {
                    let model = t.render_matrix(cam.world_position);
                    if let Some((mesh, raw)) = primitive_draw(
                        *shape,
                        *color,
                        mat.as_ref(),
                        model,
                        &self.mesh_ids,
                        paint_bases.get(e).map(|v| v.as_slice()),
                        Some(raster),
                    ) {
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                Matter::WaterVolume { .. } => {
                    if let Some((mesh, raw)) = water_draw(
                        matter,
                        mat.as_ref(),
                        &t,
                        cam.world_position,
                        &self.mesh_ids,
                        Some(raster),
                    ) {
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                // The 2D layer. A tilemap is one uploaded
                // mesh; a sprite batch is N instances off the unit quad, each
                // with its own cell and tint.
                Matter::Tilemap { .. } => {
                    let model = t.render_matrix(cam.world_position);
                    // One draw per sheet the layer actually uses.
                    let mut draws = Vec::new();
                    crate::sprite2d::tilemap_draws(
                        &self.tilemaps,
                        &self.texture_registry,
                        *e,
                        model,
                        mat.as_ref(),
                        tex,
                        &mut draws,
                    );
                    for mut draw in draws {
                        // On the 2D lighting path: the raster pass draws it
                        // Unlit, and the composite corrects that by the light's
                        // difference. The G-buffer instance is
                        // taken from the very same value, so the two cannot
                        // disagree about what is being corrected.
                        if let Some(&(rank, casts)) = lit2d.get(e) {
                            draw.2.force_unlit();
                            flat2d.push((
                                draw.0,
                                draw.1,
                                floptle_render::Light2dInstance::from_raster(&draw.2, rank, casts),
                            ));
                        }
                        match flsl {
                            Some(b) => flsl_draws.push((draw.0, draw.1, b, draw.2)),
                            None => instances.push(draw),
                        }
                    }
                }
                Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => {
                    if let Some(&mesh) = self.mesh_ids.get(floptle_core::Shape::Plane as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let px = tex.and_then(|id| raster.texture_size(id));
                        let texel = px
                            .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                            .unwrap_or([0.0, 0.0]);
                        let mut raw = crate::sprite2d::sprite_one_draw(
                            *ppu, *size, *cell, *flip_x, *flip_y, *pivot,
                            model, mat.as_ref(), px, texel,
                        );
                        // Same as a batch: unlit in the raster pass and
                        // corrected by the 2D lighting pass, so the two never
                        // light it twice.
                        if let Some(&(rank, casts)) = lit2d.get(e) {
                            raw.force_unlit();
                            flat2d.push((
                                mesh,
                                tex,
                                floptle_render::Light2dInstance::from_raster(&raw, rank, casts),
                            ));
                        }
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                Matter::SpriteBatch { size } => {
                    if let Some(&mesh) = self.mesh_ids.get(floptle_core::Shape::Plane as usize) {
                        let model = t.render_matrix(cam.world_position);
                        let texel = tex
                            .and_then(|id| raster.texture_size(id))
                            .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                            .unwrap_or([0.0, 0.0]);
                        let mut raws = Vec::new();
                        crate::sprite2d::sprite_draws(
                            &self.world, *e, *size, model, mat.as_ref(), texel, &mut raws,
                        );
                        for mut raw in raws {
                            // …and the same for a sprite batch: unlit in the
                            // raster pass, corrected by the difference.
                            if let Some(&(rank, casts)) = lit2d.get(e) {
                                raw.force_unlit();
                                flat2d.push((
                                    mesh,
                                    tex,
                                    floptle_render::Light2dInstance::from_raster(&raw, rank, casts),
                                ));
                            }
                            match flsl {
                                Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                                None => instances.push((mesh, tex, raw)),
                            }
                        }
                    }
                }
                Matter::Blob { scale } => {
                    // Blobs render in the raymarch pass — a custom fragment
                    // shader doesn't apply (the SDF stage is their world).
                    let mp = mat.as_ref().map(material_params).unwrap_or_else(blob_default_material);
                    blobs.push((t.translation, scale * t.scale.x, mp));
                }
                Matter::Mesh { asset_path } => {
                    if let Some(asset) = self.mesh_registry.get(asset_path) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params);
                        let obj_mats = self.world.get::<floptle_core::ObjectMaterials>(*e);
                        let pose = self.anim.poses.get(e).map(|v| v.as_slice());
                        let node_paint = paint_bases.get(e).map(|v| v.as_slice());
                        push_mesh_instances(gpu, raster, asset, pose, model, tex, mp.as_ref(), obj_mats, &self.texture_registry, node_paint, *e, skin_variants, &mut skin_scratch, &mut instances, &mut skin_draws, flsl, &mut flsl_draws, &self.obj_flsl_binds);
                    }
                }
                Matter::MapMesh { id } => {
                    // Renders through the same per-part path as imported models
                    // (parts = material slots), so ObjectMaterials overrides
                    // keyed by slot name work unchanged, and — since parts are
                    // one-per-slot in the same order the paint cache builds them
                    // — so does vertex paint. No rig.
                    if let Some(asset) = self.mesh_registry.get(&crate::map_edit::map_key(*id)) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params);
                        let obj_mats = self.world.get::<floptle_core::ObjectMaterials>(*e);
                        let node_paint = paint_bases.get(e).map(|v| v.as_slice());
                        push_mesh_instances(gpu, raster, asset, None, model, tex, mp.as_ref(), obj_mats, &self.texture_registry, node_paint, *e, skin_variants, &mut skin_scratch, &mut instances, &mut skin_draws, flsl, &mut flsl_draws, &self.obj_flsl_binds);
                    }
                }
                // group / terrain / camera / light / gravity / skybox / post render
                // elsewhere; Field Shapes are raymarched (globals filled below).
                Matter::Empty
                | Matter::Terrain { .. }
                | Matter::Camera { .. }
                | Matter::PointLight { .. }
                | Matter::GravityVolume { .. }
                | Matter::FieldShape { .. }
                | Matter::LightProbes { .. }
                | Matter::NavMesh { .. }
                | Matter::NavLink { .. }
                | Matter::NavArea { .. }
                | Matter::ReflectionProbe { .. }
                | Matter::Skybox { .. }
                | Matter::PostProcess { .. } => {}
            }

            // Stamp this node's surface-extras index onto everything it just
            // pushed. `0` is the neutral entry, so a node with no material (or a
            // material that sets none of this) writes the value that is already
            // there and the whole block is a no-op.
            // **The node's tint, over everything it just pushed.** A
            // multiplier, not a replacement: the model keeps its own textures
            // and its parts keep their own colours, and the whole thing goes
            // red. One stamp after the match rather than a branch in each arm,
            // for the reason the extras stamp below gives.
            apply_node_tint(
                self.world.get::<floptle_core::Tint>(*e),
                ext_from,
                &mut instances,
                &mut flsl_draws,
                &mut skin_draws,
                &mut flat2d,
            );
            if node_ext != 0 {
                use floptle_render::{ext_index_of, set_ext_index};
                // Only where nothing is set yet. A model part with its own
                // material override resolved its own extras a moment ago, and
                // the node's must not overwrite them — the override is the more
                // specific answer, exactly as it is for colour and texture.
                let fill = |raw: &mut floptle_render::InstanceRaw| {
                    if ext_index_of(raw) == 0 {
                        set_ext_index(raw, node_ext);
                    }
                };
                for (_, _, raw) in &mut instances[ext_from.0..] {
                    fill(raw);
                }
                for (_, _, _, raw) in &mut flsl_draws[ext_from.1..] {
                    fill(raw);
                }
                for d in &mut skin_draws[ext_from.2..] {
                    fill(&mut d.instance);
                }
                // `flat2d` is absent: the 2D lit pass has its own
                // shader and its own instance type, and none of this reaches it.
            }
        }

        // The terrains' extracted chunk meshes join the raster draw list, so they flow
        // through the depth prepass, field shadows/AO, SSAO and post exactly like every
        // other mesh. The raymarch does not draw them (their volume is `w = 3` — shadow +
        // AO, not drawn): a raymarched terrain facets up close and stripes under grazing
        // shadows.
        crate::terrain_edit::push_terrain_instances(
            &self.terrain_render,
            &self.terrains,
            &self.world,
            raster,
            &terrain_base_mat,
            cam.world_position,
            view_proj,
            self.mesh_ids[floptle_core::Shape::Sphere as usize],
            chunk_now,
            &mut instances,
        );

        // Scatter: thousands of props from a seed, resolved to
        // instances and drawn through the ordinary raster path — so they get the
        // ordinary lighting, fog and shadows, including the underwater fog that
        // makes a shoreline forest go murky at the same rate as its ground.
        // Nothing here is a scene node.
        {
            // Where each anchored source's node has got to this frame.
            // A celestial body orbits at ~99 units/s, so a
            // region pinned to the world slides out from under its own props in
            // about two seconds. Refreshing one transform per source is the
            // whole cost of following it: placement lives in this frame, so
            // nothing downstream is recomputed.
            for (id, name) in self.script_host.anchored_scatter() {
                let node = self
                    .world
                    .query::<floptle_core::Name>()
                    .find(|(_, n)| n.0 == name)
                    .map(|(e, _)| e);
                let frame = node.map_or(floptle_core::scatter::Frame::IDENTITY, |e| {
                    let wt = floptle_core::world_transform(&self.world, e);
                    floptle_core::scatter::Frame {
                        origin: wt.translation,
                        rot: wt.rotation.normalize(),
                    }
                });
                self.script_host.set_scatter_frame(id, frame);
            }
            let before_scatter = instances.len();
            let sources: Vec<floptle_core::scatter::ScatterSource> =
                self.script_host.scatter_sources().clone();
            if !sources.is_empty() {
                let eye = cam.world_position;
                let sim = self.sim.as_ref();
                let mut ground = |from: DVec3, dir: Vec3, max: f32| {
                    let sim = sim?;
                    let o = (from - sim.world.origin).as_vec3();
                    sim.world
                        .raycast(o, dir, max)
                        .map(|h| (h.distance, Vec3::from(h.normal)))
                };
                // Baked before the frame's GPU borrow (see
                // `bake_scatter_prototypes`); this only reads the answer. A
                // prototype may be a prefab of several parts, and resolving
                // that per prop would re-walk it twenty thousand times.
                let protos = &self.scatter_protos;
                let mut mesh_of =
                    |asset: &str| protos.get(asset).filter(|p| !p.is_empty()).cloned();
                // Measured at bake time from the same import bounds the mesh path
                // uses, so a field culls by direction as well as distance.
                let proto_radius = &self.scatter_proto_radius;
                let mut radius_of = |asset: &str| proto_radius.get(asset).copied();
                // A hard cap, logged nowhere and needing none: a source with a
                // silly density costs a frame-rate dip, never a frame that
                // never ends.
                const SCATTER_BUDGET: usize = 20_000;
                let base = MaterialParams::flat([1.0, 1.0, 1.0]);
                let _scatter_t = floptle_core::profile::Span::new();
                crate::scatter_draw::build_instances(
                    &mut self.scatter_cache,
                    &sources,
                    eye,
                    &mut mesh_of,
                    &mut radius_of,
                    &mut ground,
                    &base,
                    &frustum,
                    SCATTER_BUDGET,
                    &mut instances,
                );
                // Scatter. A field can ask for 117,000 props and make a scene
                // unplayable; `props` in the counts below is that number, and
                // this is what it cost.
                scatter_props = instances.len().saturating_sub(before_scatter);
                profile
                    .borrow_mut()
                    .record(floptle_core::profile::Bucket::Scatter, _scatter_t.ms());
            } else if self.scatter_cache.len() > 0 {
                self.scatter_cache.clear();
            }
        }

        // The gather is finished: record what it cost. `instances` is taken here
        // rather than in the loop because terrain and scatter push after it, and
        // the number a game wants is the whole submission.
        self.render_counts = crate::node_bounds::Counts {
            nodes: ents.len(),
            culled: culled_nodes,
            instances: instances.len(),
        };
        // …and the same numbers into the profile a game can read.
        // Terrain chunk and particle counts come from the systems that own them.
        {
            let chunks: usize =
                self.terrain_render.values().map(|r| r.slots.len()).sum();
            let particles = self.vfx.live_particles();
            // How many one-shots and lights are live, and how many of each a
            // ceiling refused. A cap nobody can see is the complaint — `effects` is what
            // it costs, `effectsDropped` is what it cut.
            let (effects, effects_dropped) = self.vfx.detached_counts();
            let (lights, lights_dropped) = self.light_counts;
            // Nothing is playing on a build with no sound card, and zero is
            // the honest number rather than a hidden row.
            #[cfg(feature = "devices")]
            let voices = self.audio.live_voices();
            #[cfg(not(feature = "devices"))]
            let voices = 0;
            let mut prof = profile.borrow_mut();
            // Grouping every instance into draw-call buckets is a HashSet pass
            // over the whole submission — worth its cost only when collection
            // is actually on and something will read the number. `set_counts`
            // already no-ops while off; this keeps the SUM ahead of it off too
            // ("off means off", applied to the one count here
            // pricier than a `.sum()` over an existing small collection).
            let draws = if prof.enabled() {
                count_draw_batches(&instances, &flsl_draws, &skin_draws)
            } else {
                0
            };
            prof.set_counts(floptle_core::profile::Counts {
                nodes: ents.len(),
                culled: culled_nodes,
                instances: instances.len(),
                draws,
                chunks,
                props: scatter_props,
                particles,
                effects,
                effects_dropped,
                lights,
                lights_dropped,
                voices,
                // What the 2D lighting pass will actually rasterize a second
                // time — 0 when no light can reach anything.
                flat2d: flat2d.len(),
            });
            prof.record(floptle_core::profile::Bucket::Render, gather_t.ms());
        }

        // Undo any transient scene-binding animation preview now that the draw list
        // is built — the ECS goes back to authored transforms before UI/undo/save.
        // not while recording: record keeps the previewed values live so the
        // Inspector shows what's under the playhead (edit it → it's keyed) and a
        // scrub can't diff a stale pose into spurious keys. The pre-record scene is
        // restored by stop_record_ui when ● Record turns off.
        if !self.anim_ui.record {
            self.anim.restore_preview(&mut self.world);
        }
        Some(FrameInstances {
            terrain_nearest_mask,
            sort_z,
            lights_2d,
            instances,
            flat2d,
            skin_draws,
            flsl_draws,
            blobs,
        })
    }

    /// The Scene view's selection masks: the selected meshes, skinned parts and
    /// blobs drawn once more into the outline pass. `make_rm` builds the
    /// raymarch globals for a blob set, the same way the frame's own are built.
    fn gather_masks(
        &mut self,
        cam: &RenderCamera,
        sort_z: &std::collections::HashMap<Entity, DVec3>,
        make_rm: &impl Fn(&[(DVec3, f32, MaterialParams)]) -> RaymarchGlobals,
    ) -> (Vec<(MeshId, InstanceRaw)>, Vec<floptle_render::SkinDraw>, Option<RaymarchGlobals>) {
        let Some(raster) = self.raster.as_mut() else {
            return (Vec::new(), Vec::new(), None);
        };
        let mut mask_mesh: Vec<(MeshId, InstanceRaw)> = Vec::new();
        // Selected GPU-skinned parts: the silhouette has to hug the pose, so it
        // goes through the same skinned pipeline the character shades with.
        let mut mask_skins: Vec<floptle_render::SkinDraw> = Vec::new();
        let mut mask_blob: Option<RaymarchGlobals> = None;
        // The Game view plays like a build — no selection outline there.
        let mut sel_blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        let mut sel_shapes: Vec<Entity> = Vec::new();
        for &e in &self.selection {
            let Some(m) = self.world.get::<Matter>(e) else { continue };
            // The same offset the draw uses. Without it the outline of a
            // parallaxed or sorted sprite is drawn where the node is rather
            // than where its picture is — which for a background layer is
            // most of the screen away from the thing it is outlining.
            let mut t = floptle_core::world_transform(&self.world, e);
            t.translation += sort_z.get(&e).copied().unwrap_or_default();
            match m {
                Matter::Primitive { shape, .. } => {
                    if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                        let model = t.render_matrix(cam.world_position);
                        mask_mesh.push((mesh, instance_of(model, [1.0, 1.0, 1.0])));
                    }
                }
                Matter::Tilemap { .. } => {
                    if let Some(tm) = self.tilemaps.get(&e) {
                        let model = t.render_matrix(cam.world_position);
                        // The outline hugs every page, or a layer cut from
                        // two sheets would only outline half of itself.
                        for p in &tm.pages {
                            mask_mesh.push((p.mesh, instance_of(model, [1.0, 1.0, 1.0])));
                        }
                    }
                }
                // A batch's sprites are this frame's, so outlining them
                // would trace whatever happened to be alive when you
                // clicked. The Hierarchy row is the selection you want.
                Matter::SpriteBatch { .. } => {}
                // One sprite is a quad, so it can be outlined — unlike a
                // batch, whose sprites are this frame's and would trace
                // whatever happened to be alive when you clicked.
                Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => {
                    if let Some(&mesh) = self.mesh_ids.get(floptle_core::Shape::Plane as usize)
                    {
                        let model = t.render_matrix(cam.world_position);
                        // **The same arguments the draw gets.** This passed
                        // no material and no texture size, and
                        // `sprite_world_size` falls back to the authored
                        // `size` without them — so the outline of a
                        // pixels-per-unit sprite was a differently-sized quad
                        // laid over the sprite, which reads as a stretched
                        // artefact rather than as a selection.
                        let mat = self.world.get::<Material>(e);
                        let px = mat
                            .and_then(|m| m.texture.as_deref())
                            .and_then(|p| self.texture_registry.get(p).copied())
                            .and_then(|id| raster.texture_size(id));
                        let raw = crate::sprite2d::sprite_one_draw(
                            *ppu, *size, *cell, *flip_x, *flip_y, *pivot,
                            model, mat, px, [0.0, 0.0],
                        );
                        mask_mesh.push((mesh, raw));
                    }
                }
                Matter::Mesh { asset_path } => {
                    if let Some(asset) = self.mesh_registry.get(asset_path) {
                        let model = t.render_matrix(cam.world_position);
                        if let Some(rig) = asset.rig.as_ref() {
                            // Match the posed draw so the outline hugs the pose.
                            let node_world =
                                self.anim.poses.get(&e).unwrap_or(&rig.rest_world);
                            for (i, &mid) in asset.parts.iter().enumerate() {
                                if let Some(Some(skin)) = rig.skins.get(i) {
                                    // A skinned part draws from `model` alone —
                                    // the pose is in the deform, not the matrix.
                                    // Applying node_world here too would transform
                                    // it twice and draw the outline offset from the
                                    // model. Match the draw.
                                    let raw = instance_of(model, [1.0, 1.0, 1.0]);
                                    let base = rig.skin_bases.get(i).copied().unwrap_or(0);
                                    if base != 0 {
                                        let part_node =
                                            rig.part_nodes.get(i).copied().unwrap_or(0);
                                        let palette: Vec<Mat4> = skin
                                            .joint_nodes
                                            .iter()
                                            .zip(&skin.inverse_bind)
                                            .map(|(&jn, ib)| {
                                                node_world
                                                    .get(jn)
                                                    .copied()
                                                    .unwrap_or(Mat4::IDENTITY)
                                                    * *ib
                                            })
                                            .collect();
                                        let fallback = node_world
                                            .get(part_node)
                                            .copied()
                                            .unwrap_or(Mat4::IDENTITY);
                                        let pose =
                                            raster.push_skin_pose(base, fallback, &palette);
                                        mask_skins.push(floptle_render::SkinDraw {
                                            mesh: mid,
                                            tex: None,
                                            instance: raw,
                                            pose,
                                        });
                                    } else {
                                        // CPU fallback: the visible draw baked the
                                        // pose into this entity's variant buffer.
                                        let vmid =
                                            self.skin_variants.get(e, i).unwrap_or(mid);
                                        mask_mesh.push((vmid, raw));
                                    }
                                } else {
                                    let local = rig
                                        .part_nodes
                                        .get(i)
                                        .and_then(|&n| node_world.get(n))
                                        .copied()
                                        .unwrap_or(Mat4::IDENTITY);
                                    mask_mesh.push((
                                        mid,
                                        instance_of(model * local, [1.0, 1.0, 1.0]),
                                    ));
                                }
                            }
                        } else {
                            for &mid in &asset.parts {
                                mask_mesh.push((mid, instance_of(model, [1.0, 1.0, 1.0])));
                            }
                        }
                    }
                }
                Matter::MapMesh { id } => {
                    if let Some(asset) = self.mesh_registry.get(&crate::map_edit::map_key(*id)) {
                        let model = t.render_matrix(cam.world_position);
                        for &mid in &asset.parts {
                            mask_mesh.push((mid, instance_of(model, [1.0, 1.0, 1.0])));
                        }
                    }
                }
                Matter::Blob { scale } => {
                    let mp = self
                        .world
                        .get::<Material>(e)
                        .map(material_params)
                        .unwrap_or_else(blob_default_material);
                    sel_blobs.push((t.translation, scale * t.scale.x, mp));
                }
                Matter::FieldShape { .. } => sel_shapes.push(e),
                Matter::Empty
                | Matter::Terrain { .. }
                | Matter::Camera { .. }
                | Matter::PointLight { .. }
                | Matter::GravityVolume { .. }
                | Matter::WaterVolume { .. }
                | Matter::LightProbes { .. }
                | Matter::NavMesh { .. }
                | Matter::NavLink { .. }
                | Matter::NavArea { .. }
                | Matter::ReflectionProbe { .. }
                | Matter::Skybox { .. }
                | Matter::PostProcess { .. } => {}
            }
        }
        if !sel_blobs.is_empty() || !sel_shapes.is_empty() {
            // One raymarch mask covers every selected blob (16-blob fold) and
            // field shape together.
            let mut g = make_rm(&sel_blobs);
            if !sel_shapes.is_empty() {
                crate::shaders::apply_field_shapes(&self.world, &self.flsl_shape_slots, &self.sdf_cache, &mut g, cam.world_position, Some(&sel_shapes));
            }
            mask_blob = Some(g);
        }
        (mask_mesh, mask_skins, mask_blob)
    }
}
