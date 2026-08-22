//! The editor's per-frame render: step the sim + scripts, gather the World
//! into renderer uniforms, build the egui UI, and draw (raymarch -> raster ->
//! overlays -> post). `render()` is the frame loop's single entry point.

use floptle_core::Entity;
use floptle_core::Light;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::Name;
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
use floptle_render::instance_of_mat;
use floptle_scene::MatterDoc;
use floptle_scene::ShapeDoc;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;
use crate::assets::{AssetPayload, build_assets, collect_texture_paths, is_model};
use crate::dock::{EditorTab, default_dock, focus_scripting_tab};
use crate::gizmo::{build_gizmo, Tool};
use crate::hierarchy::{node_new_menu};
use crate::prefs::{DEFAULT_PLAY_TINT, GridConfig, code_theme_path, engine_theme_path, open_external_editor, save_external_editor, save_grid, save_play_tint, save_prefer_external, save_theme_index};
use crate::shading::{blob_default_material, blob_mat_arrays, collect_shadow_proxies, material_params, post_process_uniforms, shadow_uniforms, skybox_uniforms, vol_fog_uniforms};
use crate::terrain_ui::{NewTerrainCfg, TerrainFill};
use crate::theme::{CODE_THEMES, ENGINE_THEMES};
use crate::viz::{CameraGizmo, EmitterViz, ForceViz, box_lines, camera_frustum_lines, cursor_ground, gravity_volume_lines, light_dir_lines, mesh_collider_wire_local, oriented_box_lines, particle_gizmo_lines, point_light_lines, project, rigidbody_lines, terrain_collider_wire};
use crate::export::EXPORT_TARGETS;
use crate::{Editor, EditorCmd, EditorTabViewer, FOCUS_SECS, MeshAsset, ProjectAction, Snapshot, anim, anim_ui, grab_cursor, scene_hit};

/// The extras an offscreen render needs to match what the window shows.
///
/// Bundled rather than passed as two more positional arguments because they
/// belong together conceptually — both are "this view is a real view of the
/// game, treat it like one" — and because the call already takes nine.
#[derive(Default, Clone, Copy)]
pub(crate) struct OffscreenOpts<'a> {
    /// The TEXTURE behind the depth view, which is what lets this render run the
    /// opaque depth prepass. It cannot be derived from the view: a view cannot
    /// be asked its size and cannot be copied out of.
    ///
    /// `None` means no prepass, and therefore no contact shadows, no
    /// `surfaceGap`, no reflections and no lamp shadows — right for a thumbnail
    /// and wrong for anything a player looks at.
    pub depth_tex: Option<&'a wgpu::Texture>,
    /// Which stored picture screen-space reflections read from and write to.
    /// Each view needs its OWN: the history carries the camera it was taken
    /// from, and two views sharing one would reproject each other's frames.
    pub history: HistorySlot,
}

/// Which scene-colour history an offscreen render uses.
///
/// An enum rather than a borrow because the histories live on `Editor` and this
/// call already holds `&mut self` — naming the slot lets the render reach its
/// own without the caller having to hand out a second mutable borrow of the
/// same struct.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HistorySlot {
    /// No reflections of the scene here. Thumbnails, the Inspector's camera
    /// preview, a GI bake: none of them is a view a player sees, and each would
    /// otherwise want a full-frame mip chain of its own.
    #[default]
    None,
    /// The docked Game panel — the one offscreen view that IS the game.
    GamePanel,
}

/// A node's sorting-layer rank if it takes part in 2D lighting, else `None`.
///
/// A free function over the two fields it needs, not an `&self` method: the
/// render fns hold `self.gpu` mutably for their whole body, so nothing inside
/// them can borrow all of `self`. Asked by BOTH gathers, so the Scene view and
/// the Game view cannot disagree about which surfaces are lit — the failure this
/// renderer has already paid for three times.
/// `reach` is [`floptle_render::Light2dUniform::reach`] — the ranks anything in
/// this frame can actually change. A surface no live light reaches is not on the
/// 2D path *this frame*, however its flag reads: the mask already decided it
/// contributes nothing, and honouring that here rather than in `fs_light` is the
/// difference between filtering a `u64` and instancing, uploading and
/// rasterizing the whole flat scene a second time to throw it away
/// (`floptle/0122`).
fn lit_2d_rank(
    world: &floptle_core::World,
    project: &floptle_scene::ProjectConfigDoc,
    e: Entity,
    flat_camera: bool,
    reach: u64,
) -> Option<u32> {
    if reach == 0 {
        return None;
    }
    let mode =
        world.get::<floptle_core::Lighting2D>(e).map(|l| l.mode).unwrap_or_default();
    let facts = floptle_core::Lit2DFacts { emits: false, flat_matter: true, flat_camera };
    let (is_2d, _) = floptle_core::resolve_2d(mode, facts);
    is_2d
        .then(|| {
            world
                .get::<floptle_core::Sorting>(e)
                .map(|sg| project.sorting_rank(&sg.layer))
                .unwrap_or(0)
        })
        .filter(|&r| r < 64 && reach & (1u64 << r) != 0)
}

/// The instance a `Matter::Primitive` draws, or `None` when its built-in shape
/// is not registered.
///
/// A function for the same reason [`lit_2d_rank`] is one: both gathers ask it,
/// so a cube cannot look one way in the Scene view and another in the Game
/// view. It used to be written out twice, and the two copies had already drifted
/// — the offscreen one never applied VERTEX PAINT, so a painted primitive was
/// painted on screen and plain in every other view.
///
/// `node_paint` is this node's own paint block (`paint_bases`). Every primitive
/// of a shape shares ONE MeshId, so the node's block is the only way two cubes
/// can be painted differently; falling back to the mesh's block (0 for
/// built-ins) is what an unpainted one gets. Brush paint modulates 2× (paint
/// light); a glTF import stays ×1.
fn primitive_draw(
    shape: floptle_core::Shape,
    color: [f32; 3],
    mat: Option<&Material>,
    model: Mat4,
    mesh_ids: &[MeshId],
    node_paint: Option<&[u32]>,
    raster: Option<&floptle_render::Raster>,
) -> Option<(MeshId, InstanceRaw)> {
    let &mesh = mesh_ids.get(shape as usize)?;
    let mut mp = mat.map(material_params).unwrap_or_else(|| MaterialParams::flat(color));
    let brush = node_paint.and_then(|v| v.first().copied()).filter(|&b| b != 0);
    mp.paint_modulate = brush.is_some();
    mp.paint_base =
        brush.unwrap_or_else(|| raster.map_or(0, |r| r.mesh_paint_base(mesh)));
    Some((mesh, instance_of_mat(model, &mp)))
}

/// WATER (`floptle/0038`). The instance a `Matter::WaterVolume` draws: a
/// translucent, specular surface sized to the volume the SOLVER uses, so what
/// you see is what floats you — the sea and the buoyancy can't drift apart,
/// which is exactly what happened while the ocean was a hand-placed sphere the
/// game kept in step by hand.
///
/// A frozen sea drops the translucency and the shine: ice is a surface you stand
/// on, and it should not look like something you could swim through.
///
/// Asked by BOTH gathers. It was inline in the Scene view's gather only, so an
/// ocean was there while you edited and gone the moment you looked through the
/// game's camera — the fourth time this file's two gathers have disagreed about
/// whether something exists, and the reason this is a function.
///
/// `None` for any other matter, and for a shape that is not registered.
///
/// `material` is the node's own `Material` component, if any (`floptle/0144`).
/// **Absent → drawn exactly as before this card**: the hand-tuned defaults
/// below, untouched. Present → those defaults are the FALLBACK and the
/// material's own `specular`/`specular_strength`/`shininess` win outright, the
/// same "the node's Material wins whole" rule the rest of this file uses
/// (`part_look_rule`). `alpha` is the one field that does NOT follow that rule:
/// every unauthored `Material` defaults to `alpha = 1.0`, and a water volume
/// that carries one for some other reason (today, that is almost always
/// `retro: (exempt: true)` and nothing else) must not go opaque just because
/// nobody touched the number. A frozen volume stays opaque ice regardless of
/// what the material says.
///
/// `raster` doubles as "is a Raster available at all" (a thumbnail render may
/// have none) and, when a material is present, is what turns its `retro`
/// flags, `reflectivity` and the rest of the PBR surface extras into an
/// `ext_index` — the same store `material_draw` interns into, so a water
/// volume marked `retro: (exempt: true)` reads as exempt through the exact
/// path everything else does, in both gathers that call this function.
fn water_draw(
    matter: &Matter,
    material: Option<&Material>,
    t: &Transform,
    cam_world: DVec3,
    mesh_ids: &[MeshId],
    raster: Option<&mut floptle_render::Raster>,
) -> Option<(MeshId, InstanceRaw)> {
    use floptle_core::WaterKind;
    let Matter::WaterVolume { kind, radius, half_extents, frozen, tint, .. } = matter else {
        return None;
    };
    // The built-in sphere is r = 0.85 and the cube is half = 0.7; scale the
    // node's own transform so the drawn surface lands exactly on the volume's
    // extent.
    let (shape, fit) = match kind {
        WaterKind::Sea => {
            (floptle_core::Shape::Sphere, floptle_core::math::Vec3::splat(radius / 0.85))
        }
        WaterKind::Pool => {
            (floptle_core::Shape::Cube, floptle_core::math::Vec3::from(*half_extents) / 0.7)
        }
    };
    let &mesh = mesh_ids.get(shape as usize)?;
    let mut wt = *t;
    wt.scale *= fit;
    let model = wt.render_matrix(cam_world);
    let mut mp = MaterialParams::flat(*tint);
    if *frozen {
        mp.alpha = 1.0;
        mp.specular_strength = 0.15;
        mp.shininess = 8.0;
    } else {
        mp.alpha = 0.55;
        // Specular is what makes water read as water at a distance where no
        // wave is more than a pixel.
        mp.specular_strength = 0.9;
        mp.shininess = 96.0;
        mp.specular = [1.0, 1.0, 1.0];
    }
    if let Some(m) = material {
        mp.specular = m.specular;
        mp.specular_strength = m.specular_strength;
        mp.shininess = m.shininess;
        if !*frozen && m.alpha != 1.0 {
            mp.alpha = m.alpha;
        }
    }
    if let Some(raster) = raster {
        mp.paint_base = raster.mesh_paint_base(mesh);
        if let Some(m) = material {
            let ext = floptle_render::SurfaceExtras::from_material(m);
            mp.ext_index = raster.push_surface_extras(ext);
        }
    }
    Some((mesh, instance_of_mat(model, &mp)))
}

/// This frame's 2D lights, in the shape the accumulation shader reads.
///
/// The ambient is the scene's own — a flat surface with no 2D light near it then
/// composites to exactly what the raster pass already drew, so switching 2D
/// lighting on in a scene with no lights placed changes nothing. Compositing to
/// black there would read as the feature having broken the game.
/// Every flat node on the 2D lighting path this frame, and its sorting rank.
///
/// One function, called by BOTH gathers, because 0122 asks for exactly that:
/// *the Scene-view and the Game-view gathers make the same decision, by
/// construction.* It used to be the same nine lines written out twice, which is
/// the shape this file has already paid for four times — see
/// `tests/offscreen_draws_the_same_world.rs`.
///
/// Empty when nothing can be reached, and empty *without walking the world*:
/// that is the "a scene with 2D lighting available but no light placed does zero
/// 2D lighting work" property, and a bullet hell was building a 366-entry map
/// twice a frame to reach it.
fn lit_2d_ranks(
    world: &floptle_core::World,
    project: &floptle_scene::ProjectConfigDoc,
    flat_camera: bool,
    reach: u64,
) -> HashMap<Entity, (u32, bool)> {
    if reach == 0 {
        return HashMap::new();
    }
    world
        .query::<Matter>()
        // A Sprite joins the flat set for the same reason the other two are in
        // it: it IS flat, so a 2D light should reach it. Leaving it out would
        // make the one node type actually called "Sprite" the one a torch does
        // not touch.
        .filter(|(_, m)| {
            matches!(
                m,
                Matter::Tilemap { .. } | Matter::SpriteBatch { .. } | Matter::Sprite { .. }
            )
        })
        .filter_map(|(e, _)| {
            lit_2d_rank(world, project, e, flat_camera, reach).map(|r| (e, (r, casts_2d(world, e))))
        })
        .collect()
}

/// Whether this flat surface blocks 2D light (`floptle/0125`).
///
/// The three-valued answer the Inspector has been showing since the control
/// existed, asked here for real. Under `auto` **a tilemap casts exactly where it
/// is solid** — from the colliders its tileset already declares — so a level's
/// collision *is* its light occlusion and the two can never drift apart. The
/// cover that stops a bullet is the cover that stops the light, from one piece
/// of data.
fn casts_2d(world: &floptle_core::World, e: Entity) -> bool {
    let cast = world.get::<floptle_core::Shadow2D>(e).map(|s| s.0).unwrap_or_default();
    let flat_matter = matches!(world.get::<Matter>(e), Some(Matter::Tilemap { .. }));
    let collidable = world.get::<floptle_core::Collidable>(e).is_some();
    floptle_core::resolve_shadow_2d(cast, flat_matter, collidable).0
}

/// Takes the 2D half of a split that has ALREADY happened rather than asking for
/// one: both gathers need this before the draw loop (to know what a light can
/// reach — `floptle/0122`) and again at the pass, and each was walking the
/// scene's lights a second time to build the same value twice.
fn light2d_uniform(
    world: &floptle_core::World,
    two_d: &crate::shading::LightSlots,
    view_proj: floptle_core::math::Mat4,
) -> floptle_render::Light2dUniform {
    let (n, pos, color, mask, falloff) = (two_d.count, two_d.pos, two_d.color, two_d.mask, two_d.falloff);
    // The scene's **2D base light**, always — not the 3D ambient, and not a
    // special case for "no lights placed".
    //
    // It used to be white when no 2D light existed and the 3D ambient the moment
    // one did, which put a cliff exactly where somebody places their first
    // light: a whole level dropped to 12% brightness and the tilemap read as
    // having vanished. That is how it was reported, and it is the wrong way
    // round — **adding a light must only ever add light.** So the base is its
    // own field, it defaults to white, and turning it down is the deliberate act
    // that makes a dark room for a torch to carve a circle out of.
    let a = world
        .query::<floptle_core::Light>()
        .next()
        .map(|(_, l)| l.ambient_2d)
        .unwrap_or(floptle_core::Light::default().ambient_2d);
    let ambient = [a[0], a[1], a[2], 0.0];
    floptle_render::Light2dUniform {
        count: [n as f32, 0.0, 0.0, 0.0],
        ambient,
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        // `view_proj`, `viewport` and the shadow budget are stamped by
        // `light2d_pass` from what it is actually drawing — they are facts about
        // the frame rather than about the scene's lights, and putting them here
        // would be one more thing for two gathers to forget differently.
        pos,
        color,
        falloff,
        mask,
        ..Default::default()
    }
}

/// The decision behind the 16-light-cap warning (`floptle/0116`, `floptle/0168`):
/// given how many lights just got cut and what the LAST warning was about,
/// what should the latch become and what (if anything) should the Console say.
///
/// A plain function with no `self` on purpose — both gathers need this, one of
/// them can't call a `&mut self` method at its call site (see
/// `Editor::warn_lights_dropped`), and keeping the actual decision in one place
/// is what stops the two copies from drifting.
///
/// Latched on the exact count so it says so again if a scene goes from 24
/// dropped to 30, and resets once the scene drops back under the cap so going
/// over it a second time re-warns rather than staying silent forever.
fn light_cap_warning(dropped: usize, last_warned: usize) -> (usize, Option<String>) {
    if dropped == 0 {
        return (0, None);
    }
    if dropped == last_warned {
        return (last_warned, None);
    }
    let msg = format!(
        "💡 {dropped} point light(s) are past the 16-light cap this frame and are not shading \
         anything — the sixteen contributing most at the camera win, the rest cost placement \
         time for nothing (docs/lua-api.md, node:setPointLight)."
    );
    (dropped, Some(msg))
}

impl Editor {

    /// Re-take the 🎓 Learn tab's project snapshot, at most a few times a second
    /// and only while the tab is on top of its dock leaf.
    ///
    /// `played` latches: `Check::Played` asks "have you run this yet", which
    /// stays true after you press Stop — otherwise the step would tick and then
    /// immediately un-tick itself, which reads as the editor changing its mind.
    fn refresh_learn(&mut self) {
        self.learn.played |= self.playing;
        let front = self
            .dock_state
            .as_ref()
            .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Learn));
        if !front {
            return;
        }
        let now = self.started.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
        if now < self.learn.next_scan {
            return;
        }
        self.learn.next_scan = now + crate::learn::RESCAN_SECS;
        self.learn.snap = crate::learn::scan(&self.world, &self.project_root, self.learn.played);
    }

    /// Say once when the scene's lights have gone past the sixteen-slot cap,
    /// naming how many were cut (`floptle/0116`, `floptle/0168`). Called from
    /// both gathers — the Scene view and `render_world_into` — because either
    /// can be the first (or only) one to run in a given session.
    ///
    /// The main gather can't call this directly (a live `self.gpu.as_mut()`
    /// borrow through most of `render()` conflicts with a `&mut self` method
    /// call, even though the two touch disjoint fields), so the DECISION is
    /// `light_cap_warning` — a plain function, no `self`, callable from
    /// anywhere — and this method is the thin wrapper `render_world_into` uses.
    fn warn_lights_dropped(&mut self, dropped: usize) {
        if self.lights_dropped_checked_frame == self.frame_no {
            return;
        }
        self.lights_dropped_checked_frame = self.frame_no;
        let (warned, msg) = light_cap_warning(dropped, self.lights_dropped_warned);
        self.lights_dropped_warned = warned;
        if let Some(msg) = msg {
            self.console.push(floptle_script::LogLevel::Warn, msg, None);
        }
    }

    pub(crate) fn render(&mut self) {
        // Terrain brush telegraph + throttled stroke (before the destructure, so it
        // can freely borrow `self`).
        self.terrain_frame_update();
        self.vertex_paint_frame_update();
        // ◫ Tiles: keep painting while the button is held. Here rather than in the
        // winit handler because a stroke has to follow the pointer every frame,
        // not only on the events that happen to arrive.
        if self.tool == crate::gizmo::Tool::Tiles && !self.playing {
            self.tile_frame_update(self.cursor);
        }
        // Map meshes: heal duplicated ids and re-upload edited geometry so the
        // gather below always finds a current `@map/<id>` registry entry; then
        // the Map tool's hover/selection overlay.
        self.sync_map_meshes();
        // …and re-attach any paint to the surfaces that survived the edit,
        // before anything draws with a stale block.
        self.sync_map_paint();
        self.map_edit_frame_update();
        self.tile_frame_viz();
        // 2D: rebuild any tilemap whose grid or sheet changed (`floptle/0058`).
        self.sync_tilemaps();
        // The 🎓 Learn tab answers its checks from a snapshot of the scene and
        // the project's files. Taken up here with the other whole-`self` passes,
        // before the GPU destructure below splits `self` apart — and only while
        // the tab is actually visible, because it reads every script in the
        // project and a panel nobody has open is not worth a file walk.
        self.refresh_learn();
        // The project's packages get their frame here — before the GPU state is
        // borrowed for the rest of `render`, because an extension's hooks need
        // the whole editor and the draw path holds pieces of it. What they
        // DRAW is projected further down, where `view_proj` exists.
        self.ext_clock += self.ui_frame_dt as f64;
        self.ext_tick();
        // Built here rather than inside the UI pass, where only disjoint field
        // borrows exist. Constructing it reads the keyring and restores whatever
        // session the Hub already stored, off-thread — so by the time the
        // Packages window draws, it usually already knows who you are.
        if self.account.is_none() {
            self.account = Some(floptle_account::Account::new(floptle_account::DEFAULT_BASE));
        }

        // Inspector asset preview: render the spinning model/material (or load the
        // texture) before the GPU/egui destructure borrows below. `preview_dt` is a
        // cheap peek at the frame delta — only the turntable angle uses it.
        let preview_dt = self.last.map(|l| l.elapsed().as_secs_f32()).unwrap_or(0.0).min(0.1);
        self.update_asset_preview(preview_dt);
        let preview_view = self.preview_view();

        // Live Lua syntax check for the active IDE file (drives red squiggles).
        self.check_active_script_syntax();
        // Crash safety: periodically snapshot a dirty scene to `.floptle/autosave`
        // (deleted on a real save; offered for recovery at the next open).
        self.autosave_tick();
        // Reap a finished cross-target export build (Windows-from-Linux etc.).
        self.poll_export_build();
        // Terrain volumes render PER-VOLUME, each at native resolution: moving a
        // terrain needs NO GPU work — only structural changes re-upload into the
        // shared 3D atlas (where shadow-only mesh occluders also live).
        //
        // Capture the terrain dirty state BEFORE `sync_terrain_gpu` consumes it: the atlas
        // upload feeds shadows/AO from each terrain's shadow proxy, then
        // `sync_terrain_meshes` re-extracts the PRIMARY-ray chunk meshes straight from the
        // authority field (Terrain 2.0 / P3). Structural change = full re-mesh; a sculpt
        // dab re-meshes only the chunks it touched (`terrain_chunks_dirty`).
        let terrain_full_rebuild = self.terrain_gpu_dirty;
        self.sync_terrain_gpu();
        // LOD rings center on what the player actually sees: the active game camera
        // during Play, the editor fly-camera otherwise.
        let lod_cam = if self.playing {
            floptle_core::active_camera(&self.world)
                .map(|e| floptle_core::world_transform(&self.world, e).translation)
                .unwrap_or(self.camera.position)
        } else {
            self.camera.position
        };
        // G1 residency: stream celestial terrain fields in/out by camera distance
        // (BEFORE the mesh sync so a landed field streams meshes this same frame;
        // outside the render borrows because a mid-Play arrival rebuilds the sim).
        // Hand queued `terrain.generatePlanet` fills to the generator BEFORE
        // residency runs: the fill marks its body generation-owned
        // (`planet_gen_pending`), and residency must see that mark the same
        // frame — or it adopts the freshly created body as cold and streams a
        // STALE same-id file into it (the authored scene's old planet loaded
        // under a rolled galaxy's spawn world — Ty fell straight through it).
        self.drain_terrain_generates();
        self.update_terrain_residency(lod_cam);
        self.publish_terrain_busy();
        // Background checkpoints (terrain.flush): a few chunks of encoding per
        // frame + threaded writes — autosaves must never stutter the game.
        self.step_terrain_checkpoint();
        {
            // TERRAIN (`floptle/0077`): residency, field generation and meshing.
            // `0074` came in as "I can see through unloaded terrain" and was a
            // priority bug; a number here would have shown the meshing queue.
            let _t = floptle_core::profile::Span::new();
            self.sync_terrain_meshes(terrain_full_rebuild, lod_cam);
            self.profile_record(floptle_core::profile::Bucket::Terrain, _t.ms());
        }
        self.sync_sky_texture();
        self.sync_sky_shader();
        // Texture-painted nodes keep their vertex paint via atlas-ordered mirror blocks;
        // rebuild them when vertex paint changed this frame (no-op otherwise). AFTER
        // `vertex_paint_frame_update` above, so a dab shows the same frame it lands.
        self.sync_tex_paint_mirrors();
        // Keep the Inspector's script param list in sync with each script's `defaults`
        // (cheap: cached by file mtime, selected node only) so editing a script surfaces
        // new tunables and drops removed ones live.
        self.sync_selected_script_params();
        // Whether the Game viewport is focused (precomputed before the GPU borrow): game
        // input only feeds scripts here. `game_view()` is pointer-aware in split view, so
        // when both tabs show, input goes to whichever viewport the mouse is over and the
        // Scene view stays fully interactive.
        let game_focused = self.game_view() || self.game_trap;

        // Poll the gamepads and refresh the action layer's device levels. Must
        // run before anything resolves, and before the early-out below so a pad
        // plugged in during startup is already slotted.
        self.pump_input_devices();
        self.poll_input_map_reload();
        // The half of the live loop that ISN'T the editor's own push: a texture
        // rewritten by anything (Aseprite, a build script, a git checkout)
        // re-uploads here. See image_io.rs.
        self.poll_texture_hot_reload();

        // Nothing to drive until the window + GPU stack exist. (The borrows
        // themselves are taken per stage, and by the gather/draw core below.)
        if self.gpu.is_none()
            || self.raster.is_none()
            || self.raymarch.is_none()
            || self.retro.is_none()
            || self.outline.is_none()
            || self.grid_render.is_none()
            || self.post.is_none()
            || self.egui.is_none()
            || self.window.is_none()
        {
            return;
        }

        // Which dock tab holds focus (from last frame's dock state — the raw winit
        // key handler runs between frames, so a one-frame-old read is exact). Lets
        // that handler route Delete/arrows/F to a focused timeline panel instead of
        // the scene. Fullscreen forces its own tab.
        self.focused_tab = self.fullscreen_tab.or(self
            .dock_state
            .as_mut()
            .and_then(|d| d.find_active_focused().map(|(_, t)| *t)));
        // Cleared here, set again by the dopesheet if it draws this frame. A
        // panel that is no longer on screen must not keep claiming Ctrl+C — the
        // flag has to expire on its own rather than wait to be corrected.
        self.anim_ui.sheet_hovered = false;

        let (dt, elapsed) = self.advance_clock(game_focused);
        // 🖼 Image tab: frame playback, toasts, external-change reload, and the
        // Live re-export that keeps the mesh in step with the brush.
        let image_visible = std::mem::take(&mut self.image.tab_visible);
        self.image.tick(dt);
        if image_visible {
            self.poll_image_doc_reload();
            self.step_image_live();
        }
        // History frame boundary: capture this frame's pre-edit scene+selection
        // (what `begin_edit` coalesces a gizmo/inspector drag against), and turn
        // any selection change since the last boundary into its own undo step.
        // Skipped while playing — script-driven transforms must not enter the
        // undo history — and while recording (the world carries previewed clip
        // values then; edits go to the CLIP as keys, not to scene undo).
        self.begin_history_frame();

        self.play_step(dt, game_focused);
        self.finish_input_frame();
        // Register every texture + import every mesh the particle system needs
        // BEFORE the gather that resolves them (full &mut self here — no borrow
        // race, no frame lag on the open effect).
        self.frame_no = self.frame_no.wrapping_add(1);
        self.ensure_vfx_assets();
        // Every texture this scene's materials name, before any gather looks one
        // up — see `ensure_scene_textures` for what an unregistered one looks
        // like (it looks like the material was never applied).
        self.ensure_scene_textures();
        // Compile/hot-reload `.flsl` shader materials + refresh their group(3)
        // bindings — the gathers below (main, Game viewport, camera preview)
        // all read `flsl_binds`, so this must run before any of them. Field
        // Shapes follow: their sdf shaders splice into both passes on change.
        self.ensure_flsl_materials();
        self.ensure_ui_shaders();
        self.ensure_post_shaders();
        // Anything the GPU rejected since the last frame. It no longer takes
        // the process down (see `Gpu::new`), so this is the only place it
        // becomes visible — and it has to, or a pass that silently stops
        // drawing looks like the feature never worked.
        for e in floptle_render::take_gpu_errors() {
            self.console.push(floptle_script::LogLevel::Error, format!("GPU: {e}"), None);
        }
        // Baked GI: push any pending probe upload, then advance a bake by one
        // frame's slice. Both run BEFORE the gathers below, so this frame's
        // draws see this frame's light — and a bake, which renders the scene
        // itself, cannot be re-entered from inside one of them.
        self.refresh_gi();
        self.step_gi_bake();
        self.drive_auto_bake();
        // The navmesh's own two: take a finished background bake, then decide
        // whether the level has changed enough to want another. In that order,
        // so a bake that has just landed is the one the watcher compares
        // against rather than the one before it.
        self.poll_nav_bake();
        self.tick_nav_autobake(dt);
        // …and, on the same terms, one reflection probe's capture. Six renders
        // of the scene, so it belongs here beside the bake rather than inside a
        // gather, and at most one probe a frame.
        self.step_reflection_probes();
        // The project's frame pacing, applied before anything acquires a
        // surface image. `set_vsync` early-outs when nothing changed, so this is
        // free on every frame but the one where somebody changes the setting.
        let want_vsync = match self.project.vsync {
            floptle_scene::VsyncDoc::On => floptle_render::Vsync::On,
            floptle_scene::VsyncDoc::Adaptive => floptle_render::Vsync::Adaptive,
            floptle_scene::VsyncDoc::Off => floptle_render::Vsync::Off,
        };
        let applied = self.gpu.as_mut().and_then(|gpu| gpu.set_vsync(want_vsync));
        if let Some(mode) = applied {
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("frame pacing: {want_vsync:?} → {mode:?}"),
                None,
            );
        }
        self.sync_field_shapes();

        // Edit-mode animation preview (Animating tab): pose the bound node at the
        // playhead. This must run BEFORE anything gathers draw data — the UI
        // overlay/hologram gathers and the docked Game viewport below all read the
        // ECS, so applying the preview after them meant scrubbing a property track
        // (e.g. a spritesheet `cell`) showed nothing in the editor. Scene-node
        // bindings apply transiently and are restored after the main draw list is
        // built (except while recording — see `restore_preview` below), so a
        // preview never dirties the authored scene.
        if !self.playing {
            if self.anim_ui.tab_visible {
                if let (Some(target), Some(state)) =
                    (self.anim_ui.target, self.anim_ui.sel_anim.clone())
                {
                    if self.anim_ui.preview_playing {
                        self.anim_ui.playhead += dt;
                    }
                    // Record first: capture the user's pose edits as keys BEFORE
                    // the preview re-applies the clip (which then includes them).
                    if self.anim_ui.record
                        && anim_ui::record_scan(&self.world, &mut self.anim_ui, target) {
                            self.anim_ui.clip_dirty = true;
                        }
                    // A held edit (bone gizmo/inspector DRAG) defers its disk save to
                    // pointer-up, so without this the preview keeps re-sampling the OLD
                    // clip and the bone looks frozen mid-drag. Refresh the in-memory clip
                    // + bump the revision so preview_pose rebinds to the live edit — the
                    // bone tracks the gizmo in real time. Disk save stays coalesced.
                    if self.anim_ui.clip_dirty
                        && let Some((k, d)) = self.anim_ui.clip_doc.clone() {
                            self.anim.register_clip(&k, &d);
                        }
                    anim::preview_pose(
                        &mut self.anim,
                        &mut self.world,
                        &self.mesh_registry,
                        target,
                        &state,
                        self.anim_ui.playhead,
                    );
                    if self.anim_ui.record {
                        // Re-baseline against what the preview applied, so next
                        // frame's diff sees only NEW user edits.
                        anim_ui::refresh_record_baseline(&self.world, &mut self.anim_ui, target);
                    }
                }
            } else {
                // Tab hidden: recording can't continue without its scan/preview
                // loop — stop it cleanly (restores the pre-record scene).
                if self.anim_ui.record {
                    anim_ui::stop_record_ui(&mut self.world, &mut self.anim_ui);
                    self.anim.forget_preview();
                }
                if !self.anim.poses.is_empty() || !self.anim.instances.is_empty() {
                    // Drop stale preview runtimes so models return to rest.
                    self.anim.poses.clear();
                    self.anim.instances.clear();
                }
            }
            self.anim_ui.tab_visible = false; // re-armed by the tab each frame it draws
        }

        // Game-UI layers: gather + solve on the CPU while `self` is free (the
        // draw core borrows the GPU stack); drawn over the finished frame below.
        // AFTER the animation preview, so scrubbing shows live in every view.
        // Is the game drawn over the whole WINDOW this frame? Not "does the Game
        // tab have focus" — a docked tab has focus and draws into its own rect,
        // and asking the focus question here meant the overlay was also packed
        // and drawn full-window every frame, hidden under the editor's chrome.
        // In split view it was worse than wasteful: `game_view()` follows the
        // pointer there, so screen-space canvases blinked out of the Scene view
        // whenever the mouse crossed into the game.
        let ui_view = self.game_fullscreen();
        // Screen-space overlay layers (game view only). gather_game_ui skips
        // world-space layers — those live in the scene below.
        let ui_layers = if ui_view {
            let vp = self
                .gpu
                .as_ref()
                .map(|g| [g.config.width as f32, g.config.height as f32])
                .unwrap_or([0.0, 0.0]);
            self.gather_game_ui(vp)
        } else {
            Vec::new()
        };
        // World canvases: in the Scene (authoring) view, EVERY layer renders as
        // a movable hologram at its node's transform; in game/player view, only
        // the layers whose `space` is World (screen-space ones are the overlay
        // above). Either way outlines project onto the canvas and drags come
        // back through cmd.ui_move (in design units).
        let aspect = self
            .gpu
            .as_ref()
            .map(|g| g.config.width as f32 / g.config.height.max(1) as f32)
            .unwrap_or(16.0 / 9.0);
        // …and only when the surface is actually being looked at: a docked Game
        // tab renders its own world canvases into its own target, so solving
        // every layer again for a surface hidden behind the dock is pure cost.
        let ui_world = if ui_view || self.scene_visible() {
            self.gather_ui_world(aspect, !ui_view)
        } else {
            Vec::new()
        };

        // Offscreen previews render LAST (after play_step advanced this frame's poses
        // and particles, and after ensure_vfx_assets registered their textures/meshes):
        // otherwise a docked/split Game view or the Inspector camera POV showed frozen
        // animation and missing effects — it was drawing a frame before the sim, with
        // VFX assets not yet resolved. Reuses `elapsed` so it costs no extra clock read.
        // Both take &mut self and must live outside the main GPU destructure below, so
        // this is the last safe point before it.
        // A1 target cameras render FIRST, so every later pass (previews, game
        // viewport, the surface itself) samples this frame's feed.
        self.update_render_targets(elapsed);
        self.update_camera_preview(elapsed);
        self.update_game_viewport(elapsed);
        // The ◫ UI tab's canvas — the selected layer through the real UI
        // pipeline. Runs alongside the other offscreen views, and no-ops (and
        // frees nothing but time) when the tab isn't showing.
        self.sync_ui_design_guides();
        self.update_ui_design_view();
        // The ◈ Shaders tab's per-node preview atlas (only while it's visible).
        self.update_shader_graph_preview(elapsed);
        // `stage ui` shaders read `time` from the UI globals' spare lane.
        if let Some(uir) = self.ui_render.as_mut() {
            uir.set_time(elapsed);
        }

        // Terrain surface material, resolved BEFORE the GPU destructure borrows `self.raster`
        // out (`terrain_material` is `&self`): the meshed terrain draws with it in the raster
        // pass (Terrain 2.0 / P2). Cheap; only read when terrains exist.
        let terrain_base_mat = self.terrain_material();

        // This frame's sky-shader uniforms (Inspector knobs over `.flsl` defaults), also
        // resolved before the GPU destructure takes `&mut self` — both draw sites reuse it.
        let sky_active = self.sky_shader.is_some();
        let sky_uniform_vals = self.sky_uniform_values();

        // A docked (non-fullscreen) Game tab paints its own offscreen render this
        // frame, sized+blit to its rect (single-view or split) so it never spills
        // behind panels. Read here because the destructure below takes `&mut self`
        // — and read from the one predicate the input path uses, so where the
        // pixels go and where clicks are measured cannot disagree.
        let game_offscreen = self.game_offscreen();
        // Same reason: the terrain chunks' dissolve-in clock is read before the
        // destructure below takes `&mut self` (`floptle/0067`).
        let chunk_now = self.now();
        // The frame profile, cloned out before the destructure below takes
        // `&mut self` (`floptle/0077`). It is an `Rc<RefCell<…>>` shared with the
        // Lua `perf` table, so this is a refcount bump and the numbers a game
        // reads are the same ones written here.
        let profile = self.script_host.profile().clone();

        let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(retro),
            Some(outline),
            Some(grid_render),
            Some(line_layer),
            Some(tri_layer),
            Some(particles),
            Some(post),
            Some(egui),
            Some(window),
            // Not `Some(...)`: a project with no screen shaders has no registry
            // yet, and that must not stop the frame from being drawn.
            post_shaders,
            // Likewise: the scene colour history is allocated the first frame a
            // scene asks for reflections and dropped when it stops asking, so
            // "absent" is its ordinary state and not a reason to skip the frame.
            scene_history,
            // …and likewise: a device without timestamp queries has no timer, and
            // that is a missing measurement, not a missing frame.
            mut gpu_timer,
        ) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.retro.as_mut(),
            self.outline.as_ref(),
            self.grid_render.as_mut(),
            self.line_layer.as_mut(),
            self.tri_layer.as_mut(),
            self.particles.as_mut(),
            self.post.as_mut(),
            self.egui.as_mut(),
            self.window.as_ref(),
            self.post_shaders.as_ref(),
            &mut self.scene_history,
            self.gpu_timer.as_mut(),
        ) else {
            return;
        };
        let window = window.clone();
        // One pose table per FRAME, not per pass (`floptle/0080`). A frame gathers
        // the scene several times over — the Scene view, a docked Game view, every
        // render target, the selection mask — and each of those passes reads pose
        // indices handed out by an earlier gather. Resetting between them would
        // leave the mask pointing at a table that had moved under it.
        // ⏱ Open a timing frame. `begin` refuses while the previous frame's
        // readback is still out, and `timing` carries that refusal to every mark
        // below — a frame is measured whole or not at all, because half a frame's
        // marks would report each pass against its neighbour's name.
        let timing = self.gpu_timing_open
            && gpu_timer.as_mut().map(|t| {
                t.poll();
                t.begin()
            }) == Some(true);
        macro_rules! gpu_mark {
            ($label:expr) => {
                if timing {
                    if let Some(t) = gpu_timer.as_mut() {
                        t.mark(gpu, $label);
                    }
                }
            };
        }

        raster.begin_skin_frame();
        // …and the project's era artefacts, for the same reason and in the same
        // place: a frame gathers the scene several times over, and the look has
        // to be the same in the Scene view, the Game view and every render
        // target. Setting it here — before any gather — is what makes that so
        // rather than something each gather has to remember.
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
        // fullscreened tab overrides which view is front. A DOCKED (non-fullscreen)
        // Game tab renders through its own offscreen target sized to the tab rect
        // (update_game_viewport + the tab's Image blit), so the SURFACE renders the
        // editor view underneath — this keeps the game framed to its panel instead of
        // spilling the full-window render behind the other tabs. (Cost: a docked Game
        // tab draws the scene once for the offscreen game view and once for the hidden
        // editor surface; double-click the Game tab to fullscreen it for a single
        // full-window render.) Only a FULLSCREEN Game tab renders the active camera
        // straight to the surface (it fills the whole window, so that framing is right).
        let game_view = matches!(self.fullscreen_tab, Some(EditorTab::Game));
        // The active camera's layer cull mask applies to the FULLSCREEN game
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
        // FULLSCREEN game view owns the whole surface — its rect matches the
        // full-window cursor space `input.mouse()` reports. A DOCKED game tab
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
        self.mesh_wire_gizmo.clear();
        self.particle_gizmo.clear();
        // Script debug gizmos (`gizmo.*` from Lua), projected for the SURFACE camera and
        // painted in the Scene view. The GAME view gets its own set (`game_gizmo_lines`)
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
        // Fullscreen Game tab: `cam` above already IS the active gameplay camera and the
        // viewport is the whole surface, so the same projection serves. The DOCKED game
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
        if !game_view && self.show_gizmos {
            let (gw, gh) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
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
            let filter = self.gizmo_filter;
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
                        // The node's transform positions AND scales the box, so
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
                        // The far end is in the node's OWN space, so it turns
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
                        // second. Both, because the gap between them IS the
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
            // The directional "sun" Light has no world position, so its direction gizmo
            // only shows when the Lighting node is selected — anchored in front of the
            // editor camera so it's always framed, pointing along the light direction.
            // A POSITIONAL star instead anchors at the star and points at the camera
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
            // Rigidbody collider outlines, so physics bodies are visible/placeable.
            let bodies: Vec<(Entity, floptle_core::RigidBody)> = if filter.physics {
                self.world.query::<floptle_core::RigidBody>().map(|(e, rb)| (e, *rb)).collect()
            } else {
                Vec::new()
            };
            for (e, rb) in bodies {
                let wt = floptle_core::world_transform(&self.world, e);
                let p = wt.translation;
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
                        rb.height,
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
            // Terrain collider wireframes (the SDF surfaces you walk on). Cached per
            // terrain in NODE-LOCAL coords at native resolution + rebuilt only when
            // that terrain's shape changes; here we add each node's f64 anchor and
            // re-project — so a moved terrain's wireframe follows for free.
            // Coarseness scales with each grid so the line count stays sane.
            if self.show_terrain_collider && filter.colliders {
                for (&e, t) in &self.terrains {
                    if !self.terrain_wire_world.iter().any(|(we, _)| *we == e) {
                        let stride =
                            (t.shadow.dims.into_iter().max().unwrap_or(64) / 48).max(2);
                        self.terrain_wire_world
                            .push((e, terrain_collider_wire(&t.shadow, stride)));
                    }
                }
                self.terrain_wire_world.retain(|(we, _)| self.terrains.contains_key(we));
                for (e, segs) in &self.terrain_wire_world {
                    let anchor = floptle_core::world_transform(&self.world, *e).translation;
                    for &(a, b) in segs {
                        let wa = anchor + DVec3::new(a.x as f64, a.y as f64, a.z as f64);
                        let wb = anchor + DVec3::new(b.x as f64, b.y as f64, b.z as f64);
                        if let (Some(pa), Some(pb)) = (
                            project(wa, cam.world_position, view_proj, gw, gh),
                            project(wb, cam.world_position, view_proj, gw, gh),
                        ) {
                            self.terrain_wire_gizmo.push((pa, pb));
                        }
                    }
                }
            }
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
            // `Overlay` (floptle-nav) decides that from the LINKS, so the
            // outline is drawn only where the walkable surface actually ends and
            // the seams of the cut are invisible. `⊞ Cells` puts the old
            // per-rectangle wireframe back when the bake's working is the
            // question.
            // How solid the walkable surface is drawn. Low enough that the
            // level under it stays legible — the overlay is drawn over
            // everything, so an opaque one would hide the geometry it is
            // describing — and high enough to read as a surface rather than a
            // tint. A step's ribbon is stronger because it is the answer to a
            // question somebody is deliberately asking.
            const NAV_FILL_ALPHA: f32 = 0.22;
            const NAV_STEP_ALPHA: f32 = 0.40;
            self.nav_gizmo.clear();
            self.nav_surface.clear();
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
                    // A distinct hue per region, spun by the golden ratio so
                    // neighbouring numbers never land on neighbouring colours.
                    let hue = |region: u32| crate::viz::hue_rgb((region as f32 * 0.618_034).fract());
                    let world = |p: [f32; 3]| {
                        anchor + DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64)
                    };
                    let mut line = |a: [f32; 3], b: [f32; 3], col: [f32; 3]| {
                        if let (Some(pa), Some(pb)) = (
                            project(world(a), cam.world_position, view_proj, gw, gh),
                            project(world(b), cam.world_position, view_proj, gw, gh),
                        ) {
                            self.nav_gizmo.push((pa, pb, col));
                        }
                    };

                    if self.nav_cells {
                        // Every rectangle, faintly — the bake's working.
                        for e in &overlay.cells {
                            let c = hue(e.region);
                            line(e.a, e.b, [c[0] * 0.45, c[1] * 0.45, c[2] * 0.45]);
                        }
                    }
                    // The edge of the walkable surface, bright.
                    for e in &overlay.boundary {
                        line(e.a, e.b, hue(e.region));
                    }
                    // Where two heights are genuinely joined — the picture of
                    // what `max slope` and `step height` just did.
                    for s in &overlay.steps {
                        let c = hue(s.region);
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
                    for t in &overlay.tris {
                        // Painted ground reads as painted: its own hue, and
                        // brighter, because a volume that did nothing and a
                        // volume that worked have to be tellable apart at a
                        // glance rather than by baking again and squinting.
                        let col = if t.area == floptle_nav::WALKABLE {
                            fill(hue(t.region))
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
                    for l in &overlay.links {
                        let col = if !l.resolved {
                            [1.0, 0.35, 0.3]
                        } else if !l.enabled {
                            [0.45, 0.45, 0.5]
                        } else {
                            [0.45, 0.95, 1.0]
                        };
                        line(l.from, l.to, col);
                        // A tick at each end you can enter from, so a one-way
                        // drop and a two-way ladder are not the same picture.
                        let rise = mesh.settings.step_height.max(0.25);
                        for (at, draw_it) in [(l.to, true), (l.from, l.bidirectional)] {
                            if draw_it {
                                line(at, [at[0], at[1] + rise, at[2]], col);
                            }
                        }
                    }
                    // A step's ribbon is filled too, and more strongly: it is
                    // the answer to a question somebody is actively asking.
                    for s in &overlay.steps {
                        let col = strip(hue(s.region));
                        for p in [
                            s.low[0], s.low[1], s.high[1], s.low[0], s.high[1], s.high[0],
                        ] {
                            self.nav_surface
                                .push(floptle_render::TriVertex { pos: cam_rel(p), color: col });
                        }
                    }
                }
            }
            // Mesh collider wireframes. Every Mesh node flagged Collidable OR (legacy)
            // MeshCollider when the global toggle is on, plus the SELECTED one always (so
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
                    let edges = floptle_assets::gltf_import::import(std::path::Path::new(&path))
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
            // Static PRIMITIVE collider wireframes (the "Collidable" switch on a Cube /
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
        // The map tool's gizmo is whatever its OWN transform mode says (move /
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
        // deliberately brings no second — so `next()` is *the* Lighting node
        // rather than the first of several, and `find("Lighting")` from a script
        // reaches the same one this reads (`floptle/0123`).
        //
        // If something made a second anyway, say so once: a script writing "the"
        // 2D base light and this reading "the" 2D base light would then be
        // whichever the ECS happened to yield first.
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
        // count of what the sixteen-slot cap refused (`floptle/0116`). Asked here
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
        // same `light_cap_warning` `render_world_into`'s copy calls
        // (`floptle/0168`).
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
        // does, the prepass has to RUN, which is decided far below. Reading the
        // flag off the packed lanes (rather than the World a second time) keeps
        // the answer tied to the sixteen lights that actually reached the
        // shader: a lamp ranked out of the slots casts nothing, so it must not
        // be able to switch a whole pass on either.
        let point_shadows = lit3.shape[..lit3.count.min(16)]
            .iter()
            .any(|s| (s[3] as u32) & 2 != 0);
        // Screen-space reflections read LAST frame's picture, so what the shader
        // is told here depends on whether one was ever taken — see `ssr_uniform`.
        // The matrix comes from the history itself, because only it knows which
        // camera the stored frame belongs to.
        let ssr = crate::shading::ssr_uniform(
            &light_node,
            scene_history.as_ref().is_some_and(|h| h.is_primed()),
        );
        let ssr_prev_vp = scene_history
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
        let ((fog_color, fog_params), particle_fog) =
            crate::shading::fog_uniforms_and_particles_at(&light_node, &self.world, cam.world_position);
        let (atmo_meta, atmo_color, atmo_body, atmo_params) =
            crate::shading::atmo_uniforms(&self.world, cam.world_position);
        let (star_meta, star_pos, star_color) =
            crate::shading::star_uniforms(&self.world, &light_node, cam.world_position);
        // Proxies are what lets a raster mesh cast at all, and a LAMP marches the
        // same list now — so the sun's switch alone can no longer decide whether
        // they are collected. A scene with the sun's shadows off and a torch
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
            // Meshed terrain reads the triplanar scale + the per-slot NEAREST /
            // GLOW bitmasks here (bitmasks as u32 — bit-exact at 32 slots).
            terrain_mask: [0.0, 0.22, 0.0, 0.0],
            terrain_bits: [
                crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root),
                self.terrain_glow_mask,
                0,
                0,
            ],
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
        // Per-node vertex-paint bases, resolved BEFORE the draw loop (which borrows
        // `raster` mutably, so it can't call &self helpers). Empty for unpainted scenes.
        // Every node's sorting-layer Z, resolved before the draw loop borrows
        // `raster` mutably. Empty for a scene that uses no sorting layers, which
        // is every scene until one opts in.
        let sort_z = crate::sprite2d::draw_offsets(&self.world, &self.project, cam.world_position);

        // This frame's 2D lights, built ONCE. The pass below is handed this very
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
        // RENDER, first half (`floptle/0077`): turning the scene into instances.
        // The submission itself is timed separately below and lands in the same
        // bucket — a game asking "what does rendering cost" wants one number, and
        // the two halves are not separable from Lua anyway.
        let gather_t = floptle_core::profile::Span::new();
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        // The 2D lighting G-buffer's draw list, built in THIS loop from the very
        // instances the raster pass gets (`Light2dInstance::from_raster`). That
        // is the whole mitigation for deferred's second draw path: there is no
        // second walk of the world to keep in step.
        let mut flat2d: Vec<(MeshId, Option<TexId>, floptle_render::Light2dInstance)> = Vec::new();
        // GPU-skinned parts (`floptle/0080`), gathered alongside the plain ones and
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
        // FRUSTUM CULL (`floptle/0075`). Until this existed, terrain chunks were
        // the only thing in the engine that asked whether it was on screen —
        // every mesh, map mesh, tilemap, batch and primitive became an instance
        // every frame, and roughly half of any scene is behind the camera.
        //
        // Built from the same camera-relative `view_proj` the instance matrices
        // are, so a position that is right for a draw is right for the test.
        // The rejection sits at the TOP of the loop, before the match, so every
        // arm benefits from one test rather than eight.
        let frustum = floptle_render::Frustum::from_view_proj(view_proj);
        // How much was skipped, reported in the window title beside the fps.
        let mut culled_nodes = 0usize;
        // Scatter props submitted this frame — the count `floptle/0071` needed.
        let mut scatter_props = 0usize;
        for (e, matter) in &ents {
            // Hidden nodes (Visible(false)) don't draw their geometry (a script or the
            // Inspector can toggle this); they still keep transforms, physics, children.
            if matches!(self.world.get::<floptle_core::Visible>(*e), Some(floptle_core::Visible(false))) {
                continue;
            }
            // Switched OFF (the Hierarchy/Inspector toggle) — this node or an ancestor.
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
            // A sorting layer is a Z nudge on the DRAWN transform, so ordering a
            // flat scene never moves anything the physics or a script can see.
            // Resolved before the loop (`raster` is borrowed mutably in here).
            t.translation += sort_z.get(e).copied().unwrap_or_default();
            // Off screen? Skip the whole node — the material lookups, the matrix,
            // every arm below (`floptle/0075`). Answers false for anything whose
            // extent the scene does not know, and for the Blob, which is an SDF
            // primitive that shadows things it is not itself beside.
            // A pixels-per-unit sprite is drawn at its TEXTURE's size, not at
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
            // A TEXTURE-PAINTED node ALSO draws its paint OVERLAY: the per-triangle atlas
            // mesh, coplanar over the base, alpha-blended in the transparent pass. The base
            // renders normally below — texture paint never changes how the node looks,
            // it only draws over it.
            if self.world.get::<floptle_core::TexturePaint>(*e).is_some() {
                let model = t.render_matrix(cam.world_position);
                let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                crate::paint_tex::push_painted_node(&self.world, &self.paint_tex, *e, model, &mp, &mut instances);
            }
            // The node's texture and its SURFACE EXTRAS index, both resolved by
            // the renderer: a material with normal/roughness/metallic/occlusion
            // maps comes back as ONE combined `TexId`, so every arm below (and
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
                // The 2D layer (`floptle/0058`). A tilemap is one uploaded
                // mesh; a sprite batch is N instances off the unit quad, each
                // with its own cell and tint.
                Matter::Tilemap { .. } => {
                    let model = t.render_matrix(cam.world_position);
                    // One draw per sheet the layer actually uses (`floptle/0092`).
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
                        // UNLIT, and the composite corrects that by the light's
                        // difference (`floptle/0121`). The G-buffer instance is
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
                    // shader doesn't apply (the Sdf stage is their world).
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
                        push_mesh_instances(gpu, raster, asset, pose, model, tex, mp.as_ref(), obj_mats, &self.texture_registry, node_paint, *e, skin_variants, &mut skin_scratch, &mut instances, &mut skin_draws, flsl, &mut flsl_draws);
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
                        push_mesh_instances(gpu, raster, asset, None, model, tex, mp.as_ref(), obj_mats, &self.texture_registry, node_paint, *e, skin_variants, &mut skin_scratch, &mut instances, &mut skin_draws, flsl, &mut flsl_draws);
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
                // material override resolved its OWN extras a moment ago, and
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
                // `flat2d` is deliberately absent: the 2D lit pass has its own
                // shader and its own instance type, and none of this reaches it.
            }
        }

        // Terrain 2.0 (P2): the terrains' extracted chunk meshes join the raster draw list,
        // so they flow through the depth prepass, field shadows/AO, SSAO and post exactly
        // like every other mesh. The raymarch no longer draws them (their volume is `w = 3`
        // — shadow + AO, not drawn). This is the render swap that retires the up-close
        // faceting / grazing-shadow stripes the raymarched terrain had.
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

        // SCATTER (`floptle/0036`): thousands of props from a seed, resolved to
        // instances and drawn through the ordinary raster path — so they get the
        // ordinary lighting, fog and shadows, including the underwater fog that
        // makes a shoreline forest go murky at the same rate as its ground.
        // Nothing here is a scene node.
        {
            // Where each anchored source's node has got to THIS frame
            // (`floptle/0073`). A celestial body orbits at ~99 units/s, so a
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
                // uses, so a field culls by DIRECTION as well as distance.
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
                // SCATTER. `0071` was filed as "currently unplayable" and was a
                // field asking for 117,000 props; `props` in the counts below is
                // that number, and this is what it cost.
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
        // the number a game wants is the whole submission (`floptle/0075`).
        self.render_counts = crate::node_bounds::Counts {
            nodes: ents.len(),
            culled: culled_nodes,
            instances: instances.len(),
        };
        // …and the same numbers into the profile a game can read (`floptle/0077`).
        // Terrain chunk and particle counts come from the systems that own them.
        {
            let chunks: usize =
                self.terrain_render.values().map(|r| r.slots.len()).sum();
            let particles = self.vfx.live_particles();
            // `floptle/0114` and `floptle/0116`: how many one-shots and lights
            // are live, and how many of each a ceiling refused. A cap nobody can
            // see is the thing both cards are actually about — `effects` is what
            // it costs, `effectsDropped` is what it cut.
            let (effects, effects_dropped) = self.vfx.detached_counts();
            let (lights, lights_dropped) = self.light_counts;
            let voices = self.audio.live_voices();
            let mut prof = profile.borrow_mut();
            // Grouping every instance into draw-call buckets is a HashSet pass
            // over the whole submission — worth its cost only when collection
            // is actually on and something will read the number. `set_counts`
            // already no-ops while off; this keeps the SUM ahead of it off too
            // ("off means off", `floptle/0082`, applied to the one count here
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
                // time (`floptle/0122`) — 0 when no light can reach anything.
                flat2d: flat2d.len(),
            });
            prof.record(floptle_core::profile::Bucket::Render, gather_t.ms());
        }

        // Undo any transient scene-binding animation preview now that the draw list
        // is built — the ECS goes back to authored transforms before UI/undo/save.
        // NOT while recording: record keeps the previewed values live so the
        // Inspector shows what's under the playhead (edit it → it's keyed) and a
        // scrub can't diff a stale pose into spurious keys. The pre-record scene is
        // restored by stop_record_ui when ● Record turns off.
        if !self.anim_ui.record {
            self.anim.restore_preview(&mut self.world);
        }

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
        // per project): PostStack settings + the raymarch SDF-AO params.
        let (mut post_settings, rm_ao_params) = post_process_uniforms(&self.world);
        // The player's colour-vision filter rides ON TOP of the scene's chain,
        // and deliberately survives a scene whose PostProcess node is disabled
        // (`floptle/0079`): a scene must not be able to veto an accessibility
        // setting the player turned on.
        post_settings.color_filter = self.access.color_filter.lane();
        post_settings.color_filter_strength = self.access.color_filter_strength;
        post_settings.simulate_deficiency = self.access.simulate_deficiency;
        // Film grain needs a clock or it is a dirty lens, not film. Reduced
        // motion is deliberately NOT applied here: grain is texture, not
        // movement, and freezing it makes it MORE of a fixed pattern to look at.
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
                // .w = per-slot NEAREST mask (bit i = slot i is Pixelated). The palette
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
        let mut mask_mesh: Vec<(MeshId, InstanceRaw)> = Vec::new();
        // Selected GPU-skinned parts: the silhouette has to hug the POSE, so it
        // goes through the same skinned pipeline the character shades with.
        let mut mask_skins: Vec<floptle_render::SkinDraw> = Vec::new();
        let mut mask_blob: Option<RaymarchGlobals> = None;
        // The Game view plays like a build — no selection outline there.
        if !game_view {
            let mut sel_blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
            let mut sel_shapes: Vec<Entity> = Vec::new();
            for &e in &self.selection {
                let Some(m) = self.world.get::<Matter>(e) else { continue };
                // The SAME offset the draw uses. Without it the outline of a
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
                    // One sprite IS a quad, so it can be outlined — unlike a
                    // batch, whose sprites are this frame's and would trace
                    // whatever happened to be alive when you clicked.
                    Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => {
                        if let Some(&mesh) = self.mesh_ids.get(floptle_core::Shape::Plane as usize)
                        {
                            let model = t.render_matrix(cam.world_position);
                            // **The same arguments the DRAW gets.** This passed
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
                                        // A SKINNED part draws from `model` alone —
                                        // the pose is in the deform, not the matrix.
                                        // Applying node_world here too would transform
                                        // it TWICE, which is the offset outline Ty saw
                                        // on the astronaut. Match the draw.
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
        }

        // The raymarch pass renders the blob matter (gated by the SDF-matter toggle)
        // and/or the combined terrain volume — and it's ALSO what draws a textured
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
            // are only where the volume IS, and they have to be stamped per
            // view because the field is camera-relative (ADR-0015).
            raymarch.gi().apply(&mut g, cam.world_position.into());
            g
        };

        // ---- build the egui UI (mutating the World) ----
        let mut raw_input = egui.state.take_egui_input(&window);
        // A focused game owns the keyboard (`floptle/0084`). egui hands Tab to
        // widget focus traversal before anything else sees it, which put every
        // press on the dock's tab bar and left `input.pressed("tab")` returning
        // false — the same as not being pressed, so a game bound to the most
        // conventional inventory key there is had no way to tell. Gated on a text
        // field NOT wanting input, so typing into the Console or the Inspector
        // during play still works; a click is how you go back to the editor.
        // `text_edit_focused`, not `egui_wants_keyboard_input` — the latter is
        // "any widget has focus", so clicking a Play-mode HUD button used to
        // hand Tab back to the dock for the rest of the session. See the same
        // fix at the `typing` gate in `main.rs`.
        if self.playing && game_focused && !egui.ctx.text_edit_focused() {
            crate::game_keys::claim_keys_for_game(&mut raw_input, &egui.ctx);
        }
        let ctx = egui.ctx.clone();
        // A package that shipped a typeface gets it registered here — after the
        // load pass, before anything draws with it. `set_fonts` rebuilds egui's
        // glyph atlas, so it is gated on the flag and not run per frame; a
        // project whose packages ship no fonts never reaches it at all.
        if self.ext.fonts_dirty {
            self.ext.fonts_dirty = false;
            ctx.set_fonts(crate::fonts::definitions(&self.ext.fonts));
        }
        // Apply the selected engine (chrome) theme, then a play-mode tint on top so you
        // never mistake play mode for edit mode (and lose edits on Stop). Reapplied each
        // frame so switching the theme in Preferences takes effect immediately.
        {
            let theme = ENGINE_THEMES[self.engine_theme.min(ENGINE_THEMES.len() - 1)];
            let mut vis = theme.visuals();
            if self.playing && self.play_tint_enabled {
                let [tr, tg, tb] = self.play_tint;
                let tint = |c: egui::Color32| {
                    egui::Color32::from_rgb(
                        (c.r() as u16 + tr as u16).min(255) as u8,
                        (c.g() as u16 + tg as u16).min(255) as u8,
                        (c.b() as u16 + tb as u16).min(255) as u8,
                    )
                };
                vis.panel_fill = tint(vis.panel_fill);
                vis.window_fill = tint(vis.window_fill);
                vis.extreme_bg_color = tint(vis.extreme_bg_color);
            }
            ctx.all_styles_mut(|s| {
                s.visuals = vis.clone();
                // **Leave the scrollbar its own gutter.**
                //
                // egui's scroll bars FLOAT by default: they are drawn over the
                // contents and allocate no width. So the last few pixels of
                // every scrolling panel are behind a bar — a slider's label
                // ellipsised down to its first letter, a `…` menu half over the
                // edge — and the panel looks a little bit cut off everywhere,
                // which is exactly what it is. The controls are laid out to the
                // panel's edge correctly; the edge is simply not where the
                // visible area ends.
                //
                // Allocating the bar's width moves that edge in to where things
                // can actually be seen, and every widget follows it — egui's own
                // truncation as much as `responsive::fit_here`. The bar still
                // floats and still looks the same.
                s.spacing.scroll.floating_allocated_width = s.spacing.scroll.bar_width;
            });
        }
        // Every named entity, Matter nodes and the Lighting node alike.
        let entity_names: Vec<(Entity, String)> =
            self.world.query::<Name>().map(|(e, n)| (e, n.0.clone())).collect();
        // Read before `self` is split into the panel context's borrows.
        let gi_status = crate::gi_bake::gi_status(
            &self.world,
            self.gi_bake.as_ref(),
            self.gi_baked.as_ref(),
            self.gi_show_only,
            self.gi_show_probes,
        );
        let nav_status = crate::nav_bake::nav_status(
            &self.world,
            crate::nav_bake::nav_node(&self.world).as_ref().map(|(_, m)| m),
            crate::nav_bake::NavHeld {
                mesh: self.nav_baked.as_ref(),
                seconds: self.nav_seconds,
                triangles: self.nav_triangles,
                file: self.nav_loaded_from.as_deref(),
                baking: self.nav_job.is_some(),
                coverage: self.nav_coverage.as_ref(),
            },
            &self.project_root,
        );
        let old_retro_h = self.project.retro_height;
        let old_retro_w = self.project.retro_width;
        let ppp = ctx.pixels_per_point();
        let dock_state = self.dock_state.get_or_insert_with(default_dock);
        // Bone names per rigged Mesh entity (name + parent index) — for the hierarchy's
        // expandable sub-objects and the inspector's bone-attach picker. Built read-only
        // before the borrow split so the UI never touches the mesh registry itself.
        let bone_names: HashMap<Entity, Vec<crate::RigNode>> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| match m {
                Matter::Mesh { asset_path } => self
                    .mesh_registry
                    .get(asset_path)
                    .and_then(|a| a.rig.as_ref())
                    .map(|rig| {
                        let nodes = rig
                            .skeleton
                            .nodes
                            .iter()
                            .enumerate()
                            .map(|(i, n)| crate::RigNode {
                                name: n.name.clone(),
                                parent: n.parent,
                                is_object: rig.node_is_object.get(i).copied().unwrap_or(true),
                            })
                            .collect();
                        (e, nodes)
                    }),
                _ => None,
            })
            .collect();
        // Prefill the export title from the project's title (Project Settings
        // ⏵ Game); the folder name is a poor fallback (the conventional root is
        // just `assets`, which also collides with the shipped assets folder).
        if self.export_title.is_empty() {
            self.export_title = self.project.title.clone().unwrap_or_else(|| {
                self.project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|n| n != "assets")
                    .unwrap_or_default()
            });
        }
        // The entry-scene picker's options (only scanned while the window is up).
        let fullscreen_tab = &mut self.fullscreen_tab;
        let world = &mut self.world;
        let maps = &self.maps;
        let map_sel = &self.map_sel;
        let map_mode = self.map_mode;
        let map_slot_name = &mut self.map_slot_name;
        let map_viz = &self.map_viz;
        let tile_viz = &self.tile_viz;
        let map_opts = &mut self.map_opts;
        let map_size_buf = &mut self.map_size_buf;
        let map_spec_buf = &mut self.map_spec_buf;
        let map_arm = self.map_arm;
        let map_knife_on = self.map_knife_on;
        let map_orient = &mut self.map_orient;
        let map_xform = &mut self.map_xform;
        let map_select_hidden = &mut self.map_select_hidden;
        let map_bevel = &mut self.map_bevel;
        let map_hud_open = &mut self.map_hud_open;
        let map_keys = &mut self.map_keys;
        let map_rebind = &mut self.map_rebind;
        let map_rebind_err = &mut self.map_rebind_err;
        let map_tool_on = self.tool == Tool::MapEdit;
        let map_playing = self.playing;
        // Copied, not borrowed: it is read by panels that also hold &mut borrows
        // of half the editor, and it does not change during the dock draw.
        let focused_tab = self.focused_tab;
        let has_selection = !self.selection.is_empty();
        let selection = &mut self.selection;
        let bone_selection = &mut self.bone_selection;
        let pivot_edit = &mut self.pivot_edit;
        let collapsed = &mut self.collapsed;
        let hier_fold_pending = &mut self.hier_fold_pending;
        let hier_search = &mut self.hier_search;
        let hier_scope = &mut self.hier_scope;
        // Labels only — the parked documents themselves stay on the Editor, so
        // the tab strip can name them without the panel being able to reach into
        // another document's undo stack.
        let image_parked: Vec<String> =
            self.image_stash.iter().map(|s| s.tab_label()).collect();
        let console = &mut self.console;
        let preview_zoom = &mut self.preview_zoom;
        let preview_spin = &mut self.preview_spin;
        let preview_spinning = &mut self.preview_spinning;
        let preview_material = &mut self.preview_material;
        let map_asset_preview = &mut self.map_asset_preview;
        let project = &mut self.project;
        let layer_new = &mut self.layer_new;
        let show_project_mgr = &mut self.show_project_mgr;
        let project_path_buf = &mut self.project_path_buf;
        let grid = &mut self.grid;
        let show_grid_settings = &mut self.show_grid_settings;
        // ⏱ The frame-timing panel's open flag and the last frame it collected,
        // both taken out here for the same reason everything else on this line is
        // — the UI below runs while `self` is split apart.
        let show_gpu_timing = &mut self.gpu_timing_open;
        // Read from the borrowed timer rather than back out of `self`: the frame
        // took it mutably at the destructure. `poll` has already run this frame,
        // so these are the newest results that have actually landed.
        let gpu_spans: Vec<floptle_render::Span> =
            gpu_timer.as_deref().map(|t| t.spans().to_vec()).unwrap_or_default();
        let gpu_total = gpu_timer.as_deref().map(|t| t.total_ms()).unwrap_or(0.0);
        self.gpu_timing_frames = self.gpu_timing_frames.wrapping_add(1);
        let gpu_timing_supported = gpu_timer.is_some();
        if !gpu_spans.is_empty()
            && *show_gpu_timing
            && std::env::var("FLOPTLE_GPU_TIMING").is_ok()
            && self.gpu_timing_frames.is_multiple_of(120)
        {
            println!("--- GPU frame {gpu_total:.2} ms");
            for sp in &gpu_spans {
                println!("  {:>7.3} ms  {}", sp.ms, sp.label);
            }
        }
        let show_terrain_collider = &mut self.show_terrain_collider;
        let show_navmesh = &mut self.show_navmesh;
        let nav_cells = &mut self.nav_cells;
        let show_mesh_colliders = &mut self.show_mesh_colliders;
        let rename_target = &mut self.rename_target;
        let new_scene_buf = &mut self.new_scene_buf;
        let new_asset_prompt = &mut self.new_asset_prompt;
        let show_quit_confirm = &mut self.show_quit_confirm;
        let image_close_confirm = &mut self.image_close_confirm;
        let delete_confirm = &mut self.delete_confirm;
        let layer_children_confirm = &mut self.layer_children_confirm;
        let toast = &mut self.toast;
        // Dirty tilesets ride the scene's flag here. They are not scene state —
        // they are their own files — but every gate that asks "is there unsaved
        // work" wants one answer, and a tileset's collision shapes and autotile
        // groups are hours of work that used to leave with the window.
        let scene_dirty_now = self.scene_dirty || !self.tiles.dirty.is_empty();
        // The 🖼 tab keeps its own dirty flag — an unsaved image is unsaved
        // work, and quitting past it silently is the same loss as quitting past
        // a scene.
        let image_dirty_now = self.image.dirty && self.image.doc.is_some();
        // …and one that has never been written has no filename to save under,
        // so "Save & Quit" cannot silently do it: it has to ask first.
        let image_unnamed = image_dirty_now && self.image.path.is_none();
        let new_terrain_cfg = &mut self.new_terrain_cfg;
        let pending_open_scene = &mut self.pending_open_scene;
        let vertex_brush = &mut self.vertex_brush;
        let terrain_brush = &mut self.terrain_brush;
        let terrain_voxel = &mut self.terrain_voxel;
        let terrain_textures = &mut self.terrain_textures;
        let terrain_glow = &mut self.terrain_glow_mask;
        let terrain_present = !self.terrains.is_empty();
        // Terrain 2.0 stats: volumes, resident data chunks, resident bytes — the
        // honest sparse numbers (the dense field's O(n³) voxel count is gone).
        let terrain_stats = (!self.terrains.is_empty()).then(|| {
            let chunks: usize = self.terrains.values().map(|t| t.field.data_chunks()).sum();
            let bytes: usize = self.terrains.values().map(|t| t.field.memory_bytes()).sum();
            (self.terrains.len(), chunks, bytes)
        });
        let save_flash = &mut self.save_flash;
        // What the save-status chip names on hover: the real file being edited.
        let save_status_file = if self.scene_rel.is_empty() {
            format!("scenes/{}.ron", self.scene_name)
        } else {
            self.scene_rel.clone()
        };
        let external_editor = &mut self.external_editor;
        let prefer_external = &mut self.prefer_external_editor;
        let show_preferences = &mut self.show_preferences;
        let play_tint_enabled = &mut self.play_tint_enabled;
        let play_tint = &mut self.play_tint;
        // Current theme selections (changes are routed through `cmd`, then saved + applied).
        let engine_theme = self.engine_theme;
        let code_theme = self.code_theme;
        let asset_tree = &self.asset_tree;
        let texture_settings = &self.texture_settings;
        let assets_grid = &mut self.assets_grid;
        let assets_grid_dir = &mut self.assets_grid_dir;
        let project_root = self.project_root.as_path();
        let playing = self.playing;
        // Who owns the pointer this frame, for the Game-view hint. Read as plain
        // fields (not through `game_holds_cursor`) because the closure below only
        // ever holds disjoint field borrows and a `&self` method would collide.
        let cursor_held_by_game = self.game_trap || (self.script_mouse_lock && !self.cursor_freed);
        let cursor_held_by_editor = self.cursor_freed && self.script_mouse_lock;
        let paused = self.paused;
        let game_tick_no = self.game_tick_no;
        let has_active_camera = floptle_core::active_camera(world).is_some();
        // The selected camera's POV preview texture (only when a camera is selected).
        let cam_preview = selection
            .last()
            .copied()
            .filter(|&e| matches!(world.get::<Matter>(e), Some(Matter::Camera { .. })))
            .and(self.cam_preview.as_ref().map(|p| p.tex_id));
        let particles_active = crate::dock::tab_is_front(dock_state, EditorTab::Particles);
        let game_tex = self.game_vp.as_ref().map(|p| p.tex_id);
        let game_rect = &mut self.game_rect;
        let materials = &self.materials;
        let mat_name_buf = &mut self.mat_name_buf;
        let component_clip = &self.component_clip;
        let add_component_filter = &mut self.add_component_filter;
        let layer_names = project.build_layers().names;
        let sorting_names = project.sorting_order();
        let tag_edit = &mut self.tag_edit;
        let hier_scrolled = &mut self.hier_scrolled;
        let show_material_editor = &mut self.show_material_editor;
        // The package extensions and their window. `ext_host` is handed to the
        // dock (its Scene overlays draw in the viewport), and used again after
        // for the floating panels — sequentially, so one `&mut` covers both.
        // What the last load found, read off the host BEFORE it is borrowed
        // mutably for the tab viewer — the 📦 Packages tab draws from inside
        // that viewer and cannot hold a second borrow of the host itself.
        let pkg_load = crate::packages_ui::PkgLoad::of(&self.ext);
        let ext_host = &mut self.ext;
        let ext_painted = self.ext_painted.as_slice();
        let packages_state = &mut self.packages_ui;
        let ext_project_root = self.project_root.clone();
        let ext_account = self.account.as_ref();
        // Built before the closure: `ext_menu_tree` reads the whole editor, and
        // inside the UI pass only disjoint field borrows exist.
        let ext_menus = crate::ext_wire::menu_tree(ext_host);
        let ext_focus_window = self.ext_focus_window.take();
        let ext_message = &mut self.ext_message;
        // What the packages' menu and panels decided this frame, applied after
        // the UI pass — running a Lua callback while the host is drawing would
        // be re-entering it.
        let mut ext_menu_click: Option<usize> = None;
        let mut ext_shortcut_click: Option<usize> = None;
        let mut pkg_action = crate::packages_ui::PackagesAction::default();
        let ide = &mut self.ide;
        let learn = &mut self.learn;
        let script_errors = self.script_errors.as_slice();
        let ide_diag = self.ide_diag.as_ref();
        let selected_asset = &mut self.selected_asset;
        let asset_selection = &mut self.asset_selection;
        let aspect_mode = &mut self.aspect_mode;
        let viewport_zoom = &mut self.viewport_zoom;
        let scene_rect = &mut self.scene_rect;
        let scene_name = self.scene_name.clone();
        let gizmo = self.gizmo.as_ref();
        let terrain_viz = self.terrain_viz.as_ref();
        let paint_viz = self.paint_viz.as_ref();
        let camera_gizmos = self.camera_gizmos.as_slice();
        let light_gizmos = self.light_gizmos.as_slice();
        let volume_gizmos = self.volume_gizmos.as_slice();
        let rig_gizmos = self.rig_gizmos.as_slice();
        let gi_probe_dots = self.gi_probe_dots.as_slice();
        let body_gizmos = self.body_gizmos.as_slice();
        let contact_gizmos = self.contact_gizmos.as_slice();
        let script_gizmo_lines = self.script_gizmo_lines.as_slice();
        let game_gizmo_lines = self.game_gizmo_lines.as_slice();
        // The gizmo menu's checkbox writes this directly; remember it so the change can
        // be persisted after the dock UI runs.
        let game_gizmos_before = self.game_gizmos;
        let game_gizmos = &mut self.game_gizmos;
        let terrain_wire = self.terrain_wire_gizmo.as_slice();
        let nav_wire = self.nav_gizmo.as_slice();
        let mesh_wire = self.mesh_wire_gizmo.as_slice();
        let particle_gizmo = self.particle_gizmo.as_slice();
        let show_gizmos = &mut self.show_gizmos;
        let panels = &mut self.panels;
        let panels_saved = &mut self.panels_saved;
        let mut view_lock = self.camera.lock;
        let mut view_ortho = self.camera.ortho;
        let gizmo_filter = &mut self.gizmo_filter;
        let grabbed = self.grabbed;
        let tool = self.tool;
        let context_menu = self.context_menu;
        let anim_sys = &mut self.anim;
        let vfx_sys = &mut self.vfx;
        let vfx_ui_state = &mut self.vfx_ui;
        let audio_sys = &mut self.audio;
        let mixer_ui_state = &mut self.mixer_ui;
        let anim_ui_state = &mut self.anim_ui;
        let shader_graph_state = &mut self.shader_graph;
        let image_state = &mut self.image;
        let ui_design = &mut self.ui_design;
        let shader_preview_state = &mut self.shader_preview;
        let mesh_registry = &self.mesh_registry;
        // Multiplayer harness panel state: read-only status snapshot + live knobs.
        let net_hosting = self.net_server.is_some();
        let net_peer_count = self.net_server.as_ref().map(|s| s.peers().len()).unwrap_or(0);
        let net_has_client = self.net_client.is_some();
        let net_as_player = self.net_play_client.is_some();
        // The MEASURED round trip, falling back to the transport's own number
        // until the first probe comes back. Through a relay the transport can
        // only see its own leg, so it reports host↔relay and calls it the
        // player's ping — off by a whole hop, and always in the flattering
        // direction.
        let net_rtt = self
            .net_play_client
            .as_ref()
            .map(|c| {
                c.peer_rtt_ms(floptle_net::SERVER)
                    .unwrap_or_else(|| c.stats(floptle_net::SERVER).rtt_ms)
            })
            .unwrap_or(0.0);
        // Per-player pings, host side — what a relay could never report.
        let net_peer_rtts = self.net_server.as_ref().map(|s| s.peer_rtts()).unwrap_or_default();
        let net_predicted_name = self
            .net_predictor
            .as_ref()
            .and_then(|(e, _)| world.get::<Name>(*e).map(|n| n.0.clone()));
        let net_pred_stats = self
            .net_predictor
            .as_ref()
            .map(|(_, p)| (p.corrections, p.confirmations, p.last_error));
        let net_late_inputs = self
            .net_hidden
            .as_ref()
            .map(|h| h.session.late_inputs())
            .or_else(|| self.net_server.as_ref().map(|s| s.late_inputs()))
            .unwrap_or(0);
        // Client-side input timing, from the server's InputAck feedback —
        // the only place a JOINER can see whether its inputs run late.
        let net_input_ack = self.net_play_client.as_ref().and_then(|c| c.input_ack());
        // Rollback health (docs/rollback-netcode-design.md §7 P6): a fighting
        // game's connection quality is rollback depth and mispredict rate, not
        // ping — and the stall indicator is the one readout a player NEEDS,
        // because a stalled sim looks like the game running slightly slow and
        // is otherwise indistinguishable from a bad frame rate.
        let net_rollback = self.net_rollback.as_ref().map(|d| {
            crate::rollback_session::RollbackStats::with_session(
                d,
                self.net_server.as_ref().or(self.net_play_client.as_ref()),
            )
        });
        // (referee tick, live tick) — how far behind the authoritative sim is.
        let referee = self
            .net_referee
            .as_ref()
            .map(|r| (r.tick(), self.net_rollback.as_ref().map(|d| d.net.current()).unwrap_or(0)));
        let replays = crate::shadow::list_replays(&self.project_root);
        // Interest management is the one feature whose job is to NOT send
        // things, so with no readout it is indistinguishable from a bug: set
        // the radius too tight and distant objects quietly stop moving, with
        // nothing anywhere saying why. `None` when it's off, which is a
        // different statement from "on, and culling nothing".
        let net_interest = self.net_server.as_ref().and_then(|s| {
            let cfg = s.interest();
            cfg.enabled.then(|| (cfg, s.interest_stats()))
        });
        // A REAL session (QUIC) has no hub: the link is the actual network, so
        // the simulated latency/loss sliders and ghost worlds don't apply.
        let net_is_real = (self.net_server.is_some() || self.net_play_client.is_some())
            && self.net_hub.is_none();
        if self.net_host_port.is_empty() {
            self.net_host_port = "7777".into();
        }
        if self.net_join_addr.is_empty() {
            self.net_join_addr = "quic://127.0.0.1:7777".into();
        }
        if self.net_relay_addr.is_empty() {
            // The Floptle Cloud rendezvous relay (task 0005): a DNS-only
            // record straight to the host — the name is the stable contract
            // even if the box moves. Self-hosters just type their own.
            self.net_relay_addr = "relay.fopull.com:7788".into();
        }
        let net_host_port = &mut self.net_host_port;
        let net_join_addr = &mut self.net_join_addr;
        let net_relay_addr = &mut self.net_relay_addr;
        let net_join_code = &mut self.net_join_code;
        let net_lobby_code = self.net_lobby_code.clone();
        // A snapshot of the profile, taken before the UI closure so the readout
        // never holds the `RefCell` across a frame that also writes it.
        let mut perf_snapshot = PerfSnapshot::take(&profile.borrow());
        perf_snapshot.pacing = Pacing {
            mean_ms: self.frame_ms,
            p99_ms: self.frame_low_ms,
            // `refresh_period` is in SECONDS (it is compared against `dt`).
            refresh_ms: self.refresh_period * 1000.0,
            snap_rate: self.dt_snap_rate,
            present_wait_ms: self.present_wait_ms,
            cost_ms: (self.frame_ms - self.present_wait_ms).max(0.0),
        };
        let show_net_panel = &mut self.show_net_panel;
        let show_perf_panel = &mut self.show_perf_panel;
        // Applied after the UI closure, because turning collection on or off
        // needs the profile and the closure has the fields split.
        let mut perf_toggle: Option<bool> = None;
        // Player mode (an exported build / --play): no editor chrome at all —
        // the Game view IS the window. F1 (handled at the winit layer) toggles
        // the multiplayer window, which still works for LAN/relay sessions.
        let player_mode = self.player_mode;
        let play_t = self.play_t;
        let ui_overlay_snapshot = self.ui_overlay.clone();
        let ref_kinds = &self.ref_kinds;
        let script_meta = &mut self.script_meta;
        let ui_canvas_snapshot = self.ui_canvas.clone();
        let show_export = &mut self.show_export;
        // Relative export folders resolve against the project's PARENT (shown
        // live in the dialog) — never the process CWD, which depends on how
        // the editor was launched.
        let export_base =
            self.project_root.parent().unwrap_or(&self.project_root).to_path_buf();
        if self.export_dir.trim().is_empty() {
            self.export_dir = "builds".into();
        }
        let export_dir = &mut self.export_dir;
        let export_title = &mut self.export_title;
        let export_target = &mut self.export_target;
        let export_building = self.export_job.is_some();
        let export_status = &self.export_status;
        let export_done = self.export_done.clone();
        let autosave_prompt = self.autosave_prompt.clone();
        let crash_prompt = self.crash_prompt.clone();
        let scene_name_now = self.scene_name.clone();
        let net_latency_ticks = &mut self.net_latency_ticks;
        let net_loss = &mut self.net_loss;
        let net_ghosts = &mut self.net_ghosts;
        // ⚙ Settings tab inputs. Only gathered when the tab is actually open,
        // so a closed Settings tab costs nothing per frame.
        let settings_open = dock_state.find_tab(&crate::dock::EditorTab::Settings).is_some();
        // Accessibility is `Copy`, so the tab edits a copy and reports back
        // (`floptle/0079`) — no field borrow to thread through the tab viewer.
        let access = self.access;
        let settings_scene_files = if settings_open {
            crate::project::scene_files_in(&self.project_root)
        } else {
            Vec::new()
        };
        let settings_pad_names =
            if settings_open { self.pads.slot_names() } else { Vec::new() };
        let (settings_input_map, settings_input_pending) = {
            let sys = self.script_host.input_system().borrow();
            if settings_open {
                (sys.map().clone(), sys.pending_rebind().cloned())
            } else {
                (floptle_input::InputMap::default(), None)
            }
        };
        let settings_section = &mut self.settings_section;
        let settings_search = &mut self.settings_search;
        let input_scan = &self.input_scan;
        let input_new_action = &mut self.input_new_action;
        let input_test_state = &self.input_test_state;
        let mut cmd = EditorCmd::default();
        let mut want_save = false;
        let mut want_save_project = false;
        // Set inside the egui closure (which only holds field borrows), applied after it —
        // the same deferral `want_save` uses. `want_save_all` = full Ctrl+S save on quit;
        // `want_exit` = actually leave the app once the save has run.
        let mut want_save_all = false;
        let mut want_exit = false;
        let mut frame_pointer_down = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            let pointer_down = ui.input(|i| i.pointer.any_down());
            frame_pointer_down = pointer_down;
            // ---- top menu bar (never in a build) ----
            if !player_mode {
            egui::Panel::top("menu_bar").show(ui, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui.button("New / Open Project…").clicked() {
                            *show_project_mgr = true;
                            ui.close();
                        }
                        if ui.button("Close Project").clicked() {
                            cmd.project_action = Some(ProjectAction::Close);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Save Scene").clicked() {
                            want_save = true;
                            ui.close();
                        }
                        if ui.button("Save Project").clicked() {
                            want_save_project = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("Open Project Folder")
                            .on_hover_text("show the project (assets, scenes, scripts) in your file manager")
                            .clicked()
                        {
                            cmd.open_folder = Some(std::path::PathBuf::new()); // empty = project root
                            ui.close();
                        }
                        if ui
                            .button("Export Game…")
                            .on_hover_text(
                                "stamp out a runnable build: the engine + your project, for \
                                 any platform — Windows, Linux or macOS, from whichever \
                                 one you're on",
                            )
                            .clicked()
                        {
                            *show_export = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Exit").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.menu_button("Edit", |ui| {
                        if ui.button("Undo  (Ctrl+Z)").clicked() { cmd.undo = true; ui.close(); }
                        if ui.button("Redo  (Ctrl+Y)").clicked() { cmd.redo = true; ui.close(); }
                        ui.separator();
                        // Selection-dependent items grey out with nothing selected
                        // (Paste stays — it depends on the clipboard, not selection).
                        if ui.add_enabled(has_selection, egui::Button::new("Copy  (Ctrl+C)")).clicked() { cmd.copy = true; ui.close(); }
                        if ui.button("Paste  (Ctrl+V)").clicked() { cmd.paste = true; ui.close(); }
                        if ui.add_enabled(has_selection, egui::Button::new("Duplicate  (Ctrl+D)")).clicked() { cmd.duplicate = true; ui.close(); }
                        if ui.add_enabled(has_selection, egui::Button::new("Delete  (Del)")).clicked() { cmd.delete = true; ui.close(); }
                        ui.separator();
                        if ui.button("Project Settings").on_hover_text(
                            "Opens the ⚙ Settings tab — drag it wherever you like, or dock it beside the viewport.",
                        ).clicked() {
                            cmd.open_settings = true;
                            ui.close();
                        }
                        if ui.button("Preferences…").clicked() {
                            *show_preferences = true;
                            ui.close();
                        }
                    });
                    // The same catalog as the Hierarchy's ✚ New menu — one source of truth.
                    ui.menu_button("Add", |ui| node_new_menu(ui, &mut cmd, None));
                    ui.menu_button("View", |ui| {
                        ui.checkbox(&mut grid.show, "Grid");
                        ui.checkbox(&mut grid.snap, "Snap to grid");
                        if ui.button("Grid Settings…").clicked() {
                            *show_grid_settings = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.checkbox(&mut *show_terrain_collider, "Terrain collider wireframe")
                            .on_hover_text("show the terrain's collision surface (what the player walks on)");
                        ui.checkbox(&mut *show_mesh_colliders, "Collider wireframes (mesh + shapes)")
                            .on_hover_text("show every static collider — walkable meshes and Collidable Cube/Sphere/Capsule shapes (the selected one always shows)");
                        ui.checkbox(&mut *show_navmesh, "Navmesh")
                            .on_hover_text(
                                "show where characters can walk as one filled surface, a colour \
                                 per connected area, with the joins between elevations drawn \
                                 where a character can actually take them (the Nav Mesh node \
                                 always shows its own when selected)",
                            );
                        ui.add_enabled_ui(*show_navmesh, |ui| {
                            ui.checkbox(&mut *nav_cells, "    ⊞ …and the rectangles it was cut into")
                                .on_hover_text(
                                    "the bake's working: every convex rectangle the walkable \
                                     surface was divided into. Useful for judging cell size; \
                                     it is not what the ground looks like",
                                );
                        });
                    });
                    // Tool windows + panels live under Window (View = viewport display).
                    // Every entry opens/focuses its window (close them from the
                    // window itself) — one consistent behavior.
                    ui.menu_button("Window", |ui| {
                        if ui.button("◑ Material Editor").clicked() {
                            *show_material_editor = true;
                            ui.close();
                        }
                        if ui.button("◎ Animation Controller").on_hover_text("the state-graph editor: states, transitions, fades, layers").clicked() {
                            cmd.focus_anim_graph = true;
                            ui.close();
                        }
                        if ui.button("⏱ Animating").on_hover_text("the animation timeline: preview, keys, events").clicked() {
                            cmd.focus_animating = true;
                            ui.close();
                        }
                        if ui
                            .checkbox(&mut *show_gpu_timing, "⏱ Frame timing")
                            .on_hover_text(
                                "where the frame's time actually goes, measured on the GPU pass \
                                 by pass. Nothing is measured while this is shut, so leaving it \
                                 off costs nothing",
                            )
                            .changed()
                        {
                            ui.close();
                        }
                        if ui.button("Δ Terrain tools").clicked() {
                            cmd.focus_terrain = true;
                            ui.close();
                        }
                        if ui.button("▦ Model tools").clicked() {
                            cmd.focus_map = true;
                            ui.close();
                        }
                        if ui
                            .button("🖼 Image editor")
                            .on_hover_text(
                                "draw a texture in the engine — pixels, paint and vectors,                                  with the mesh updating as you paint",
                            )
                            .clicked()
                        {
                            cmd.focus_image = true;
                            ui.close();
                        }
                        if ui
                            .button("📦 Packages")
                            .on_hover_text(
                                "install, switch off or write a package — editor tools, \
                                 scripts and art anybody can make and share",
                            )
                            .clicked()
                        {
                            cmd.focus_packages = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.label(
                            egui::RichText::new("your layout is saved when you close the editor")
                                .small()
                                .weak(),
                        );
                        if ui
                            .button("⟲ Reset layout")
                            .on_hover_text(
                                "put every panel back where it starts: Hierarchy + Map left, \
                                 viewports and graph editors centre, Inspector right, \
                                 project and timelines below — and forget the saved one, so \
                                 it stays reset",
                            )
                            .clicked()
                        {
                            cmd.reset_layout = true;
                            ui.close();
                        }
                        if ui
                            .button("⟲ Reset window size")
                            .on_hover_text(
                                "back to 1280×720 where it can be seen, and forget where this \
                                 window was — what to press if it opened somewhere awkward",
                            )
                            .clicked()
                        {
                            cmd.reset_window = true;
                            ui.close();
                        }
                    });
                    // HELP, and specifically somewhere to REPORT things. The tracker used
                    // to appear once, in the Hub's About tab, which is not where anybody
                    // is standing when something goes wrong.
                    ui.menu_button("Help", |ui| {
                        if ui
                            .button("🎓 Learn — follow-along tutorials")
                            .on_hover_text(
                                "build a platformer, a top-down RPG or Flappy step by \
                                 step, with each step ticking itself off as your project \
                                 comes to match it",
                            )
                            .clicked()
                        {
                            cmd.focus_learn = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("🐛 Report a bug")
                            .on_hover_text(crate::ISSUES_URL)
                            .clicked()
                        {
                            crate::open_issue_tracker(None);
                            ui.close();
                        }
                        if ui.button("📖 Scripting docs").clicked() {
                            let _ = floptle_script::open_in_browser(crate::DOCS_URL);
                            ui.close();
                        }
                        if ui.button("🌐 fopull.com").clicked() {
                            let _ = floptle_script::open_in_browser("https://fopull.com/");
                            ui.close();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new(format!("Floptle {}", env!("CARGO_PKG_VERSION"))).small());
                    });
                    // Whatever the project's packages registered, grouped by
                    // the first segment of each path — so two packages both
                    // filing under "Tools" build one menu, not two.
                    for group in &ext_menus {
                        ui.menu_button(&group.title, |ui| {
                            for (label, idx) in &group.items {
                                if ui.button(label).clicked() {
                                    ext_menu_click = Some(*idx);
                                    ui.close();
                                }
                            }
                        });
                    }
                    ui.separator();
                    let play_label = if playing { "⏹ Stop  (F1)" } else { "⏵ Play  (F1)" };
                    if ui.button(play_label).clicked() {
                        cmd.toggle_play = true;
                    }
                    if playing {
                        let pause_label = if paused { "⏵ Resume  (F2)" } else { "⏸ Pause  (F2)" };
                        if ui.button(pause_label).clicked() {
                            cmd.toggle_pause = true;
                        }
                        // Frame-step: only meaningful while frozen. One click = exactly
                        // one fixedUpdate tick (scripts, physics, animation), then stop
                        // again — how you find out whether a jab is 4 frames of startup
                        // or 5.
                        ui.add_enabled_ui(paused, |ui| {
                            // Backwards first, so the pair reads left-to-right as a
                            // scrubber rather than as two unrelated buttons.
                            if ui
                                .button("⏮ Back  (Shift+F3)")
                                .on_hover_text(
                                    "put the simulation back exactly one gameplay tick.\n\n                                     A simulation isn't invertible, so this reads the \
                                     ROLLBACK state ring rather than re-deriving anything: \
                                     it needs a rollback session running, and reaches back \
                                     as far as the ring keeps (about a fifth of a second).",
                                )
                                .clicked()
                            {
                                cmd.step_tick_back = true;
                            }
                            if ui
                                .button("⏭ Step  (F3)")
                                .on_hover_text(
                                    "advance exactly one gameplay tick — scripts, \
                                     physics and animation each move one frame",
                                )
                                .clicked()
                            {
                                cmd.step_tick = true;
                            }
                        });
                        // The tick counter, so an observed event has a frame NUMBER you
                        // can put in a frame-data table.
                        ui.label(
                            egui::RichText::new(format!("tick {game_tick_no}")).monospace().weak(),
                        )
                        .on_hover_text("gameplay ticks since Play started (60 Hz)");
                    }
                    if ui
                        .button(if net_hosting { "🌐 hosting" } else { "🌐" })
                        .on_hover_text("Multiplayer — host & join locally, latency/loss sliders (docs/netcode-design.md)")
                        .clicked()
                    {
                        *show_net_panel = !*show_net_panel;
                    }
                    // ⏱ Frame cost (`floptle/0077`). Opening it turns collection
                    // on; closing it turns collection off, so the profiler costs
                    // nothing when nobody is looking at it — which is the only
                    // way one stays switched on.
                    if ui
                        .button(if *show_perf_panel { "⏱ profiling" } else { "⏱" })
                        .on_hover_text(
                            "Frame cost — where the time goes, per subsystem and per \
                             script. Readable from Lua too (perf.*), so a game can \
                             assert its own budget in a smoke test.",
                        )
                        .clicked()
                    {
                        *show_perf_panel = !*show_perf_panel;
                        perf_toggle = Some(*show_perf_panel);
                    }
                    // The view is now chosen by the Scene / Game dock tabs (the editor
                    // free-fly view vs the active-camera gameplay view), not a toggle here.

                    // ---- save status (right end of the bar, always visible) ----
                    // Whatever tab you're docked in, this answers "are my changes
                    // on disk?": a quiet "✔ saved" at rest, an amber "● unsaved"
                    // the moment an edit lands, and a brief green glow when a
                    // save completes. Right-aligned so nothing else ever moves.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let dt = ui.input(|i| i.stable_dt).min(0.1);
                        *save_flash = (*save_flash - dt).max(0.0);
                        let quiet = ui.visuals().weak_text_color();
                        // The two signal colours, from the one place they live
                        // (`theme::signal`) — an unsaved change is a warn and a
                        // save that landed is a good, the same amber and green
                        // as everywhere else in the editor.
                        let (label, color, hover) = if scene_dirty_now {
                            (
                                "● unsaved",
                                crate::theme::signal::WARN,
                                format!("{save_status_file} has unsaved changes — click here (or Ctrl+S) to save"),
                            )
                        } else {
                            // Glow bright right after a save, settle to quiet
                            // (t = 0 IS the resting state — one branch, one wording).
                            let t = (*save_flash / Editor::SAVE_FLASH_SECS).clamp(0.0, 1.0);
                            (
                                "✔ saved",
                                quiet.lerp_to_gamma(crate::theme::signal::GOOD, t),
                                format!("{save_status_file} is saved"),
                            )
                        };
                        let text = egui::RichText::new(label).color(color);
                        if playing {
                            // Saving is blocked during Play (Play changes aren't
                            // kept) — say so instead of failing quietly.
                            ui.add_enabled(false, egui::Button::new(text).frame(false))
                                .on_disabled_hover_text(
                                    "can't save during Play — press Stop first (Play changes aren't kept)",
                                );
                        } else if scene_dirty_now {
                            // Only a BUTTON when there is something to save. A
                            // chip that looks pressable and does nothing is the
                            // small dead interaction this is meant to replace.
                            if ui
                                .add(egui::Button::new(text).frame(false))
                                .on_hover_text(hover)
                                .clicked()
                            {
                                want_save = true;
                            }
                        } else {
                            ui.label(text).on_hover_text(hover);
                        }
                    });
                });
            });
            }

            // ---- ⏱ frame cost (`floptle/0077`) ----
            if *show_perf_panel {
                let mut open = true;
                egui::Window::new("⏱ Frame cost")
                    .open(&mut open)
                    .default_width(320.0)
                    .show(ui, |ui| {
                        perf_readout(ui, &perf_snapshot);
                    });
                if !open {
                    *show_perf_panel = false;
                    perf_toggle = Some(false);
                }
            }

            // ---- 🌐 multiplayer harness (Host & Join locally) ----
            if *show_net_panel {
                let mut open = true;
                egui::Window::new("🌐 Multiplayer")
                    .open(&mut open)
                    .default_width(280.0)
                    .show(ui, |ui| {
                        if !playing {
                            ui.label("Enter Play mode, then host or join a session here.");
                            ui.small(
                                "Test alone (a hidden ghost client over a simulated link), \
                                 or for real: host on a UDP port and a friend with this \
                                 project joins over the network.",
                            );
                            return;
                        }
                        if net_hosting && !net_peer_rtts.is_empty() {
                            ui.small(
                                net_peer_rtts
                                    .iter()
                                    .map(|(p, r)| format!("peer {p}: {r:.0} ms"))
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            )
                            .on_hover_text(
                                "measured host↔player round trip. Probed end to end rather \
                                 than read off the transport, because through a relay the \
                                 transport only sees its own leg — it would report host↔relay \
                                 and call it the player's ping.",
                            );
                        }
                        // ---- interest management, when the host turned it on ----
                        if let Some((cfg, stats)) = &net_interest {
                            ui.separator();
                            ui.label(format!(
                                "👁 interest · {:.0} m radius · {} KB/s per client",
                                cfg.radius,
                                cfg.budget_bytes_per_sec / 1024
                            ))
                            .on_hover_text(
                                "each client is told about its own neighbourhood instead of the \
                                 whole world. Nothing is dropped for good — what doesn't fit \
                                 the budget accrues priority and goes in a later snapshot.",
                            );
                            if stats.is_empty() {
                                ui.small("no clients yet — nothing to build a relevant set from");
                            }
                            for (peer, st) in stats {
                                let line = format!(
                                    "peer {peer}: {} of {} sent · {} B{}",
                                    st.sent,
                                    st.relevant,
                                    st.bytes,
                                    if st.deferred > 0 {
                                        format!(" · {} waiting", st.deferred)
                                    } else {
                                        String::new()
                                    }
                                );
                                // A backlog that never clears is the one shape
                                // worth colouring: it means the budget cannot
                                // keep up with the scene, and distant things
                                // will visibly lag rather than merely update
                                // less often.
                                if st.deferred > st.sent && st.sent > 0 {
                                    ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                                        .on_hover_text(
                                            "more entities are waiting for a turn than got one. \
                                             They are not lost — they accrue priority — but if \
                                             this stays high, raise interestBudget or lower the \
                                             radius.",
                                        );
                                } else {
                                    ui.small(line).on_hover_text(
                                        "relevant = what this client may hear about at all; \
                                         sent = what fit in the last snapshot's budget.",
                                    );
                                }
                            }
                        }
                        if let Some(rb) = net_rollback.as_ref() {
                            ui.separator();
                            if rb.stalled {
                                ui.colored_label(
                                    egui::Color32::from_rgb(255, 170, 60),
                                    "⚔ ROLLBACK · waiting for input",
                                )
                                .on_hover_text(
                                    "past the depth cap the sim waits instead of guessing \
                                     further: the game runs slightly slow rather than \
                                     teleporting the opponent. It catches up on its own.",
                                );
                            } else {
                                // Delay and mispredict rate on ONE line, because
                                // neither means anything alone: a rollback
                                // implementation working perfectly and one badly
                                // misconfigured look identical from outside, and
                                // "delay 2 — 99% guessed" is the whole diagnosis
                                // (floptle/0049).
                                let line = format!(
                                    "⚔ ROLLBACK · {} fighter(s) · delay {} · {:.0}% guessed",
                                    rb.fighters,
                                    rb.input_delay,
                                    rb.mispredict_rate * 100.0,
                                );
                                // Only once there is enough of a match to judge:
                                // the opening ticks always guess.
                                let bad = rb.mispredict_rate > 0.5 && rb.current > 120;
                                if bad {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(255, 170, 60),
                                        line,
                                    )
                                    .on_hover_text(format!(
                                        "almost every tick is being guessed and re-simulated. \
                                         Nothing is broken — the fight is identical on both \
                                         machines — but this peer is doing several times the \
                                         work and it feels like it. The delay is too low for \
                                         this link: raise it between matches with \
                                         net.setInputDelay(n) (max {}), or set \
                                         net.host{{ inputDelay = n }}.",
                                        floptle_net::MAX_DELAY
                                    ));
                                } else {
                                    ui.label(line);
                                }
                            }
                            ui.small(format!(
                                "corrections {} · depth last {} / max {} / avg {:.1} · \
                                 ring {} ticks / {} KB",
                                rb.corrections,
                                rb.last_depth,
                                rb.max_depth_seen,
                                rb.average_depth,
                                rb.ring_ticks,
                                rb.ring_bytes / 1024,
                            ))
                            .on_hover_text(
                                "the delay is FIXED for the session — it never changes \
                                 mid-match, because how the game feels must not. These \
                                 numbers are the measurement you choose it from: a healthy \
                                 match sits at low average depth.",
                            );
                            // WHO is starved, and on what. A frozen match used
                            // to look identical from both screens; this names
                            // the side that stopped keeping up (floptle/0039).
                            ui.small(format!(
                                "frontier · confirmed {} of {} simulated ({} ahead)",
                                rb.confirmed,
                                rb.current,
                                rb.current.saturating_sub(rb.confirmed),
                            ))
                            .on_hover_text(
                                "\"confirmed\" is the newest tick every peer's REAL input is \
                                 known for. Everything past it was simulated from a guess and \
                                 can still be corrected. When the gap reaches the depth cap \
                                 the sim stalls — so a gap pinned at the cap means someone's \
                                 input has stopped arriving.",
                            );
                            for (peer, frontier, backlog) in &rb.peers {
                                let who = if *peer == floptle_net::SERVER {
                                    "host".to_string()
                                } else {
                                    format!("peer {peer}")
                                };
                                // A backlog past the fan-out window is a peer
                                // that has stopped confirming — the shape of a
                                // starved or departed player, not of a slow one.
                                let stuck = *backlog > 24;
                                let line =
                                    format!("   {who} · frontier {frontier} · {backlog} tick(s) held");
                                if stuck {
                                    ui.colored_label(egui::Color32::from_rgb(255, 170, 60), line)
                                        .on_hover_text(
                                            "this peer has stopped confirming ticks: the host \
                                             is holding its inputs and re-sending them, and \
                                             will keep doing so until they land. If it stays \
                                             here, that peer is the one that fell out of the \
                                             match.",
                                        );
                                } else {
                                    ui.small(line);
                                }
                            }
                            // Checksum status. "Never checked" and "checked and
                            // agreeing" are very different states to be in.
                            if rb.desynced {
                                ui.colored_label(
                                    egui::Color32::from_rgb(255, 90, 90),
                                    "⚠ DESYNCED — the peers no longer agree",
                                )
                                .on_hover_text(
                                    "from the reported tick on, the two machines are playing \
                                     different matches. The Console names the tick. Usual \
                                     causes: a gameplay value outside snapshot()/restore(), \
                                     an unseeded rng() (use net.random()), or reading node.x \
                                     inside fixedUpdate instead of node.tickPos.",
                                );
                            } else if rb.checksum_tick > 0 {
                                ui.small(format!(
                                    "✔ checksums agree through tick {}",
                                    rb.checksum_tick
                                ));
                            } else {
                                ui.small("checksums: none due yet (every 30 confirmed ticks)");
                            }
                            if let Some(rf) = referee {
                                ui.small(format!(
                                    "⚖ referee at tick {} ({} behind)",
                                    rf.0,
                                    rf.1.saturating_sub(rf.0)
                                ))
                                .on_hover_text(
                                    "a second simulation of this match on the host, advanced \
                                     only to ticks every peer's input has actually arrived \
                                     for. It never guesses and never rolls back, so it is \
                                     never wrong — only behind. Every peer's checksum is \
                                     judged against it, which is the difference between \
                                     \"someone is out of sync\" and \"THAT machine is\".",
                                );
                            }
                            ui.separator();
                        }
                        // Replays. A match's inputs and its seed ARE the match,
                        // so a replay is kilobytes and playing it back is
                        // re-simulation rather than re-enactment.
                        if !replays.is_empty() {
                            ui.small("🎞 replays");
                            for (name, path) in &replays {
                                if ui
                                    .button(name.as_str())
                                    .on_hover_text(
                                        "re-simulate this match in a headless second world. \
                                         Enter Play on its scene first — a replay is the match \
                                         run again, so it needs the world it was played in.",
                                    )
                                    .clicked()
                                {
                                    cmd.net_play_replay = Some(path.clone());
                                }
                            }
                            ui.separator();
                        }
                        // Dev-only rehearsal knob. The section only exists at
                        // all when FLOPTLE_NET_IMPAIR was set on the command
                        // line, so it cannot appear in front of someone who did
                        // not ask for it — the whole point is that a real
                        // session can never be silently degraded from the UI.
                        if let Some(knob) = Editor::net_impair() {
                            let mut imp = knob.get();
                            let before = imp;
                            let hot = imp.is_active();
                            ui.colored_label(
                                if hot {
                                    egui::Color32::from_rgb(255, 170, 60)
                                } else {
                                    egui::Color32::GRAY
                                },
                                "⚠ LINK IMPAIRMENT (dev build)",
                            )
                            .on_hover_text(
                                "adds latency and loss to THIS build's real transports (QUIC \
                                 and the relay) so a rollback match can be rehearsed at match \
                                 conditions between two instances on one desk. It is not a \
                                 network emulator — no jitter, no reordering — and it is not \
                                 a substitute for the two-machine acceptance run.",
                            );
                            let rtt = imp.rtt_ms();
                            ui.add(
                                egui::Slider::new(&mut imp.latency_ms, 0..=250)
                                    .text(format!("one-way ms  (≈{rtt} ms RTT)")),
                            );
                            let mut loss_pct = imp.loss * 100.0;
                            if ui
                                .add(egui::Slider::new(&mut loss_pct, 0.0..=25.0).text("% loss"))
                                .changed()
                            {
                                imp.loss = loss_pct / 100.0;
                            }
                            if hot && ui.button("off").clicked() {
                                imp = floptle_net::Impairment::default();
                            }
                            if imp != before {
                                knob.set(imp);
                            }
                            ui.small(
                                "reliable traffic is never dropped — a real reliable channel \
                                 retransmits, so dropping handshakes would only invent \
                                 failures the field can't produce.",
                            );
                            ui.separator();
                        }
                        if net_as_player {
                            ui.label(format!(
                                "🎮 you are a REMOTE PLAYER · rtt {net_rtt:.0} ms"
                            ));
                            match &net_predicted_name {
                                Some(n) => ui.small(format!(
                                    "predicting \"{n}\" locally — orange ghosts = the hidden server's truth. Raise latency/loss and feel it stay responsive."
                                )),
                                None => ui.small(
                                    "spectating (no Predicted node) — give your character a Networked component with mode 'Predicted (owner)'",
                                ),
                            };
                            if let Some((corr, conf, last)) = net_pred_stats {
                                let total = corr + conf;
                                let pct = if total > 0 {
                                    100.0 * corr as f64 / total as f64
                                } else {
                                    0.0
                                };
                                ui.small(format!(
                                    "reconciles: {conf} confirmed · {corr} corrected ({pct:.0}%) · last error {:.0} mm · late inputs {net_late_inputs}",
                                    last * 1000.0
                                ))
                                .on_hover_text("healthy prediction: corrections near 0%, late inputs near 0 (a brief burst right after dragging the latency slider is normal — the server pauses to refill the input pipeline). Constant growth = the sims disagree — report it");
                            }
                        } else {
                            match (net_hosting, net_has_client) {
                                (false, _) => {
                                    // The simulated-link harness is an editor
                                    // dev tool — a BUILD's menu is just the
                                    // real hosting/joining flows.
                                    if !player_mode {
                                        ui.label("Test alone (simulated link)");
                                        if ui.button("⏵ Host + join a local client").clicked() {
                                            cmd.net_host_local = true;
                                            cmd.net_join_local = true;
                                        }
                                        if ui
                                            .button("🎮 Test as remote player (predicted)")
                                            .on_hover_text("the play world becomes a CLIENT predicting against a hidden authoritative server — your character stays responsive at any latency, the server keeps the truth")
                                            .clicked()
                                        {
                                            cmd.net_play_as_client = true;
                                        }
                                        ui.separator();
                                    }
                                    ui.label(if player_mode {
                                        "Host — friends join with a lobby code"
                                    } else {
                                        "Real network — via relay (lobby codes)"
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("relay");
                                        ui.add(
                                            egui::TextEdit::singleline(net_relay_addr)
                                                .desired_width(150.0)
                                                .hint_text("relay host:port"),
                                        );
                                    });
                                    ui.horizontal(|ui| {
                                        if ui
                                            .button("⏵ Host — get a lobby code")
                                            .on_hover_text("registers a lobby on the relay above and shows a five-letter CODE for friends. Nobody port-forwards; run `floptle-relay` anywhere both machines can reach.")
                                            .clicked()
                                        {
                                            cmd.net_host_relay = Some(net_relay_addr.clone());
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("code");
                                        let r = ui.add(
                                            egui::TextEdit::singleline(net_join_code)
                                                .desired_width(70.0)
                                                .hint_text("ABCDE"),
                                        );
                                        if r.changed() {
                                            *net_join_code = net_join_code.to_uppercase();
                                        }
                                        let ok = !net_join_code.trim().is_empty();
                                        if ui
                                            .add_enabled(ok, egui::Button::new("⏵ Join by code"))
                                            .on_hover_text("joins the lobby with this code, through the relay above")
                                            .clicked()
                                        {
                                            cmd.net_join_quic = Some(format!(
                                                "relay://{}/{}",
                                                net_relay_addr.trim(),
                                                net_join_code.trim()
                                            ));
                                        }
                                    });
                                    ui.separator();
                                    ui.label("Real network — direct (LAN / self-host)");
                                    ui.horizontal(|ui| {
                                        ui.label("port");
                                        ui.add(
                                            egui::TextEdit::singleline(net_host_port)
                                                .desired_width(60.0),
                                        );
                                        if ui.button("⏵ Host on LAN").clicked() {
                                            cmd.net_host_quic =
                                                Some(net_host_port.trim().parse().unwrap_or(7777));
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(net_join_addr)
                                                .desired_width(170.0)
                                                .hint_text("quic://ip:port"),
                                        );
                                        if ui.button("⏵ Join").clicked() {
                                            cmd.net_join_quic = Some(net_join_addr.clone());
                                        }
                                    });
                                    ui.small(
                                        "both machines run THIS project. Player slots = the \
                                         scene's Predicted nodes in order (#1 the host, #2+ \
                                         joiners) — or spawn one per joiner (player_spawner.lua). \
                                         Scripts: net.host{relay=\"…\"} / net.join(\"relay://…/CODE\")",
                                    );
                                }
                                (true, false) if !net_is_real => {
                                    ui.label("hosting · 0 ghost clients");
                                    if ui.button("➕ Join a local ghost client").clicked() {
                                        cmd.net_join_local = true;
                                    }
                                }
                                _ => {
                                    ui.label(format!(
                                        "hosting · {net_peer_count} client(s) connected"
                                    ));
                                    if let Some(code) = &net_lobby_code {
                                        ui.horizontal(|ui| {
                                            ui.label("lobby code:");
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(code).strong().monospace(),
                                            ).selectable(true));
                                            if ui.small_button("copy").clicked() {
                                                ui.ctx().copy_text(code.clone());
                                            }
                                        });
                                    }
                                    if net_is_real && net_peer_count > 0 {
                                        ui.small(format!("late inputs {net_late_inputs} — near zero is healthy"));
                                    }
                                }
                            }
                        }
                        if net_hosting || net_as_player {
                            ui.separator();
                            if net_is_real {
                                ui.label("real link (QUIC)");
                                ui.small("latency and loss are whatever the network gives you — the sliders only shape the simulated harness");
                            } else {
                                ui.label("simulated link");
                                let mut lat = *net_latency_ticks as i32;
                                if ui
                                    .add(egui::Slider::new(&mut lat, 0..=30).text("latency (ticks)"))
                                    .on_hover_text("one-way, in gameplay ticks — 6 ticks ≈ 100 ms round trip")
                                    .changed()
                                {
                                    *net_latency_ticks = lat as u64;
                                }
                                ui.add(
                                    egui::Slider::new(net_loss, 0.0..=0.9)
                                        .text("packet loss")
                                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                                );
                                ui.checkbox(net_ghosts, "show client ghosts (cyan)")
                                    .on_hover_text("where the ghost client believes every networked node is — the gap to the real object is the interp delay");
                            }
                            ui.separator();
                            if ui.button("⏹ End session").clicked() {
                                cmd.net_stop_session = true;
                            }
                        }
                    });
                if !open {
                    *show_net_panel = false;
                }
            }

            // ---- net-stats overlay: one compact line while a session runs, so
            // connection health is visible without the 🌐 panel open ----
            if playing && (net_hosting || net_as_player) {
                egui::Area::new(egui::Id::new("net_stats_overlay"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::RIGHT_TOP, [-10.0, 40.0])
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            let kind = if net_is_real { "net" } else { "sim" };
                            let mut line = if net_as_player {
                                let timing = net_input_ack
                                    .map(|(margin, late)| {
                                        format!(" · input margin {margin:+} · late in {late}")
                                    })
                                    .unwrap_or_default();
                                format!("🌐 client ({kind}) · rtt {net_rtt:.0} ms{timing}")
                            } else {
                                format!(
                                    "🌐 host ({kind}) · {net_peer_count} peer(s) · late in {net_late_inputs}"
                                )
                            };
                            if let Some((corr, conf, last)) = net_pred_stats {
                                let total = corr + conf;
                                let clean =
                                    if total > 0 { 100.0 * conf as f64 / total as f64 } else { 100.0 };
                                line.push_str(&format!(
                                    " · predict {clean:.0}% clean · err {:.0} mm",
                                    last * 1000.0
                                ));
                            }
                            ui.small(line);
                        });
                    });
            }

            // ---- player-mode hint: the only chrome a build shows, and only
            // for the first seconds (until the UI system gives games real menus) ----
            if player_mode && play_t < 8.0 && !(net_hosting || net_as_player) {
                egui::Area::new(egui::Id::new("player_hint"))
                    .order(egui::Order::Foreground)
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -14.0])
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.small("F1 — multiplayer");
                        });
                    });
            }

            // ---- dockable panels: Hierarchy / Inspector / Assets / Scene + Scripting ----
            // The Scene tab is transparent so the 3D render shows through; the others
            // paint opaque over it. Users can drag/re-dock/tab these freely.
            //
            // Clear the Scene rect first: egui_dock only runs the ACTIVE tab's `ui`,
            // so if Scene is tabbed behind Scripting, scene_ui never runs and the rect
            // would otherwise stay pinned to the old viewport region — letting clicks,
            // context-menus and model-drops fall through onto whatever panel now
            // occupies that space. `scene_ui` re-arms it only on frames it draws.
            *scene_rect = None;
            let mut viewer = EditorTabViewer {
                world,
                selection,
                maps,
                map_sel,
                map_mode,
                map_slot_name,
                map_viz,
                tile_viz,
                map_opts,
                tiles: &mut self.tiles,
                tile_tools: &mut self.tile_tools,
                map_size_buf,
                map_spec_buf,
                map_arm,
                map_knife_on,
                map_orient,
                map_xform,
                map_select_hidden,
                map_bevel,
                map_tool_on,
                map_playing,
                map_hud_open,
                map_keys,
                map_rebind,
                map_rebind_err,
                gizmo_tool,
                ui_overlay: &ui_overlay_snapshot,
                ui_canvas: &ui_canvas_snapshot,
                ref_kinds,
                script_meta,
                bone_selection,
                pivot_edit,
                fullscreen_tab,
                focused_tab,
                hier_search,
                hier_scope,
                collapsed,
                hier_fold_pending,
                bone_names: &bone_names,
                console,
                preview: preview_view.clone(),
                preview_zoom,
                preview_spin,
                preview_spinning,
                preview_material,
                map_asset_preview,
                entity_names: &entity_names,
                gi: gi_status,
                nav: nav_status.clone(),
                materials,
                mat_name_buf,
                flsl_cache: &self.flsl_cache,
                ui_flsl_cache: &self.ui_flsl_cache,
                post_flsl_cache: &self.post_flsl_cache,
                ui_styles: &self.ui_styles,
                ui_tokens: &self.ui_tokens,
                ui_design,
                sdf_cache: &self.sdf_cache,
                sky_uniforms: self.sky_shader.as_ref().map_or(&[], |(_, _, u)| u.as_slice()),
                component_clip,
                add_component_filter,
                layer_names: &layer_names,
                sorting_names: &sorting_names,
                tag_edit,
                hier_scrolled,
                show_material_editor,
                asset_tree,
                texture_settings,
                cam_preview,
                has_active_camera,
                vertex_brush,
                terrain_brush,
                terrain_voxel,
                terrain_textures,
                terrain_glow,
                terrain_present,
                terrain_stats,
                assets_grid,
                assets_grid_dir,
                project_root,
                selected_asset,
                asset_selection,
                ide,
                learn,
                script_errors,
                ide_diag,
                gizmo,
                terrain_viz,
                paint_viz,
                camera_gizmos,
                light_gizmos,
                volume_gizmos,
                rig_gizmos,
                gi_probe_dots,
                body_gizmos,
                contact_gizmos,
                script_gizmo_lines,
                ext: ext_host,
                ext_painted,
                game_gizmo_lines,
                game_gizmos,
                terrain_wire,
                nav_wire,
                mesh_wire,
                particle_gizmo,
                show_gizmos,
                panels,
                view_lock: &mut view_lock,
                view_ortho: &mut view_ortho,
                gizmo_filter,
                grabbed,
                tool,
                scene_rect: &mut *scene_rect,
                game_rect,
                game_offscreen,
                game_tex,
                aspect: aspect_mode,
                zoom: viewport_zoom,
                scene_name: &scene_name,
                editing_prefab: self.editing_prefab.is_some(),
                ppp,
                code_theme,
                anim: anim_sys,
                vfx: vfx_sys,
                vfx_ui: vfx_ui_state,
                audio: audio_sys,
                mixer_ui: mixer_ui_state,
                project,
                particles_active,
                anim_ui: anim_ui_state,
                shader_graph: shader_graph_state,
                image: image_state,
                image_parked: &image_parked,
                shader_preview: shader_preview_state,
                mesh_registry,
                pointer_down,
                playing,
                settings: crate::settings_ui::SettingsCtx {
                    scene_files: &settings_scene_files,
                    layer_new,
                    section: settings_section,
                    search: settings_search,
                    input_map: &settings_input_map,
                    input_pending: settings_input_pending.as_ref(),
                    input_scan,
                    input_test: input_test_state,
                    pad_names: &settings_pad_names,
                    input_new_action,
                    access,
                },
                packages: packages_state,
                packages_ctx: crate::packages_ui::PkgCtx {
                    project_root: &ext_project_root,
                    load: &pkg_load,
                    account: ext_account,
                },
                packages_action: &mut pkg_action,
                cmd: &mut cmd,
            };
            // Fullscreen: one tab maximized over the whole window (double-click a tab to
            // toggle). A slim header lets you restore (or press Esc); the dock layout is
            // untouched underneath and comes back exactly as it was.
            if let Some(ft) = *viewer.fullscreen_tab {
                let mut exit = false;
                // A build has nothing to restore TO — no header, and Escape
                // belongs to the game (cursor release), not the layout.
                if !player_mode {
                    // A PANEL, not a bare `ui.horizontal`. A plain row paints no
                    // background of its own, so the strip it occupied stayed
                    // transparent and the 3D surface render showed through it —
                    // a band of scene along the top edge of every maximized tab,
                    // whichever tab it was. A panel fills itself, the same way
                    // the menu bar above it always has.
                    egui::Panel::top("fullscreen_header").show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .button(format!("⛶ Restore  ·  {}", ft.title()))
                                .on_hover_text(
                                    "double-click a tab to toggle fullscreen · Esc to restore",
                                )
                                .clicked()
                            {
                                exit = true;
                            }
                            ui.small("double-click a tab or press Esc to restore");
                        });
                    });
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        exit = true;
                    }
                }
                // Scene/Game are transparent (the 3D shows through); every other tab
                // needs an opaque fill so the surface render doesn't bleed behind it.
                // Everything from here down belongs to the tab — take the whole
                // remaining rect rather than letting a stray margin leave a seam.
                let body = ui.available_rect_before_wrap();
                if !matches!(ft, EditorTab::Scene | EditorTab::Game) {
                    let bg = ui.style().visuals.panel_fill;
                    ui.painter().rect_filled(body, 0.0, bg);
                }
                let mut t = ft;
                let mut body_ui = ui.new_child(
                    egui::UiBuilder::new().max_rect(body).layout(*ui.layout()),
                );
                egui_dock::TabViewer::ui(&mut viewer, &mut body_ui, &mut t);
                if exit {
                    *viewer.fullscreen_tab = None;
                }
            } else {
                egui_dock::DockArea::new(dock_state)
                    .style(egui_dock::Style::from_egui(ui.style()))
                    .show_inside(ui, &mut viewer);
            }

            if *viewer.game_gizmos != game_gizmos_before {
                crate::prefs::save_game_gizmos(*viewer.game_gizmos);
            }
            // Where the Scene view's floating panels ended up. Compared against
            // what is on disk rather than written on every change, because a
            // drag is a change per frame and that would be a file write per
            // frame of it.
            let panels_now = *viewer.panels;
            if panels_now != *panels_saved {
                crate::prefs::save_viewport_panels(&panels_now);
                *panels_saved = panels_now;
            }
            // The Scene view's plane lock, chosen in the viewport toolbar.
            // `set_lock` snaps the camera square without moving it.
            // Read both out in one go: each is a `&mut` into the local below, so
            // the borrow has to be finished with before either can be written.
            let (chosen_lock, chosen_ortho) = (*viewer.view_lock, *viewer.view_ortho);
            view_lock = chosen_lock;
            view_ortho = chosen_ortho;

            // ---- the packages' own panels ----
            // Floating windows, like every other tool window here: they can be
            // moved and resized, they remember where they were put, and a
            // package cannot take a docked slot away from the editor's own
            // panels. Drawn AFTER the dock, so a panel is over the viewport it
            // is about.
            for i in 0..ext_host.windows.len() {
                if !ext_host.windows[i].open {
                    continue;
                }
                let title = ext_host.windows[i].title.clone();
                let id = ext_host.windows[i].id;
                let mut open = true;
                let win = egui::Window::new(&title)
                    .id(egui::Id::new(("ext_window", id)))
                    .open(&mut open)
                    .default_width(320.0)
                    .resizable(true);
                // `ed.window(...):focus()` brings the panel to the front. It
                // does NOT move it: a window that jumps to the middle of the
                // screen because a script mentioned it is a window somebody has
                // to put back.
                if ext_focus_window == Some(i) {
                    ui.ctx().move_to_top(egui::LayerId::new(
                        egui::Order::Middle,
                        egui::Id::new(("ext_window", id)),
                    ));
                }
                win.show(ui, |ui| ext_host.draw_window(i, ui));
                if !open {
                    ext_host.set_window_open(i, false);
                }
            }

            // 📦 Packages is a dock tab now, drawn with the other tabs — see
            // `EditorTab::Packages`. Nothing to draw here.

            // ---- what a package's `ed.message` asked to say ----
            if let Some((title, body)) = ext_message.clone() {
                let mut open = true;
                let mut dismissed = false;
                egui::Window::new(&title)
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui, |ui| {
                        ui.label(&body);
                        dismissed = ui.button("OK").clicked();
                    });
                if !open || dismissed {
                    *ext_message = None;
                }
            }

            // ---- a package's keyboard shortcut ----
            // Read here rather than in the editor's own key handling so an
            // extension cannot fire while a text field has the keyboard.
            if !ext_host.shortcuts.is_empty() && !ui.ctx().egui_wants_keyboard_input() {
                let pressed = crate::ext_wire::pressed_shortcut(ui.ctx());
                if let Some(p) = pressed {
                    ext_shortcut_click = ext_host.shortcuts.iter().position(|s| s.keys == p);
                }
            }

            // Viewport drop: spawn a model when an asset is released over the Scene
            // tab (panel drops — script-on-node — are consumed by those tabs first).
            // No opaque region is allocated, so the viewport never greys mid-drag.
            if egui::DragAndDrop::has_payload_of_type::<AssetPayload>(ui.ctx())
                && ui.input(|i| i.pointer.any_released())
            {
                let pos = ui.input(|i| i.pointer.interact_pos());
                let over_scene = matches!((pos, *scene_rect), (Some(p), Some(r)) if r.contains(p));
                if over_scene
                    && let Some(p) = egui::DragAndDrop::take_payload::<AssetPayload>(ui.ctx()) {
                        cmd.drop_asset = Some(p.path.clone());
                    }
            }

            // ---- Export Game… (File menu): binary + assets + manifest ----
            if *show_export {
                let mut open = true;
                egui::Window::new("📦 Export Game")
                    .open(&mut open)
                    .resizable(false)
                    .default_width(340.0)
                    .show(ui, |ui| {
                        ui.label(
                            "A build = this engine binary + the project folder. It runs \
                             the game directly (no editor) — F1 in-game opens the \
                             multiplayer menu.",
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Title");
                            ui.text_edit_singleline(export_title);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Folder");
                            ui.text_edit_singleline(export_dir)
                                .on_hover_text("the build lands here (created if missing)");
                        });
                        // Exactly where that lands — no guessing at relative paths.
                        let resolved = {
                            let t = export_dir.trim();
                            let p = std::path::Path::new(t);
                            if p.is_absolute() { p.to_path_buf() } else { export_base.join(p) }
                        };
                        ui.small(format!("→  {}", resolved.display()));
                        ui.horizontal(|ui| {
                            ui.label("Target");
                            egui::ComboBox::from_id_salt("export_target")
                                .selected_text(EXPORT_TARGETS[*export_target].label)
                                .show_ui(ui, |ui| {
                                    for (i, t) in EXPORT_TARGETS.iter().enumerate() {
                                        ui.selectable_value(export_target, i, t.label);
                                    }
                                });
                        });
                        ui.small(
                            "any target exports from any machine: the engine binary for \
                             that platform is downloaded once (matched to this engine \
                             version, checksum-verified) and reused after that. No \
                             compiler or toolchain needed.",
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let can = !export_building && !export_dir.trim().is_empty();
                            if ui.add_enabled(can, egui::Button::new("📦 Export")).clicked() {
                                cmd.export_game =
                                    Some((export_dir.trim().to_string(), *export_target));
                            }
                            if export_building {
                                ui.spinner();
                            }
                        });
                        if let Some(status) = export_status {
                            ui.add_space(4.0);
                            ui.label(status.as_str());
                        }
                        if let Some(done) = &export_done
                            && ui.button("📂 Open build folder").clicked()
                        {
                            cmd.open_folder = Some(done.clone());
                        }
                    });
                if !open {
                    *show_export = false;
                }
            }

            // ---- last run crashed ----
            if let Some(note) = &crash_prompt {
                let first = note.lines().find(|l| l.starts_with("panic:")).unwrap_or("").to_string();
                egui::Window::new("⚠ Floptle crashed last time")
                    .resizable(false)
                    .collapsible(false)
                    .default_width(460.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(
                            "The previous session ended in a crash. A report was saved — \
                             sending it is the single most useful thing you can do about it.",
                        );
                        if !first.is_empty() {
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new(&first).monospace().small());
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .button("🐛 Report it")
                                .on_hover_text(
                                    "opens the issue tracker with the version, platform and \
                                     backtrace already filled in — you can read and edit it \
                                     before posting. Nothing is sent automatically.",
                                )
                                .clicked()
                            {
                                cmd.crash_report = Some(true);
                            }
                            if ui.button("Not now").clicked() {
                                cmd.crash_report = Some(false);
                            }
                        });
                    });
            }

            // ---- autosave recovery (a newer autosave than the scene file) ----
            if let Some(auto) = &autosave_prompt {
                let age = std::fs::metadata(auto)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| {
                        let s = d.as_secs();
                        if s < 120 { format!("{s} s ago") } else { format!("{} min ago", s / 60) }
                    })
                    .unwrap_or_else(|| "recently".into());
                egui::Window::new("💾 Recover unsaved work?")
                    .resizable(false)
                    .collapsible(false)
                    .default_width(360.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!(
                            "'{scene_name_now}' has an AUTOSAVE newer than its saved file                              (written {age}) — usually the editor closed with unsaved                              changes. Restore it?"
                        ));
                        ui.small("Restoring loads the autosaved version (still unsaved —                                   Ctrl+S to keep it). Discard deletes the autosave.");
                        ui.horizontal(|ui| {
                            if ui.button("♻ Restore autosave").clicked() {
                                cmd.autosave_action = Some(true);
                            }
                            if ui.button("🗑 Discard it").clicked() {
                                cmd.autosave_action = Some(false);
                            }
                        });
                    });
            }

            // Project Settings used to be a fixed-size modal window here. It's
            // now the ⚙ Settings DOCK TAB (see `settings_ui.rs`): draggable,
            // dockable beside the viewport, searchable, and closed by default.

            // ---- preferences window (user-wide editor settings) ----
            egui::Window::new("Preferences")
                .open(show_preferences)
                .resizable(false)
                .default_width(320.0)
                .show(ui.ctx(), |ui| {
                    ui.label("External editor — \"Open in IDE\"");
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(external_editor)
                                .desired_width(150.0)
                                .hint_text("code"),
                        );
                        if ui.button("Save").clicked() {
                            cmd.set_external_editor = Some(external_editor.clone());
                        }
                    });
                    ui.small("Binary name or path (e.g. code, codium, subl). VSCode-family editors open the project folder and jump to the file. Saved as a user preference.");
                    if ui
                        .checkbox(prefer_external, "Open scripts in my external editor")
                        .on_hover_text("When on, double-clicking a script (or its Edit button, or a console line) opens it here instead of the in-engine IDE.")
                        .changed()
                    {
                        cmd.set_prefer_external = Some(*prefer_external);
                    }

                    ui.add_space(12.0);
                    ui.label("Play-mode tint");
                    ui.separator();
                    let mut tint_changed = ui
                        .checkbox(play_tint_enabled, "Tint the editor while playing")
                        .on_hover_text("Tints the editor chrome while in play mode so you never mistake it for edit mode (and lose edits on Stop).")
                        .changed();
                    ui.add_enabled_ui(*play_tint_enabled, |ui| {
                        // The stored value is an additive RGB offset, so editing it as a color
                        // reads naturally: black = no tint, brighter = a stronger nudge.
                        let mut col =
                            egui::Color32::from_rgb(play_tint[0], play_tint[1], play_tint[2]);
                        ui.horizontal(|ui| {
                            ui.label("tint amount");
                            if ui.color_edit_button_srgba(&mut col).changed() {
                                *play_tint = [col.r(), col.g(), col.b()];
                                tint_changed = true;
                            }
                        });
                        ui.small("Color added to the editor background while playing (black = no tint).");
                        if ui.button("Reset to default").clicked() {
                            *play_tint = DEFAULT_PLAY_TINT;
                            tint_changed = true;
                        }
                    });
                    if tint_changed {
                        cmd.set_play_tint = Some((*play_tint_enabled, *play_tint));
                    }

                    ui.add_space(12.0);
                    ui.label("Themes");
                    ui.separator();
                    // Engine (chrome) theme.
                    ui.horizontal(|ui| {
                        ui.label("Engine theme");
                        let cur = engine_theme.min(ENGINE_THEMES.len() - 1);
                        egui::ComboBox::from_id_salt("engine_theme_combo")
                            .selected_text(ENGINE_THEMES[cur].name)
                            .show_ui(ui, |ui| {
                                for (i, t) in ENGINE_THEMES.iter().enumerate() {
                                    if ui.selectable_label(i == cur, t.name).clicked() {
                                        cmd.set_engine_theme = Some(i);
                                    }
                                }
                            });
                    });
                    ui.small("Recolors the editor windows, panels and menus.");
                    // Code-editor theme.
                    ui.horizontal(|ui| {
                        ui.label("Editor theme");
                        let cur = code_theme.min(CODE_THEMES.len() - 1);
                        egui::ComboBox::from_id_salt("code_theme_combo")
                            .selected_text(CODE_THEMES[cur].name)
                            .show_ui(ui, |ui| {
                                for (i, t) in CODE_THEMES.iter().enumerate() {
                                    if ui.selectable_label(i == cur, t.name).clicked() {
                                        cmd.set_code_theme = Some(i);
                                    }
                                }
                            });
                    });
                    ui.small("Syntax colors + background of the in-engine script editor.");
                });

            // ---- frame timing window ----
            //
            // **The number that answers "why is this slow".** Wall-clock frame
            // time says a frame was slow; this says which pass was, which is the
            // only version of the question anybody can act on. Measured with GPU
            // timestamps, so it is time the card spent rather than time the CPU
            // spent asking — those differ by orders of magnitude and it is
            // routinely the second one that looks fine.
            egui::Window::new("⏱ Frame timing")
                .open(show_gpu_timing)
                .resizable(false)
                .default_width(300.0)
                .show(ui.ctx(), |ui| {
                    if !gpu_timing_supported {
                        ui.label("This GPU does not offer timestamp queries.");
                        ui.small(
                            "Nothing can be measured per pass here. The frame cost in the title \
                             bar still applies.",
                        );
                        return;
                    }
                    if gpu_spans.is_empty() {
                        ui.label("measuring…");
                        ui.small("a frame's timings arrive a frame or two after it is drawn");
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.strong(format!("{gpu_total:.2} ms"));
                        ui.small("on the GPU, this frame");
                    });
                    ui.separator();
                    let worst = gpu_spans.iter().map(|s| s.ms).fold(0.0f32, f32::max).max(1e-4);
                    for s in &gpu_spans {
                        ui.horizontal(|ui| {
                            // A bar, because the ordering is the point: the eye
                            // finds the longest one without reading six numbers.
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(96.0, 12.0),
                                egui::Sense::hover(),
                            );
                            let vis = ui.visuals();
                            ui.painter().rect_filled(rect, 2.0, vis.extreme_bg_color);
                            let w = (s.ms / worst).clamp(0.0, 1.0) * rect.width();
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height())),
                                2.0,
                                vis.selection.bg_fill,
                            );
                            ui.label(format!("{:>6.2} ms", s.ms));
                            ui.label(&s.label);
                        });
                    }
                    ui.separator();
                    ui.small(
                        "GPU time per pass. A frame the display is pacing shows a small total \
                         here and a large one in the title bar — that is the display waiting, \
                         not the scene costing.",
                    );
                });

            // ---- grid settings window ----
            egui::Window::new("Grid Settings")
                .open(show_grid_settings)
                .resizable(false)
                .default_width(240.0)
                .show(ui.ctx(), |ui| {
                    let mut changed = false;
                    changed |= ui.checkbox(&mut grid.show, "show grid").changed();
                    changed |= ui.checkbox(&mut grid.snap, "snap objects to grid").changed();
                    changed |= ui.add(egui::Slider::new(&mut grid.size, 0.1..=10.0).text("cell size")).changed();
                    changed |= ui.add(egui::Slider::new(&mut grid.extent, 4..=120).text("extent (cells)")).changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut grid.y_offset, 0.0..=50.0)
                                .text("drop below camera")
                                .suffix(" m"),
                        )
                        .on_hover_text("How far below the camera the grid floor sits. Your value is saved between sessions.")
                        .changed();
                    changed |= ui.add(egui::Slider::new(&mut grid.alpha, 0.0..=1.0).text("opacity")).changed();
                    ui.horizontal(|ui| {
                        ui.label("color");
                        changed |= ui.color_edit_button_rgb(&mut grid.color).changed();
                    });
                    if ui.small_button("Reset to defaults").clicked() {
                        *grid = GridConfig::default();
                        changed = true;
                    }
                    // Persist the grid settings whenever a control changes (so they don't
                    // reset every launch).
                    if changed {
                        cmd.save_grid = true;
                    }
                });

            // ---- viewport context menu (RMB click on an object / empty space) ----
            if let Some((pos, hit)) = context_menu {
                egui::Area::new(egui::Id::new("ctx_menu"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(190.0);
                            // ---- ▦ Model tool: the operations for what's selected ----
                            //
                            // Right-click is where people look for "what can I do to
                            // this?" — every one of these was previously a key you had
                            // to already know, or a button on a panel that may not even
                            // be open. Same `MapOp`s the panel emits, so there is one
                            // implementation and no stale subset.
                            if tool == Tool::MapEdit {
                                let sel = map_sel.as_ref();
                                let nv = sel.map_or(0, |s| s.verts.len());
                                let ne = sel.map_or(0, |s| s.edges.len());
                                let nf = sel.map_or(0, |s| s.faces.len());
                                let any = nv + ne + nf > 0;
                                // Chosen here, applied after the closures — `cmd` is
                                // borrowed by the outer menu.
                                let mut pick: Option<crate::map_edit::MapOp> = None;
                                let mut detach = false;
                                let mut mode_pick: Option<crate::map_edit::MapSubMode> = None;
                                ui.label(
                                    egui::RichText::new(match map_mode {
                                        crate::map_edit::MapSubMode::Vertex => format!("{nv} vertices"),
                                        crate::map_edit::MapSubMode::Edge => format!("{ne} edges"),
                                        crate::map_edit::MapSubMode::Face => format!("{nf} faces"),
                                    })
                                    .small()
                                    .weak(),
                                );
                                ui.separator();
                                let op = |ui: &mut egui::Ui,
                                              label: &str,
                                              tip: &str,
                                              on: bool,
                                              o: crate::map_edit::MapOp,
                                              pick: &mut Option<crate::map_edit::MapOp>| {
                                    if ui.add_enabled(on, egui::Button::new(label)).on_hover_text(tip).clicked() {
                                        *pick = Some(o);
                                    }
                                };
                                if map_mode == crate::map_edit::MapSubMode::Face {
                                    op(ui, "Extrude  (E)", "push the selected faces out along their average normal", nf > 0, crate::map_edit::MapOp::Extrude, &mut pick);
                                    op(ui, "Inset  (I)", "a smaller copy of each face inside itself", nf > 0, crate::map_edit::MapOp::Inset, &mut pick);
                                    op(ui, "Subdivide", "split each face into four", nf > 0, crate::map_edit::MapOp::Subdivide, &mut pick);
                                    op(ui, "Bridge", "join two face outlines with a tube", nf == 2, crate::map_edit::MapOp::Bridge, &mut pick);
                                    op(ui, "Flip", "reverse the winding — turn a face inside out", nf > 0, crate::map_edit::MapOp::FlipFaces, &mut pick);
                                    if ui.add_enabled(nf > 0, egui::Button::new("Detach")).on_hover_text("split the selected faces off into their own map node").clicked() {
                                        detach = true;
                                    }
                                    op(ui, "Delete faces  (Del)", "remove them, leaving a hole", nf > 0, crate::map_edit::MapOp::DeleteFaces, &mut pick);
                                } else {
                                    op(ui, "Weld selected", "merge vertices closer than the weld radius into one", nv > 1 || ne > 0, crate::map_edit::MapOp::WeldSelected, &mut pick);
                                    op(ui, "Snap to grid", "move the selection onto the grid", any, crate::map_edit::MapOp::SnapToGrid, &mut pick);
                                }
                                ui.separator();
                                ui.menu_button("Select", |ui| {
                                    op(ui, "All", "", true, crate::map_edit::MapOp::SelectAll, &mut pick);
                                    op(ui, "None", "", any, crate::map_edit::MapOp::SelectNone, &mut pick);
                                    op(ui, "Invert", "everything of this kind that isn't selected", true, crate::map_edit::MapOp::SelectInvert, &mut pick);
                                    ui.separator();
                                    op(ui, "Grow", "add the neighbouring ring", any, crate::map_edit::MapOp::Grow, &mut pick);
                                    op(ui, "Shrink", "drop the outermost ring", any, crate::map_edit::MapOp::Shrink, &mut pick);
                                    op(ui, "Linked", "everything connected to the selection", any, crate::map_edit::MapOp::SelectConnected, &mut pick);
                                    op(ui, "Coplanar", "faces lying in the same plane", nf > 0, crate::map_edit::MapOp::SelectCoplanar, &mut pick);
                                    op(ui, "Edge loop", "run along the quad loop", ne > 0, crate::map_edit::MapOp::SelectLoop, &mut pick);
                                    ui.separator();
                                    op(ui, "Warped faces", "faces whose corners no longer lie in one plane — the ones that look folded", true, crate::map_edit::MapOp::SelectNonPlanar, &mut pick);
                                });
                                ui.menu_button("Mode", |ui| {
                                    for m in [
                                        crate::map_edit::MapSubMode::Vertex,
                                        crate::map_edit::MapSubMode::Edge,
                                        crate::map_edit::MapSubMode::Face,
                                    ] {
                                        if ui.radio(map_mode == m, m.label()).clicked() {
                                            mode_pick = Some(m);
                                        }
                                    }
                                });
                                if let Some(o) = pick {
                                    cmd.map_op = Some(o);
                                    cmd.close_menu = true;
                                }
                                if detach {
                                    cmd.map_detach = true;
                                    cmd.close_menu = true;
                                }
                                if let Some(m) = mode_pick {
                                    cmd.set_map_mode = Some(m);
                                    cmd.close_menu = true;
                                }
                                ui.separator();
                            }
                            if hit.is_some() {
                                if ui.button("Duplicate  (Ctrl+D)").clicked() {
                                    cmd.duplicate = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Copy  (Ctrl+C)").clicked() {
                                    cmd.copy = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Delete  (Del)").clicked() {
                                    cmd.delete = true;
                                    cmd.close_menu = true;
                                }
                                ui.separator();
                            }
                            if ui.button("Paste  (Ctrl+V)").clicked() {
                                cmd.paste = true;
                                cmd.close_menu = true;
                            }
                            // The SAME node catalog as the Hierarchy's ✚ New and
                            // the menu-bar Add — one list, no stale subset.
                            ui.menu_button("Add", |ui| {
                                crate::hierarchy::node_new_menu(ui, &mut cmd, None);
                                cmd.close_menu |=
                                    cmd.add.is_some() || cmd.add_ui.is_some();
                            });
                        });
                    });
            }

            // ---- new / open project window (rfd unavailable ⏵ a text path) ----
            egui::Window::new("Project")
                .open(show_project_mgr)
                .resizable(false)
                .default_width(420.0)
                .show(ui.ctx(), |ui| {
                    ui.label("A project is a folder holding scenes/, models/, scripts/, …");
                    ui.horizontal(|ui| {
                        ui.label("path");
                        ui.add(
                            egui::TextEdit::singleline(project_path_buf)
                                .desired_width(290.0)
                                .hint_text("/path/to/project"),
                        );
                    });
                    ui.horizontal(|ui| {
                        let p = project_path_buf.trim().to_string();
                        if ui.add_enabled(!p.is_empty(), egui::Button::new("Open")).clicked() {
                            cmd.project_action = Some(ProjectAction::Open(p.clone()));
                        }
                        if ui.add_enabled(!p.is_empty(), egui::Button::new("Create New")).clicked() {
                            cmd.project_action = Some(ProjectAction::New(p));
                        }
                    });
                    ui.add_space(4.0);
                    ui.small("Open loads an existing folder; Create New scaffolds a fresh one.");
                });

            // ---- rename modal (for the asset browser) ----
            if let Some((path, buf)) = rename_target.as_mut() {
                let mut open = true;
                let mut close = false;
                // The fixed suffix = everything after the FIRST dot, so compound
                // extensions (.prefab.ron, .vfx.ron) ride along whole. Folders
                // have no suffix.
                let ext = if Path::new(path.as_str()).is_dir() {
                    String::new()
                } else {
                    Path::new(path.as_str())
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.find('.').map(|i| n[i..].to_string()))
                        .unwrap_or_default()
                };
                egui::Window::new("Rename")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.small(path.as_str());
                        // Edit just the base name; the extension rides along as a suffix.
                        let edit = ui
                            .horizontal(|ui| {
                                let e = ui.add(
                                    egui::TextEdit::singleline(buf)
                                        .desired_width(240.0)
                                        .hint_text("name"),
                                );
                                if !ext.is_empty() {
                                    ui.monospace(&ext);
                                }
                                e
                            })
                            .inner;
                        edit.request_focus();
                        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            let valid = !buf.trim().is_empty();
                            if ui.add_enabled(valid, egui::Button::new("Rename")).clicked() || (enter && valid) {
                                cmd.do_rename = Some((path.clone(), buf.clone()));
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *rename_target = None;
                }
            }

            // ---- new scene modal ----
            if let Some(buf) = new_scene_buf.as_mut() {
                let mut open = true;
                let mut close = false;
                egui::Window::new("New scene")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(300.0)
                    .show(ui.ctx(), |ui| {
                        ui.label("Name your new blank scene:");
                        let edit = ui.add(
                            egui::TextEdit::singleline(buf).desired_width(260.0).hint_text("scene name"),
                        );
                        edit.request_focus();
                        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            let valid = !buf.trim().is_empty();
                            if ui.add_enabled(valid, egui::Button::new("Create")).clicked() || (enter && valid) {
                                cmd.new_scene = Some(buf.clone());
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *new_scene_buf = None;
                }
            }

            // ---- name a new asset ----
            //
            // One modal for every "✚ New <thing>" that writes a file, so the
            // rule is the same everywhere: you name it, then it exists. The
            // words come from the kind; the mechanics (Enter to accept, Escape
            // or ✖ to cancel, empty is refused) do not vary.
            if let Some((kind, buf)) = new_asset_prompt.as_mut() {
                let (title, prompt, hint) = kind.words();
                let kind = *kind;
                let mut open = true;
                let mut close = false;
                egui::Window::new(title)
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(300.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(prompt);
                        let edit = ui.add(
                            egui::TextEdit::singleline(buf).desired_width(260.0).hint_text(hint),
                        );
                        edit.request_focus();
                        let enter =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            let valid = !buf.trim().is_empty();
                            if ui.add_enabled(valid, egui::Button::new("Create")).clicked()
                                || (enter && valid)
                            {
                                match kind {
                                    crate::NewAsset::Effect(e) => {
                                        cmd.do_new_particles = Some((e, buf.clone()));
                                    }
                                }
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *new_asset_prompt = None;
                }
            }

            // ---- quit with unsaved changes ----
            if *show_quit_confirm {
                let mut open = true;
                let mut close = false;
                egui::Window::new("Unsaved changes")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        match (scene_dirty_now, image_dirty_now) {
                            (true, true) => ui.label("The scene and the open image have unsaved changes."),
                            (true, false) => ui.label("The scene has unsaved changes."),
                            (false, true) => ui.label("The open image has unsaved changes."),
                            (false, false) => ui.label("Quit Floptle?"),
                        };
                        ui.horizontal(|ui| {
                            // Save & Quit: save everything, THEN close (the save runs after
                            // this closure, then `about_to_wait` exits — a real close, not the
                            // no-op ViewportCommand this app used to send).
                            let save_label =
                                if image_unnamed { "💾 Save…" } else { "💾 Save & Quit" };
                            if (scene_dirty_now || image_dirty_now)
                                && ui.button(save_label)
                                    .on_hover_text(if image_unnamed {
                                        "the image has never been saved — it needs a name, so this \
                                         stays open"
                                    } else {
                                        "save everything, then close"
                                    })
                                    .clicked()
                            {
                                want_save_all = true;
                                want_exit = !image_unnamed;
                                close = true;
                            }
                            // Discard: leave WITHOUT saving.
                            if ui.button("Discard & Quit").clicked() {
                                want_exit = true;
                                close = true;
                            }
                            // Cancel: just dismiss — no save, no exit.
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *show_quit_confirm = false;
                }
            }

            // ---- closing an image with unsaved changes ----
            //
            // Three answers, because there are three things a person means.
            // The old code offered ONE — "save first" — and a document that has
            // never been named cannot be saved without a name, so that answer
            // was sometimes not available and the close simply never happened.
            // Discard is the arm that was missing, and it is the arm that turns
            // "I'm stuck editing this image" back into an ordinary decision.
            if let Some(which) = *image_close_confirm {
                let mut decided = None;
                let mut open = true;
                egui::Window::new("Close this image?")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(340.0)
                    .show(ui.ctx(), |ui| {
                        ui.label("This image has unsaved changes.");
                        ui.small(
                            "Saving writes the layered .flimg and the flat .png beside it.",
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            // Only offered for the LIVE document: saving a parked
                            // one would mean making it live first, and a button
                            // that silently switches which image you are looking
                            // at is worse than not being there.
                            if which.is_none() && ui.button("💾  Save & close").clicked() {
                                decided = Some(1);
                            }
                            if ui
                                .button("🗑  Discard")
                                .on_hover_text("close it and lose the changes")
                                .clicked()
                            {
                                decided = Some(2);
                            }
                            if ui.button("Cancel").clicked() {
                                decided = Some(0);
                            }
                        });
                    });
                if !open {
                    decided = Some(0);
                }
                match decided {
                    Some(0) => *image_close_confirm = None,
                    Some(1) => *image_close_confirm = Some(which), // saved below, then closed
                    Some(2) => {
                        *image_close_confirm = None;
                        cmd.image_discard = Some(which);
                    }
                    _ => {}
                }
                if decided == Some(1) {
                    *image_close_confirm = None;
                    cmd.image_save_then_close = true;
                }
            }

            // ---- transient toast (save confirmation etc.) — top-center, fades out ----
            if let Some((msg, secs)) = toast.as_mut() {
                *secs -= ui.input(|i| i.stable_dt).min(0.1);
                if *secs <= 0.0 {
                    *toast = None;
                } else {
                    let a = (*secs).clamp(0.0, 1.0); // fade over the last second
                    egui::Area::new(egui::Id::new("save-toast"))
                        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
                        .interactable(false)
                        .show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style())
                                .fill(egui::Color32::from_rgba_unmultiplied(30, 120, 60, (220.0 * a) as u8))
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(msg.as_str())
                                            .color(egui::Color32::from_white_alpha((255.0 * a) as u8))
                                            .strong(),
                                    );
                                });
                        });
                }
            }

            // ---- delete asset confirmation (deletion is irreversible) ----
            if let Some(paths) = delete_confirm.clone() {
                let mut open = true;
                let mut close = false;
                let name = |p: &String| {
                    Path::new(p)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone())
                };
                egui::Window::new("Delete asset")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(340.0)
                    .show(ui.ctx(), |ui| {
                        match paths.as_slice() {
                            [p] if Path::new(p).is_dir() => {
                                ui.label(format!(
                                    "Delete the folder \"{}\" and everything in it?",
                                    name(p)
                                ));
                            }
                            [p] => {
                                ui.label(format!("Delete \"{}\"?", name(p)));
                            }
                            many => {
                                ui.label(format!("Delete these {} files?", many.len()));
                                for p in many.iter().take(8) {
                                    ui.small(format!("  {}", name(p)));
                                }
                                if many.len() > 8 {
                                    ui.small(format!("  …and {} more", many.len() - 8));
                                }
                            }
                        }
                        ui.small("This can't be undone.");
                        ui.horizontal(|ui| {
                            if ui.button("🗑 Delete").clicked() {
                                cmd.do_delete_asset = Some(paths.clone());
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *delete_confirm = None;
                }
            }

            // ---- collision layer: do the children come too? ----
            //
            // See `LayerChildrenPrompt` for why this is a question and not a
            // default. Both answers are offered as buttons that say what they
            // will do, with the counts in them — "Yes/No" on a dialog nobody
            // reads carefully is how the wrong one gets clicked every time.
            if let Some(pending) = layer_children_confirm.clone() {
                let mut open = true;
                let mut close = false;
                let n_targets = pending.targets.len();
                let n_kids = pending.children.len();
                egui::Window::new("Collision layer")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(380.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!(
                            "{} has {} child node{} under it.",
                            if n_targets == 1 {
                                "This node".to_string()
                            } else {
                                format!("These {n_targets} nodes have")
                            },
                            n_kids,
                            if n_kids == 1 { "" } else { "s" },
                        ));
                        ui.small(format!(
                            "Put them on \"{}\" as well? A collider usually hangs under the \
                             node you just changed, so leaving the children behind is often \
                             why a layer change looks like it did nothing.",
                            pending.layer
                        ));
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .button(format!("⬇  Include the {n_kids} children"))
                                .clicked()
                            {
                                let mut all = pending.targets.clone();
                                all.extend_from_slice(&pending.children);
                                cmd.do_set_layer = Some(crate::SetLayer {
                                    targets: all,
                                    layer: pending.layer.clone(),
                                });
                                close = true;
                            }
                            if ui
                                .button(if n_targets == 1 {
                                    "Just this node".to_string()
                                } else {
                                    format!("Just these {n_targets}")
                                })
                                .clicked()
                            {
                                cmd.do_set_layer = Some(crate::SetLayer {
                                    targets: pending.targets.clone(),
                                    layer: pending.layer.clone(),
                                });
                                close = true;
                            }
                            // Closing the window IS Cancel, and cancel changes
                            // nothing — the layer has not been written yet.
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *layer_children_confirm = None;
                }
            }

            // ---- new terrain dialog ----
            // Lets a fresh terrain arrive already the size/look you want (a tiny
            // rock-grey patch or a massive grass field) instead of always starting as
            // the same small default slab you'd otherwise have to sculpt/fill out by
            // hand — see NewTerrainCfg.
            if let Some(cfg) = new_terrain_cfg.as_mut() {
                let mut open = true;
                let mut close = false;
                egui::Window::new("New terrain")
                    .open(&mut open)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.label("Footprint (X/Z) and thickness (Y), world units:");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut cfg.size_xz)
                                    .range(0.5..=4000.0)
                                    .speed(1.0)
                                    .prefix("size ")
                                    .suffix(" (x/z)"),
                            );
                            ui.add(
                                egui::DragValue::new(&mut cfg.thickness)
                                    .range(0.2..=500.0)
                                    .speed(0.5)
                                    .prefix("thick ")
                                    .suffix(" (y)"),
                            );
                        });
                        // The size/detail pair silently decides quality, and the old
                        // copy here ("set detail higher before sculpting a large one")
                        // Terrain 2.0: the field is sparse and unbounded — the dialog
                        // sizes a STARTING slab, and memory scales with the surface,
                        // not the volume. Show the honest estimate live.
                        let (chunks, mb) = crate::terrain_ui::new_terrain_preview(
                            cfg.size_xz,
                            cfg.thickness,
                            *terrain_voxel,
                        );
                        ui.small(format!(
                            "→ voxel {:.2} units · ~{chunks} chunks · ~{mb:.1} MB (sparse — grows as you sculpt)",
                            *terrain_voxel,
                        ));
                        ui.horizontal(|ui| {
                            ui.label("color");
                            ui.color_edit_button_rgb(&mut cfg.color);
                        });
                        ui.label("texture (optional — paints the whole slab)");
                        let mut tex_list = Vec::new();
                        collect_texture_paths(asset_tree, &mut tex_list);
                        let cur_label = if cfg.texture.is_empty() {
                            "(none — flat color)".to_string()
                        } else {
                            Path::new(&cfg.texture)
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default()
                        };
                        egui::ComboBox::from_id_salt("new_terrain_tex")
                            .selected_text(cur_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(cfg.texture.is_empty(), "(none — flat color)")
                                    .clicked()
                                {
                                    cfg.texture.clear();
                                }
                                for p in &tex_list {
                                    let n = Path::new(p)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    if ui.selectable_label(&cfg.texture == p, n).clicked() {
                                        cfg.texture = p.clone();
                                    }
                                }
                            });
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Create").clicked() {
                                cmd.create_terrain = Some(cfg.clone());
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open || close {
                    *new_terrain_cfg = None;
                }
            }

            // ---- open-scene unsaved-changes confirm ----
            if let Some(path) = pending_open_scene.clone() {
                let name = Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                // One gate, both directions: a prefab replaces the world exactly
                // as thoroughly as a scene does, so it comes through here too and
                // only the wording differs (`floptle/0090`).
                let kind = if crate::assets::is_prefab(&path) { "prefab" } else { "scene" };
                let name = name.trim_end_matches(".prefab").to_string();
                let mut keep = true;
                egui::Window::new("Unsaved changes")
                    .open(&mut keep)
                    .resizable(false)
                    .collapsible(false)
                    .default_width(320.0)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!("Open {kind} \"{name}\"?"));
                        ui.label("The current scene has unsaved changes.");
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Save & open").clicked() {
                                cmd.do_open_scene = Some((path.clone(), true));
                                *pending_open_scene = None;
                            }
                            if ui.button("Discard & open").clicked() {
                                cmd.do_open_scene = Some((path.clone(), false));
                                *pending_open_scene = None;
                            }
                            if ui.button("Cancel").clicked() {
                                *pending_open_scene = None;
                            }
                        });
                    });
                if !keep {
                    *pending_open_scene = None;
                }
            }

            // ---- who has the pointer ------------------------------------------
            // A grabbed cursor is invisible by definition, so the ONE thing that
            // says how to get it back cannot itself be the cursor. Without this
            // the way out (Escape) was findable only by reading the source, and
            // what people did instead was alt-tab out of the whole application
            // to reach the Inspector.
            //
            // Only while playing, only over the Game view, and only when the
            // pointer is actually contested — a game that never grabs never
            // sees it.
            if playing
                && (cursor_held_by_game || cursor_held_by_editor)
                && let Some(r) = *game_rect
            {
                let (msg, fg) = if cursor_held_by_editor {
                    ("Click the game to give the mouse back", egui::Color32::from_rgb(150, 210, 255))
                } else {
                    ("Esc — free the mouse", egui::Color32::from_rgb(215, 220, 230))
                };
                egui::Area::new(egui::Id::new("pointer_owner_hint"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(r.center().x, r.max.y - 34.0))
                    .pivot(egui::Align2::CENTER_CENTER)
                    // Purely a label: it must never eat the click that hands
                    // the pointer back, which lands in this very corner.
                    .interactable(false)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_black_alpha(150))
                            .corner_radius(9.0)
                            .inner_margin(egui::Margin::symmetric(10, 5))
                            .show(ui, |ui| ui.colored_label(fg, msg));
                    });
            }

            // (Terrain tools live in the dockable Terrain tab now; the gizmo paints
            // inside the Scene tab, clipped to its rect.)
        });
        if view_lock != self.camera.lock {
            self.camera.set_lock(view_lock);
        }
        if view_ortho != self.camera.ortho {
            self.camera.set_ortho(view_ortho);
        }
        egui.state.handle_platform_output(&window, full_output.platform_output);
        // egui-winit's cursor-icon handling calls set_cursor_visible(true) whenever
        // the hover icon changes — un-hiding a cursor the game grabbed. Re-assert
        // the hide while any lock is held so the pointer can't flicker back.
        // A script lock only hides the cursor while it's actually OVER the game
        // view: where the grab is only a Confine (X11), the pointer can reach
        // the Inspector mid-play — it must be visible there to tweak values.
        // Where the grab is a real Lock it cannot travel at all, which is what
        // Escape (`cursor_freed`) is for.
        // (cursor_over_game and game_holds_cursor are inlined as plain field
        // reads — this scope holds a mutable gpu borrow, so a `&self` method
        // here would borrow the whole editor)
        let over_game = scene_hit(&egui.ctx, self.cursor, self.game_rect);
        let game_has_it = self.script_mouse_lock && !self.cursor_freed;
        if self.game_trap || (game_has_it && over_game) {
            window.set_cursor_visible(false);
        } else if self.script_mouse_lock {
            // Off the game view with the lock still wanted — or held back by
            // Escape — force the show. egui only un-hides on an icon CHANGE,
            // which may never fire, and a cursor you freed but cannot see is
            // the same bug as one you never freed.
            window.set_cursor_visible(true);
        }
        if self.project.retro_height != old_retro_h
            || self.project.retro_width != old_retro_w
        {
            let (rw, rh) = self
                .project
                .retro_size(gpu.config.width as f32 / gpu.config.height.max(1) as f32);
            retro.resize_to(gpu, rw, rh);
        }

        // Post-processing (SSAO/bloom/vignette, from the scene's PostProcess node —
        // gathered above) runs at the resolution the scene was composited at: the
        // retro internal res in retro mode (BEFORE the nearest-neighbor upscale, so
        // AO/bloom/vignette land on the same chunky pixel grid as the scene), else
        // full frame res. The stack lazily re-sizes when retro toggles/resizes.
        let post_size =
            if self.project.retro { retro.resolution() } else { (gpu.config.width, gpu.config.height) };
        post.configure(gpu, post_size.0, post_size.1, self.project.retro);

        // Screen-space reflections need somewhere to keep last frame's picture.
        // Allocated the first frame a scene asks for them and dropped again when
        // it stops: this is a full-frame mip chain, and much the largest thing
        // the renderer holds, so a project that never turns reflections on must
        // not carry one. It follows the COMPOSITED size, which in retro mode is
        // the internal resolution — reflecting a full-res picture into a 320×240
        // scene would be sharper than anything else in the frame.
        let ssr_on = light_node.reflections;
        // Glass needs the same stored picture, for the opposite reason: not to
        // reflect the scene but to see through it. So the texture is allocated
        // when EITHER asks, and a scene with a single window in it gets one
        // without having to switch reflections on as well.
        let glass = raster.any_transmissive(&instances);
        {
            let fmt = gpu.scene_format();
            let rebuilt = if ssr_on || glass {
                match scene_history.as_mut() {
                    Some(h) => h.resize_to(&gpu.device, post_size.0, post_size.1, fmt),
                    None => {
                        *scene_history = Some(floptle_render::SceneHistory::new(
                            &gpu.device,
                            post_size.0,
                            post_size.1,
                            fmt,
                        ));
                        true
                    }
                }
            } else {
                scene_history.take().is_some()
            };
            // A bind group is immutable, so it is rebuilt only when the texture
            // behind it actually changed — not every frame, which would allocate
            // a bind group per frame for as long as the editor was open.
            if rebuilt {
                let bind = scene_history.as_ref().map(|h| (h.view(), h.sampler()));
                raymarch.set_scene_history(gpu, bind);
            }
        }

        // ---- draw: scene into the retro target, blit, then egui on top ----
        // Timed, because blocking here is not the same thing as being slow — see
        // `present_wait_ms`.
        let wait_t = std::time::Instant::now();
        let acquired = gpu.acquire();
        let wait_ms = wait_t.elapsed().as_secs_f32() * 1000.0;
        self.present_wait_ms = if self.present_wait_ms > 0.0 {
            self.present_wait_ms * 0.9 + wait_ms * 0.1
        } else {
            wait_ms
        };
        match acquired {
            Some(frame) => {
                // The scene ALWAYS renders into the post input, whether or not
                // any effect is switched on.
                //
                // It used to go straight to the swapchain when the chain was
                // empty, which was a real saving when both were the same 8-bit
                // sRGB texture. They are not any more: the scene renders in the
                // floating-point scene format, the window takes 8-bit sRGB, and
                // the chain's terminal pass is the only thing that knows how to
                // get from one to the other. So "no effects" is now a chain of
                // exactly one pass rather than a different route.
                let depth =
                    if self.project.retro { retro.depth_view() } else { gpu.depth_view() };
                let color = post.input_view();
                // `rm_draw` already accounts for the matter toggle + terrain presence;
                // with nothing to raymarch the globals still upload so the raster
                // pass's field group (shadows/AO/proxies) sees this frame's data.
                // …and RENDER, second half: the passes themselves.
                let draw_t = floptle_core::profile::Span::new();
                // The sky, into the environment map, so surfaces have something
                // to reflect. Before every other pass and after the globals,
                // because the capture evaluates the sky through the very
                // uniforms `rm` carries — and every frame, because skies move:
                // a cached one would be wrong exactly when someone was watching
                // it change. It costs a 256×128 draw and eight tinier ones.
                gpu_mark!("sky / environment");
                raymarch.upload_globals(gpu, rm);
                raymarch.capture_env(gpu);
                // A custom shader that measures the gap to the surface behind it
                // (`surfaceGap` — shoreline foam, soft particles, contact glow)
                // needs the prepass too, even with nothing to raymarch. Without
                // this the effect works in a terrain scene and silently does
                // nothing in a scene made of meshes, which is the only kind of
                // scene most of these shaders are ever put in.
                let depth_wanted = raster.flsl_draws_want_depth(&flsl_draws);
                let raster_clear = if rm_draw
                    || wants_prepass(depth_wanted, ssr_on, point_shadows, contact[0] > 0.5)
                {
                    // Opaque depth prepass: primes the depth buffer (early-z kills
                    // hidden raster fragments before their shadow-marching shader
                    // runs) and caps the raymarch at the nearest mesh per pixel.
                    let depth_tex =
                        if self.project.retro { retro.depth_texture() } else { gpu.depth_texture() };
                    let hist = scene_history.as_ref().map(|h| (h.view(), h.sampler()));
                    gpu_mark!("depth prepass");
                    prepass_and_bind(
                        gpu, raster, raymarch, globals, &instances, &flsl_draws, &skin_draws,
                        depth_tex, hist,
                    );
                    if rm_draw {
                        gpu_mark!("matter / terrain");
                        raymarch.draw_into_primed(gpu, color, depth, rm);
                        None
                    } else {
                        // Nothing to raymarch: the prepass ran purely so the
                        // depth texture exists to be read. The raster pass still
                        // owns the frame, so it clears as usual — `prime_tex` is
                        // its own copy and survives that.
                        raymarch.upload_globals(gpu, rm);
                        Some(clear.map(|c| c as f64))
                    }
                } else {
                    raymarch.upload_globals(gpu, rm);
                    Some(clear.map(|c| c as f64))
                };
                gpu_mark!("opaque + lighting");
                raster.draw_scene_with(
                    gpu, color, depth, globals, &instances, &flsl_draws, &skin_draws,
                    raster_clear, Some(raymarch.field_bind()),
                );
                let composited = {
                    let d = if self.project.retro {
                        retro.depth_texture()
                    } else {
                        gpu.depth_texture()
                    }
                    .size();
                    (d.width.max(1), d.height.max(1))
                };
                // Posterize, HERE — over the art the raster and raymarch passes
                // just drew and before a light touches it. The palette is what
                // the setting quantizes; the light is a multiplier on top of it
                // (`floptle/0127`).
                if let Some(q) = post_settings.palette() {
                    raster.quantize_palette(gpu, color, composited, q);
                }
                // 2D lighting composites over the scene the raster pass just
                // drew, so a lit tilemap replaces its own unlit pixels. Runs on
                // BOTH draw paths — see `lit_2d_rank`.
                gpu_mark!("2D lighting");
                raster.light2d_pass(
                    gpu,
                    color,
                    depth,
                    composited,
                    view_proj.to_cols_array_2d(),
                    &lights_2d,
                    &flat2d,
                );
                // ---- glass ------------------------------------------------
                // The scene is finished except for the things you can see
                // through. Capture it, hand that capture to the shader as "what
                // is behind", and draw them.
                //
                // The capture is what makes refraction possible at all: a
                // surface cannot sample a picture it is already in, and the only
                // picture that exists during the main pass is the previous
                // frame's — which has the glass in it. Its tint would deepen
                // every frame it stayed on screen.
                gpu_mark!("glass");
                if glass && let Some(h) = scene_history.as_mut() {
                    // The stored picture is THIS frame's, taken from THIS
                    // camera, so the reprojection is the identity — say so, or
                    // the reflections on the glass would look up last frame's
                    // matrix against a texture that is not last frame's.
                    let mut glass_rm = rm;
                    glass_rm.ssr_prev_vp = view_proj.to_cols_array_2d();
                    raymarch.upload_globals(gpu, glass_rm);
                    // Far to near, re-capturing between: each layer of glass
                    // samples a picture holding the panes BEHIND it and none of
                    // the panes in front. One layer is one capture and one pass,
                    // exactly as before.
                    let cuts = raster.transmissive_cuts(
                        &instances,
                        &skin_draws,
                        light_node.refraction_layers,
                    );
                    for layer in 0..=cuts.len() {
                        h.capture(gpu, color, view_proj, cam.world_position);
                        // The capture writes into the SAME texture every time, so
                        // the bind group it belongs to stays valid — rebinding is
                        // only for the frame that (re)created it.
                        if layer == 0 {
                            raymarch.bind_frame_targets(
                                gpu,
                                raster.prepass_view(),
                                Some((h.view(), h.sampler())),
                            );
                        }
                        raster.draw_transmissive(
                            gpu, color, depth, globals, &instances, &skin_draws,
                            Some(raymarch.field_bind()), &cuts, layer,
                        );
                    }
                }
                // Script-drawn 3D lines (draw.line — the map's orbit conics).
                if !self.script_lines.is_empty() {
                    let verts: Vec<floptle_render::LineVertex> = self
                        .script_lines
                        .iter()
                        .flat_map(|l| {
                            let a = (DVec3::from(l.a) - cam.world_position).as_vec3();
                            let b = (DVec3::from(l.b) - cam.world_position).as_vec3();
                            [
                                floptle_render::LineVertex { pos: [a.x, a.y, a.z], color: l.color },
                                floptle_render::LineVertex { pos: [b.x, b.y, b.z], color: l.color },
                            ]
                        })
                        .collect();
                    line_layer.draw(gpu, color, depth, view_proj, &verts);
                }
                // The navmesh's walkable surface, filled. Before the script
                // triangles so a game's own gizmos draw on top of it rather
                // than under a translucent floor. Already camera-relative —
                // `nav_surface` was built that way in the gather pass.
                if !self.nav_surface.is_empty() {
                    tri_layer.draw(gpu, color, depth, view_proj, &self.nav_surface);
                }
                // Script-drawn FILLED triangles (draw.tri/cone/disc — solid gizmos).
                if !self.script_tris.is_empty() {
                    let verts: Vec<floptle_render::TriVertex> = self
                        .script_tris
                        .iter()
                        .flat_map(|t| {
                            let a = (DVec3::from(t.a) - cam.world_position).as_vec3();
                            let b = (DVec3::from(t.b) - cam.world_position).as_vec3();
                            let c = (DVec3::from(t.c) - cam.world_position).as_vec3();
                            [
                                floptle_render::TriVertex { pos: [a.x, a.y, a.z], color: t.color },
                                floptle_render::TriVertex { pos: [b.x, b.y, b.z], color: t.color },
                                floptle_render::TriVertex { pos: [c.x, c.y, c.z], color: t.color },
                            ]
                        })
                        .collect();
                    tri_layer.draw(gpu, color, depth, view_proj, &verts);
                }
                // Live particles: after all opaque work (they depth-test against
                // meshes AND raymarched matter), before post/retro — so they're
                // AO'd/bloomed and pixelate with the scene.
                if !vfx_batches.is_empty() {
                    particles.draw(
                        gpu,
                        color,
                        depth,
                        crate::vfx::particle_globals(&cam, aspect, fog_color, particle_fog),
                        &vfx_instances,
                        &vfx_batches,
                        raster,
                    );
                }
                // Keep this frame's picture, for the NEXT frame's reflections.
                //
                // Here and not later: everything that belongs to the scene has
                // drawn — the raymarched world, the meshes, the palette
                // quantise, the 2D light pass, the particles — and nothing that
                // does not has started. Post is still to come, and reflecting a
                // tonemapped, bloomed, vignetted frame would put the grade into
                // the reflection and then grade it a second time on the way out.
                //
                // The editor's own furniture (the grid, gizmos, selection
                // outlines) is deliberately on the far side of this line too: a
                // mirror must not show the reference grid.
                if let Some(h) = scene_history.as_mut() {
                    h.capture(gpu, color, view_proj, cam.world_position);
                }
                // The reference grid is an editor aid — Scene view only, and
                // deliberately AFTER the capture above: it is not part of the
                // scene, and a mirror that reflected the editor's own graph
                // paper would be showing something that does not exist.
                if self.grid.show && !game_view {
                    let c = self.grid.color;
                    grid_render.draw(
                        gpu,
                        color,
                        depth,
                        view_proj,
                        cam.world_position,
                        self.grid.size,
                        self.grid.extent,
                        self.grid.y_offset,
                        [c[0], c[1], c[2], self.grid.alpha],
                    );
                }
                // ---- Scene-view UI canvases: the layers as world planes at
                // their node transforms, depth-tested into the scene (the
                // "physically in the world" authoring view). Also projects the
                // element outlines for the Scene tab's select/drag overlay.
                self.ui_overlay.clear();
                self.ui_canvas.clear();
                let ui_gizmos = self.show_gizmos;
                if !ui_world.is_empty()
                    && let Some(uir) = self.ui_render.as_mut()
                {
                    let vp_mat = cam.view_proj(aspect);
                    let (w_px, h_px) = (gpu.config.width as f32, gpu.config.height as f32);
                    let srect = self.scene_rect.unwrap_or(egui::Rect::NOTHING);
                    crate::ui_game::draw_ui_world(
                        gpu,
                        raster,
                        uir,
                        &self.texture_registry,
                        (&self.ui_flsl_cache, &self.ui_flsl_binds),
                        color,
                        depth,
                        cam.world_position,
                        vp_mat,
                        &ui_world,
                    );
                    for (_, placed, origin, right, down, design_vp) in &ui_world {
                        let rel = floptle_core::math::Vec3::new(
                            (origin[0] - cam.world_position.x) as f32,
                            (origin[1] - cam.world_position.y) as f32,
                            (origin[2] - cam.world_position.z) as f32,
                        );
                        let r3 = floptle_core::math::Vec3::from(*right);
                        let d3 = floptle_core::math::Vec3::from(*down);
                        // Project element rects → Scene-tab overlay entries
                        // (gizmos — the master Gizmos toggle hides them, the
                        // canvas CONTENT stays since it's your actual UI).
                        if !ui_gizmos {
                            continue;
                        }
                        let to_screen = |p: floptle_core::math::Vec3| -> Option<egui::Pos2> {
                            let clip = vp_mat * p.extend(1.0);
                            if clip.w <= 0.01 {
                                return None;
                            }
                            let ndc = clip / clip.w;
                            Some(egui::pos2(
                                (ndc.x * 0.5 + 0.5) * w_px,
                                (1.0 - (ndc.y * 0.5 + 0.5)) * h_px,
                            ))
                        };
                        for pl in placed {
                            let [x, y, w, h] = pl.rect;
                            let corners = [
                                rel + r3 * x + d3 * y,
                                rel + r3 * (x + w) + d3 * y,
                                rel + r3 * x + d3 * (y + h),
                                rel + r3 * (x + w) + d3 * (y + h),
                            ];
                            let pts: Vec<egui::Pos2> =
                                corners.iter().filter_map(|c| to_screen(*c)).collect();
                            if pts.len() < 4 {
                                continue;
                            }
                            let (mut minx, mut miny, mut maxx, mut maxy) =
                                (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                            for p in &pts {
                                minx = minx.min(p.x);
                                miny = miny.min(p.y);
                                maxx = maxx.max(p.x);
                                maxy = maxy.max(p.y);
                            }
                            // px → egui points, relative to the Scene rect.
                            let sx = (minx / ppp) - srect.min.x;
                            let sy = (miny / ppp) - srect.min.y;
                            let sw = (maxx - minx) / ppp;
                            let sh = (maxy - miny) / ppp;
                            // Drag scale: overlay points per design unit.
                            let scale = if w > 0.5 { sw / w } else { 1.0 };
                            self.ui_overlay.push((pl.id, [sx, sy, sw, sh], scale.max(0.001)));
                        }
                        // Canvas bounds gizmo: the layer's design viewport as a
                        // projected quadrilateral (Scene-tab points).
                        let (cw, chh) = (design_vp[0], design_vp[1]);
                        let corners = [
                            rel,
                            rel + r3 * cw,
                            rel + r3 * cw + d3 * chh,
                            rel + d3 * chh,
                        ];
                        let pts: Vec<egui::Pos2> =
                            corners.iter().filter_map(|c| to_screen(*c)).collect();
                        if pts.len() == 4 {
                            let mut quad = [[0.0f32; 2]; 4];
                            for (i, p) in pts.iter().enumerate() {
                                quad[i] = [p.x / ppp - srect.min.x, p.y / ppp - srect.min.y];
                            }
                            self.ui_canvas.push(quad);
                        }
                    }
                }

                // Post runs BEFORE any retro upscale, at the scene's composited
                // resolution. SSAO reads whichever depth the scene rendered with;
                // in retro mode the chain outputs into the retro color target so
                // the nearest-neighbor blit carries the finished effects up with
                // the same chunky pixels as the scene.
                // Capture the composited scene into the UI backdrop BEFORE post
                // consumes it, so frosted-glass UI (`backdrop()`) works in
                // fullscreen/player. Retro mode is the one case with nothing to
                // capture at this size — its offscreen is the retro internal
                // resolution, not the frame's — so there the backdrop is cleared
                // and `backdrop()` reads black rather than a stale capture.
                //
                // What is captured is the scene BEFORE the tonemap, so anything
                // brighter than white clamps on the way into the (8-bit) backdrop
                // texture. Frosted glass is a blur of what is behind it, not a
                // measurement, so that is the right trade rather than a second
                // floating-point full-screen texture.
                if !ui_layers.is_empty()
                    && let Some(uir) = self.ui_render.as_mut()
                {
                    if !self.project.retro {
                        let mut enc = gpu.device.create_command_encoder(
                            &wgpu::CommandEncoderDescriptor { label: Some("ui-backdrop") },
                        );
                        uir.capture_backdrop(
                            gpu,
                            &mut enc,
                            post.input_view(),
                            gpu.config.width,
                            gpu.config.height,
                        );
                        gpu.queue.submit(Some(enc.finish()));
                    } else {
                        uir.clear_backdrop();
                    }
                }
                {
                    let proj = cam.proj_matrix(aspect);
                    let ssao_frame = floptle_render::SsaoFrame {
                        depth: if self.project.retro { retro.depth_view() } else { gpu.depth_view() },
                        proj: proj.to_cols_array_2d(),
                        inv_proj: proj.inverse().to_cols_array_2d(),
                    };
                    // In retro mode the chain writes the retro colour target and
                    // the nearest-neighbour blit carries the finished picture up;
                    // otherwise it writes the window.
                    let out = if self.project.retro { retro.color_view() } else { &frame.view };
                    // Focus-on-a-node is resolved HERE, against the camera this
                    // view is actually rendering from, so the Scene view shows
                    // its own focus while you fly around instead of the game
                    // camera's.
                    let mut ps = post_settings;
                    if let Some(d) =
                        crate::shading::dof_focus_distance(&self.world, cam.world_position)
                    {
                        ps.dof_focus = d;
                    }
                    // Motion blur is a GAME-view effect. The Scene view is a
                    // tool: you have to be able to place a prop while the
                    // camera is still coasting, and a viewport that smears
                    // whenever you orbit is a viewport you fight.
                    if game_view {
                        self.motion_prev = Some(crate::shading::motion_frame(
                            &mut ps,
                            self.motion_prev,
                            cam.view_proj(aspect),
                            cam.world_position,
                            if self.project.retro {
                                retro.resolution().1
                            } else {
                                gpu.config.height
                            },
                        ));
                    } else {
                        ps.motion_blur = 0.0;
                    }
                    gpu_mark!("post (AO, bloom, blur…)");
                    post.run_with(gpu, &ps, Some(&ssao_frame), out, post_shaders);
                }
                if self.project.retro {
                    if self.project.retro_integer_scale {
                        let dest =
                            [gpu.config.width as f32, gpu.config.height.max(1) as f32];
                        retro.blit_integer(gpu, &frame.view, dest);
                    } else {
                        retro.blit(gpu, &frame);
                    }
                }

                profile
                    .borrow_mut()
                    .record(floptle_core::profile::Bucket::Render, draw_t.ms());

                // ---- game UI: over the finished frame (native res), before
                // the editor's own chrome. One instanced pass per frame.
                if !ui_layers.is_empty()
                    && let Some(uir) = self.ui_render.as_mut()
                {
                    let vp = [gpu.config.width as f32, gpu.config.height as f32];
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
                                let shader =
                                    uic.get(p).and_then(|e| e.compiled.as_ref()).map(|(_, id)| *id)?;
                                Some((shader, uib.get(&owner)?.binding))
                            },
                            &mut ui_instances,
                            &mut ui_batches,
                        );
                    }
                    // Backdrop for frosted-glass UI was captured before post ran
                    // (post-on path); if there was no sampleable source it was
                    // cleared to black. Draw the UI over the finished frame.
                    uir.draw(gpu, &frame.view, vp, &ui_instances, &ui_batches, raster);
                }

                // Selection outline: mask the selected object's silhouette (full
                // frame res, so it stays crisp over the retro scene) then edge-detect
                // it onto the frame. Works for meshes and the SDF blob alike.
                let masked = if !mask_mesh.is_empty() {
                    raster.draw_mask(gpu, outline.mask_view(), globals, &mask_mesh, &mask_skins);
                    true
                } else if let Some(brm) = mask_blob {
                    raymarch.draw_mask(gpu, outline.mask_view(), brm);
                    true
                } else {
                    false
                };
                if masked {
                    outline.composite(gpu, &frame.view, [1.0, 1.0, 1.0, 1.0], 1.3);
                }

                // egui composited over the final frame
                let ppp = full_output.pixels_per_point;
                let tris = ctx.tessellate(full_output.shapes, ppp);
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [gpu.config.width, gpu.config.height],
                    pixels_per_point: ppp,
                };
                for (id, delta) in &full_output.textures_delta.set {
                    egui.renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
                }
                gpu_mark!("editor UI");
                let mut encoder = gpu
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("egui") });
                egui.renderer.update_buffers(&gpu.device, &gpu.queue, &mut encoder, &tris, &screen);
                {
                    let mut pass = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("egui"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &frame.view,
                                depth_slice: None,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Load,
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        })
                        .forget_lifetime();
                    egui.renderer.render(&mut pass, &tris, &screen);
                }
                gpu.queue.submit([encoder.finish()]);
                // ⏱ Close the frame BEFORE `present`: what happens after this is
                // the display's business, and the panel's job is to account for
                // the work the engine asked for.
                if timing && let Some(t) = gpu_timer.as_mut() {
                    t.end(gpu);
                }
                for id in &full_output.textures_delta.free {
                    egui.renderer.free_texture(id);
                }
                frame.present();
            }
            None => {
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
            }
        }

        if want_save_all {
            // Quit-time full save (scene + project + scripts), with its own toast.
            self.save_all();
            // An unnamed image opened its "save as" dialog instead of saving —
            // put the tab in front of it, or that dialog is behind whatever the
            // user was actually looking at.
            if self.image.save_name.is_some() {
                self.focus_image_tab();
            }
        } else if want_save || cmd.save_scene {
            self.save_scene();
        }
        if (want_save_project || cmd.save_project)
            && let Err(e) = floptle_scene::save_project(&self.project, &self.project_cfg_path()) {
                eprintln!("  save project failed: {e}");
            }
        // Edit ⏵ Project Settings opens (or focuses) the ⚙ Settings TAB — it
        // docks like anything else, so there is no modal to dismiss.
        if cmd.open_settings && let Some(d) = self.dock_state.as_mut() {
            crate::dock::focus_settings_tab(d);
        }
        if let Some(edits) = cmd.input_edits.take() {
            self.apply_input_edits(edits);
        }
        // The ⚙ Settings tab drives its own edits (it has `&mut self`); what
        // remains here is the per-frame upkeep it needs while VISIBLE: keep the
        // script scan fresh, and settle a press-to-bind that just landed.
        let settings_front = self
            .dock_state
            .as_ref()
            .is_some_and(|d| crate::dock::tab_is_front(d, crate::dock::EditorTab::Settings));
        if settings_front {
            let dir = self.project_root.join("scripts");
            let now = self.play_t.max(elapsed);
            self.input_scan.poll(&dir, now);
            // Auto-commit a capture — click +, press the input, done. Escape
            // always backs out, so an accidental arm is never a trap.
            let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            self.settle_pending_rebind(escape);
        }
        // A quit decision (Save & Quit / Discard & Quit): the save above has run, so leave
        // now — `about_to_wait` sees this and exits the winit loop.
        if want_exit {
            self.pending_exit = true;
        }

        self.apply_frame_commands(cmd, frame_pointer_down);
        // ---- what the packages asked for this frame ----
        // Menu items and shortcuts run their Lua HERE, not in the UI pass: a
        // callback may open a panel, edit the scene or reload the package it
        // belongs to, and none of that can happen while the host is drawing.
        if let Some(i) = ext_menu_click {
            self.ext.run_menu(i);
        }
        if let Some(i) = ext_shortcut_click {
            self.ext.run_shortcut(i);
        }
        self.apply_ext_commands();
        if let Some(dir) = pkg_action.open_folder {
            crate::project::open_in_file_manager(&dir);
        }
        if pkg_action.reload {
            self.ext_reload();
        }
        if self.ext.wants_repaint() {
            ctx.request_repaint();
        }
        // Collection on while the panel is shut can only be a script's doing, so
        // that is how ownership is known — no extra channel from Lua.
        self.perf_enabled_by_script =
            self.script_host.profile().borrow().enabled() && !self.show_perf_panel;
        // Opening ⏱ starts collecting; closing it stops. But a game that called
        // `perf.enable(true)` itself keeps it on — closing the panel must not
        // silently break the budget check a smoke test depends on.
        if let Some(on) = perf_toggle
            && (on || !self.perf_enabled_by_script)
        {
            self.script_host.profile().borrow_mut().enable(on);
        }

        // The frame is over: fold every bucket into its history (`floptle/0077`).
        // Once, at the very end, so a subsystem that reported in several pieces —
        // physics per tick, scripts per pass — contributes one figure per frame.
        // A no-op while collection is off.
        self.script_host.profile().borrow_mut().end_frame();
    }

    /// Add a subsystem's cost to this frame (`floptle/0077`).
    ///
    /// A one-line helper because the alternative is `self.script_host.profile()
    /// .borrow_mut().record(...)` at every measured site, and a borrow that long
    /// spelled out eight times is eight chances to hold it across something that
    /// also wants it.
    pub(crate) fn profile_record(&self, bucket: floptle_core::profile::Bucket, ms: f32) {
        self.script_host.profile().borrow_mut().record(bucket, ms);
    }

    /// Live syntax check for the active IDE file (drives the red squiggle):
    /// Lua through the script host, `.flsl` through the shader compiler.
    fn check_active_script_syntax(&mut self) {
        self.ide_diag = self.ide.active.and_then(|i| self.ide.open.get(i)).and_then(|f| {
            if f.path.ends_with(".lua") {
                self.script_host.check_syntax(&f.text)
            } else if crate::assets::is_shader(&f.path) {
                Editor::check_flsl_syntax(&f.text)
            } else {
                None
            }
        });
    }

    /// Per-frame GPU sync for SDF matter: upload structurally-changed terrain
    /// volumes + shadow-occluder bakes into the shared 3D atlas (or just the
    /// dabbed region on the fast sculpt path), and refresh the texture palette.
    pub(crate) fn sync_terrain_gpu(&mut self) {
        // Terrain volumes render PER-VOLUME, each at native resolution: moving a
        // terrain needs NO GPU work at all — its f64 anchor is read fresh every frame
        // when the globals are built. Only structural changes (add/edit/delete/resize)
        // re-upload the volume set into the shared 3D atlas. Static collider MESHES
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
                            Some(Matter::Terrain { id }) => *id,
                            _ => 0,
                        };
                        (id, e)
                    })
                    .collect();
                items.sort_by_key(|(id, _)| *id);
                let entities: Vec<Entity> = items.iter().map(|&(_, e)| e).collect();
                // Occluders upload AFTER the terrains (stable order by asset + name,
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
                self.terrain_wire_world.retain(|(we, _)| *we != e);
            }
        }
        // (see terrain_nearest_mask for the per-slot filter bits)
        // Re-upload the terrain texture palette when it changes. Each slot resolves
        // to a 256² layer (empty / unreadable slots become white so indices align).
        if self.terrain_textures_dirty {
            // Every slot is resampled to the palette's 256². Honour the texture's OWN
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
                // Meshed terrain (P2/P6) draws in the RASTER pass, so it needs its own copy
                // of the palette + the same per-slot nearest mask.
                if let Some(raster) = self.raster.as_mut() {
                    raster.set_terrain_palette(gpu, &layers, mask);
                }
            }
            self.terrain_textures_dirty = false;
        }
    }

    /// (Re)upload the skybox equirect when the Skybox node's texture changes.
    fn sync_sky_texture(&mut self) {
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
    fn sync_sky_shader(&mut self) {
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
        // project_root onto them gave assets/assets/… ENOENT, so NO picked sky
        // shader ever loaded (the Material-shader double-join bug, same fix).
        let abs = self.resolve_asset_path(&path);
        let mtime = std::fs::metadata(&abs)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        // Unchanged (same path + mtime) → nothing to do.
        if self.sky_shader.as_ref().is_some_and(|(p, mt, _)| *p == path && *mt == mtime) {
            return;
        }
        let Ok(src) = std::fs::read_to_string(&abs) else {
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
    fn sky_uniform_values(&self) -> [[f32; 4]; 16] {
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

    /// How many frame times the 1% low is taken over — about two seconds at
    /// 144 Hz, which is long enough for a periodic hitch to land in it and short
    /// enough that the number still tracks what the scene is doing now.
    const FRAME_LOG: usize = 512;

    /// Bank one frame time (milliseconds) for the 1% low.
    fn record_frame_time(&mut self, ms: f32) {
        if self.frame_log.len() != Self::FRAME_LOG {
            self.frame_log = vec![0.0; Self::FRAME_LOG];
            self.frame_log_at = 0;
            self.frame_log_len = 0;
        }
        self.frame_log[self.frame_log_at] = ms;
        self.frame_log_at = (self.frame_log_at + 1) % Self::FRAME_LOG;
        self.frame_log_len = (self.frame_log_len + 1).min(Self::FRAME_LOG);
    }

    /// The 1% low: the MEAN of the worst 1% of frame times in the log, ms.
    ///
    /// **The worst frames, reported as a time rather than as a rate.** "1% low
    /// fps" is the usual name, but the honest quantity is the frame time — it is
    /// what is being measured, it averages correctly, and inverting it invites
    /// exactly the reciprocal-of-a-mean error this readout was fixed for.
    ///
    /// The mean of the worst 1%, not the 99th percentile, and the difference is
    /// not pedantic: a hitch that happens on almost exactly 1% of frames puts
    /// the p99 index right at the boundary, so the single sample it lands on is
    /// as likely to be a good frame as a bad one and the readout blinks between
    /// 6.9 and 40. Averaging the tail reports the tail.
    fn frame_time_low(&self) -> f32 {
        if self.frame_log_len == 0 {
            return self.frame_ms;
        }
        let mut v: Vec<f32> = self.frame_log[..self.frame_log_len].to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // At least one sample, so a short log answers with its worst frame
        // rather than with nothing.
        let n = (((v.len() as f32) * 0.01).ceil() as usize).clamp(1, v.len());
        v[v.len() - n..].iter().sum::<f32>() / n as f32
    }

    /// Re-read the display's refresh period, without throwing away a good one.
    ///
    /// **A `None` from `current_monitor()` means "ask again", not "there is no
    /// display."** On Wayland it returns `None` from `create_window` until the
    /// surface is mapped, and `None` again on an output hotplug, a monitor
    /// change or a window drag. The old read mapped that straight onto `0.0`,
    /// and `period <= 0.0` is how dt snapping switches itself off — so the
    /// anti-jitter path `docs/subsystems/time.md` §10 calls load-bearing was
    /// inert for the whole of startup, and for the whole poll interval after
    /// every later hiccup, on a machine where `available_monitors()` was
    /// answering correctly the entire time (`floptle/0160`).
    fn reread_refresh_period(&mut self) {
        let Some(w) = self.window.as_ref() else { return };
        let hz = |m: &winit::monitor::MonitorHandle| m.refresh_rate_millihertz();
        let current = w.current_monitor().as_ref().and_then(hz);
        // Only asked for when there is nothing else to go on, so it is computed
        // lazily — enumerating monitors is not free and the common case never
        // needs it.
        let any = || w.primary_monitor().or_else(|| w.available_monitors().next()).as_ref().and_then(hz);
        self.refresh_period = chosen_refresh_period(self.refresh_period, current, any);
    }

    /// Put every node in `targets` on `layer`, as ONE undo step.
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
            // "Default" is the ABSENCE of the component, not a value of it —
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

    /// Frame-time smoothing: SNAP the measured dt to the nearest whole multiple
    /// of the display's refresh period when it's close. Under vsync (Fifo) a
    /// frame's true screen time IS a whole number of refresh periods — the
    /// CPU-side measurement just adds 1–3 ms of scheduler noise on top, and
    /// feeding that noise into the fixed-step accumulator moves everything the
    /// interpolation renders by `velocity × noise` every frame (the moving-
    /// jitter that came and went with window mode / load). The residual error
    /// is banked and folded back a hair per frame, so long-term time stays
    /// wall-clock exact (a fast clip on the bank absorbs one-off stalls).
    fn smooth_dt(&mut self, raw: f32) -> f32 {
        // (Re)read the monitor's refresh rate occasionally — cheap, and the
        // window can move between monitors.
        if self.refresh_poll == 0 {
            // 60 frames rather than 240. The poll interval is also the length of
            // every outage below, so a long one turned a momentary hiccup into
            // seconds of unsnapped dt.
            self.refresh_poll = 60;
            self.reread_refresh_period();
        }
        self.refresh_poll -= 1;
        let period = self.refresh_period;
        if period <= 0.0 || raw <= 0.0 {
            self.dt_snap_rate *= 0.99;
            return raw;
        }
        let n = (raw / period).round();
        // Snap only when the measurement is close to a whole vsync count (and
        // at least one) — a giant hitch or an uncapped frame passes through.
        if n < 1.0 || (raw - n * period).abs() > period * 0.12 {
            self.dt_snap_error = self.dt_snap_error.clamp(-period, period);
            // A miss is not a fault — an uncapped frame legitimately misses. A
            // long RUN of them means the snap is inert, which is a different
            // thing and worth being able to see (`floptle/0160`).
            self.dt_snap_rate *= 0.99;
            return raw;
        }
        self.dt_snap_rate = self.dt_snap_rate * 0.99 + 0.01;
        self.dt_snap_error += raw - n * period;
        // Fold the banked truth back in slowly (≤0.25 ms/frame): time stays
        // exact without re-introducing per-frame noise.
        let give = self.dt_snap_error.clamp(-0.00025, 0.00025);
        self.dt_snap_error -= give;
        n * period + give
    }

    /// Advance the frame clock: `dt`/`elapsed`, the editor fly-camera (unless
    /// the Game view owns input), the smoothed FPS title, and the F-key focus
    /// glide. Returns `(dt, elapsed)`.
    fn advance_clock(&mut self, game_focused: bool) -> (f32, f32) {
        let now = Instant::now();
        let raw_dt = self.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        let dt = self.smooth_dt(raw_dt);
        // This frame's time for UI style transitions. Handed to the RUNTIME
        // rather than to a pass, because several passes style the same tree in
        // a frame and each needs the real `dt`; `begin_frame` is what stops an
        // element charging it twice (see `Editor::ui_style_dt`).
        self.ui_style_dt = dt.min(0.25);
        self.ui_style_rt.begin_frame();
        // The ◫ UI tab's canvas runs its own style runtime (so a previewed
        // hover there can't fight the Game view's real one) and therefore needs
        // its own clock. Read, not drained: it renders exactly once a frame.
        self.ui_design_dt = dt.min(0.25);
        self.ui_frame_dt = dt.min(0.25);
        let elapsed = self.started.map(|s| (now - s).as_secs_f32()).unwrap_or(0.0);
        self.poll_ui_styles(elapsed);
        self.fog_time = elapsed; // drifts the volumetric-fog noise (offscreen views too)
        // Don't drive the editor (Scene) camera while the Game viewport is focused — that
        // input belongs to the game (e.g. the mouse is over the Game view in split mode).
        if !game_focused {
            self.camera.update(&self.input, dt);
            // Mouse wheel dollies the editor camera when hovering the Scene viewport.
            // Consumed here (before scripts / finish_input_frame): scripts only read
            // scroll while the Game view is focused, so this never steals game input.
            if self.input_scroll != 0.0 && self.cursor_over_scene() {
                self.camera.dolly(self.input_scroll);
                self.input_scroll = 0.0;
            }
        }

        // FPS in the window title (smoothed, refreshed a few times a second).
        if dt > 0.0 {
            // **Smooth the frame TIME and invert at display time.** Smoothing
            // `1.0 / dt` averages a reciprocal, which is biased toward the fast
            // frames — and the more bimodal the distribution, the more wildly it
            // flatters. See `Editor::fps` for the capture where it read 4312 fps
            // against a true 144 (`floptle/0160`).
            let ms = dt * 1000.0;
            self.frame_ms = if self.frame_ms > 0.0 { self.frame_ms * 0.9 + ms * 0.1 } else { ms };
            self.fps = 1000.0 / self.frame_ms.max(1e-4);
            self.record_frame_time(ms);
            self.fps_timer += dt;
            if self.fps_timer >= 0.4 {
                self.fps_timer = 0.0;
                if let Some(window) = self.window.as_ref() {
                    // The frame's OWN cost beside the rate, because they answer
                    // different questions and an fps number alone cannot tell
                    // "this scene is expensive" from "this display is pacing
                    // us". A scene costing 8 ms and presenting at 20 fps is the
                    // second, and used to be indistinguishable from the first.
                    let cost = (self.frame_ms - self.present_wait_ms).max(0.0);
                    // Reaches the Console too, not only the ⏱ panel and the
                    // title — the panel is opt-in and the title is easy not to
                    // read closely, and this is exactly the report a user who
                    // is NOT looking for it needs to see (`floptle/0169`).
                    match fifo_pacing_multiple(self.present_wait_ms, cost, self.refresh_period * 1000.0) {
                        Some(n) if n != self.fifo_pacing_warned => {
                            self.fifo_pacing_warned = n;
                            self.console.push(
                                floptle_script::LogLevel::Warn,
                                format!(
                                    "⏱ the DISPLAY is pacing this frame, not the scene: {:.1} ms \
                                     spent waiting on `acquire` every {n}th refresh, while the \
                                     scene itself costs {cost:.1} ms. Try Project Settings ⏵ \
                                     Rendering ⏵ Frame pacing.",
                                    self.present_wait_ms
                                ),
                                None,
                            );
                        }
                        None => self.fifo_pacing_warned = 0,
                        Some(_) => {} // same multiple as last time — already said
                    }
                    // The 1% low beside the mean, because a bimodal frame time
                    // is exactly the distribution that feels worst and the only
                    // one a mean cannot show. The ⏱ panel has always reported a
                    // worst column for this reason; the title bar is what people
                    // actually read, and it used to disagree with it.
                    self.frame_low_ms = self.frame_time_low();
                    let low = self.frame_low_ms;
                    window.set_title(&format!(
                        "Floptle Editor — {}{} — {:.0} fps ({:.1} ms/frame, 1% low {:.1}, cost {:.1}) — {} nodes ({} off screen), {} instances",
                        self.scene_name,
                        if self.scene_dirty { " •" } else { "" },
                        self.fps,
                        self.frame_ms,
                        low,
                        cost,
                        self.render_counts.nodes,
                        self.render_counts.culled,
                        self.render_counts.instances
                    ));
                }
            }
        }

        // Glide an in-progress focus (F). Any WASD/Space/C input hands control back
        // to the user immediately. Only the camera position eases; the view angle is
        // left to mouse-look, so you can look around mid-glide.
        if self.focus_anim.is_some() {
            let moving = self.input.forward
                || self.input.back
                || self.input.left
                || self.input.right
                || self.input.up
                || self.input.down;
            if moving {
                self.focus_anim = None;
            } else {
                let (from, to, t) = {
                    let a = self.focus_anim.as_mut().unwrap();
                    a.t += dt;
                    (a.from, a.to, a.t)
                };
                let k = (t / FOCUS_SECS).clamp(0.0, 1.0);
                let eased = 1.0 - (1.0 - k).powi(3); // ease-out cubic
                self.camera.position = from.lerp(to, eased as f64);
                if k >= 1.0 {
                    self.focus_anim = None;
                }
            }
        }
        (dt, elapsed)
    }

    /// One play-mode step (ordering: scripts → animation → physics): feed body
    /// state / input / assets / animator info to the script host, run the Lua
    /// scripts, apply their writes (models, mouse lock, velocities, heights),
    /// advance the animators, then step the sim. Clears stale script errors
    /// when not playing.
    pub(crate) fn play_step(&mut self, dt: f32, game_focused: bool) {
        // Play mode: advance the (pausable) script clock and run the Lua scripts
        // attached to nodes (ADR-0003). Scripts hot-reload as their files change.
        if self.playing {
            // A scene transition a script queued LAST frame happens first —
            // at a frame boundary, never mid-frame under the scripts that
            // asked for it (offline/host = switch; joined client = refused).
            for req in std::mem::take(&mut self.pending_scene) {
                let swap = req.is_swap();
                self.perform_scene_request(&req);
                // A full swap ends the queue: the requests behind it named the
                // world that no longer exists, and the new scene's own `start`
                // is the right place to ask for anything else.
                if swap {
                    break;
                }
            }
            // Pausing freezes the clock AND the frame delta scripts see, so
            // dt-driven motion stops too (not just `time`-driven motion).
            let sdt = if self.paused { 0.0 } else { dt };
            self.play_t += sdt;
            // Direct field access (not the `scripts_dir()` method) so we don't take
            // a whole-`self` borrow while gpu/egui are mutably borrowed here.
            let dir = self.project_root.join("scripts");
            // Feed the physics body state to scripts so they can read node.grounded and
            // read/write node.vx/vy/vz (a script sets velocity, physics then integrates).
            if let Some(sim) = self.sim.as_ref() {
                let mut states = HashMap::new();
                for r in sim.body_states() {
                    states.insert(r.entity.index(), crate::play::body_state(&r));
                }
                for (eid, vel, up, grounded, pos) in sim.compound_states() {
                    states.insert(
                        eid,
                        floptle_script::BodyState {
                            vel: [vel.x, vel.y, vel.z],
                            up: [up.x, up.y, up.z],
                            grounded,
                            height: 0.0,
                            pos: [pos.x, pos.y, pos.z],
                            // Compounds resolve contacts per SHAPE with real
                            // impulses; "the floor under it" isn't one normal.
                            ground_normal: None,
                            wall_normal: None,
                        },
                    );
                }
                self.script_host.set_bodies(states);
            }
            // The active camera's view angles ride every input snapshot
            // (`input.aimYaw()`): camera-relative movement stays deterministic
            // under prediction because the aim IS part of the input command.
            let aim = floptle_core::active_camera(&self.world).map(|e| {
                let wt = floptle_core::world_transform(&self.world, e);
                let (yaw, pitch, _) = wt.rotation.to_euler(floptle_core::math::EulerRot::YXZ);
                [yaw, pitch]
            });
            // Repeaters first: a list whose count changed last frame gets its
            // rows NOW, so this frame's layout, hit-testing and hooks all see
            // the same set of rows the player is looking at.
            let ui_t = floptle_core::profile::Span::new();
            self.ui_repeaters();
            // Game-UI interaction (buttons + draggable sliders): detect hover/press/
            // click against this frame's layout BEFORE scripts run, so a dragged
            // slider's value is already in the ECS when `update` reads it. The hook
            // events dispatch to Lua right after the run.
            self.ui_interact();
            // GAME UI: repeater expansion, the layout solve and hit-testing.
            self.profile_record(floptle_core::profile::Bucket::Ui, ui_t.ms());
            // Feed the player input to scripts (the Lua `input` API) — but ONLY while the
            // Game view is focused. In the Scene view you're editing, not playing, so the
            // game gets neutral input (the character stops moving) even though physics
            // keeps simulating.
            // …and while the EDITOR is holding the pointer (Escape, with the
            // game still asking for it), the mouse half of that input is the
            // editor's. Freeing the cursor would otherwise be half a fix: the
            // camera script keeps reading raw motion, so the view spins the
            // whole way over to the Inspector and every click on it also
            // reaches the game. Keys keep flowing — the game is still playing.
            let mouse_is_the_editors = self.cursor_freed;
            let frame_input = if game_focused {
                floptle_script::InputSnapshot {
                    keys_down: self.input_keys.clone(),
                    keys_pressed: self.input_keys_pressed.clone(),
                    keys_released: self.input_keys_released.clone(),
                    // Whatever a focused text field did not eat.
                    typed: self.input_typed.clone(),
                    mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                    mouse_delta: if mouse_is_the_editors {
                        (0.0, 0.0)
                    } else {
                        self.input_mouse_delta
                    },
                    scroll: if mouse_is_the_editors { 0.0 } else { self.input_scroll },
                    buttons_down: if mouse_is_the_editors {
                        [false; 3]
                    } else {
                        self.input_buttons
                    },
                    buttons_pressed: if mouse_is_the_editors {
                        [false; 3]
                    } else {
                        self.input_buttons_pressed
                    },
                    aim,
                }
            } else {
                floptle_script::InputSnapshot { aim, ..Default::default() }
            };
            self.script_host.set_input(frame_input.clone());
            // …and the ACTION layer's frame domain, off the same devices and
            // the same focus rule, so `input.action(...)` and `input.key(...)`
            // agree about whether the game is being played this frame.
            self.resolve_frame_actions(sdt, game_focused);
            // Lend the sim's colliders to scripts so `raycast(...)` works this frame
            // (physics doesn't step until after scripts, so this is safe). The sim
            // origin rides along so ray coordinates convert world ↔ sim frame.
            if let Some(sim) = self.sim.as_mut() {
                self.script_host
                    .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
            }
            // …and the dynamic bodies' hulls (copies), so rays can hit
            // players/crates and identify the node (`hit.node`).
            if let Some(sim) = self.sim.as_ref() {
                self.script_host.set_hulls(sim.body_hulls(&self.world));
            }
            // Lend the asset root (for `assets.getFile/getContents`) and the material
            // presets (so `node.material = "Gold"` resolves) for this frame's scripts.
            self.script_host.set_project_root(self.project_root.clone());
            // The running scene's name, for `scene.current()`.
            self.script_host.set_scene_name(&self.scene_name);
            // The scene's bodies of water, in WORLD coordinates — `water.depthAt`
            // answers the same question the solver does, from the same geometry,
            // so a swim state can never disagree with the physics floating it.
            self.script_host.set_water_volumes(crate::shading::water_infos(&self.world));
            self.script_host.set_materials(
                self.materials.iter().map(|(n, d)| (n.clone(), d.to_material())).collect(),
            );
            // Feed each animator's state (layers/current/time) so scripts can read
            // anim:state()/:time()/:clips() this frame.
            self.script_host.set_anim_info(anim::build_info(&self.anim));
            // Feed each particle node's state so scripts can read
            // node:particles():isPlaying()/:alive() this frame.
            self.script_host.set_vfx_info(self.vfx.script_info(&self.world));
            // Feed sound playback state so scripts can read sound:isPlaying()/
            // :position() this frame.
            self.script_host.set_audio_info(self.audio.script_info());
            // Feed each assembly's live compound state (`assembly.info`).
            self.feed_assembly_info();
            self.script_host.run(&mut self.world, &dir, sdt, self.play_t);
            // UI hook events (clicked / hoverStart / …) fire against the run's
            // fresh scene mirror, with their own write flush.
            let ui_events = std::mem::take(&mut self.ui_events);
            self.script_host.run_ui_hooks(&mut self.world, &ui_events);
            self.script_errors = self.script_host.errors().to_vec();
            // Apply any mouse lock/unlock a script requested this frame (grab + hide the
            // cursor for free-look, or release it). The state persists until changed/Stop.
            // DEDUPED against the current state: shipped camera scripts call
            // setMouseLocked every frame from update(), and re-issuing the OS grab at
            // frame rate tears down/recreates the pointer lock each time (on Wayland
            // that reads as a flickering, uncontrollable cursor).
            if let Some(want) = self.script_host.take_mouse_lock() {
                // An explicit UNLOCK is a game saying "the pointer is mine now",
                // and it has to release the editor's click-to-play trap too —
                // that is a second, invisible lock owner the game has no way to
                // reach. Deduping it against `script_mouse_lock` would drop the
                // call entirely in the case that matters, because a game opening
                // a menu never locked the mouse in the first place.
                let freed_trap = !want && std::mem::take(&mut self.game_trap);
                // …and it ends any editor override, because there is nothing
                // left to override: the game and the editor now agree that the
                // pointer is loose, and a game that opens its own menu two
                // minutes after you pressed Escape must get its clicks.
                if !want {
                    self.cursor_freed = false;
                }
                if want != self.script_mouse_lock {
                    self.script_mouse_lock = want;
                    // While the editor is holding the pointer, the game's wish
                    // is RECORDED and not applied — it lands the moment you
                    // click back into the Game view. This is the whole reason
                    // Escape works against a camera script that re-locks every
                    // frame: the re-lock is a no-op until you say so.
                    if !self.cursor_freed
                        && let Some(window) = self.window.as_ref()
                    {
                        self.cursor_lock_soft = grab_cursor(window, want);
                    }
                } else if freed_trap
                    && let Some(window) = self.window.as_ref()
                {
                    // Nothing changed as far as the script flag goes, but the
                    // trap was the one holding the OS grab — let it go.
                    self.cursor_lock_soft = grab_cursor(window, false);
                }
            }
            // A `scene.load(...)` from this frame's scripts: queued, performed
            // at the top of the next frame (see above).
            // `ui.focus(node)` from a script. Applied after the run so the
            // hooks fire on the next frame's pass, in the same place engine-
            // driven focus changes fire them — one code path, one ordering.
            if let Some(want) = self.script_host.take_ui_focus_request() {
                self.ui_focus_set(want);
            }
            self.pending_scene.extend(self.script_host.take_scene_requests());
            // Accessibility (`floptle/0079`): the settings a game's options menu
            // wrote this frame come back OUT, and the captions it asked for join
            // the on-screen queue. Read after the run so a menu that changes text
            // scale is honoured by the very next layout.
            self.access = self.script_host.access();
            // Captions age out on their own — a line nobody removed is a line
            // covering the game.
            for c in &mut self.captions {
                c.1 -= sdt;
            }
            self.captions.retain(|c| c.1 > 0.0);
            for c in self.script_host.take_captions() {
                // Newest last, and a modest cap: captions are read in order, and a
                // game spamming them would otherwise cover its own screen.
                self.captions.push((c.text, c.seconds));
                if self.captions.len() > 4 {
                    self.captions.remove(0);
                }
            }
            // `water.setFrozen(node, on)` — freezing is a STATE, so it lands on
            // the node and the physics field is rebuilt from it. The rebuild
            // preserves live velocities, so a sea freezing under a swimmer does
            // not fling them.
            let freezes = self.script_host.take_water_freezes();
            if !freezes.is_empty() {
                let mut changed = false;
                for (eid, on) in freezes {
                    let Some(e) =
                        self.world.entity_with::<Matter>(eid)
                    else {
                        continue;
                    };
                    match self.world.get_mut::<Matter>(e) {
                        Some(Matter::WaterVolume { frozen, .. }) => {
                            changed |= *frozen != on;
                            *frozen = on;
                        }
                        _ => self.console.push(
                            floptle_script::LogLevel::Warn,
                            "water.setFrozen: that node is not a Water Volume".into(),
                            None,
                        ),
                    }
                }
                if changed {
                    self.rebuild_sim();
                }
            }
            // GPU-load any models a script swapped via `node.model` (the Matter is
            // already updated by run; re-importing here means the new mesh renders
            // THIS frame).
            self.load_script_swapped_models();
            // `physics.step([n])` from a script — the same frame-stepper as ⏭. Drained
            // in the FRAME pass, not inside the tick loop: once the tick is frozen that
            // loop doesn't run, so a request drained there could never be the thing that
            // unfreezes it. And before animation, so the step it releases advances the
            // pose on the same frame as the gameplay tick rather than one behind.
            let steps = self.script_host.take_frame_steps();
            if steps > 0 {
                self.step_tick(steps);
            }
            // Animation: bind + apply queued Lua animator commands + advance every
            // controller (ordering: scripts → animation → physics), then dispatch
            // fired clip events back into the node's scripts.
            let anim_cmds = self.script_host.take_anim_commands();
            // A frame-step advances animation by exactly the ticks it is about to
            // release (the tick loop consumes `tick_steps` further down), so the pose
            // you are looking at belongs to the gameplay frame you stopped on. Paused
            // with no step pending, `sdt` is already 0.
            let anim_dt = if self.paused {
                self.tick_steps as f32 * self.game_tick.step
            } else {
                sdt
            };
            let anim_t = floptle_core::profile::Span::new();
            let fired = anim::advance_animators(
                &mut self.anim,
                &mut self.world,
                &self.mesh_registry,
                anim_dt,
                anim_cmds,
            );
            // ANIMATION: clip sampling, blending, pose composition and CPU
            // skinning. The number `floptle/0080` needs before and after.
            self.profile_record(floptle_core::profile::Bucket::Animation, anim_t.ms());
            for (eid, func) in fired {
                self.script_host.call_function(&mut self.world, eid, &func);
            }
            // Animator warnings (e.g. play() on a state name the controller
            // doesn't have) surface in the Console, once per name.
            for msg in self.anim.warnings.drain(..) {
                self.console.push(floptle_script::LogLevel::Warn, msg, None);
            }
            // Event handlers can log/raise — surface those in the Scripting tab
            // (run() cleared + snapshotted errors before the dispatch above).
            if !self.script_host.errors().is_empty() {
                self.script_errors = self.script_host.errors().to_vec();
            }
            // Apply script velocity writes, then run the GAMEPLAY TICK loop (docs/
            // netcode-design.md §3): each banked 60 Hz tick runs `fixedUpdate` with a
            // per-tick input snapshot, applies its writes, and steps physics exactly one
            // tick — the deterministic unit netcode snapshots/prediction share. Rendered
            // transforms interpolate across the current tick (anti-stutter). Gravity is
            // rebuilt from the scene's GravityVolume node(s) every frame (cheap scan) so
            // tweaking mode/strength/radius takes effect immediately. The active camera
            // is the floating-origin focus: drift far enough and the sim recenters on it.
            let focus = floptle_core::active_camera(&self.world)
                .map(|e| floptle_core::world_transform(&self.world, e).translation);
            if let Some(sim) = self.sim.as_mut() {
                sim.world.gravity = Self::build_gravity_field(&self.world, sim.world.origin);
                // Water is rebuilt every frame for the same reason gravity is
                // (`floptle/0141`): a WaterVolume spawned, moved, resized,
                // disabled or destroyed while the game is running must be in
                // the solver's field the same frame it is in the renderer's —
                // `water_draw` already gathers from the live world every
                // frame, so the two were disagreeing about *when* a pool
                // exists, not about what it is. Same cost shape as gravity's
                // scan (cheap on a level of a few thousand nodes); "static
                // per step" (the doc comment on `PhysicsWorld::water`) is a
                // claim about determinism within one tick, not about being
                // built once per session.
                sim.world.water = Self::build_water_field(&self.world, sim.world.origin);
                sim.world.set_colliders(self.script_host.take_colliders()); // reclaim before stepping
                // Live Inspector edits: re-read RigidBody tunables (shape/size, friction,
                // restitution, gravity, pos/rot locks) into the running bodies each frame —
                // no teleport.
                sim.sync_dynamic_params(&self.world);
                // `update`'s velocity/height writes apply before the first tick, so
                // frame-pass controllers (the pre-fixedUpdate style) behave as before.
                for (eid, v) in self.script_host.take_body_changes() {
                    sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                }
                for (eid, h) in self.script_host.take_body_height_changes() {
                    sim.set_body_height(eid, h);
                }
                for (eid, p) in self.script_host.take_body_pos_changes() {
                    sim.set_body_position(eid, DVec3::new(p[0], p[1], p[2]));
                }
            }
            // Assembly commands from the frame pass (`assembly.forceAt` in
            // `update`, splits from UI handlers): forces arm the coming ticks,
            // splits happen now.
            self.drain_assembly_cmds();
            // Terrain edits queued by the frame pass (`terrain.sculpt/dig/...`):
            // applied to the authority field + the sim's collider copy before any
            // tick steps, so physics never disagrees with the surface.
            self.drain_script_terrain_ops();
            if self.sim.is_some() {
                self.game_tick.accumulate(sdt);
                // Frame-step. While frozen the clock banks nothing (so unpausing can
                // never release a burst of caught-up ticks) and `tick_steps` is the only
                // thing that lets a tick through. Draining the accumulator also drops
                // `alpha` to 0, so what you look at between steps is the tick pose
                // itself, not an interpolated one — which is the whole point of
                // stopping on a frame.
                let stepping = self.paused;
                if stepping {
                    self.game_tick.reset();
                } else {
                    // Steps queued while running are meaningless — never let them bank.
                    self.tick_steps = 0;
                }
                loop {
                    if stepping {
                        if self.tick_steps == 0 {
                            break;
                        }
                        self.tick_steps -= 1;
                    } else if !self.game_tick.tick() {
                        break;
                    }
                    self.game_tick_no += 1;
                    // Celestial rails FIRST (solar demo S2): body nodes + their
                    // terrain collider anchors + gravity centers + the space.*
                    // snapshot all reflect THIS tick before scripts and physics.
                    self.update_space_rails(self.game_tick.step as f64);
                    // Per-tick input: consume the tick accumulators (edges bank between
                    // ticks so a between-tick press is never lost). Neutral when the
                    // Game view isn't focused — but still consumed, so stale edges
                    // don't fire on refocus.
                    let snap = if game_focused {
                        // The accumulators are DRAINED either way — while the
                        // editor holds the pointer the mouse half is dropped
                        // rather than banked, so nothing fires in a burst the
                        // moment you hand the cursor back.
                        let (dx, dy) = std::mem::take(&mut self.tick_mouse_delta);
                        let wheel = std::mem::take(&mut self.tick_scroll);
                        let pressed = std::mem::take(&mut self.tick_buttons_pressed);
                        let mine = self.cursor_freed;
                        floptle_script::InputSnapshot {
                            keys_down: self.input_keys.clone(),
                            keys_pressed: std::mem::take(&mut self.tick_keys_pressed),
                            keys_released: std::mem::take(&mut self.tick_keys_released),
                            typed: std::mem::take(&mut self.tick_typed),
                            mouse: self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0)),
                            mouse_delta: if mine { (0.0, 0.0) } else { (dx, dy) },
                            scroll: if mine { 0.0 } else { wheel },
                            buttons_down: if mine { [false; 3] } else { self.input_buttons },
                            buttons_pressed: if mine { [false; 3] } else { pressed },
                            aim,
                        }
                    } else {
                        self.tick_keys_pressed.clear();
                        self.tick_keys_released.clear();
                        self.tick_typed.clear();
                        self.tick_mouse_delta = (0.0, 0.0);
                        self.tick_scroll = 0.0;
                        self.tick_buttons_pressed = [false; 3];
                        floptle_script::InputSnapshot { aim, ..Default::default() }
                    };
                    // Keep what the scripts saw: prediction records + ships it.
                    self.last_tick_input = snap.clone();
                    self.script_host.set_input(snap);
                    // The action layer's tick domain — the one with input
                    // history, so motions and buffers advance exactly once per
                    // tick regardless of framerate. Drains the banked edges.
                    //
                    // A ROLLBACK session owns that domain instead: every peer's
                    // input, including ours, is written into its slot at its
                    // APPLIED tick and history advances exactly once from
                    // there. Resolving devices here as well would advance it a
                    // second time and halve every motion window on the local
                    // player only (see `InputSystem::sample_tick`). The driver
                    // also runs its fighters' hooks and steps their bodies —
                    // the rest of this tick then runs for everything else.
                    if self.net_rollback.is_some() {
                        self.net_rollback_tick(game_focused);
                    } else {
                        self.resolve_tick_actions(self.game_tick.step, game_focused);
                    }
                    if let Some(sim) = self.sim.as_mut() {
                        // Fresh body state for THIS tick (post previous tick's physics).
                        let mut states = HashMap::new();
                        for r in sim.body_states() {
                            states.insert(r.entity.index(), crate::play::body_state(&r));
                        }
                        for (eid, vel, up, grounded, pos) in sim.compound_states() {
                            states.insert(
                                eid,
                                floptle_script::BodyState {
                                    vel: [vel.x, vel.y, vel.z],
                                    up: [up.x, up.y, up.z],
                                    grounded,
                                    height: 0.0,
                                    pos: [pos.x, pos.y, pos.z],
                                    ground_normal: None,
                                    wall_normal: None,
                                },
                            );
                        }
                        self.script_host.set_bodies(states);
                        // Lend colliders so `raycast(...)` works inside `fixedUpdate` too.
                        self.script_host.set_colliders(
                            std::mem::take(&mut sim.world.colliders),
                            sim.world.origin,
                        );
                        self.script_host.set_hulls(sim.body_hulls(&self.world));
                    }
                    // `time` on the fixed pass is the deterministic tick clock.
                    let tick_time = self.game_tick_no as f32 * self.game_tick.step;
                    // Real hosting: each REMOTE player's Predicted node runs
                    // with ITS OWNER's replayed input for this tick — the
                    // one-script model (§6), server side. Those nodes are
                    // filtered out of the global passes; run_*_for bypasses
                    // the filters. The host's own input is restored after.
                    if !self.net_remote_predicted.is_empty() && self.net_server.is_some() {
                        if let Some(s) = self.net_server.as_mut() {
                            // Tick-start pump so this tick's freshest client
                            // inputs are in the buffer before scripts consume.
                            s.pump_server(&self.world, self.game_tick_no);
                        }
                        for (e, owner) in self.net_remote_predicted.clone() {
                            let Some(s) = self.net_server.as_mut() else { break };
                            let inp = s.input_for(owner, self.game_tick_no);
                            crate::input_actions::apply_net_input_to(&self.script_host, &inp);
                            self.script_host.run_frame_for(
                                &mut self.world,
                                e.index(),
                                self.game_tick.step,
                                tick_time,
                            );
                            self.script_host.run_fixed_for(
                                &mut self.world,
                                e.index(),
                                self.game_tick.step,
                                tick_time,
                            );
                        }
                        self.script_host.set_input(self.last_tick_input.clone());
                    }
                    // A predicted node's `update` rides the tick clock (its
                    // frame pass is filtered) so client + server integrate the
                    // same controller identically — see net.rs.
                    if let Some((pe, _)) = &self.net_predictor {
                        let pe = pe.index();
                        self.script_host.run_frame_for(
                            &mut self.world,
                            pe,
                            self.game_tick.step,
                            tick_time,
                        );
                    }
                    self.feed_assembly_info();
                    self.script_host.run_fixed(&mut self.world, self.game_tick.step, tick_time);
                    if let Some(sim) = self.sim.as_mut() {
                        sim.world.set_colliders(self.script_host.take_colliders()); // reclaim
                        // Apply the tick's writes, then step physics exactly one tick.
                        sim.sync_dynamic_params(&self.world);
                        for (eid, v) in self.script_host.take_body_changes() {
                            sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                        }
                        for (eid, h) in self.script_host.take_body_height_changes() {
                            sim.set_body_height(eid, h);
                        }
                        for (eid, p) in self.script_host.take_body_pos_changes() {
                            sim.set_body_position(eid, DVec3::new(p[0], p[1], p[2]));
                        }
                    }
                    // `fixedUpdate`'s assembly thrust arms THIS tick's substeps.
                    self.drain_assembly_cmds();
                    // This tick's terrain edits (`fixedUpdate` digs) land BEFORE the
                    // step: the tick that dug the hole also falls into it.
                    self.drain_script_terrain_ops();
                    // Bound crash loss on `save.*` data: flush every ~5 s of ticks
                    // (a clean no-op while the store is unchanged).
                    if self.game_tick_no.is_multiple_of(300) {
                        self.script_host.flush_save();
                    }
                    // `physics.pause(on)` gates the WHOLE physics step (scripts,
                    // rails and streaming keep running — loading screens hold
                    // the world still while it assembles). Queued held forces
                    // are dropped, not banked: unpausing must not fire a burst
                    // of accumulated thrust.
                    if let Some(on) = self.script_host.take_physics_pause_request() {
                        self.physics_paused = on;
                        self.script_host.set_physics_paused(on);
                    }
                    if let Some(sim) = self.sim.as_mut() {
                        if self.physics_paused {
                            sim.clear_held_forces();
                        } else {
                            // PHYSICS (`floptle/0077`). Timed per TICK and
                            // accumulated, because a frame can run several — a
                            // per-frame timer would report the last tick and hide
                            // a catch-up frame, which is exactly the spike a game
                            // notices.
                            let t = floptle_core::profile::Span::new();
                            sim.step_tick(self.game_tick.step, focus);
                            let ms = t.ms();
                            self.script_host
                                .profile()
                                .borrow_mut()
                                .record(floptle_core::profile::Bucket::Physics, ms);
                        }
                    }
                    // Collision / trigger events from THIS tick, dispatched to
                    // BOTH nodes' scripts: `onCollisionEnter/Stay/Exit(node,
                    // other, hit)` for solid contacts (incl. body-vs-body),
                    // `onTriggerEnter/Stay/Exit` when a Trigger collider is
                    // involved. Events fire where physics runs (offline, the
                    // server, a predicted owner) — never during replays.
                    let touches =
                        self.sim.as_mut().map(|s| s.take_touch_events()).unwrap_or_default();
                    for ev in touches {
                        use floptle_physics::TouchPhase;
                        let func = match (ev.sensor, ev.phase) {
                            (true, TouchPhase::Enter) => "onTriggerEnter",
                            (true, TouchPhase::Stay) => "onTriggerStay",
                            (true, TouchPhase::Exit) => "onTriggerExit",
                            (false, TouchPhase::Enter) => "onCollisionEnter",
                            (false, TouchPhase::Stay) => "onCollisionStay",
                            (false, TouchPhase::Exit) => "onCollisionExit",
                        };
                        let p = [ev.point.x, ev.point.y, ev.point.z];
                        let n = [ev.normal.x, ev.normal.y, ev.normal.z];
                        self.script_host.call_touch(&mut self.world, ev.a, func, ev.b, p, n);
                        self.script_host.call_touch(&mut self.world, ev.b, func, ev.a, p, n);
                    }
                    // A handler's body writes (knockback, bounce) land THIS
                    // tick, not the next one.
                    if let Some(sim) = self.sim.as_mut() {
                        for (eid, v) in self.script_host.take_body_changes() {
                            sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                        }
                    }
                    // Netcode rides the tick (docs/netcode-design.md §9): session
                    // commands, server snapshot send, ghost-client apply, RPC/event
                    // dispatch — all after physics, all on the deterministic clock.
                    self.net_tick(self.game_tick_no);
                }
                if let Some(sim) = self.sim.as_mut() {
                    // Render this frame partway into the current tick: smooth at any fps.
                    sim.writeback_interpolated(&mut self.world, self.game_tick.alpha());
                }
                // Prediction corrections render as a decaying nudge, not a snap:
                // the rendered transform carries the (shrinking) error offset.
                if let Some((pe, pred)) = &self.net_predictor
                    && pred.error_offset != [0.0; 3]
                    && let Some(tr) = self.world.get_mut::<Transform>(*pe)
                {
                    tr.translation +=
                        floptle_core::math::DVec3::from_array(pred.error_offset);
                }
            }
            // `lateUpdate` — the CAMERA pass: after physics and the interpolated
            // writeback, so followers sample this frame's FINAL poses. (A camera
            // positioned in `update` reads LAST frame's pose — a follow error of
            // velocity × dt that turns frame-time noise into visible jitter.)
            // The tick loop overwrote the input snapshot with per-tick state —
            // restore the FRAME snapshot first, so mouse/scroll reads in
            // lateUpdate see this frame's input, not the last tick's leftovers.
            self.script_host.set_input(frame_input);
            // Re-lend the sim's state for the late pass: the tick loop reclaimed
            // the colliders before stepping, so without this an orbit camera's
            // wall raycast would see NO static geometry. Hulls and body state are
            // refreshed too — post-step, so `raycast` hits bodies where they
            // rendered and `node.vx/grounded` reads this frame's final values.
            if let Some(sim) = self.sim.as_mut() {
                let mut states = HashMap::new();
                for r in sim.body_states() {
                    states.insert(r.entity.index(), crate::play::body_state(&r));
                }
                // Compound roots read like bodies too (node.vx / up_x /
                // grounded on a vessel) — before the collider hand-off, since
                // their gravity-up needs the collider set.
                for (eid, vel, up, grounded, pos) in sim.compound_states() {
                    states.insert(
                        eid,
                        floptle_script::BodyState {
                            vel: [vel.x, vel.y, vel.z],
                            up: [up.x, up.y, up.z],
                            grounded,
                            height: 0.0,
                            pos: [pos.x, pos.y, pos.z],
                            // Compounds resolve contacts per SHAPE with real
                            // impulses; "the floor under it" isn't one normal.
                            ground_normal: None,
                            wall_normal: None,
                        },
                    );
                }
                self.script_host.set_bodies(states);
                self.script_host
                    .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
            }
            if let Some(sim) = self.sim.as_ref() {
                self.script_host.set_hulls(sim.body_hulls(&self.world));
            }
            self.script_host.run_late(&mut self.world, sdt, self.play_t);
            if let Some(sim) = self.sim.as_mut() {
                sim.world.set_colliders(self.script_host.take_colliders()); // reclaim
                // A velocity write from lateUpdate still lands (applied next
                // step) — but the camera pass shouldn't steer bodies; drain so
                // nothing double-applies with next frame's `update` writes.
                for (eid, v) in self.script_host.take_body_changes() {
                    sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
                }
            }
            // Surface fixedUpdate errors alongside the frame pass's.
            if !self.script_host.errors().is_empty() {
                self.script_errors = self.script_host.errors().to_vec();
            }
            // Immediate-mode 3D lines queued this frame — by `update`, `fixedUpdate`
            // AND `lateUpdate` — drained once per frame, REPLACING the list (an
            // idle script clears its lines). Drained here, after the late pass,
            // so a camera-pass drawer (the solar map) lands the SAME frame as the
            // camera it positioned — draining per tick left the lines a frame
            // behind an interpolated camera.
            self.script_lines = self.script_host.take_draw_lines();
            self.script_tris = self.script_host.take_draw_tris();
            self.script_rects = self.script_host.take_draw_rects();
            self.script_texts = self.script_host.take_draw_texts();
            // Script debug gizmos queued this frame — by `update` AND `fixedUpdate` —
            // drained once here (drawn by the viewport overlay), plus the multiplayer
            // harness's ghost-client markers.
            self.script_gizmos = self.script_host.take_gizmos();
            self.net_ghost_gizmos();
            // Prefab spawns + node destroys scripts queued this frame — applied
            // before attachments/particles so a spawned node is complete (body,
            // meshes, callback-configured) within this same frame.
            self.apply_script_spawns();
            // Scatter prototypes: resolved here, before the frame's GPU borrow,
            // because baking a prefab imports models and that needs `&mut self`.
            self.bake_scatter_prototypes();
            // Bone attachments resolve AFTER physics: physics moves the mesh ROOT (a
            // character body), while animation only bent the bones — so a weapon on a
            // bone must read the POST-physics mesh world or it swims a frame behind.
            anim::resolve_attachments(&self.anim, &mut self.world, &self.mesh_registry);
            // 2D cameras follow AFTER all of that, for the same reason bone
            // attachments do: a camera chasing a player has to read where the
            // player ended up this frame, not where they started it.
            floptle_core::camera2d::step_all(&mut self.world, sdt, self.play_t as f64);
            // Particles tick last: emitter node transforms are final for the frame
            // (scripts → animation → physics → attachments → particles). Apply any
            // play/stop/restart a script queued this frame first, so it lands now.
            // `floptle/0115`: everything from here to the end of `advance` is the
            // particles bucket. It had no producer at all, so `perf.ms("particles")`
            // answered a confident 0.0 while collection was ON — which reads as
            // "particles are free", the one answer a profiler must never give by
            // accident.
            let vfx_t = floptle_core::profile::Span::new();
            let vfx_cmds = self.script_host.take_vfx_commands();
            self.vfx.apply_script_commands(&self.world, vfx_cmds);
            // Fire-and-forget one-shots a script requested this frame (spawnEffect).
            for (key, p, v) in self.script_host.take_spawn_effects() {
                let vel = floptle_core::math::Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32);
                self.vfx.spawn_detached(&key, floptle_core::math::DVec3::from_array(p), vel);
            }
            // Hand particles the LIVE gravity field so `GravityMode::Field` effects fall
            // toward planets (same field the rigidbodies use), not world −Y.
            let vfx_grav = self.sim.as_ref().map(|s| crate::vfx::VfxGravity {
                field: &s.world.gravity,
                colliders: &s.world.colliders,
                origin: s.world.origin,
            });
            self.vfx.advance(&self.world, sdt, vfx_grav);
            self.profile_record(floptle_core::profile::Bucket::Particles, vfx_t.ms());
            // Audio: apply queued Lua commands, then tick voices against the
            // final node transforms (same ordering rationale as particles).
            let audio_t = floptle_core::profile::Span::new();
            let audio_cmds = self.script_host.take_audio_commands();
            let root = self.project_root.clone();
            if !audio_cmds.is_empty() {
                // `node:sound():setClip(...)` mutates the component (a string —
                // outside the numeric mirror); the diff in advance() restarts
                // the voice on the new clip.
                for cmd in &audio_cmds {
                    if let floptle_script::AudioCmd::SourceSetClip { ent, clip } = cmd {
                        let target = self
                            .world
                            .query::<floptle_audio::AudioSource>()
                            .find(|(e, _)| e.index() == *ent)
                            .map(|(e, _)| e);
                        if let Some(e) = target
                            && let Some(src) = self.world.get_mut::<floptle_audio::AudioSource>(e)
                        {
                            src.clip = clip.clone();
                        }
                    }
                }
                self.audio.apply_script_commands(&self.world, &root, audio_cmds);
            }
            // Listener = the active camera's ears.
            let listener = floptle_core::active_camera(&self.world)
                .map(|e| {
                    let wt = floptle_core::world_transform(&self.world, e);
                    floptle_audio::Listener {
                        position: wt.translation,
                        forward: (wt.rotation * floptle_core::math::Vec3::NEG_Z).as_dvec3(),
                        right: (wt.rotation * floptle_core::math::Vec3::X).as_dvec3(),
                    }
                })
                .unwrap_or_default();
            for e in self.audio.advance(&self.world, &root, listener) {
                // EndBehavior::Destroy — the sound finished, its node goes too.
                self.world.despawn(e);
                self.selection.retain(|s| *s != e);
            }
            // `floptle/0115`: audio had no bucket at all, so a game whose mixer
            // was the expensive thing could profile every frame and never see it.
            self.profile_record(floptle_core::profile::Bucket::Audio, audio_t.ms());
        } else if !self.script_errors.is_empty() {
            self.script_errors.clear();
        }
    }

    /// GPU-load models a script swapped via `node.model` so they render this
    /// frame (rigged import first, static fallback).
    pub(crate) fn load_script_swapped_models(&mut self) {
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return;
        };
        for (_eid, path) in self.script_host.take_model_changes() {
            if !self.mesh_registry.contains_key(&path) {
                // Rigged first (animated glTF keeps its node tree + clips).
                match floptle_assets::import_rigged(std::path::Path::new(&path)) {
                    Ok(Some(model)) => {
                        let parts: Vec<MeshId> = model
                            .parts
                            .iter()
                            .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                            .collect();
                        let part_meta = model
                            .parts
                            .iter()
                            .map(|p| crate::PartMeta {
                                material: p.material.clone(),
                                base_color: p.base_color,
                                textured: p.texture.is_some(),
                            })
                            .collect();
                        let overrides =
                            crate::rig_overrides::RigOverrides::load(std::path::Path::new(&path));
                        if let Some(f) = overrides.texture_filter {
                            let s = crate::assets::TexSetting { filter: f, ..Default::default() };
                            for &mid in &parts {
                                raster.set_mesh_sampling(gpu, mid, s.to_sampling());
                            }
                        }
                        let rig = anim::rig_from_model(&model, &overrides);
                        self.mesh_registry.insert(
                            path.clone(),
                            MeshAsset {
                                parts,
                                part_meta,
                                tex_filter: overrides.texture_filter,
                                size: model.size,
                                rig: Some(rig),
                            },
                        );
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("  rig swap-import {path} failed ({e}); trying static"),
                }
                match floptle_assets::gltf_import::import(std::path::Path::new(&path)) {
                    Ok(model) => {
                        let parts: Vec<MeshId> = model
                            .parts
                            .iter()
                            .map(|p| raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i])))
                            .collect();
                        let part_meta = model
                            .parts
                            .iter()
                            .map(|p| crate::PartMeta {
                                material: p.material.clone(),
                                base_color: p.base_color,
                                textured: p.texture.is_some(),
                            })
                            .collect();
                        self.mesh_registry.insert(
                            path.clone(),
                            MeshAsset {
                                parts,
                                part_meta,
                                tex_filter: None,
                                size: model.size,
                                rig: None,
                            },
                        );
                    }
                    Err(e) => eprintln!("  swap-import {path} failed: {e}"),
                }
            }
        }
    }

    /// End-of-input bookkeeping: clear the per-frame key/button edges, re-pin a
    /// CONFINE-only cursor grab, and drain script logs into the Console.
    fn finish_input_frame(&mut self) {
        // Clear per-frame input edges after scripts consumed them.
        self.input_keys_pressed.clear();
        self.input_keys_released.clear();
        self.input_typed.clear();
        self.ui_text_ops.clear();
        self.input_buttons_pressed = [false; 3];
        self.input_mouse_delta = (0.0, 0.0);
        self.input_scroll = 0.0;
        // The per-TICK accumulators are consumed by the gameplay-tick loop while
        // playing; outside play they'd grow unbounded, so drain them here instead.
        if !self.playing {
            self.tick_keys_pressed.clear();
            self.tick_keys_released.clear();
            self.tick_typed.clear();
            self.tick_buttons_pressed = [false; 3];
            self.tick_mouse_delta = (0.0, 0.0);
            self.tick_scroll = 0.0;
            // Same for the action layer's banked edges, and for the same
            // reason: nothing consumes them outside Play, so they would grow
            // without bound and then all fire at once on the first tick.
            self.tick_input_edges.0.clear();
            self.tick_input_edges.1.clear();
        }
        // A CONFINE-only grab (X11 has no OS cursor lock) still lets the pointer
        // wander inside the window — pin it ourselves while a look/pan/lock/trap is
        // active. Look/pan read RAW device motion, so re-centering never pollutes
        // the deltas. A trapped Game cursor re-centers to the GAME rect (not the
        // window) so a Confined pointer stays inside the viewport it's playing in.
        if self.cursor_lock_soft
            && (self.game_holds_cursor() || self.input.looking || self.panning || self.game_trap)
            && let Some(window) = self.window.as_ref()
        {
            let sz = window.inner_size();
            let (cx, cy) = match self.game_surface_px() {
                Some((org, size)) if self.game_trap => {
                    ((org[0] + size[0] * 0.5) as u32, (org[1] + size[1] * 0.5) as u32)
                }
                _ => (sz.width / 2, sz.height / 2),
            };
            let _ = window.set_cursor_position(winit::dpi::PhysicalPosition::new(cx, cy));
        }
        // Hand the cursor back the moment the game puts something clickable on
        // screen — asked every frame, not only at the click that trapped.
        self.release_trap_for_ui();
        // Safety: never stay trapped once play stops (e.g. Stop while trapped, or a
        // layout change hid the Game tab). Escape/Stop already handle the common path.
        if self.game_trap && !self.playing {
            self.game_trap = false;
            if let Some(window) = self.window.as_ref() {
                self.cursor_lock_soft = grab_cursor(window, false);
            }
        }
        // Same safety for the editor's pointer override: it only means anything
        // against a running game, and a stale one would eat the first lock the
        // next session asks for.
        if self.cursor_freed && !self.playing {
            self.cursor_freed = false;
        }
        self.drain_script_logs();
    }

    /// Move whatever the scripts said this frame into the Console.
    ///
    /// Its own function because there are two loops that have to do it and only
    /// one of them is a frame: `finish_input_frame` for the editor, and
    /// `floptle run` for a headless one. When this lived inline in the frame,
    /// a headless run collected nothing at all and reported "nothing raised"
    /// for a project whose script was raising every step — the worst answer
    /// available, because it is confident and wrong.
    pub(crate) fn drain_script_logs(&mut self) {
        for l in self.script_host.drain_logs() {
            // Mirrored to the terminal as well, so running from one
            // (`cargo run`) does not mean opening the Console panel to see
            // `log(...)` output. On **stderr**: stdout belongs to whatever the
            // caller asked for, and a verb's `--json` document is on it.
            eprintln!("[lua] {}", l.msg);
            self.console.push(l.level, l.msg, l.source);
        }
    }

    /// Apply the frame's deferred [`EditorCmd`] intents — runs after every
    /// gpu/egui borrow has ended, so `self` is fully free again.
    pub(crate) fn apply_frame_commands(&mut self, mut cmd: EditorCmd, frame_pointer_down: bool) {
        // ---- apply UI commands (gpu/egui borrows have ended; `self` is free) ----
        if let Some(action) = cmd.project_action {
            match action {
                ProjectAction::New(p) => self.new_project(PathBuf::from(p)),
                ProjectAction::Open(p) => {
                    let path = PathBuf::from(p);
                    if path.is_dir() {
                        self.open_project(path);
                    } else {
                        eprintln!("  open project: not a folder: {}", path.display());
                    }
                }
                ProjectAction::Close => self.close_project(),
            }
        }
        if let Some(tool) = cmd.set_tool {
            self.set_tool(tool);
        }
        if let Some(path) = cmd.open_script {
            self.ide.open_file(&path);
        }
        if let Some(path) = cmd.open_script_pref {
            self.open_script_preferred(&path);
        }
        if let Some((name, line)) = cmd.open_log_source {
            self.open_source_at(&name, line);
        }
        if cmd.focus_learn
            && let Some(dock) = self.dock_state.as_mut()
        {
            crate::dock::focus_learn_tab(dock);
        }
        if cmd.focus_scripting
            && let Some(dock) = self.dock_state.as_mut() {
                focus_scripting_tab(dock);
            }
        if cmd.close_menu {
            self.context_menu = None;
        }
        if cmd.undo {
            self.undo();
        }
        if cmd.redo {
            self.redo();
        }
        if cmd.copy {
            self.copy_selected();
        }
        if cmd.paste {
            self.paste();
        }
        if cmd.duplicate {
            self.duplicate_selected();
        }
        if cmd.delete {
            self.delete_selected();
        }
        if let Some((ents, on)) = cmd.set_enabled {
            self.record();
            for e in ents {
                if on {
                    self.world.remove::<floptle_core::Disabled>(e);
                } else {
                    self.world.insert(e, floptle_core::Disabled);
                }
            }
            self.scene_dirty = true;
            // Physics is built from the world at Play; a mid-Play toggle has to rebuild
            // or the switched-off node keeps colliding with nothing on screen.
            self.rebuild_sim();
        }
        if let Some(m) = cmd.add {
            let name = match &m {
                MatterDoc::Primitive { shape: ShapeDoc::Sphere, .. } => "Sphere",
                MatterDoc::Primitive { shape: ShapeDoc::Cube, .. } => "Cube",
                MatterDoc::Primitive { shape: ShapeDoc::Capsule, .. } => "Capsule",
                MatterDoc::Primitive { shape: ShapeDoc::Plane, .. } => "Plane",
                MatterDoc::Blob { .. } => "Blob",
                MatterDoc::Mesh { .. } => "Mesh",
                MatterDoc::Empty => "Group",
                MatterDoc::MapMesh { .. } => "Model Mesh",
                MatterDoc::Terrain { .. } => "Terrain",
                MatterDoc::NavMesh { .. } => "Nav Mesh",
                MatterDoc::NavLink { .. } => "Nav Link",
                MatterDoc::NavArea { .. } => "Nav Area",
                MatterDoc::Camera { .. } => "Camera",
                MatterDoc::PointLight { .. } => "Point Light",
                MatterDoc::GravityVolume { .. } => "Gravity Volume",
                MatterDoc::WaterVolume { .. } => "Water Volume",
                MatterDoc::FieldShape { .. } => "Field Shape",
                MatterDoc::Tilemap { .. } => "Tilemap",
                MatterDoc::SpriteBatch { .. } => "Sprite Batch",
                MatterDoc::Sprite { .. } => "Sprite",
                MatterDoc::Skybox { .. } => "Skybox",
                MatterDoc::PostProcess { .. } => "Post Processing",
                MatterDoc::LightProbes { .. } => "Light Probes",
                MatterDoc::ReflectionProbe { .. } => "Reflection Probe",
            };
            // A navmesh's id keys its baked file, so a second one in the same
            // scene must not arrive holding the first one's. The menu cannot
            // know what is already here, so the id is assigned on the way in.
            let m = if let MatterDoc::NavMesh { .. } = &m {
                let next = self
                    .world
                    .query::<floptle_core::Matter>()
                    .filter_map(|(_, m)| match m {
                        floptle_core::Matter::NavMesh { id, .. } => Some(*id),
                        _ => None,
                    })
                    .max()
                    .map_or(1, |n| n + 1);
                MatterDoc::from(&floptle_core::Matter::default_nav_mesh(next))
            } else if let MatterDoc::NavLink { .. } = &m {
                // A link's id is how a script names it and how a bake matches it
                // back, so two links sharing one is two links a game cannot tell
                // apart.
                let next = self
                    .world
                    .query::<floptle_core::Matter>()
                    .filter_map(|(_, m)| match m {
                        floptle_core::Matter::NavLink { id, .. } => Some(*id),
                        _ => None,
                    })
                    .max()
                    .map_or(1, |n| n + 1);
                MatterDoc::from(&floptle_core::Matter::default_nav_link(next))
            } else {
                m
            };
            self.add_node(name, m);
        }
        if let Some(what) = cmd.add_ui {
            self.add_ui_node(what);
            // Bring the ◫ UI tab up: the thing you just added is a flat screen
            // element, and hunting for the tab that shows it is the kind of
            // friction that keeps people typing coordinates instead.
            if let Some(dock) = self.dock_state.as_mut() {
                crate::dock::focus_ui_tab(dock);
            }
        }
        if let Some(shape) = cmd.add_map_shape {
            self.add_map_shape(shape);
        }
        if let Some(op) = cmd.map_op.take() {
            self.apply_map_op(op);
        }
        // ◫ Tiles intents, in the order they were pressed.
        if !cmd.tile_cmds.is_empty() {
            let cmds = std::mem::take(&mut cmd.tile_cmds);
            self.apply_tile_cmds(cmds);
        }
        if let Some(mode) = cmd.set_map_mode {
            // Converts rather than clears — see `set_map_mode`.
            self.set_map_mode(mode);
        }
        if let Some(on) = cmd.set_map_knife {
            self.set_map_knife(on);
            // Cutting needs the tool, same as drawing does.
            if on && self.tool != Tool::MapEdit {
                self.set_tool(Tool::MapEdit);
                self.set_map_knife(true); // set_tool clears it on the way in
            }
        }
        if let Some(arm) = cmd.set_map_arm {
            self.map_draw = None;
            self.set_map_knife(false); // drawing and cutting both own the click
            self.map_arm = arm;
            // Drawing needs the tool: arming from the tab turns it on rather
            // than leaving a button that visibly does nothing.
            if arm.is_some() && self.tool != Tool::MapEdit {
                self.set_tool(Tool::MapEdit);
                self.map_arm = arm; // set_tool clears the arm on the way in
            }
        }
        if cmd.map_detach {
            self.map_detach_selection();
        }
        if let Some(q) = cmd.map_turn {
            self.map_turn(q);
        }
        if cmd.map_prune {
            let n = self.prune_map_orphans();
            self.map_note(
                floptle_script::LogLevel::Debug,
                if n == 0 {
                    "no unused map geometry to clean".to_string()
                } else {
                    format!("cleaned {n} unused map mesh(es) from this scene's sidecar")
                },
            );
        }
        // Latch "pointer on a UI overlay interact" for the raw LMB handler (which
        // runs between frames): while set, presses belong to egui, not pick/gizmo.
        self.ui_overlay_hot = cmd.ui_hot;
        // A UI move/resize drag is one coalesced undo step (banked on the first
        // frame of the gesture via the pre-edit frame_snapshot; closed when the
        // pointer releases and `editing` resets). Without this, dragging/resizing
        // a UI element in the Scene view left no undo point.
        if !cmd.ui_move.is_empty() || cmd.ui_resize.is_some() {
            self.begin_edit();
        }
        for (idx, d) in &cmd.ui_move {
            let ent = self.world.entity_with::<Transform>(*idx);
            if let Some(e) = ent
                && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
            {
                crate::ui_game::nudge_place(&mut spec.place, *d);
                self.world.insert(e, spec);
            }
        }
        // Rect-tool resize: grow/shrink toward the dragged side, keeping the
        // OPPOSITE edge visually fixed — Free positions and Pin offsets get the
        // exact compensation for their placement mode.
        if let Some((idx, dsize, from_min, cur)) = cmd.ui_resize {
            let ent = self.world.entity_with::<Transform>(idx);
            if let Some(e) = ent
                && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
            {
                for a in 0..2 {
                    if dsize[a] == 0.0 {
                        continue;
                    }
                    let old = cur[a].max(1.0);
                    let new = (old + dsize[a]).max(1.0);
                    let d = new - old;
                    spec.size[a] = match spec.size[a] {
                        // % keeps tracking the parent (scaled proportionally);
                        // px adjusts; fit/grow become concrete px on first drag.
                        floptle_ui::Size::Pct(p) => floptle_ui::Size::Pct(p * new / old),
                        floptle_ui::Size::Fixed(v) => floptle_ui::Size::Fixed((v + d).max(1.0)),
                        _ => floptle_ui::Size::Fixed(new),
                    };
                    match &mut spec.place {
                        floptle_ui::Place::Free { pos } => {
                            if from_min[a] {
                                pos[a] -= d;
                            }
                        }
                        floptle_ui::Place::Pin { anchor, offset } => {
                            let f = anchor.factors()[a];
                            offset[a] += d * if from_min[a] { f - 1.0 } else { f };
                        }
                        // Dragging an edge shrinks that side's margin so the box
                        // grows toward the drag (margin is [L, T, R, B]).
                        floptle_ui::Place::Stretch { margin, .. } => {
                            let side = if from_min[a] { a } else { a + 2 };
                            margin[side] -= d;
                        }
                    }
                }
                self.world.insert(e, spec);
            }
        }
        // ◫ UI tab writes. Each is an ordinary component edit, banked as one
        // undo step like any Inspector change.
        if !cmd.ui_order.is_empty()
            || !cmd.ui_set_visible.is_empty()
            || cmd.ui_set_text.is_some()
            || !cmd.ui_set_style.is_empty()
            || !cmd.ui_paste_look.is_empty()
        {
            self.begin_edit();
            let ent = |world: &floptle_core::World, idx: u32| {
                world.entity_with::<Transform>(idx)
            };
            for (idx, order) in &cmd.ui_order {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.order = *order;
                    self.world.insert(e, spec);
                }
            }
            for (idx, vis) in &cmd.ui_set_visible {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.visible = *vis;
                    self.world.insert(e, spec);
                }
            }
            if let Some((idx, text)) = &cmd.ui_set_text
                && let Some(e) = ent(&self.world, *idx)
                && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                && let Some(t) = spec.text.as_mut()
            {
                t.text = text.clone();
                self.world.insert(e, spec);
            }
            for (idx, name) in &cmd.ui_set_style {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.style = name.clone();
                    self.world.insert(e, spec);
                }
            }
            // Pasting a LOOK copies the visual properties only: placement, size
            // and the element's children are what make it that element, and
            // nothing about "make this look like that" should move it.
            for (idx, src) in &cmd.ui_paste_look {
                if let Some(e) = ent(&self.world, *idx)
                    && let Some(mut spec) = self.world.get::<floptle_ui::ElementSpec>(e).cloned()
                {
                    spec.shape = src.shape.clone();
                    spec.opacity = src.opacity;
                    spec.tint = src.tint;
                    spec.rotation = src.rotation;
                    spec.scale = src.scale;
                    spec.pivot = src.pivot;
                    spec.style = src.style.clone();
                    if let (Some(dst), Some(s)) = (spec.text.as_mut(), src.text.as_ref()) {
                        let keep = std::mem::take(&mut dst.text);
                        *dst = s.clone();
                        dst.text = keep;
                    }
                    if let (Some(dst), Some(s)) = (spec.stack.as_mut(), src.stack.as_ref()) {
                        dst.pad = s.pad;
                        dst.gap = s.gap;
                    }
                    self.world.insert(e, spec);
                }
            }
            self.scene_dirty = true;
        }
        if cmd.ui_reload_styles {
            self.reload_ui_styles();
        }
        if cmd.inspector_changed {
            self.begin_edit();
        }
        // ---- baked GI ------------------------------------------------------
        if cmd.gi_changed {
            self.gi_dirty = true;
        }
        if let Some(v) = cmd.gi_show_only {
            self.gi_show_only = v;
            self.gi_dirty = true;
        }
        if let Some(v) = cmd.gi_show_probes {
            self.gi_show_probes = v;
        }
        if cmd.recapture_probes {
            self.recapture_reflection_probes();
        }
        if cmd.gi_bake && !self.start_gi_bake() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "nothing to bake: the scene has no enabled Light Probes node".into(),
                None,
            );
        }
        if cmd.nav_bake {
            self.bake_nav();
        }
        if cmd.nav_clear {
            if let Some((_, floptle_core::Matter::NavMesh { id, .. })) =
                crate::nav_bake::nav_node(&self.world)
            {
                let _ = std::fs::remove_file(self.nav_path(id));
            }
            self.nav_baked = None;
            self.nav_overlay = None;
            self.nav_seconds = 0.0;
            self.nav_triangles = 0;
            self.publish_nav_mesh();
        }
        if cmd.gi_cancel {
            self.cancel_gi_bake();
        }
        if cmd.gi_clear {
            self.gi_baked = None;
            self.gi_dirty = true;
            let _ = std::fs::remove_file(self.gi_path());
        }
        // Close the undo-coalescing session whenever the pointer isn't held. A drag
        // (gizmo, DragValue, UI move) keeps the button down across frames, so it stays
        // ONE step; but a discrete edit (checkbox, combo pick, typed value) releases
        // the button, so this frees `editing` and the NEXT edit banks its own pre-edit
        // snapshot. Without it, `editing` stuck true after any non-drag edit and every
        // following edit silently coalesced into it — the "undo doesn't work on
        // property edits" bug. (The raw LMB-release handler also clears it; this is the
        // reliable backstop for keyboard/scroll/click edits that skip that path.)
        if !frame_pointer_down {
            self.editing = false;
        }
        // Persist pending animation-asset edits even when their tab is hidden
        // (the tabs flush on draw; this covers edits left behind a tab switch).
        if !frame_pointer_down {
            if self.anim_ui.graph_dirty {
                if let (Some(k), Some(doc)) =
                    (self.anim_ui.graph_key.clone(), self.anim_ui.graph_doc.clone())
                {
                    self.anim.save_controller(&self.project_root, &k, &doc);
                }
                self.anim_ui.graph_dirty = false;
            }
            if self.anim_ui.clip_dirty {
                if let Some((k, d)) = self.anim_ui.clip_doc.clone() {
                    self.anim.save_clip(&self.project_root, &k, &d);
                }
                self.anim_ui.clip_dirty = false;
            }
        }
        if cmd.toggle_play {
            self.toggle_play();
        }
        if cmd.net_host_local {
            self.net_start_hosting();
        }
        if cmd.net_join_local {
            self.net_join_local();
        }
        if cmd.net_play_as_client {
            self.net_play_as_client();
        }
        if cmd.net_stop_session {
            self.net_stop("panel");
        }
        if let Some(port) = cmd.net_host_quic {
            self.net_host_quic(port);
        }
        if let Some(p) = cmd.net_play_replay {
            self.net_play_replay(&p);
        }
        if let Some(addr) = cmd.net_join_quic {
            let a = addr.trim().to_string();
            if let Some(rest) = a.strip_prefix("relay://") {
                match rest.rsplit_once('/') {
                    Some((raddr, code)) => self.net_join_relay(raddr, code),
                    None => self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("join \"{a}\": expected relay://host:port/CODE"),
                        None,
                    ),
                }
            } else {
                self.net_join_quic(a.trim_start_matches("quic://"));
            }
        }
        if let Some(addr) = cmd.net_host_relay {
            self.net_host_relay(addr.trim());
        }
        if let Some((dir, target)) = cmd.export_game {
            self.begin_export(dir, target);
        }
        if cmd.step_tick {
            self.step_tick(1);
        }
        if cmd.step_tick_back {
            self.step_tick_back();
        }
        if cmd.toggle_pause {
            self.toggle_pause();
        }
        if let Some(path) = cmd.drop_asset {
            self.drop_asset(&path);
        }
        if let Some(path) = cmd.convert_model {
            self.start_model_conversion(&path);
        }
        // Free until one is running: a `try_recv` on nothing is a branch.
        self.poll_model_conversion();
        if let Some(path) = cmd.import_map {
            // The Assets browser's "Add to scene": no drop point, so the group
            // lands in front of the camera (the `add_node_at` convention).
            self.import_map_file(&path, None);
        }
        if let Some((path, e)) = cmd.drop_script_on {
            self.attach_script_file(&path, Some(e));
        }
        if let Some((script_path, e)) = cmd.attach_named {
            let path = self.project_root.join(&script_path);
            self.attach_script_file(&path.to_string_lossy(), Some(e));
        }
        if let Some(file) = cmd.open_in_editor {
            open_external_editor(&self.external_editor, &self.project_root, &file, 1);
        }
        if let Some(c) = cmd.set_external_editor {
            save_external_editor(&c);
            self.external_editor = c;
        }
        if let Some(v) = cmd.set_prefer_external {
            save_prefer_external(v);
            self.prefer_external_editor = v;
        }
        if let Some((en, tint)) = cmd.set_play_tint {
            save_play_tint(en, tint);
            self.play_tint_enabled = en;
            self.play_tint = tint;
        }
        if cmd.save_grid {
            save_grid(&self.grid);
        }
        if let Some(i) = cmd.set_engine_theme {
            self.engine_theme = i;
            save_theme_index(engine_theme_path(), i);
        }
        if let Some(i) = cmd.set_code_theme {
            self.code_theme = i;
            save_theme_index(code_theme_path(), i);
        }
        if let Some((name, doc)) = cmd.save_material {
            let dir = self.materials_dir();
            let _ = floptle_scene::save_material(&name, &doc, &dir);
            self.materials = self.load_materials();
            self.mat_name_buf.clear();
            self.asset_tree = build_assets(&self.project_root);
        }
        if let Some(e) = cmd.add_material {
            self.record();
            for e in self.selected_group(e) {
                // Seed from each node's own primitive color (else white), so a
                // multi-selection keeps twelve colours instead of taking one.
                let base = match self.world.get::<Matter>(e) {
                    Some(Matter::Primitive { color, .. }) => *color,
                    _ => [1.0, 1.0, 1.0],
                };
                self.world.insert(e, Material::tinted(base));
            }
        }
        if let Some(e) = cmd.reset_transform {
            self.record();
            // The whole selection, like every other component action here: a
            // reset that only reached the node you happened to click would be a
            // surprise the first time it matters.
            for e in self.selected_group(e) {
                if let Some(t) = self.world.get_mut::<Transform>(e) {
                    *t = Transform::IDENTITY;
                }
            }
        }
        // **Get a model's own pictures out of it.** See `model_textures` —
        // everything a dev wants to do next with a model's art (layer over it,
        // recolour it, point one part at a different copy) starts with the file
        // existing.
        if let Some(path) = cmd.extract_model_textures {
            let abs = self.resolve_asset_path(&path);
            match crate::model_textures::extract_model_textures(&abs, &path, &self.project_root) {
                Ok(written) => {
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "extracted {} texture(s) from {path}: {}",
                            written.len(),
                            written.iter().map(|e| e.path.as_str()).collect::<Vec<_>>().join(", ")
                        ),
                        None,
                    );
                    // They are real project assets now — the Assets panel and
                    // every texture picker have to see them without a restart.
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("could not extract {path}'s textures: {e}"),
                    None,
                ),
            }
        }
        // Override ONE sub-object's material, seeded with what that part already
        // looks like — its imported colour AND, if the model brought one, its
        // texture (extracted on the spot, because an override that names no
        // texture draws untextured and "override" must not mean "go blank").
        if let Some((e, key, model)) = cmd.override_object_material {
            self.record();
            let (base, textured, material) = self
                .mesh_registry
                .get(&model)
                .and_then(|a| {
                    a.part_meta.iter().enumerate().find(|(i, pm)| {
                        a.override_key(*i) == Some(key.as_str()) || pm.material == key
                    })
                })
                .map(|(_, pm)| (pm.base_color, pm.textured, pm.material.clone()))
                .unwrap_or(([1.0; 3], false, key.clone()));
            let mut mat = Material::tinted(base);
            if textured {
                let abs = self.resolve_asset_path(&model);
                let existing =
                    crate::model_textures::extracted_file(&self.project_root, &model, &material);
                mat.texture = match existing {
                    Some(p) => Some(p),
                    None => match crate::model_textures::extract_model_textures(
                        &abs,
                        &model,
                        &self.project_root,
                    ) {
                        Ok(written) => {
                            self.asset_tree = build_assets(&self.project_root);
                            written
                                .iter()
                                .find(|x| x.material == material)
                                .map(|x| x.path.clone())
                        }
                        Err(err) => {
                            self.console.push(
                                floptle_script::LogLevel::Warn,
                                format!(
                                    "{key}: could not extract this part's texture ({err}) — the \
                                     override starts untextured"
                                ),
                                None,
                            );
                            None
                        }
                    },
                };
            }
            let mut om =
                self.world.get::<floptle_core::ObjectMaterials>(e).cloned().unwrap_or_default();
            om.0.insert(key, mat);
            self.world.insert(e, om);
        }
        if let Some(e) = cmd.remove_material {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<Material>(e);
            }
        }
        if let Some(e) = cmd.add_rigidbody {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_core::RigidBody::default());
            }
            self.rebuild_sim();
        }
        if let Some(e) = cmd.remove_rigidbody {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_core::RigidBody>(e);
            }
            self.rebuild_sim();
        }
        if let Some(e) = cmd.add_celestial {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_core::CelestialBody::default());
            }
            self.rebuild_sim(); // they're gravity sources now
        }
        if let Some(e) = cmd.remove_celestial {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_core::CelestialBody>(e);
            }
            self.rebuild_sim();
        }
        if let Some(e) = cmd.add_networked {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_core::Replicated::default());
            }
        }
        if let Some((e, key)) = cmd.add_particles {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(
                    e,
                    floptle_core::ParticleSystem { asset: key.clone(), play_on_start: true },
                );
                // Attached mid-play: start emitting right away (live-tweak discipline).
                if self.playing {
                    self.vfx.spawn(e, &key);
                }
            }
        }
        // ✚ Effect asks for a name BEFORE it writes anything.
        //
        // It used to invent `NewEffect`, `NewEffect1`, `NewEffect2` and hand you
        // the timeline — so naming your own effect meant renaming a file that a
        // node already pointed at, which is the moment nobody does it. Asking
        // first is also the only order that is safe: a `.vfx.ron` renamed after
        // the fact leaves the `ParticleSystem.asset` on the node pointing at the
        // old key.
        if let Some(e) = cmd.new_particles {
            self.new_asset_prompt = Some((crate::NewAsset::Effect(e), String::new()));
        }
        if let Some((e, name)) = cmd.do_new_particles {
            // Sanitised into a filename here rather than refused in the modal: a
            // space in an effect name is a reasonable thing to type.
            let stem = crate::assets::sanitize_asset_name(&name);
            let mut n = 0;
            let (key, path) = loop {
                let key =
                    if n == 0 { format!("vfx/{stem}") } else { format!("vfx/{stem}{n}") };
                let path = self.project_root.join(format!("{key}{}", floptle_scene::VFX_EXT));
                if !path.exists() {
                    break (key, path);
                }
                n += 1;
            };
            let doc = crate::vfx::starter_effect_doc(key.rsplit('/').next().unwrap_or(&key));
            if let Err(err) = floptle_scene::save_vfx_effect(&doc, &path) {
                eprintln!("  new effect {key} failed: {err}");
            } else {
                self.vfx.rescan(&self.project_root);
                self.asset_tree = build_assets(&self.project_root);
                self.record();
                self.world.insert(
                    e,
                    floptle_core::ParticleSystem { asset: key.clone(), play_on_start: true },
                );
                if self.playing {
                    self.vfx.spawn(e, &key);
                }
                // Fresh effect → straight into the timeline editor.
                cmd.open_particle_editor = Some(key);
            }
        }
        if let Some(e) = cmd.remove_particles {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_core::ParticleSystem>(e);
            }
        }
        if let Some(e) = cmd.add_audio {
            self.record();
            for e in self.selected_group(e) {
                self.world.insert(e, floptle_audio::AudioSource::default());
            }
        }
        if let Some(e) = cmd.remove_audio {
            self.record();
            for e in self.selected_group(e) {
                self.world.remove::<floptle_audio::AudioSource>(e);
            }
        }
        if let Some(key) = cmd.preview_audio.take() {
            let rel = crate::assets::asset_rel_path(&key, &self.project_root).replace('\\', "/");
            let root = self.project_root.clone();
            self.audio.preview(&root, &rel);
        }
        if cmd.mixer_changed {
            // Live-apply: the running play session tracks the edit too (its
            // runtime overlay restarts from the edited graph).
            if self.playing {
                self.audio.runtime_mixer = Some(self.project.mixer.clone());
            }
            let mixer = self.project.mixer.clone();
            self.audio.apply_mixer(&mixer);
        }
        if let Some((e, on)) = cmd.set_mesh_collider {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::MeshCollider);
                } else {
                    self.world.remove::<floptle_core::MeshCollider>(e);
                }
            }
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_collidable {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::Collidable);
                } else {
                    // Clear both the new marker and any legacy mesh-collider marker.
                    self.world.remove::<floptle_core::Collidable>(e);
                    self.world.remove::<floptle_core::MeshCollider>(e);
                }
            }
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_nav_exclude {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::NavMeshExclude);
                } else {
                    self.world.remove::<floptle_core::NavMeshExclude>(e);
                }
            }
        }
        if cmd.rebuild_physics {
            self.rebuild_sim();
        }
        if let Some((e, on)) = cmd.set_trigger {
            self.record();
            for e in self.selected_group(e) {
                if on {
                    self.world.insert(e, floptle_core::Trigger);
                } else {
                    self.world.remove::<floptle_core::Trigger>(e);
                }
            }
            self.rebuild_sim(); // the sensor flag bakes into the static collider
        }
        if let Some((e, layer, order)) = cmd.set_sorting {
            self.record();
            // Default-at-0 is the absence of the component, so a node put back
            // to the default stops carrying one and its scene stops mentioning
            // sorting at all.
            // The MODE is not this command's to change — it has its own control
            // — so it is carried over rather than reset. And it joins the
            // default test: a Y-sorted node on the Default layer at order 0 is
            // NOT the default, and dropping its component would silently turn
            // Y-sorting off the first time somebody touched the layer picker.
            let mode = self
                .world
                .get::<floptle_core::Sorting>(e)
                .map(|s| s.mode)
                .unwrap_or_default();
            if layer == floptle_core::DEFAULT_SORTING_LAYER
                && order == 0
                && mode == floptle_core::SortMode::default()
            {
                self.world.remove::<floptle_core::Sorting>(e);
            } else {
                self.world.insert(e, floptle_core::Sorting { layer, order, mode });
            }
        }
        if let Some((e, mode)) = cmd.set_sort_mode {
            self.record();
            let cur = self.world.get::<floptle_core::Sorting>(e).cloned().unwrap_or_default();
            // Same default test as the layer/order path above, with the mode in
            // it: back to `order` on the Default layer at 0 = no component, so
            // the scene stops mentioning sorting entirely.
            if mode == floptle_core::SortMode::default()
                && cur.order == 0
                && (cur.layer.is_empty() || cur.layer == floptle_core::DEFAULT_SORTING_LAYER)
            {
                self.world.remove::<floptle_core::Sorting>(e);
            } else {
                self.world.insert(e, floptle_core::Sorting { mode, ..cur });
            }
            self.scene_dirty = true;
        }
        if let Some((e, p)) = cmd.set_parallax {
            self.record();
            // Identity IS the absence of the component, the same rule sorting
            // and 2D lighting follow — so a layer put back to 1,1 stops carrying
            // one and its scene stops mentioning parallax.
            if p.is_identity() {
                self.world.remove::<floptle_core::Parallax>(e);
            } else {
                self.world.insert(e, p);
            }
            self.scene_dirty = true;
        }
        if let Some((e, lit)) = cmd.set_lighting_2d {
            self.record();
            // Auto with no layer list IS the absence of the component, exactly
            // as with sorting above — so a node put back to the default stops
            // carrying one and its scene stops mentioning 2D lighting.
            if lit == floptle_core::Lighting2D::default() {
                self.world.remove::<floptle_core::Lighting2D>(e);
            } else {
                self.world.insert(e, lit);
            }
        }
        if let Some((e, cast)) = cmd.set_shadow_2d {
            self.record();
            if cast == floptle_core::Cast2D::Auto {
                self.world.remove::<floptle_core::Shadow2D>(e);
            } else {
                self.world.insert(e, floptle_core::Shadow2D(cast));
            }
        }
        if let Some((e, c)) = cmd.set_camera_2d {
            self.record();
            match c {
                // The live half (where the follow has got to, any shake running)
                // is deliberately left at its default here: this is an EDIT to
                // the rule, and inheriting a play session's position into an
                // authored camera is how a camera moves when you change its
                // dead zone.
                Some(c) => self.world.insert(e, c),
                None => {
                    self.world.remove::<floptle_core::camera2d::Camera2D>(e);
                }
            }
        }
        if let Some(req) = cmd.do_set_layer.clone() {
            // Already answered — the modal put the final target list here.
            self.apply_layer(&req.targets, &req.layer);
        }
        if let Some(req) = cmd.set_layer.clone() {
            // Children make the scope of this edit a real question rather than a
            // detail — see `LayerChildrenPrompt`. Ask once, covering the whole
            // selection, and only when there is actually something to ask about.
            let kids = self.descendants_of(&req.targets);
            if kids.is_empty() {
                self.apply_layer(&req.targets, &req.layer);
            } else {
                self.layer_children_confirm = Some(crate::LayerChildrenPrompt {
                    targets: req.targets,
                    children: kids,
                    layer: req.layer,
                });
            }
        }
        if let Some(a) = cmd.access {
            // One set of values, two ways in: this pane and a game's own options
            // menu (`access.*`). Pushed into the host so Lua reads back what the
            // editor just set, rather than the two disagreeing (`floptle/0079`).
            self.access = a;
            self.script_host.set_access(a);
        }
        if let Some((old, new)) = cmd.rename_layer {
            // The open scene's nodes follow a Project-Settings layer rename
            // (fires per keystroke, so they never detach mid-edit). "Default"
            // as the new name = the component becomes redundant — drop it.
            let on_old: Vec<Entity> = self
                .world
                .query::<floptle_core::Layer>()
                .filter(|(_, l)| l.0 == old)
                .map(|(e, _)| e)
                .collect();
            for e in on_old {
                if new == floptle_core::layers::DEFAULT_LAYER {
                    self.world.remove::<floptle_core::Layer>(e);
                } else {
                    self.world.insert(e, floptle_core::Layer(new.clone()));
                }
            }
            self.rebuild_sim();
        }
        if let Some((e, mt)) = cmd.set_matter {
            // Switch the node's "type" (mutually-exclusive components). Terrain owns an
            // out-of-ECS SDF field, so never morph one through here — and the mandatory
            // PostProcess node keeps its type (nothing else may become one either).
            if !matches!(
                self.world.get::<Matter>(e),
                Some(Matter::Terrain { .. } | Matter::PostProcess { .. })
            ) && !matches!(mt, Matter::PostProcess { .. })
            {
                // Becoming a Mesh: GPU-load the model so it renders this frame.
                if let Matter::Mesh { asset_path } = &mt {
                    self.import_model(&asset_path.clone());
                }
                self.record();
                self.world.insert(e, mt);
                self.rebuild_sim();
            }
        }
        if let Some(path) = cmd.import_model {
            self.import_model(&path);
        }
        if let Some((e, vis)) = cmd.set_visible {
            self.record();
            self.world.insert(e, floptle_core::Visible(vis));
        }
        if let Some(clip) = cmd.copy_component {
            self.component_clip = Some(clip);
        }
        if let Some(e) = cmd.paste_component {
            self.paste_onto(e);
        }
        if let Some((e, name)) = cmd.apply_preset
            && let Some((_, doc)) = self.materials.iter().find(|(n, _)| n == &name) {
                let mat = doc.to_material();
                self.record();
                self.world.insert(e, mat);
            }
        if let Some(path) = cmd.extract_textures {
            self.extract_textures(&path);
        }
        if let Some((mesh, idx)) = cmd.select_bone {
            // Select a model object/bone from the Inspector's Objects & Rig lists —
            // mutually exclusive with node/asset selection (like the Hierarchy tree).
            self.bone_selection = Some((mesh, idx));
            self.selection.clear();
            self.selected_asset = None;
        }
        if let Some((mesh, name, p)) = cmd.set_object_pivot {
            self.apply_object_pivot(mesh, &name, Vec3::from(p));
        }
        if let Some((child, mesh, bone)) = cmd.attach_to_bone {
            // A BoneAttach's local Transform is in the target model's space, so it
            // must be a direct child of that Mesh.  Preserve the scene-world pose as
            // its bone-local offset before normalizing the hierarchy; this supports
            // meshes nested under sockets/Empties as well as direct children.
            let child_world = floptle_core::world_transform(&self.world, child);
            let offset = crate::anim::bone_world_transform(
                &self.anim,
                &self.world,
                &self.mesh_registry,
                mesh,
                &bone,
            )
            .map(|bone_world| {
                // Componentwise TRS inverse (matches resolve_attachments) — a
                // mirrored mesh keeps its negative scale on the right axis.
                let local = bone_world.inv_mul(&child_world);
                if local.translation.is_finite() && local.scale.is_finite() {
                    local
                } else {
                    floptle_core::Transform::IDENTITY
                }
            })
            .unwrap_or(floptle_core::Transform::IDENTITY);
            self.world.insert(child, floptle_core::Parent(mesh));
            self.world.insert(child, floptle_core::BoneAttach { target: mesh, bone, offset });
        }
        if let Some((mesh, child, parent)) = cmd.set_object_parent {
            // Persist an object re-parent to the model's `.rig.ron` sidecar, then
            // re-import the model so the new hierarchy takes effect live and every
            // instance rebinds against the reordered skeleton.
            if let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned() {
                let abs = self.resolve_asset_path(&asset_path);
                let mut ov = crate::rig_overrides::RigOverrides::load(&abs);
                // "" = model root (an explicit reparent-to-root, distinct from absent).
                ov.reparent.insert(child, parent.unwrap_or_default());
                if let Err(e) = ov.save(&abs) {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("save rig override failed: {e}"),
                        None,
                    );
                }
                self.mesh_registry.remove(&asset_path);
                self.import_model(&asset_path);
                self.anim.revision += 1; // force every instance to rebind
                self.bone_selection = None; // node indices changed after the re-sort
            }
        }
        if let Some((path, filter)) = cmd.set_model_filter {
            // Persist the embedded-texture filter to the model's sidecar, then drop
            // the registration — the ensure sweep re-imports it next frame with the
            // new sampling (skin variants self-heal on the new MeshIds).
            let abs = self.resolve_asset_path(&path);
            let mut ov = crate::rig_overrides::RigOverrides::load(&abs);
            ov.texture_filter = filter;
            if let Err(e) = ov.save(&abs) {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("saving {}: {e}", abs.display()),
                    None,
                );
            }
            self.mesh_registry.remove(&path);
        }
        if let Some(mesh) = cmd.mirror_model
            && let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned()
        {
            let abs = self.resolve_asset_path(&asset_path);
            match floptle_assets::mirror_apply(&abs) {
                Ok(r) => {
                    // Carry any object re-parenting onto the mirrored model (same node
                    // names), so the sidecar keeps working after the bake.
                    let src_side = crate::rig_overrides::RigOverrides::sidecar_path(&abs);
                    if src_side.exists() {
                        let _ = std::fs::copy(
                            &src_side,
                            crate::rig_overrides::RigOverrides::sidecar_path(&r.output),
                        );
                    }
                    let rel = r
                        .output
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let split: Vec<String> =
                        r.split.iter().map(|(l, _)| l.trim_end_matches(".L").trim_end_matches(".R").to_string()).collect();
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "Mirror-apply → {rel}  ·  welded {:?}  ·  split L/R {:?}  ·  kept {:?}  \
                             (assign the new model in the Inspector to use it)",
                            r.welded, split, r.kept
                        ),
                        None,
                    );
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("Mirror-apply failed: {e}"),
                    None,
                ),
            }
        }
        if let Some((mesh, object)) = cmd.add_hair_rig
            && let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned()
        {
            let abs = self.resolve_asset_path(&asset_path);
            match floptle_assets::add_flow_rig(&abs, &object, 5) {
                Ok(r) => {
                    // Carry any object re-parenting/pivots onto the rigged model
                    // (same node names), so the sidecar keeps working after the bake.
                    let src_side = crate::rig_overrides::RigOverrides::sidecar_path(&abs);
                    if src_side.exists() {
                        let _ = std::fs::copy(
                            &src_side,
                            crate::rig_overrides::RigOverrides::sidecar_path(&r.output),
                        );
                    }
                    let rel = r
                        .output
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "Flow-rig → {rel}  ·  {} got a {}-bone chain  \
                             (assign the new model, then pose the {}_root chain to make it flow)",
                            r.object, r.bones, r.object
                        ),
                        None,
                    );
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("Flow-rig failed: {e}"),
                    None,
                ),
            }
        }
        if let Some(path) = cmd.extract_anims {
            self.anim_ui.probes.remove(&path); // refresh the model's clip list
            match anim::extract_clips(&mut self.anim, &self.project_root, &path) {
                Ok(keys) => {
                    self.console.push(
                        floptle_script::LogLevel::Debug,
                        format!(
                            "extracted {} animation clip(s) → assets/animations/",
                            keys.len()
                        ),
                        None,
                    );
                    self.asset_tree = build_assets(&self.project_root);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("extract animations failed: {e}"),
                    None,
                ),
            }
        }
        if let Some((e, key)) = cmd.set_anim_controller {
            self.record();
            match key {
                Some(k) => {
                    self.world.insert(e, floptle_core::AnimController { asset: k });
                }
                None => {
                    self.world.remove::<floptle_core::AnimController>(e);
                }
            }
            // Live in Play: the runtime rebinds lazily on the next animator advance.
        }
        if let Some(key) = cmd.open_anim_graph {
            cmd.focus_anim_graph = true;
            self.anim_ui.graph_key = Some(key);
            self.anim_ui.graph_doc = None; // reload the working copy
            self.anim_ui.graph_dirty = false;
            self.anim_ui.sel_state = None;
            self.anim_ui.sel_trans = None;
        }
        if let Some(attach) = cmd.new_anim_controller {
            cmd.focus_anim_graph = true;
            self.anim_ui.new_ctl_buf = Some(String::new());
            self.anim_ui.focus_prompt = true;
            self.anim_ui.new_ctl_attach = attach;
            self.anim_ui.new_ctl_dir = cmd.new_anim_controller_dir.take().and_then(|d| {
                Path::new(&d)
                    .strip_prefix(&self.project_root)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            });
        }
        if let Some(path) = cmd.open_shader_graph {
            self.open_shader_in_graph(&path);
        }
        if let Some(path) = cmd.import_aseprite {
            self.import_aseprite_sheet(&path);
        }
        if let Some((path, cols, rows)) = cmd.new_sprite_anim {
            self.write_sprite_anim(&path, cols, rows);
        }
        if let Some(path) = cmd.open_image {
            self.open_image_doc(&path);
        }
        if let Some(form) = cmd.image_new {
            self.new_image_doc(&form);
        }
        if cmd.image_save {
            self.save_image_doc();
        }
        if let Some(name) = cmd.image_save_as {
            self.save_image_doc_as(&name);
        }
        if let Some(what) = cmd.image_export {
            self.export_image(what);
        }
        if cmd.image_save_palette {
            self.save_image_palette();
        }
        // Closing goes through `close_image_doc`, which asks about unsaved work
        // and takes DISCARD for an answer. It used to refuse outright and say
        // "save first" — which is not a thing you can do to a document that has
        // never had a name, so the only exit was closing the project.
        if cmd.image_close {
            self.close_image_doc(None);
        }
        if let Some(i) = cmd.image_close_tab {
            self.close_image_doc(Some(i));
        }
        if let Some(i) = cmd.image_activate {
            self.activate_image_doc(i);
        }
        if cmd.image_new_from_clipboard {
            self.new_image_from_clipboard();
        }
        if let Some(which) = cmd.image_discard {
            self.discard_image_doc(which);
        }
        if cmd.image_save_then_close {
            // An unnamed document routes to Save As, which is a dialog, so the
            // close waits for it rather than happening behind it.
            if self.image.path.is_some() {
                self.save_image_doc();
                if !self.image.dirty {
                    self.discard_image_doc(None);
                }
            } else {
                self.image.save_name = Some(String::new());
                self.image.toast("give it a name first — then close it");
            }
        }
        if let Some(key) = cmd.open_particle_editor {
            cmd.focus_particles = true;
            self.vfx_ui.open(key);
        }
        if cmd.focus_particles
            && let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::Particles) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::Particles);
                }
            }
        if cmd.focus_animating
            && let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::Animation) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::Animation);
                }
            }
        if cmd.focus_anim_graph
            && let Some(dock) = self.dock_state.as_mut() {
                if let Some(path) = dock.find_tab(&EditorTab::AnimGraph) {
                    let _ = dock.set_active_tab(path);
                } else {
                    dock.push_to_focused_leaf(EditorTab::AnimGraph);
                }
            }
        if let Some((children, parent)) = cmd.reparent {
            self.reparent_many(&children, parent);
        }
        if let Some((matter, parent)) = cmd.add_parented {
            self.add_parented(matter, parent);
        }
        if cmd.paint_fill {
            // Same target routing as Clear: filling VERTEX blocks while the UI says
            // ▦ Texture would silently stomp vertex work.
            if self.vertex_brush.target == crate::paint_ui::PaintTarget::Texture {
                self.tex_fill_selected();
            } else {
                self.paint_fill_selected();
            }
        }
        if cmd.paint_clear {
            // Texture target → drop the painted texture (back to the original material tex);
            // Vertex target → clear the per-vertex colors.
            if self.vertex_brush.target == crate::paint_ui::PaintTarget::Texture {
                for e in self.selection.clone() {
                    self.clear_texture_paint(e);
                }
            } else {
                self.paint_clear_selected();
            }
        }
        if cmd.open_new_terrain {
            self.new_terrain_cfg = Some(NewTerrainCfg::default());
        }
        if let Some(cfg) = cmd.create_terrain {
            self.create_terrain(&cfg);
            self.focus_terrain();
        }
        if let Some(parent) = cmd.add_camera {
            self.add_camera_node(parent);
        }
        if let Some((path, setting)) = cmd.set_texture_setting.take() {
            self.apply_texture_setting(&path, setting);
        }
        if let Some(e) = cmd.set_active_camera {
            self.set_active_camera(e);
        }
        if let Some(e) = cmd.camera_from_view {
            self.camera_to_view(e);
        }
        if cmd.clear_terrain {
            let nodes: Vec<Entity> = self.terrains.keys().copied().collect();
            if !nodes.is_empty() {
                self.record();
                for e in nodes {
                    self.world.despawn(e);
                }
                self.terrains.clear();
                self.active_terrain = None;
                self.terrain_gpu_dirty = true;
            }
        }
        if cmd.terrain_palette_changed {
            self.terrain_textures_dirty = true;
        }
        if let Some(fill) = cmd.fill_terrain
            && let Some(e) = self.target_terrain() {
                // Snapshot for undo (one step), then fill the whole field. Fills only
                // modify EXISTING chunks, so the stored set is the exact undo cover.
                let id = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&e) {
                    let undo = t.field.snapshot_chunks(&t.field.all_chunk_coords());
                    self.push_history(Snapshot::Terrain(id, undo));
                }
                if let Some(t) = self.terrains.get_mut(&e) {
                    match fill {
                        TerrainFill::Color(c) => t.field.fill_color(c),
                        TerrainFill::Texture(slot) => t.field.fill_texture(slot),
                    }
                    t.rebuild_shadow();
                    self.terrain_gpu_dirty = true;
                }
            }
        if cmd.fill_bounds
            && let Some(e) = self.target_terrain() {
                let id = match self.world.get::<Matter>(e) {
                    Some(Matter::Terrain { id }) => *id,
                    _ => 0,
                };
                if let Some(t) = self.terrains.get(&e) {
                    // Fill-bounds may CREATE chunks inside the bounds box — cover the
                    // stored set plus that box so undo can also REMOVE them.
                    let mut cand = t.field.all_chunk_coords();
                    if let Some((lo, hi)) = t.field.bounds() {
                        let pad = t.field.band() + 2.0 * t.field.voxel();
                        cand.extend(t.field.chunks_in_world_box(
                            lo - Vec3::splat(pad),
                            hi + Vec3::splat(pad),
                        ));
                        cand.sort_unstable();
                        cand.dedup();
                    }
                    let undo = t.field.snapshot_chunks(&cand);
                    self.push_history(Snapshot::Terrain(id, undo));
                }
                let (top, floor, inset, color) = (
                    self.terrain_brush.fill_top,
                    self.terrain_brush.fill_floor,
                    self.terrain_brush.fill_inset,
                    self.terrain_brush.color,
                );
                if let Some(t) = self.terrains.get_mut(&e) {
                    // Mirror cover = chunks present BEFORE ∪ AFTER the fill, so
                    // chunks the fill removed clear from the sim copy too.
                    let mut coords = t.field.all_chunk_coords();
                    t.field.fill_bounds(top, floor, inset, color);
                    t.rebuild_shadow();
                    self.terrain_gpu_dirty = true;
                    coords.extend(t.field.all_chunk_coords());
                    coords.sort_unstable();
                    coords.dedup();
                    self.mirror_terrain_chunks_to_sim(e, &coords);
                }
            }
        if cmd.focus_terrain {
            self.focus_terrain();
        }
        if cmd.focus_tiles {
            // The tab AND the tool: reaching the Tiles tab and finding the pointer
            // still on Select is the "why is nothing painting" moment, and it is
            // avoidable with one line.
            if let Some(dock) = self.dock_state.as_mut() {
                crate::dock::focus_tiles_tab(dock);
            }
            self.tool = Tool::Tiles;
            // …and make the node you came from the layer, since that is the one you
            // were looking at when you pressed the button.
            if let Some(e) = self.primary()
                && matches!(self.world.get::<Matter>(e), Some(Matter::Tilemap { .. }))
            {
                self.tile_tools.layer = Some(e);
            }
        }
        if cmd.focus_image {
            self.focus_image_tab();
        }
        if cmd.focus_map {
            self.focus_map();
        }
        if cmd.focus_packages
            && let Some(dock) = self.dock_state.as_mut()
        {
            crate::dock::focus_packages_tab(dock);
        }
        if cmd.reset_layout {
            self.dock_state = Some(crate::dock::default_dock());
            // Throw the saved file away too, not just this session's state. A
            // reset that only lasts until the next crash is not a reset — and
            // "reset the layout" is the thing somebody reaches for precisely
            // when the editor is misbehaving.
            crate::layout::forget_dock();
        }
        if cmd.reset_window {
            crate::layout::forget_window();
            if let Some(window) = self.window.as_ref() {
                let d = crate::layout::WindowPlace::default();
                window.set_maximized(false);
                let _ = window.request_inner_size(winit::dpi::LogicalSize::new(d.width, d.height));
            }
        }
        if let Some(path) = cmd.open_scene {
            // Opening a scene ends any play session FIRST — Stop restores the
            // pre-Play scene (name, world, terrain), so the unsaved-changes
            // prompt and its save below operate on real edit state, never on
            // play-simulation state or a mid-play `scene.load(...)`'s scene.
            if self.playing {
                self.toggle_play();
            }
            // Opening a scene replaces the world — prompt first if there are unsaved
            // edits, otherwise switch immediately.
            if self.scene_dirty {
                self.pending_open_scene = Some(path);
            } else {
                self.open_scene_file(&path);
            }
        }
        if let Some(path) = cmd.open_prefab {
            // Same shape as opening a scene, for the same reason: this replaces
            // the world (`floptle/0090`).
            if self.playing {
                self.toggle_play();
            }
            if self.scene_dirty {
                self.pending_open_scene = Some(path);
            } else {
                self.open_prefab_file(&path);
            }
        }
        if let Some((path, save_first)) = cmd.do_open_scene {
            if save_first {
                self.save_all();
            }
            if crate::assets::is_prefab(&path) {
                self.open_prefab_file(&path);
            } else {
                self.open_scene_file(&path);
            }
        }
        if cmd.open_new_scene {
            self.new_scene_buf = Some(String::new());
        }
        if let Some((e, kind, func)) = cmd.run_editor_action {
            self.run_editor_action(e, &kind, &func);
        }
        // Adopt any finished background planet generations. (The runtime queue
        // DRAINS earlier in the frame — before residency, see render_frame's
        // ordering comment; editor actions drain inside `run_editor_action`.)
        self.poll_terrain_generates();
        if let Some(name) = cmd.new_scene {
            self.new_scene(&name);
        }
        if cmd.refresh_assets {
            self.asset_tree = build_assets(&self.project_root);
            self.anim.rescan(&self.project_root);
            self.vfx.rescan(&self.project_root);
            self.anim_ui.probes.clear(); // re-probe model animation lists
        }
        if let Some(dir) = cmd.new_folder_in {
            self.new_folder(&dir);
        }
        if let Some(dir) = cmd.new_script_in {
            self.new_script(&dir);
        }
        if let Some(dir) = cmd.new_shader_in {
            self.new_shader(&dir);
            // The graph tab's ✚ New: show the fresh shader on the canvas too
            // (the naming modal from new_shader stays up over it).
            if cmd.new_shader_to_graph
                && let Some((p, _)) = self.rename_target.clone()
            {
                self.open_shader_in_graph(&p);
            }
        }
        if let Some(path) = cmd.rename_asset {
            // Seed the rename modal with the current base name (the extension is shown as a
            // fixed suffix in the modal, so you edit just the name).
            let p = Path::new(&path);
            // Seed with the BASE name (up to the first dot) — the modal shows
            // the rest as a fixed suffix, compound extensions included.
            let full = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let name = if p.is_dir() {
                full
            } else {
                full.split('.').next().unwrap_or_default().to_string()
            };
            self.rename_target = Some((path, name));
        }
        if let Some((from, to)) = cmd.do_rename {
            self.rename_asset(&from, &to);
        }
        if let Some(paths) = cmd.delete_asset {
            // Deleting files/folders is irreversible — always confirm first.
            self.delete_confirm = Some(paths);
        }
        if let Some(paths) = cmd.do_delete_asset {
            self.delete_assets(&paths);
        }
        if let Some((sources, dest)) = cmd.move_assets {
            self.move_assets(&sources, &dest);
        }
        if let Some((sources, dest)) = cmd.import_files {
            self.import_files(&sources, &dest);
        }
        if let Some(dir) = cmd.pick_import_dir {
            self.open_import_dialog(dir);
        }
        // Drain a completed native import dialog (see open_import_dialog).
        if let Some((rx, dir)) = &self.import_rx {
            match crate::native_dialog::poll(rx) {
                crate::native_dialog::Answer::Waiting => {}
                crate::native_dialog::Answer::Chose(files) => {
                    let dir = dir.clone();
                    self.import_rx = None;
                    self.import_files(&files, &dir);
                }
                crate::native_dialog::Answer::Closed => self.import_rx = None,
            }
        }
        if let Some((roots, dir)) = cmd.save_prefab {
            self.save_prefab(&roots, &dir);
        }
        if let Some((path, parent)) = cmd.instantiate_prefab {
            // No parent = place in front of the camera (like Add-menu nodes);
            // with a parent, the authored root transform is the local offset.
            let at = parent.is_none().then(|| {
                let cam = self.camera.render_camera();
                cam.world_position + (cam.rotation * Vec3::NEG_Z * 5.0).as_dvec3()
            });
            self.instantiate_prefab(&path, at, parent);
        }
        if let Some(dir) = cmd.open_folder {
            // Empty path = "the project root" (the File-menu shortcut).
            let target = if dir.as_os_str().is_empty() { self.project_root.clone() } else { dir };
            crate::project::open_in_file_manager(&target);
        }
        if let Some(send) = cmd.crash_report {
            if let Some(note) = self.crash_prompt.take()
                && send
            {
                crate::open_issue_tracker(Some(&note));
            }
            self.crash_prompt = None;
        }
        if let Some(restore) = cmd.autosave_action {
            if restore {
                self.restore_autosave();
            } else if let Some(auto) = self.autosave_prompt.take() {
                let _ = std::fs::remove_file(auto);
            }
        }
        // Pre-warm a model being dragged so its live ghost can render next frame
        // (the gather can't import — gpu/raster are borrowed there).
        if let Some(p) =
            self.egui.as_ref().and_then(|e| egui::DragAndDrop::payload::<AssetPayload>(&e.ctx))
            && is_model(&p.path) && !self.mesh_registry.contains_key(&p.path) {
                let path = p.path.clone();
                self.import_model(&path);
            }
    }

    /// **Register every texture this scene's materials name, before the gather
    /// asks for them.**
    ///
    /// A gather cannot do it itself — `gpu`/`raster` are borrowed there — so it
    /// resolves a material's texture by looking the path up in the registry, and
    /// a path that never got here comes back `None`. `None` does not draw
    /// nothing: it means "no override", so the mesh's OWN imported texture draws
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
    /// said nothing about it. It belongs to the FRAME, and both paths call it.
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
        // Node Materials AND per-object override materials — an override's
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
        // The SURFACE MAPS too — normal, roughness, metallic, occlusion. They go
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
    fn ensure_vfx_assets(&mut self) {
        let mut tex: Vec<String> = Vec::new();
        let mut meshes: Vec<String> = Vec::new();
        let push = |v: &mut Vec<String>, p: &str| {
            if !p.is_empty() && !v.iter().any(|q| q == p) {
                v.push(p.to_string());
            }
        };
        // The open working doc first (it holds edits not yet in the registry).
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



    /// Render the whole scene from `cam` (at `aspect`) into offscreen color+depth views —
    /// the shared body behind the Inspector camera preview and the split-view Game render.
    /// `cull_mask` is the rendering camera's layer bitmask (bit i = project
    /// layer i; `u32::MAX` = everything). `skip_tex` excludes one material
    /// texture from resolution — a target camera must not sample its OWN
    /// render target mid-pass (wgpu forbids attachment+sampled in one pass).
    /// The scene-colour history a given slot owns, if it has one.
    fn history_slot(&self, slot: HistorySlot) -> Option<&floptle_render::SceneHistory> {
        match slot {
            HistorySlot::None => None,
            HistorySlot::GamePanel => self.game_scene_history.as_ref(),
        }
    }

    /// Allocate, resize or drop an offscreen view's stored picture to match what
    /// it is being asked for. Returns whether the texture behind it changed.
    ///
    /// Sized to the COMPOSITED resolution it is handed, so a docked Game panel
    /// reflects at the resolution it is drawn at — and, in retro mode, at the
    /// retro resolution, exactly as the window does. A reflection sharper than
    /// the picture around it reads as a bug in the picture.
    fn sync_offscreen_history(
        &mut self,
        slot: HistorySlot,
        want: bool,
        size: (u32, u32),
    ) -> bool {
        if slot == HistorySlot::None {
            return false;
        }
        let Some(gpu) = self.gpu.as_ref() else { return false };
        let fmt = gpu.scene_format();
        let (pw, ph) = (size.0.max(1), size.1.max(1));
        let dev = &gpu.device;
        let hist = match slot {
            HistorySlot::None => return false,
            HistorySlot::GamePanel => &mut self.game_scene_history,
        };
        if !want {
            return hist.take().is_some();
        }
        match hist.as_mut() {
            Some(h) => h.resize_to(dev, pw, ph, fmt),
            None => {
                *hist = Some(floptle_render::SceneHistory::new(dev, pw, ph, fmt));
                true
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_world_into(
        &mut self,
        color: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        cam: &RenderCamera,
        aspect: f32,
        elapsed: f32,
        cull_mask: u32,
        skip_tex: Option<TexId>,
        // The target's pixel size. Explicit because only a VIEW is passed and a
        // view cannot be asked how big it is — and the 2D lighting G-buffer has
        // to match the frame exactly or the composite lands stretched.
        size: (u32, u32),
        opts: OffscreenOpts<'_>,
    ) {
        // The same pre-warm the window frame does. This path is reached without
        // one by `floptle shot` and by any embedder driving the editor headless,
        // and a gather here resolves textures through exactly the same registry:
        // without this, a shot of a scene draws every model in the texture it
        // was IMPORTED with, whatever its materials say.
        self.ensure_scene_textures();
        let view_proj = cam.view_proj(aspect);
        // Layer names resolve to bits only when a mask actually culls.
        let layer_table = (cull_mask != u32::MAX).then(|| self.project.build_layers());
        // Read from the scene's own PostProcess node rather than taken as an
        // argument: every caller of this path — the Game view, a render target, a
        // thumbnail — is showing the same scene, and posterize is that scene's
        // palette. A parameter would be one more thing three call sites have to
        // remember to pass the same way.
        let palette = crate::shading::post_process_uniforms(&self.world).0.palette();

        // Sky-shader uniforms (Inspector knobs over `.flsl` defaults), resolved before any
        // GPU borrow — the offscreen / Game render reuses the same values as the editor view.
        let sky_active = self.sky_shader.is_some();
        let sky_uniform_vals = self.sky_uniform_values();

        let light_node = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
        let sun = crate::shading::sun_vec(&self.world, &light_node, cam.world_position);
        let li = light_node.intensity;
        // Whether `Auto` reads as 2D in this scene, asked once rather than per node.
        let flat_camera = floptle_core::active_camera(&self.world).is_some_and(|ce| {
            matches!(self.world.get::<Matter>(ce), Some(Matter::Camera { ortho: true, .. }))
        });
        // ONE split, both halves — the 3D slots the globals want and the 2D ones
        // the gather filters by. This path used to walk the scene's lights twice
        // (`collect_point_lights` here, and again to build the 2D uniform at the
        // pass), which is half of what `floptle/0122` measured.
        let off_split = crate::shading::split_point_lights(
            &self.world,
            cam.world_position,
            &self.project.sorting_order(),
            flat_camera,
        );
        let lit3 = off_split.three_d;
        let (pl_count, pl_pos, pl_col, pl_shape, pl_rot, pl_cone) = (
            [lit3.count as f32, 0.0, 0.0, 0.0],
            lit3.pos,
            lit3.color,
            lit3.shape,
            lit3.rot,
            lit3.cone,
        );
        // Same question and same answer as the window path: what a reflective
        // surface sees when the screen-space march finds nothing.
        let (probe_meta, probe_pos, probe_half) = crate::reflect_capture::probe_uniforms(
            &self.world,
            &self.probe_slots,
            self.capturing_probes,
            cam.world_position,
            crate::shading::reflection_clamp(&light_node),
        );
        // Any lamp casting here? Same question and same answer as the window
        // path — a local shadow marches the prepass, so this decides whether one
        // has to run.
        let point_shadows = lit3.shape[..lit3.count.min(16)]
            .iter()
            .any(|s| (s[3] as u32) & 2 != 0);
        let (sh_params, sh_tint, sh_extra) = shadow_uniforms(&light_node);
        let contact = crate::shading::contact_uniform(&light_node);
        let ((fog_color, fog_params), particle_fog) =
            crate::shading::fog_uniforms_and_particles_at(&light_node, &self.world, cam.world_position);
        let (atmo_meta, atmo_color, atmo_body, atmo_params) =
            crate::shading::atmo_uniforms(&self.world, cam.world_position);
        let (star_meta, star_pos, star_color) =
            crate::shading::star_uniforms(&self.world, &light_node, cam.world_position);
        // Proxies are what lets a raster mesh cast at all, and a LAMP marches the
        // same list now — so the sun's switch alone can no longer decide whether
        // they are collected. A scene with the sun's shadows off and a torch
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
            // Meshed terrain reads the triplanar scale + the per-slot NEAREST /
            // GLOW bitmasks here (bitmasks as u32 — bit-exact at 32 slots).
            terrain_mask: [0.0, 0.22, 0.0, 0.0],
            terrain_bits: [
                crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root),
                self.terrain_glow_mask,
                0,
                0,
            ],
        };

        // Camera-relative instances + blobs, exactly like the main gather —
        // including the frustum cull, built from THIS camera's matrix.
        let off_frustum = floptle_render::Frustum::from_view_proj(view_proj);
        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        // Per-node paint, resolved BEFORE the draw loop (which borrows `raster`
        // mutably, so it can't call &self helpers). This path renders the world too, so
        // painted props must look identical here. Empty for unpainted scenes.
        // Every node's sorting-layer Z, resolved before the draw loop borrows
        // `raster` mutably. Empty for a scene that uses no sorting layers, which
        // is every scene until one opts in. (`flat_camera` was asked above, with
        // the light split that needs it.)
        let sort_z = crate::sprite2d::draw_offsets(&self.world, &self.project, cam.world_position);

        // The same one value the pass is handed below, from the same split — and
        // the same helper the Scene view uses, so this view cannot decide a
        // different set of lit surfaces from that one (`floptle/0122`).
        let lights_2d = light2d_uniform(&self.world, &off_split.two_d, view_proj);
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
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        // The 2D lighting G-buffer's draw list, built in THIS loop from the very
        // instances the raster pass gets (`Light2dInstance::from_raster`). That
        // is the whole mitigation for deferred's second draw path: there is no
        // second walk of the world to keep in step.
        let mut flat2d: Vec<(MeshId, Option<TexId>, floptle_render::Light2dInstance)> = Vec::new();
        // GPU-skinned parts (`floptle/0080`), gathered alongside the plain ones and
        // drawn through the skinned pipelines in the same passes.
        let mut skin_draws: Vec<floptle_render::SkinDraw> = Vec::new();
        // Custom `.flsl` materials draw offscreen too (bindings were refreshed
        // by ensure_flsl_materials before any gather this frame).
        let mut flsl_draws: Vec<floptle_render::FlslDraw> = Vec::new();
        let mut blobs: Vec<(DVec3, f32, MaterialParams)> = Vec::new();
        // Reused scratch for CPU vertex skinning (deformed vertices, re-uploaded per part),
        // exactly like the main gather — so offscreen views animate skinned meshes too.
        let mut skin_scratch: Vec<floptle_render::Vertex> = Vec::new();
        // How much the frustum cull skipped, published below alongside the rest
        // of this gather's counts — see `floptle/0167`: this whole gather used
        // to publish nothing, so a Game-view session's `perf.counts()` was
        // whatever the Scene view had last computed, or all zero if it never
        // ran this session.
        let mut culled_nodes = 0usize;
        for (ent, matter) in &ents {
            if matches!(self.world.get::<floptle_core::Visible>(*ent), Some(floptle_core::Visible(false))) {
                continue;
            }
            if floptle_core::is_disabled(&self.world, *ent) {
                continue;
            }
            // Camera cull mask: skip nodes on layers this camera doesn't render.
            if let Some(lt) = &layer_table
                && (cull_mask >> lt.index_for(&self.world, *ent)) & 1 == 0
            {
                continue;
            }
            let mut t = floptle_core::world_transform(&self.world, *ent);
            // …and the same nudge here, or the Game view would sort differently
            // from the Scene view — the drift this file has already had three
            // times over.
            t.translation += sort_z.get(ent).copied().unwrap_or_default();
            // …and the same cull the screen uses (`floptle/0075`), against THIS
            // camera's frustum. An offscreen target that culled differently from
            // the window would be a mirror showing a different room.
            // A pixels-per-unit sprite is drawn at its TEXTURE's size, not at
            // its `size` field, so culling has to know the texture — otherwise a
            // sixteen-unit sprite is culled on a radius of half a unit and pops
            // out of existence at the edge of the screen.
            let sprite_px = matches!(matter, Matter::Sprite { .. })
                .then(|| {
                    let p = self.world.get::<Material>(*ent)?.texture.clone()?;
                    let id = self.texture_registry.get(&p).copied()?;
                    self.raster.as_ref()?.texture_size(id)
                })
                .flatten();
            if crate::node_bounds::node_is_off_screen(
                &self.world, &self.mesh_registry, &self.anim.poses,
                *ent, matter, &t, cam.world_position, &off_frustum, sprite_px,
            ) {
                culled_nodes += 1;
                continue;
            }
            let mat = self.world.get::<Material>(*ent).cloned();
            // Texture-painted node → ALSO push its paint overlay; the base draws normally
            // below (see the main path).
            if self.world.get::<floptle_core::TexturePaint>(*ent).is_some() {
                let model = t.render_matrix(cam.world_position);
                let mp = mat.as_ref().map(material_params).unwrap_or_else(|| MaterialParams::flat([1.0, 1.0, 1.0]));
                crate::paint_tex::push_painted_node(&self.world, &self.paint_tex, *ent, model, &mp, &mut instances);
            }
            let tex = mat
                .as_ref()
                .and_then(|m| m.texture.as_deref())
                .and_then(|p| self.texture_registry.get(p).copied())
                .filter(|id| Some(*id) != skip_tex);
            let flsl = self.flsl_binds.get(ent).map(|b| b.binding);
            // Where this node's draws begin, for the tint stamp after the match.
            let tint_from = (instances.len(), flsl_draws.len(), skin_draws.len(), flat2d.len());
            match matter {
                // Same helper as the main gather, vertex paint and all — this
                // arm used to build its own instance and forgot the paint, so a
                // painted cube was painted in the Scene view and plain in the
                // Game view.
                Matter::Primitive { shape, color } => {
                    let model = t.render_matrix(cam.world_position);
                    if let Some((mesh, raw)) = primitive_draw(
                        *shape,
                        *color,
                        mat.as_ref(),
                        model,
                        &self.mesh_ids,
                        paint_bases.get(ent).map(|v| v.as_slice()),
                        self.raster.as_ref(),
                    ) {
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                // …and water, which this arm claimed was drawn by the raymarch
                // and is not: it is a raster instance, and only the Scene view's
                // gather ever built one. An ocean was there while you edited the
                // scene and gone the moment you looked through the game's own
                // camera.
                Matter::WaterVolume { .. } => {
                    if let Some((mesh, raw)) = water_draw(
                        matter,
                        mat.as_ref(),
                        &t,
                        cam.world_position,
                        &self.mesh_ids,
                        self.raster.as_mut(),
                    ) {
                        match flsl {
                            Some(b) => flsl_draws.push((mesh, tex, b, raw)),
                            None => instances.push((mesh, tex, raw)),
                        }
                    }
                }
                Matter::Blob { scale } => {
                    let mp = mat.as_ref().map(material_params).unwrap_or_else(blob_default_material);
                    blobs.push((t.translation, scale * t.scale.x, mp));
                }
                Matter::Mesh { asset_path } => {
                    // Same animated/skinned gather as the main surface path (shared
                    // helper) — a docked/split Game view or camera preview must show the
                    // character moving, not frozen in bind pose. gpu/raster are freshly
                    // borrowed here (disjoint fields; the loop's earlier world/texture
                    // borrows already produced owned values).
                    if let (Some(gpu), Some(raster), Some(asset)) = (
                        self.gpu.as_ref(),
                        self.raster.as_mut(),
                        self.mesh_registry.get(asset_path),
                    ) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params);
                        let obj_mats = self.world.get::<floptle_core::ObjectMaterials>(*ent);
                        let pose = self.anim.poses.get(ent).map(|v| v.as_slice());
                        let node_paint = paint_bases.get(ent).map(|v| v.as_slice());
                        push_mesh_instances(
                            gpu, raster, asset, pose, model, tex, mp.as_ref(), obj_mats,
                            &self.texture_registry, node_paint,
                            *ent, &mut self.skin_variants,
                            &mut skin_scratch, &mut instances, &mut skin_draws, flsl,
                            &mut flsl_draws,
                        );
                    }
                }
                // Blockout geometry, through the same per-part path as imported
                // models (parts = material slots). Without this arm the Game
                // view — and every camera preview / render target, which all
                // come through here — drew the level as empty air while the
                // Scene view showed it fine.
                Matter::MapMesh { id } => {
                    if let (Some(gpu), Some(raster), Some(asset)) = (
                        self.gpu.as_ref(),
                        self.raster.as_mut(),
                        self.mesh_registry.get(&crate::map_edit::map_key(*id)),
                    ) {
                        let model = t.render_matrix(cam.world_position);
                        let mp = mat.as_ref().map(material_params);
                        let obj_mats = self.world.get::<floptle_core::ObjectMaterials>(*ent);
                        let node_paint = paint_bases.get(ent).map(|v| v.as_slice());
                        push_mesh_instances(
                            gpu, raster, asset, None, model, tex, mp.as_ref(), obj_mats,
                            &self.texture_registry, node_paint,
                            *ent, &mut self.skin_variants,
                            &mut skin_scratch, &mut instances, &mut skin_draws, flsl,
                            &mut flsl_draws,
                        );
                    }
                }
                // The 2D layer, for exactly the reason the arm above it exists.
                // A tilemap and a sprite batch were gathered only by the main
                // surface pass, so every view that comes through here — the
                // docked or split Game view, a camera preview, any render
                // target — drew a 2D game as an empty background while the
                // Scene view showed the level. Which is to say a 2D game was
                // invisible in the one view that is the game.
                Matter::Tilemap { .. } => {
                    let model = t.render_matrix(cam.world_position);
                    let mut draws = Vec::new();
                    crate::sprite2d::tilemap_draws(
                        &self.tilemaps,
                        &self.texture_registry,
                        *ent,
                        model,
                        mat.as_ref(),
                        tex,
                        &mut draws,
                    );
                    for mut draw in draws {
                        // On the 2D lighting path: the raster pass draws it
                        // UNLIT, and the composite corrects that by the light's
                        // difference (`floptle/0121`). The G-buffer instance is
                        // taken from the very same value, so the two cannot
                        // disagree about what is being corrected.
                        if let Some(&(rank, casts)) = lit2d.get(ent) {
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
                        let px = self
                            .raster
                            .as_ref()
                            .zip(tex)
                            .and_then(|(r, id)| r.texture_size(id));
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
                        if let Some(&(rank, casts)) = lit2d.get(ent) {
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
                        let texel = self
                            .raster
                            .as_ref()
                            .zip(tex)
                            .and_then(|(r, id)| r.texture_size(id))
                            .map(|[w, h]| [1.0 / w.max(1.0), 1.0 / h.max(1.0)])
                            .unwrap_or([0.0, 0.0]);
                        let mut raws = Vec::new();
                        crate::sprite2d::sprite_draws(
                            &self.world, *ent, *size, model, mat.as_ref(), texel, &mut raws,
                        );
                        for mut raw in raws {
                            // …and the same for a sprite batch: unlit in the
                            // raster pass, corrected by the difference.
                            if let Some(&(rank, casts)) = lit2d.get(ent) {
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
                // Listed rather than `_ => {}`, and that is the point. This
                // match silently dropped Tilemap and SpriteBatch for two
                // releases, and had already done the same to MapMesh before
                // them — a catch-all arm cannot be reviewed, and the next kind
                // of matter would have joined them without a word. Naming each
                // one makes the compiler ask.
                //
                // Everything below is drawn somewhere else in THIS function or
                // is not drawable at all:
                Matter::Terrain { .. } => {} // push_terrain_instances, further down
                Matter::FieldShape { .. } => {} // the raymarch pass
                Matter::Skybox { .. } => {}      // skybox_uniforms → the sky stage
                Matter::PointLight { .. } => {}  // collect_point_lights, into globals
                Matter::Camera { .. } => {}      // it is the eye, not a thing seen
                Matter::PostProcess { .. } => {} // post_process_uniforms
                Matter::LightProbes { .. } => {} // baked GI: uniforms + one texture
                // Drawn as an outline in the Scene view, and nothing at all in
                // the game: a navmesh is a thing to path on, not to look at.
                Matter::NavMesh { .. } | Matter::NavLink { .. } | Matter::NavArea { .. } => {}
                // The capture is six renders of its own, taken elsewhere; here
                // it is four uniform lanes and one texture, like the GI above.
                Matter::ReflectionProbe { .. } => {}
                Matter::GravityVolume { .. } => {} // physics only; no visual
                Matter::Empty => {}              // a transform with nothing on it
            }
            // …and the same tint the window path applies, through the same
            // function: a model tinted in the Scene view and plain in the Game
            // view is the drift this file's test exists to catch.
            apply_node_tint(
                self.world.get::<floptle_core::Tint>(*ent),
                tint_from,
                &mut instances,
                &mut flsl_draws,
                &mut skin_draws,
                &mut flat2d,
            );
        }

        let (sky_params, sky_tint, sky_rot, sky_solid) = skybox_uniforms(&self.world);
        let clear = [sky_solid[0], sky_solid[1], sky_solid[2], 1.0];
        // SDF AO from the scene's PostProcess node shades SDF matter in offscreen
        // views too (previews + the split Game viewport).
        let (_, rm_ao_params) = post_process_uniforms(&self.world);
        let terrain_mat = self.terrain_material();
        // Terrain 2.0 (P2): the meshed terrain draws in this offscreen/Game view too, or a
        // docked Game viewport would show empty ground (its volume is `w = 3`, not drawn by
        // the raymarch). Same instance push as the main Scene view.
        if let Some(raster) = self.raster.as_ref() {
            crate::terrain_edit::push_terrain_instances(
                &self.terrain_render,
                &self.terrains,
                &self.world,
                raster,
                &terrain_mat,
                cam.world_position,
                view_proj,
                self.mesh_ids[floptle_core::Shape::Sphere as usize],
                self.now(),
                &mut instances,
            );
        }
        // …and the counts a game can read via `perf.counts()` (`floptle/0077`,
        // `floptle/0167`). This gather used to publish none of this: every view
        // that comes through it — the docked or split Game view, `floptle
        // shot`, a render target — left the profile holding whatever the
        // Scene-view gather in `render()` had last written, or all zero if that
        // gather never ran this session. That is exactly why a real 40-light
        // scene read `lights=0` in one session and correctly in another: the
        // number was never this camera's, it was whichever gather happened to
        // run last. `lights`/`lightsDropped` come from `off_split` above,
        // computed for THIS camera and THIS frame.
        self.light_counts = (off_split.three_d.count + off_split.two_d.count, off_split.dropped);
        self.warn_lights_dropped(off_split.dropped);
        {
            let chunks: usize = self.terrain_render.values().map(|r| r.slots.len()).sum();
            let particles = self.vfx.live_particles();
            let (effects, effects_dropped) = self.vfx.detached_counts();
            let (lights, lights_dropped) = self.light_counts;
            let voices = self.audio.live_voices();
            let profile = self.script_host.profile().clone();
            // Same "off means off" guard as the main gather — see there.
            let draws = if profile.borrow().enabled() {
                count_draw_batches(&instances, &flsl_draws, &skin_draws)
            } else {
                0
            };
            profile.borrow_mut().set_counts(floptle_core::profile::Counts {
                nodes: ents.len(),
                culled: culled_nodes,
                instances: instances.len(),
                draws,
                chunks,
                // This gather does not draw scatter (unlike the Scene view's),
                // so a scatter-heavy scene under-reports its props here. A
                // separate, real gap — not this card — see the ledger.
                props: 0,
                particles,
                effects,
                effects_dropped,
                lights,
                lights_dropped,
                voices,
                flat2d: flat2d.len(),
            });
        }
        let show_blobs = self.project.matter && !blobs.is_empty();
        // A textured skybox is DRAWN by the raymarch pass (missed rays sample the
        // sky) — keep it running even with no terrain/blobs in the scene.
        let rm_draw = show_blobs
            || !self.terrains.is_empty()
            || sky_params[0] >= 0.5
            || self.sky_shader.is_some() // a procedural sky shader must run the raymarch (sky pass)
            || !self.flsl_shape_slots.is_empty();
        // Reflections in an offscreen view, on exactly the terms the window gets
        // them: this view's own stored picture, allocated the first frame it is
        // asked for and dropped when it stops being. A view with no history slot
        // (a thumbnail, a bake) reports off and reflects the sky, which is what
        // it did before any of this existed.
        let want_ssr = light_node.reflections
            && opts.history != HistorySlot::None
            && opts.depth_tex.is_some();
        // Glass wants the same picture for the opposite reason — see the window
        // path. Asked of the raster's material store, which is shared, so the
        // answer is the same one the window path gets for the same scene.
        let glass = opts.history != HistorySlot::None
            && self.raster.as_ref().is_some_and(|r| r.any_transmissive(&instances));
        let ssr_rebuilt = self.sync_offscreen_history(opts.history, want_ssr || glass, size);
        let history = self.history_slot(opts.history);
        let ssr = crate::shading::ssr_uniform(
            &light_node,
            want_ssr && history.is_some_and(|h| h.is_primed()),
        );
        let ssr_prev_vp = history
            .and_then(|h| h.prev_view_proj(cam.world_position))
            .unwrap_or(floptle_core::math::Mat4::IDENTITY)
            .to_cols_array_2d();
        // Cloned out here because the draw block below borrows `self.raster` and
        // `self.raymarch` mutably, and the history lives on `self` too. A view
        // and a sampler are both refcounted handles, so this is two bumps.
        let history_bind =
            history.map(|h| (h.view().clone(), h.sampler().clone()));
        let _ = ssr_rebuilt;
        let rm = {
            let mut arr = [[0.0f32; 4]; 16];
            let n = blobs.len().min(16);
            if show_blobs {
                for (i, (c, s, _)) in blobs.iter().take(16).enumerate() {
                    let cr = (*c - cam.world_position).as_vec3();
                    arr[i] = [cr.x, cr.y, cr.z, s.max(0.05)];
                }
            }
            let (blob_tint, blob_emissive, blob_specular, blob_params, blob_rim) =
                if show_blobs { blob_mat_arrays(&blobs) } else { blob_mat_arrays(&[]) };
            let tm = &terrain_mat;
            let (vol_fog_a, vol_fog_b, vol_fog_c) =
                vol_fog_uniforms(&light_node, self.fog_time, cam.world_position.y as f32);
            let mut g = RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                light_dir: sun,
                light_color: [light_node.color[0] * li, light_node.color[1] * li, light_node.color[2] * li, 0.0],
                ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
                bg: [clear[0], clear[1], clear[2], 1.0],
                center: [0.0; 4],
                params: [elapsed, if show_blobs { n as f32 } else { 0.0 }, 0.0, 0.0],
                vol_center: [[0.0; 4]; 16],
                vol_half: [[1.0, 1.0, 1.0, 0.5]; 16],
                vol_atlas: [[0.0; 4]; 16],
                vol_dims: [[1.0, 1.0, 1.0, 0.0]; 16],
                // .w = per-slot NEAREST mask (bit i = slot i is Pixelated). The palette
                // is one texture_2d_array with one sampler, so the shader can't pick a
                // sampler per slot — it reads this mask and selects the result instead.
                terrain_tint: [
                    tm.color[0],
                    tm.color[1],
                    tm.color[2],
                    // Legacy raymarch path packs the mask in an f32 lane — exact for
                    // slots 0..23 only; the meshed raster path uses u32 terrain_bits.
                    crate::terrain_edit::terrain_nearest_mask(&self.terrain_textures, &self.texture_settings, &self.project_root)
                        as f32,
                ],
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
                vol_fog_a,
                vol_fog_b,
                vol_fog_c,
                contact,
                ssr,
                ssr_prev_vp,
                probe_meta,
                probe_pos,
                probe_half,
                atmo_meta,
                atmo_color,
                atmo_body,
                atmo_params,
                star_meta,
                star_pos,
                star_color,
                // vol_tight_* are renderer-patched at draw time (default: unbounded).
                ..Default::default()
            };
            // Sky shader in the offscreen / Game view too.
            if sky_active {
                g.sky_meta = [1.0, 0.0, 0.0, 0.0];
                g.sky_uniforms = sky_uniform_vals;
            }
            Self::fill_terrain_volumes(&self.terrains, &self.terrain_slots, &self.mesh_occluders, &self.occluder_slots, &self.world, &mut g, cam.world_position);
            crate::shaders::apply_field_shapes(&self.world, &self.flsl_shape_slots, &self.sdf_cache, &mut g, cam.world_position, None);
            if let Some(rmarch) = self.raymarch.as_ref() {
                rmarch.gi().apply(&mut g, cam.world_position.into());
            }
            g
        };

        // Live particles render in offscreen views too (the split Game viewport
        // must show what the game shows).
        let vfx_preview_on = !self.playing
            && self
                .dock_state
                .as_ref()
                .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Particles));
        let mut vfx_instances: Vec<floptle_render::ParticleInstance> = Vec::new();
        let mut vfx_batches: Vec<floptle_render::ParticleBatch> = Vec::new();
        self.vfx.collect(
            &self.world,
            cam,
            &self.texture_registry,
            vfx_preview_on,
            &mut vfx_instances,
            &mut vfx_batches,
        );
        let vfx_mesh_draws = self.vfx.collect_mesh_draws(&self.world, cam, vfx_preview_on);
        resolve_mesh_particles(&self.mesh_registry, &vfx_mesh_draws, &mut instances);

        if let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(particles),
            Some(line_layer),
            Some(tri_layer),
        ) = (
            self.gpu.as_ref(),
            self.raster.as_mut(),
            self.raymarch.as_mut(),
            self.particles.as_mut(),
            self.line_layer.as_mut(),
            self.tri_layer.as_mut(),
        ) {
            // Everything below is the draw. If the tuple above does not match,
            // the `else` at the end of it says so — see there.
            // The opaque depth prepass, HERE as well as on the window path.
            // Contact shadows, `surfaceGap`, screen-space reflections and lamp
            // shadows all read it, and without it every one of them silently
            // does nothing — which is exactly how a docked Game panel came to
            // look different from the same game fullscreen.
            //
            // It runs when something actually reads it, and needs the depth
            // TEXTURE (a view cannot be copied into), so a caller that has none
            // opts out by construction rather than by forgetting.
            let wants_depth = wants_prepass(
                raster.flsl_draws_want_depth(&flsl_draws),
                ssr[0] > 0.5,
                point_shadows,
                contact[0] > 0.5,
            );
            if let Some(dtex) = opts.depth_tex.filter(|_| wants_depth || rm_draw) {
                let hist = history_bind.as_ref().map(|(v, s)| (v, s));
                prepass_and_bind(
                    gpu, raster, raymarch, globals, &instances, &flsl_draws, &skin_draws,
                    dtex, hist,
                );
            } else {
                // No prepass this view: unbind, or this render would march the
                // LAST view's depth buffer — a different camera at a different
                // size, which is worse than marching nothing.
                raymarch.bind_frame_targets(gpu, None, None);
            }
            let raster_clear = if rm_draw {
                raymarch.draw_into(gpu, color, depth, rm);
                None
            } else {
                // Nothing to raymarch, but the raster field group still needs this
                // frame's shadow/proxy data (mesh-only scenes cast via proxies).
                raymarch.upload_globals(gpu, rm);
                Some(clear.map(|c| c as f64))
            };
            raster.draw_scene_with(
                gpu, color, depth, globals, &instances, &flsl_draws, &skin_draws,
                raster_clear, Some(raymarch.field_bind()),
            );
            // The palette quantize, before the light — the same order the surface
            // path uses, and it has to be the same or a docked Game view would
            // posterize its lighting while the Scene view did not (`floptle/0127`,
            // and the two gathers have drifted over exactly this shape before).
            if let Some(q) = palette {
                raster.quantize_palette(gpu, color, (size.0.max(1), size.1.max(1)), q);
            }
            raster.light2d_pass(
                gpu,
                color,
                depth,
                (size.0.max(1), size.1.max(1)),
                view_proj.to_cols_array_2d(),
                &lights_2d,
                &flat2d,
            );
            // Glass, on the same terms and in the same place as the window
            // path: capture what is behind, then draw the things you can see
            // through. `self.game_scene_history` is a different field from the
            // ones this block borrows, so it is reachable from inside it.
            if glass
                && let Some(h) = match opts.history {
                    HistorySlot::None => None,
                    HistorySlot::GamePanel => self.game_scene_history.as_mut(),
                }
            {
                let mut glass_rm = rm;
                glass_rm.ssr_prev_vp = view_proj.to_cols_array_2d();
                raymarch.upload_globals(gpu, glass_rm);
                let cuts = raster.transmissive_cuts(
                    &instances,
                    &skin_draws,
                    light_node.refraction_layers,
                );
                for layer in 0..=cuts.len() {
                    h.capture(gpu, color, view_proj, cam.world_position);
                    if layer == 0 {
                        raymarch.bind_frame_targets(
                            gpu,
                            raster.prepass_view(),
                            Some((h.view(), h.sampler())),
                        );
                    }
                    raster.draw_transmissive(
                        gpu, color, depth, globals, &instances, &skin_draws,
                        Some(raymarch.field_bind()), &cuts, layer,
                    );
                }
            }
            // Script-drawn 3D lines (draw.line — the map's orbit conics).
            if !self.script_lines.is_empty() {
                let verts: Vec<floptle_render::LineVertex> = self
                    .script_lines
                    .iter()
                    .flat_map(|l| {
                        let a = (DVec3::from(l.a) - cam.world_position).as_vec3();
                        let b = (DVec3::from(l.b) - cam.world_position).as_vec3();
                        [
                            floptle_render::LineVertex { pos: [a.x, a.y, a.z], color: l.color },
                            floptle_render::LineVertex { pos: [b.x, b.y, b.z], color: l.color },
                        ]
                    })
                    .collect();
                line_layer.draw(gpu, color, depth, view_proj, &verts);
            }
            // Script-drawn FILLED triangles (draw.tri/cone/disc — solid gizmos).
            if !self.script_tris.is_empty() {
                let verts: Vec<floptle_render::TriVertex> = self
                    .script_tris
                    .iter()
                    .flat_map(|t| {
                        let a = (DVec3::from(t.a) - cam.world_position).as_vec3();
                        let b = (DVec3::from(t.b) - cam.world_position).as_vec3();
                        let c = (DVec3::from(t.c) - cam.world_position).as_vec3();
                        [
                            floptle_render::TriVertex { pos: [a.x, a.y, a.z], color: t.color },
                            floptle_render::TriVertex { pos: [b.x, b.y, b.z], color: t.color },
                            floptle_render::TriVertex { pos: [c.x, c.y, c.z], color: t.color },
                        ]
                    })
                    .collect();
                tri_layer.draw(gpu, color, depth, view_proj, &verts);
            }
            if !vfx_batches.is_empty() {
                particles.draw(
                    gpu,
                    color,
                    depth,
                    crate::vfx::particle_globals(cam, aspect, fog_color, particle_fog),
                    &vfx_instances,
                    &vfx_batches,
                    raster,
                );
            }
        } else {
            // **A view that cannot draw says so.** This binds six pieces of the
            // device at once, and until now a missing one meant the whole render
            // silently did nothing — the caller got a valid, entirely black
            // frame and no reason for it. That is how `floptle shot` shipped its
            // first picture: 960x540 of black, exit 0.
            //
            // Once, not per view per frame: a device that is missing a piece is
            // missing it for good, and sixty lines a second would bury it.
            if !self.warned_incomplete_device {
                self.warned_incomplete_device = true;
                let missing = [
                    ("gpu", self.gpu.is_none()),
                    ("raster", self.raster.is_none()),
                    ("raymarch", self.raymarch.is_none()),
                    ("particles", self.particles.is_none()),
                    ("lines", self.line_layer.is_none()),
                    ("tris", self.tri_layer.is_none()),
                ]
                .iter()
                .filter(|(_, missing)| *missing)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ");
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!(
                        "nothing can be drawn: this renderer was never given {missing}.                          Everything a scene render needs is set up in `Editor::init_gpu_side`."
                    ),
                    None,
                );
            }
        }
        // Keep this view's picture, for its own next frame's reflections. Same
        // place in the order as the window path: everything belonging to the
        // scene has drawn and nothing that does not has started.
        if let (Some(gpu), HistorySlot::GamePanel) = (self.gpu.as_ref(), opts.history)
            && let Some(h) = self.game_scene_history.as_mut()
        {
            h.capture(gpu, color, view_proj, cam.world_position);
        }
    }
}

/// Gather one `Matter::Mesh`'s draw instances. Rigged meshes animate: each part
/// either rides its (possibly animated) node rigidly (R6-style), or — for a TRUE
/// vertex-skinned part (Ty) — is CPU-deformed by this frame's bone palette, its
/// vertices re-uploaded, and drawn at the mesh matrix. `pose` is the node's animated
/// world matrices (falls back to the rig rest pose). Static (unrigged) meshes just
/// draw every part at `model`.
///
/// Does this view need the opaque depth prepass to run?
///
/// **One function, called from both render paths.** Every feature that reads the
/// prepass silently does nothing without it — no error, no warning, just a
/// picture missing something — so a view that forgets one term is a view where
/// that feature quietly stops existing. The two paths had already drifted: the
/// window's condition was missing CONTACT SHADOWS, so in a scene made of meshes
/// with reflections and lamp shadows both off, contact shadows worked in a
/// docked Game panel and did nothing in the window beside it.
///
/// Adding a feature that reads the prepass means adding a parameter here, which
/// is a compile error at both call sites rather than a silent omission at one.
fn wants_prepass(flsl_wants_depth: bool, ssr: bool, point_shadows: bool, contact: bool) -> bool {
    flsl_wants_depth || ssr || point_shadows || contact
}

/// Run this view's opaque depth prepass **and bind it**, in one call.
///
/// One call because the two have to happen together and had twice been written
/// apart: first the bind was inside the `if rm_draw` arm, so every feature that
/// reads the prepass silently did nothing in a scene made of meshes; then it was
/// guarded on "was the target reallocated?", which is permanently false once a
/// frame draws two views, so the window drew with the docked Game panel's depth
/// buffer and stored picture and reflections came and went.
///
/// Both are the same mistake — a per-view resource bound less often than per
/// view — and neither errors. Running and binding cannot be separated here, so
/// they are not separable at the call site either.
#[allow(clippy::too_many_arguments)]
fn prepass_and_bind(
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    raymarch: &mut floptle_render::Raymarch,
    globals: floptle_render::Globals,
    instances: &[(MeshId, Option<TexId>, floptle_render::InstanceRaw)],
    flsl: &[floptle_render::FlslDraw],
    skins: &[floptle_render::SkinDraw],
    depth_tex: &wgpu::Texture,
    history: Option<(&wgpu::TextureView, &wgpu::Sampler)>,
) {
    raster.depth_prepass_with(gpu, globals, instances, flsl, skins, depth_tex);
    raymarch.bind_frame_targets(gpu, raster.prepass_view(), history);

}

/// Multiply a node's [`Tint`](floptle_core::Tint) into everything it pushed
/// into this frame.
///
/// A function rather than a loop written twice because the Scene view and
/// `render_world_into` both have to do it: a kind of drawing tinted on one path
/// and not the other is a model that is red while you edit it and plain in the
/// game, which is the drift `offscreen_draws_the_same_world` exists to catch.
///
/// `from` is where this node's instances START — everything after it belongs to
/// this node and nothing before it does.
pub(crate) fn apply_node_tint(
    tint: Option<&floptle_core::Tint>,
    from: (usize, usize, usize, usize),
    instances: &mut [(MeshId, Option<TexId>, InstanceRaw)],
    flsl_draws: &mut [floptle_render::FlslDraw],
    skin_draws: &mut [floptle_render::SkinDraw],
    // The 2D lighting G-buffer. A flat node on the lit path draws UNLIT in the
    // raster pass and is corrected by the light composite, which reads THIS
    // copy of the colour — so a tint applied only to the raster instance is
    // corrected back out again by a pass that never heard about it.
    flat2d: &mut [(MeshId, Option<TexId>, floptle_render::Light2dInstance)],
) {
    let Some(t) = tint.filter(|t| !t.is_identity()) else { return };
    for (_, _, raw) in &mut instances[from.0..] {
        t.apply(&mut raw.color);
    }
    for (_, _, _, raw) in &mut flsl_draws[from.1..] {
        t.apply(&mut raw.color);
    }
    for d in &mut skin_draws[from.2..] {
        t.apply(&mut d.instance.color);
    }
    for (_, _, lit) in &mut flat2d[from.3..] {
        t.apply(&mut lit.tint);
    }
}

/// How many draw calls this frame's meshes cost (`floptle/0167`).
///
/// `Counts::draws` was a literal `0` — never computed, always answering the
/// question it exists for with a lie. `draw_scene_with` (`floptle-render`)
/// buckets by `(mesh, texture[, flsl binding])` and issues one instanced
/// `draw_indexed` per bucket, opaque and blended kept apart; this counts the
/// same groups without duplicating that bucketing GPU-side. It folds opaque
/// and blended together, so a group that has instances in both phases is one
/// real draw call counted as one here — a coarse number a game can act on
/// beats the `0` it replaces. Terrain chunks, particle batches and 2D/UI
/// batches are their own passes with their own counts already (`chunks`,
/// `particles`) and are not included here.
fn count_draw_batches(
    instances: &[(MeshId, Option<TexId>, InstanceRaw)],
    flsl: &[floptle_render::FlslDraw],
    skins: &[floptle_render::SkinDraw],
) -> usize {
    let mut mesh_tex: std::collections::HashSet<(u32, Option<u32>)> = std::collections::HashSet::new();
    for (mesh, tex, _) in instances {
        mesh_tex.insert((mesh.0, tex.map(|t| t.0)));
    }
    let mut flsl_groups: std::collections::HashSet<(u32, Option<u32>, u32)> =
        std::collections::HashSet::new();
    for (mesh, tex, bind, _) in flsl {
        flsl_groups.insert((mesh.0, tex.map(|t| t.0), bind.0));
    }
    let mut skin_groups: std::collections::HashSet<(u32, Option<u32>)> = std::collections::HashSet::new();
    for s in skins {
        skin_groups.insert((s.mesh.0, s.tex.map(|t| t.0)));
    }
    mesh_tex.len() + flsl_groups.len() + skin_groups.len()
}

/// **Which material one part of a model draws with.**
///
///   this object's override  ▸  the node's Material  ▸  the part as imported
///
/// The most specific one wins, WHOLE — its colour, its texture, its maps, its
/// retro flags. Its own function because that sentence is the contract, and it
/// used to be three-quarters true: a node Material multiplied its colour into
/// each part's imported colour while its texture replaced outright, so a model
/// given a new material kept the old picture on it and only the emissive
/// appeared to work. A rule that applies half a material is not a rule anybody
/// can predict, and this is the place it is stated once.
pub(crate) enum PartLook<'a> {
    /// This sub-object's own override material.
    Override(&'a floptle_core::Material),
    /// The node-level Material, over every part of the model.
    Node(&'a MaterialParams),
    /// Nothing supersedes: the part's imported base colour (and, at the draw,
    /// its imported texture).
    Imported([f32; 3]),
}

pub(crate) fn part_look_rule<'a>(
    obj_mats: Option<&'a floptle_core::ObjectMaterials>,
    override_key: Option<&str>,
    // The glTF MATERIAL this part was imported with — the other name the same
    // part answers to. See below for why both.
    material_name: Option<&str>,
    node_material: Option<&'a MaterialParams>,
    imported_base: [f32; 3],
) -> PartLook<'a> {
    // **A part answers to its object name AND to its material name.**
    //
    // The object name is the precise one — it addresses ONE sub-object — but it
    // is not the name anybody has. Import de-duplicates repeated node names, so
    // an avatar whose torso node is called `Torso` in Blender is keyed `Torso#2`
    // here, and an override written as `Torso` matched nothing at all and said
    // nothing about it.
    //
    // The material name is the one on the model's own materials list, the one a
    // glTF author chose, and usually the one that means something across the
    // parts: a character's `Clothing` covers the torso and both arms, which is
    // exactly the grouping a clothing system wants to address at once.
    //
    // Object first, so the precise name still wins where both exist.
    if let Some(om) = obj_mats {
        if let Some(m) = override_key.and_then(|k| om.0.get(k)) {
            return PartLook::Override(m);
        }
        if let Some(m) = material_name.and_then(|k| om.0.get(k)) {
            return PartLook::Override(m);
        }
    }
    match node_material {
        Some(m) => PartLook::Node(m),
        None => PartLook::Imported(imported_base),
    }
}

/// Shared by the main surface gather AND the offscreen `render_world_into` so the
/// fullscreen, docked, split, and camera-preview views all animate identically —
/// previously the offscreen path drew every mesh rigidly at its root, so a character
/// looked frozen whenever the Game view wasn't the fullscreen/focused one.
#[allow(clippy::too_many_arguments)]
fn push_mesh_instances(
    gpu: &floptle_render::Gpu,
    raster: &mut floptle_render::Raster,
    asset: &MeshAsset,
    pose: Option<&[Mat4]>,
    model: Mat4,
    tex: Option<TexId>,
    // The node-level Material's params (None = the node has no Material — parts
    // fall back to their imported base-color factor, matching runtime builds).
    mp: Option<&MaterialParams>,
    // Per-SUB-OBJECT material overrides (the `ObjectMaterials` component) + the
    // texture registry to resolve their texture paths (pre-warmed each frame).
    obj_mats: Option<&floptle_core::ObjectMaterials>,
    texture_registry: &HashMap<String, TexId>,
    // This node's own per-part paint bases (the brush's work). `None` → fall back to
    // whatever the mesh imported with, so Blender paint still shows on unpainted nodes.
    node_paint: Option<&[u32]>,
    // The drawing entity + the per-entity skinned-buffer cache: each entity bakes
    // its pose into its OWN clone of a skinned part's vertex buffer, so instances
    // of one model animate independently.
    entity: Entity,
    variants: &mut anim::SkinVariants,
    skin_scratch: &mut Vec<floptle_render::Vertex>,
    instances: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
    // GPU-skinned parts land here instead of `instances` (`floptle/0080`): same
    // mesh, same material, but drawn through the `vs_skin` pipelines with this
    // draw's bone palette. Several characters of one model stay ONE draw call,
    // which the CPU path could not manage — it had to give each entity a private
    // vertex buffer to bake its pose into.
    skins: &mut Vec<floptle_render::SkinDraw>,
    flsl: Option<floptle_render::FlslBindingId>,
    flsl_out: &mut Vec<floptle_render::FlslDraw>,
) {
    // A node's custom `.flsl` material routes every part through the shader's
    // pipeline instead of the built-in one — same instance data either way.
    let mut push = |mid: MeshId, ptex: Option<TexId>, raw: InstanceRaw| match flsl {
        Some(b) => flsl_out.push((mid, ptex, b, raw)),
        None => instances.push((mid, ptex, raw)),
    };
    // **A part's look, by one rule: the most specific material wins, whole.**
    //
    //   this object's override  ▸  the node's Material  ▸  the part as imported
    //
    // Whichever of those applies is the material, entire — its colour, its
    // texture, its maps, its retro flags. A material is a statement of what a
    // surface looks like, and half-applying one is what made this confusing:
    // the node Material used to MULTIPLY its colour into each part's imported
    // colour while its texture replaced outright, so "I gave it a new material
    // and it still has the old picture on it, but the emissive works" was the
    // exact and correct description of what the engine did.
    //
    // A model that looks right therefore carries no node Material at all. One is
    // how you say "this whole model is made of THIS" — and per-object overrides
    // are how you say it about one part.
    let part_look = |raster: &mut floptle_render::Raster,
                         asset: &MeshAsset,
                         part: usize|
     -> (Option<TexId>, MaterialParams) {
        // The part's own imported base-colour factor — its share of the model's
        // built-in look, which is what draws when nothing supersedes it.
        let base = asset.part_meta.get(part).map(|pm| pm.base_color).unwrap_or([1.0; 3]);
        let mat_name = asset.part_meta.get(part).map(|pm| pm.material.as_str());
        match part_look_rule(obj_mats, asset.override_key(part), mat_name, mp, base) {
            // An override is a whole material, surface maps and retro flags
            // included — resolved the same way a node's own Material is, so
            // "give this one object a normal map" works.
            PartLook::Override(m) => {
                let (t, p) = crate::shading::material_draw(raster, gpu, m, texture_registry, None);
                (Some(t.unwrap_or_else(|| raster.white_texture(gpu))), p)
            }
            // `tex` is this node Material's own texture. `None` there does NOT
            // mean "keep what the part had" — a bind of `None` is what makes the
            // MESH's texture draw, which is the imported look this material is
            // superseding. An untextured material means untextured, so it says
            // so with white.
            PartLook::Node(m) => (Some(tex.unwrap_or_else(|| raster.white_texture(gpu))), *m),
            PartLook::Imported(base) => (tex, MaterialParams::flat(base)),
        }
    };
    // Vertex paint is per-PART: import splits a model per-material into parts with
    // their own vertex arrays, so each part owns its own paint block. Instances of a
    // part share its base — same block, same draw call.
    let painted = |raster: &floptle_render::Raster, mid: MeshId, part: usize, base: MaterialParams| {
        let mut m = base;
        let brush = node_paint.and_then(|p| p.get(part).copied()).filter(|&b| b != 0);
        // Brush paint modulates 2× (paint light AND shadow); imported glTF COLOR_0 stays a
        // plain ×1 multiply, per the glTF convention (white = identity).
        m.paint_modulate = brush.is_some();
        m.paint_base = brush.unwrap_or_else(|| raster.mesh_paint_base(mid));
        m
    };
    let Some(rig) = asset.rig.as_ref() else {
        for (i, &mid) in asset.parts.iter().enumerate() {
            let (ptex, pmp) = part_look(raster, asset, i);
            push(mid, ptex, instance_of_mat(model, &painted(raster, mid, i, pmp)));
        }
        return;
    };
    let node_world = pose.unwrap_or(rig.rest_world.as_slice());
    for (i, &mid) in asset.parts.iter().enumerate() {
        let part_node = rig.part_nodes.get(i).copied().unwrap_or(0);
        let (ptex, pmp) = part_look(raster, asset, i);
        if let Some(Some(skin)) = rig.skins.get(i) {
            let raw = instance_of_mat(model, &painted(raster, mid, i, pmp));
            let skin_base = rig.skin_bases.get(i).copied().unwrap_or(0);
            // A custom `.flsl` material routes the part through its own pipeline,
            // which has no skinned variant — those parts keep the CPU deform.
            if skin_base != 0 && flsl.is_none() {
                // GPU skinning: hand the pose over and draw the SHARED bind-pose
                // buffer. `push_skin_pose` is the same arithmetic `cpu_skin_part`
                // applies per vertex, done once per draw instead of once per vertex.
                let palette: Vec<Mat4> = skin
                    .joint_nodes
                    .iter()
                    .zip(&skin.inverse_bind)
                    .map(|(&jn, ib)| node_world.get(jn).copied().unwrap_or(Mat4::IDENTITY) * *ib)
                    .collect();
                let fallback = node_world.get(part_node).copied().unwrap_or(Mat4::IDENTITY);
                let pose = raster.push_skin_pose(skin_base, fallback, &palette);
                skins.push(floptle_render::SkinDraw { mesh: mid, tex: ptex, instance: raw, pose });
            } else {
                // Fallback: the skinning store refused this part (it is bounded by
                // the instance lane that addresses it), or a custom shader owns the
                // draw. CPU-skin into this ENTITY's private clone, as before —
                // paint lives in `vpaint`, keyed by vertex_index, so the re-upload
                // can't stomp it, and paint/texture lookups stay on `mid`.
                let draw_mid = variants.variant_for(gpu, raster, entity, i, mid);
                anim::cpu_skin_part(skin, part_node, node_world, skin_scratch);
                raster.update_mesh_vertices(gpu, draw_mid, skin_scratch);
                push(draw_mid, ptex, raw);
            }
        } else {
            let local = node_world.get(part_node).copied().unwrap_or(Mat4::IDENTITY);
            push(mid, ptex, instance_of_mat(model * local, &painted(raster, mid, i, pmp)));
        }
    }
}

/// Resolve mesh-particle draws to raster instances (camera-relative model matrix
/// plus alpha-aware tinted material) and append them to `instances`. Free function
/// so callers pass just `&mesh_registry`, a disjoint field borrow, while `gpu` and
/// `raster` are held by the main render's destructure.
fn resolve_mesh_particles(
    mesh_registry: &HashMap<String, MeshAsset>,
    draws: &[floptle_vfx::MeshDraw],
    instances: &mut Vec<(MeshId, Option<TexId>, InstanceRaw)>,
) {
    for md in draws {
        let Some(asset) = mesh_registry.get(&md.asset_path) else { continue };
        for (model, color) in &md.instances {
            let mut mp = MaterialParams::flat([color[0], color[1], color[2]]);
            mp.alpha = color[3];
            let raw = instance_of_mat(*model, &mp);
            for &mid in &asset.parts {
                instances.push((mid, None, raw));
            }
        }
    }
}

/// What the ⏱ readout draws, copied out of the profile before the UI closure
/// (`floptle/0077`).
///
/// A snapshot rather than a borrow because the UI closure runs inside the frame's
/// split borrows and the profile is also being written this frame — and because a
/// readout that could change halfway through drawing itself would show a bucket
/// total that disagreed with the rows under it.
struct PerfSnapshot {
    on: bool,
    frames: u64,
    buckets: Vec<(&'static str, floptle_core::profile::Cost)>,
    scripts: Vec<(String, floptle_core::profile::Cost)>,
    accounted_ms: f32,
    counts: floptle_core::profile::Counts,
    /// How the frames are actually arriving, and whether dt snapping is
    /// managing to do anything about it (`floptle/0160`).
    pacing: Pacing,
}

/// What the display path is doing, as opposed to what the scene costs.
///
/// The two get confused constantly — "my game runs at 300 fps and stutters" is
/// almost never a scene that is slow — so they are reported side by side and
/// named differently.
#[derive(Clone, Copy, Default)]
struct Pacing {
    /// Smoothed frame time, ms.
    mean_ms: f32,
    /// 99th-percentile frame time over the last couple of seconds, ms.
    p99_ms: f32,
    /// The display's refresh period in ms, or 0 if nothing could be read.
    refresh_ms: f32,
    /// Share of recent frames the dt snap actually applied to, 0..1.
    snap_rate: f32,
    /// Smoothed time blocked inside `acquire` (`Editor::present_wait_ms`) —
    /// the display path, not the scene. `floptle/0169`: the piece the ⏱ panel
    /// had the numbers for and never compared.
    present_wait_ms: f32,
    /// `mean_ms - present_wait_ms`: what the frame cost apart from waiting on
    /// the display. The same subtraction the window-title `cost` figure does.
    cost_ms: f32,
}

impl PerfSnapshot {
    fn take(p: &floptle_core::profile::FrameProfile) -> Self {
        Self {
            pacing: Pacing::default(),
            on: p.enabled(),
            frames: p.frames(),
            buckets: floptle_core::profile::Bucket::ALL
                .into_iter()
                .map(|b| (b.name(), p.bucket(b).unwrap_or_default()))
                .collect(),
            scripts: p.scripts(),
            accounted_ms: p.accounted_ms().unwrap_or(0.0),
            counts: p.counts(),
        }
    }
}

#[cfg(test)]
mod readout_tests {
    /// **The readout has to smooth frame time, not its reciprocal.**
    ///
    /// The acceptance case from `floptle/0160`: a frame sequence alternating
    /// 2 ms and 30 ms. Sixty-two frames a second are genuinely arriving (16 ms
    /// mean), and an EMA over `1.0 / dt` reports something near 265 — it spends
    /// half its samples at 500 fps and a reciprocal does not average.
    ///
    /// The real capture was worse: bursts of 0.08 ms frames between 16 ms blocks
    /// read as 4312 fps against a true 144.
    #[test]
    fn the_fps_readout_averages_frame_time_not_its_reciprocal() {
        let seq: Vec<f32> = (0..400)
            .map(|i| if i % 2 == 0 { 0.002 } else { 0.030 })
            .collect();
        let true_fps = seq.len() as f32 / seq.iter().sum::<f32>();
        assert!((true_fps - 62.5).abs() < 0.1, "the sequence really is ~62 fps: {true_fps}");

        // What the readout does now: smooth the time, invert at the end.
        let mut frame_ms = 0.0f32;
        for &dt in &seq {
            let ms = dt * 1000.0;
            frame_ms = if frame_ms > 0.0 { frame_ms * 0.9 + ms * 0.1 } else { ms };
        }
        let reported = 1000.0 / frame_ms;

        // What it used to do: smooth the reciprocal.
        let mut old = 0.0f32;
        for &dt in &seq {
            let inst = 1.0 / dt;
            old = if old > 0.0 { old * 0.9 + inst * 0.1 } else { inst };
        }

        assert!(
            (reported - true_fps).abs() < 6.0,
            "reported {reported} against a true {true_fps}"
        );
        assert!(
            old > 200.0,
            "the old formula really did overstate this badly — if it doesn't, this \
             test is no longer measuring the bug it was written for (got {old})"
        );
    }

    /// The 1% low has to SEE the slow frames. A mean cannot, which is the whole
    /// reason it is reported beside one.
    #[test]
    fn the_one_percent_low_reports_the_worst_frames_not_the_average() {
        let mut ed = crate::Editor::default();
        // 99 good frames and one 40 ms hitch, repeated — the shape that adds
        // well under a millisecond to an average and is the only thing anybody
        // is ever chasing.
        for i in 0..500 {
            ed.record_frame_time(if i % 100 == 99 { 40.0 } else { 6.9 });
        }
        let low = ed.frame_time_low();
        assert!(low > 30.0, "the hitch has to be visible in the 1% low, got {low}");

        // And a steady stream reports a steady low — it must not manufacture a
        // spike out of an even distribution.
        let mut steady = crate::Editor::default();
        for _ in 0..500 {
            steady.record_frame_time(6.9);
        }
        assert!((steady.frame_time_low() - 6.9).abs() < 0.01);
    }

    /// The 16-light cap says so ONCE per count, not every frame, and says
    /// nothing while nothing is being cut (`floptle/0168`).
    #[test]
    fn the_light_cap_warns_once_per_count_and_falls_silent_under_it() {
        fn warns(ed: &crate::Editor) -> usize {
            ed.console
                .entries
                .iter()
                .filter(|e| e.level == floptle_script::LogLevel::Warn)
                .count()
        }
        let mut ed = crate::Editor::default();
        // Each `ed.frame_no += 1` moves to a new simulated frame.
        // `render_world_into` runs several times per `render()` (Game view,
        // camera previews, a GI bake), each a different camera, so two calls
        // at the SAME frame_no simulate two cameras in one frame — which must
        // not each get their own say.

        ed.frame_no += 1;
        ed.warn_lights_dropped(0);
        assert_eq!(warns(&ed), 0, "nothing was cut — nothing to say");

        ed.frame_no += 1;
        ed.warn_lights_dropped(24);
        assert_eq!(warns(&ed), 1, "24 dropped lights must say so");
        // SAME frame, a second camera reporting a DIFFERENT count (an
        // orthographic minimap beside a perspective main camera, say) — must
        // not be read as the count "changing" and re-warn.
        ed.warn_lights_dropped(30);
        assert_eq!(warns(&ed), 1, "a second gather in the SAME frame must not get its own warning");

        ed.frame_no += 1;
        ed.warn_lights_dropped(24);
        assert_eq!(warns(&ed), 1, "the same count again must not repeat itself every frame");

        ed.frame_no += 1;
        ed.warn_lights_dropped(30);
        assert_eq!(warns(&ed), 2, "MORE lights dropped is new information and re-warns");

        ed.frame_no += 1;
        ed.warn_lights_dropped(0);
        assert_eq!(warns(&ed), 2, "back under the cap — quiet again");
        ed.frame_no += 1;
        ed.warn_lights_dropped(24);
        assert_eq!(warns(&ed), 3, "over the cap a second time must warn again, not stay latched off");
    }
}

/// Which refresh period to hold, given what the platform just said.
///
/// Pulled out of [`Editor::reread_refresh_period`] because it is the whole of
/// `floptle/0160`'s second half and it cannot be tested through a real window.
///
/// * `held` — what we already believe (0 = nothing yet).
/// * `current` — `current_monitor()`'s refresh in mHz, if it answered.
/// * `any` — a monitor, any monitor, in mHz. Only consulted when nothing is
///   known at all, because on a mixed-refresh desktop it is a guess.
fn chosen_refresh_period(held: f32, current: Option<u32>, any: impl FnOnce() -> Option<u32>) -> f32 {
    if let Some(mhz) = current.filter(|&m| m > 0) {
        return 1000.0 / mhz as f32;
    }
    // A `None` means "ask again", not "there is no display" — mapping it onto
    // 0.0 is what switched dt snapping off for a whole session.
    if held > 0.0 {
        return held;
    }
    any().filter(|&m| m > 0).map(|mhz| 1000.0 / mhz as f32).unwrap_or(0.0)
}

/// Is the DISPLAY pacing the frame, rather than the scene being slow
/// (`floptle/0169`)?
///
/// The signature `docs/subsystems/renderer.md` already describes: `acquire`
/// blocks for very close to a whole multiple (≥2) of the refresh period —
/// the compositor presenting every Nth vblank rather than every one — while
/// the frame's OWN work (`cost_ms`, the same subtraction the window title's
/// "cost" figure already does) is small next to that wait. A scene that is
/// genuinely heavy can also land near a multiple by coincidence, which is
/// exactly why `cost_ms` is the second half of the test: a real 40 ms scene
/// waiting 50 ms is a slow scene, not this.
///
/// Returns the multiple when detected, so a message can say "every Nth
/// refresh" rather than just "something is off".
fn fifo_pacing_multiple(present_wait_ms: f32, cost_ms: f32, refresh_ms: f32) -> Option<u32> {
    if refresh_ms <= 0.0 || present_wait_ms <= 0.0 {
        return None;
    }
    let n = (present_wait_ms / refresh_ms).round();
    // Same 12% band `smooth_dt` snaps dt within, so "close to a multiple"
    // means the same thing in both places.
    if n < 2.0 || (present_wait_ms - n * refresh_ms).abs() > refresh_ms * 0.12 {
        return None;
    }
    if cost_ms > refresh_ms * 0.5 {
        return None; // the frame is doing real work — this is not a null scene
    }
    Some(n as u32)
}

#[cfg(test)]
mod fifo_pacing_tests {
    use super::fifo_pacing_multiple;

    /// The card's own capture: 50.0 ms `acquire` on a 16.68 ms (59.95 Hz)
    /// refresh — three refreshes, near-zero scene cost either side of it.
    #[test]
    fn the_cards_own_capture_is_detected_as_three_refreshes() {
        assert_eq!(fifo_pacing_multiple(50.0, 0.3, 16.68), Some(3));
    }

    /// An ordinary vsynced frame — `acquire` near ONE refresh — is not this.
    /// One refresh of waiting is just vsync working; the signature is being
    /// held for *more* than the display's own pace warrants.
    #[test]
    fn one_refresh_of_waiting_is_ordinary_vsync_not_the_bug() {
        assert_eq!(fifo_pacing_multiple(16.7, 0.3, 16.68), None);
    }

    /// A GENUINELY slow scene that happens to cost close to two refreshes is
    /// not this — the whole point of the `cost_ms` half of the test.
    #[test]
    fn a_scene_that_is_actually_slow_is_not_reported_as_display_pacing() {
        assert_eq!(fifo_pacing_multiple(33.3, 30.0, 16.68), None);
    }

    /// No known refresh rate, or nothing waited on `acquire`: nothing to say.
    #[test]
    fn nothing_to_compare_against_says_nothing() {
        assert_eq!(fifo_pacing_multiple(50.0, 0.3, 0.0), None);
        assert_eq!(fifo_pacing_multiple(0.0, 0.3, 16.68), None);
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::chosen_refresh_period;

    /// Measured on Ty's machine with `present_stats`: `current_monitor()` is
    /// **NONE** at window creation and becomes `Some(DP-2, 144001)` once the
    /// surface is mapped, while `available_monitors()` reports
    /// `[HDMI-A-1 59951, DP-2 144001]` correctly the entire time.
    #[test]
    fn a_none_from_current_monitor_never_zeroes_a_good_period() {
        let dp2 = Some(144_001);
        let hdmi = || Some(59_951);

        // Startup: nothing known and nothing current. Snapping used to be dead
        // here for 240 frames; a monitor — any monitor — beats no snapping.
        let boot = chosen_refresh_period(0.0, None, hdmi);
        assert!(boot > 0.0, "startup must not come up with snapping switched off");

        // The surface maps and the real output answers.
        let live = chosen_refresh_period(boot, dp2, hdmi);
        // In SECONDS: `refresh_period` is compared against `dt`, not against a
        // millisecond readout. 144.001 Hz -> 6.944 ms.
        assert!((live - 1.0 / 144.001).abs() < 1e-6, "{live}");

        // A later transient None — an output hotplug, a window drag — must KEEP
        // the good value rather than replace it with a guess about the other
        // monitor or with zero.
        let after = chosen_refresh_period(live, None, hdmi);
        assert_eq!(after, live, "a transient None is 'ask again', not 'no display'");

        // And a display that reports nonsense is treated as no answer at all.
        assert_eq!(chosen_refresh_period(live, Some(0), hdmi), live);
        assert_eq!(chosen_refresh_period(0.0, None, || None), 0.0, "genuinely nothing to go on");
    }
}

/// Draw how the frames are ARRIVING, beside what they cost.
///
/// **Two different questions, and an fps number answers neither on its own.** A
/// scene costing 8 ms that presents at 20 fps is a display path pacing the
/// engine; the same 8 ms at 120 fps is the same scene doing fine. And when dt
/// snapping goes inert — the measured frame time stops landing near a whole
/// multiple of the reported refresh, because the window is on a different output
/// than `current_monitor()` names, or nothing is pacing to vblank — the raw
/// scheduler jitter goes straight into the fixed-step accumulator and the render
/// judders by `velocity x noise`. That used to happen in total silence
/// (`floptle/0160`), which is the worst possible way for a load-bearing path to
/// be switched off.
fn pacing_readout(ui: &mut egui::Ui, p: &Pacing) {
    if p.mean_ms <= 0.0 {
        return;
    }
    let warn = egui::Color32::from_rgb(230, 150, 90);
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("frames arriving").strong());
        ui.label(
            egui::RichText::new(format!(
                "{:.2} ms mean   {:.2} ms 1% low   ({:.0} fps)",
                p.mean_ms,
                p.p99_ms,
                1000.0 / p.mean_ms.max(1e-4)
            ))
            .monospace(),
        );
    });
    // A 1% low several times the mean IS the stutter, whatever the fps says.
    if p.p99_ms > p.mean_ms * 2.0 {
        ui.small(
            egui::RichText::new(format!(
                "⚠ the worst 1% of frames take {:.1}x the average. That is what a \
                 stutter is, and an fps number cannot show it.",
                p.p99_ms / p.mean_ms.max(1e-4)
            ))
            .color(warn),
        );
    }
    // The display pacing the frame, not the scene being slow (`floptle/0169`):
    // `acquire` blocked for a whole multiple of the refresh period while the
    // frame's own work barely registers. This used to be indistinguishable
    // from "the scene is heavy" — both numbers were already on screen and
    // never compared.
    if let Some(n) = fifo_pacing_multiple(p.present_wait_ms, p.cost_ms, p.refresh_ms) {
        ui.small(
            egui::RichText::new(format!(
                "⚠ the DISPLAY is pacing this, not the scene: {:.1} ms of every frame is \
                 spent waiting on `acquire` — every {n}th refresh — while the scene itself \
                 costs {:.1} ms. Try Project Settings ⏵ Rendering ⏵ Frame pacing.",
                p.present_wait_ms, p.cost_ms
            ))
            .color(warn),
        );
    }
    if p.refresh_ms <= 0.0 {
        ui.small(
            egui::RichText::new(
                "⚠ no display refresh rate available, so dt snapping is off — frame-time \
                 jitter is reaching the simulation clock unfiltered.",
            )
            .color(warn),
        );
        return;
    }
    let hz = 1000.0 / p.refresh_ms;
    if p.snap_rate >= 0.5 {
        ui.small(format!(
            "dt snapping: on — {:.2} ms refresh ({hz:.4} Hz), applied to {:.0}% of frames.",
            p.refresh_ms,
            p.snap_rate * 100.0
        ));
    } else {
        ui.small(
            egui::RichText::new(format!(
                "⚠ dt snapping is inert — the display reports {:.2} ms ({hz:.4} Hz) but frames \
                 are arriving every {:.2} ms, so they aren't landing on whole refreshes and the \
                 snap can't apply (it caught {:.0}% of them). Usually the window is on a \
                 different output than the one being reported, or the present mode isn't pacing \
                 to vblank. Frame-time jitter is reaching the simulation clock.",
                p.refresh_ms,
                p.mean_ms,
                p.snap_rate * 100.0
            ))
            .color(warn),
        );
    }
}

/// Draw the frame-cost readout.
///
/// Two columns per row on purpose: the rolling mean AND the worst frame of the
/// last second. The spike is what anybody is ever chasing, and a mean hides it —
/// a 40 ms hitch once a second adds under a millisecond to a 60-frame average.
fn perf_readout(ui: &mut egui::Ui, s: &PerfSnapshot) {
    if !s.on {
        ui.label("Not collecting.");
        ui.small(
            "Collection is off by default because a profiler that costs a frame is a \
             profiler people turn off. Close and reopen this panel to start.",
        );
        return;
    }
    if s.frames == 0 {
        ui.label("Measuring — numbers appear next frame.");
        return;
    }
    ui.small(
        "worst = the worst single frame in the last second. That is the column to \
         read; a hitch is invisible in an average.",
    );
    ui.add_space(4.0);
    pacing_readout(ui, &s.pacing);
    ui.add_space(4.0);
    egui::Grid::new("perf-buckets").num_columns(3).striped(true).show(ui, |ui| {
        ui.label(egui::RichText::new("").strong());
        ui.label(egui::RichText::new("avg ms").strong());
        ui.label(egui::RichText::new("worst ms").strong());
        ui.end_row();
        for (name, c) in &s.buckets {
            ui.label(*name);
            ui.label(egui::RichText::new(format!("{:6.2}", c.ms)).monospace());
            // The worst column carries the colour, since it is the one being read.
            let hot = c.worst_ms > 8.0;
            let text = egui::RichText::new(format!("{:6.2}", c.worst_ms)).monospace();
            ui.label(if hot { text.color(egui::Color32::from_rgb(230, 150, 90)) } else { text });
            ui.end_row();
        }
        ui.label(egui::RichText::new("accounted").weak());
        ui.label(egui::RichText::new(format!("{:6.2}", s.accounted_ms)).monospace().weak());
        ui.label("");
        ui.end_row();
    });
    // "accounted", not "total": vsync, the OS and the GPU finishing are outside
    // every bucket, and a readout claiming to add up to the frame time without
    // doing so is worse than one that never claimed it.
    ui.small("accounted = these buckets added up. Not the frame time — vsync, the OS and the GPU finishing are outside all of them.");

    ui.add_space(6.0);
    ui.separator();
    // BY SCRIPT NAME. The whole point: "scripts: 6 ms" does not answer "which of
    // my scripts is doing this".
    ui.label(egui::RichText::new("per script").strong());
    if s.scripts.is_empty() {
        ui.small("No scripts have run since collection started.");
    } else {
        egui::Grid::new("perf-scripts").num_columns(3).striped(true).show(ui, |ui| {
            for (name, c) in s.scripts.iter().take(12) {
                ui.label(name);
                ui.label(egui::RichText::new(format!("{:6.2}", c.ms)).monospace());
                ui.label(egui::RichText::new(format!("{:6.2}", c.worst_ms)).monospace());
                ui.end_row();
            }
        });
        if s.scripts.len() > 12 {
            ui.small(format!("…and {} more, cheaper", s.scripts.len() - 12));
        }
    }

    ui.add_space(6.0);
    ui.separator();
    // Counts, because three of the four "the engine is slow" tickets were
    // answerable from one of these alone.
    ui.label(egui::RichText::new("counts").strong());
    let c = &s.counts;
    ui.label(
        egui::RichText::new(format!(
            "{} nodes ({} off screen)\n{} instances, {} draws\n{} terrain chunks\n{} scatter props\n{} particles",
            c.nodes, c.culled, c.instances, c.draws, c.chunks, c.props, c.particles
        ))
        .monospace(),
    );
    ui.add_space(4.0);
    ui.small("All of this is readable from Lua as perf.* — assert a budget in a smoke test rather than waiting for a player to notice.");
}

#[cfg(test)]
mod lit_2d_tests {
    use super::*;
    use floptle_core::{Lighting2D, Lit2D, Sorting, World};

    /// **A material applies whole, or it is not the material.**
    ///
    /// Reported as: "I override a model's material, my new material has a
    /// texture, but it is still showing the texture of the normal model — and
    /// if I change the emission I can see the object get brighter." That is one
    /// rule applied to three quarters of a material: the node Material's params
    /// were taken (hence the emission), its colour was MULTIPLIED into the
    /// part's imported colour, and its texture replaced only if it had one.
    ///
    /// The rule is now: the most specific material wins, entire.
    #[test]
    fn the_most_specific_material_wins_whole() {
        use crate::render_frame::{PartLook, part_look_rule};

        let imported = [0.2, 0.4, 0.6];
        // A node Material that says nothing but its colour. It still supersedes.
        let node = MaterialParams::flat([1.0, 0.0, 0.0]);

        match part_look_rule(None, Some("Torso#2"), Some("Clothing"), Some(&node), imported) {
            PartLook::Node(m) => assert_eq!(
                m.color,
                [1.0, 0.0, 0.0],
                "a node Material is the model's look, not a tint over it — multiplying it \
                 into {imported:?} is what made a new material keep the old look"
            ),
            _ => panic!("a node Material must supersede the imported look"),
        }

        // Nothing on the node: the part keeps exactly what it was imported with.
        match part_look_rule(None, Some("Torso#2"), Some("Clothing"), None, imported) {
            PartLook::Imported(c) => assert_eq!(c, imported),
            _ => panic!("with no Material anywhere the model wears its own"),
        }

        // An override beats the node Material, for its object only.
        let mut om = floptle_core::ObjectMaterials::default();
        om.0.insert("Torso#2".into(), floptle_core::Material::tinted([0.0, 1.0, 0.0]));
        match part_look_rule(Some(&om), Some("Torso#2"), Some("Clothing"), Some(&node), imported) {
            PartLook::Override(m) => assert_eq!(m.color, [0.0, 1.0, 0.0]),
            _ => panic!("the object's own material is the most specific one"),
        }
        // …and the parts it does not name still take the node Material.
        match part_look_rule(Some(&om), Some("LeftLeg#2"), Some("Pants"), Some(&node), imported) {
            PartLook::Node(m) => assert_eq!(m.color, [1.0, 0.0, 0.0]),
            _ => panic!("an override is for ITS object, not the model"),
        }

        // **The material name addresses its parts too**, which is the whole
        // clothing case: `Clothing` is one material across a torso and two arms,
        // and import renamed every one of those objects (`Torso` → `Torso#2`),
        // so the object name is not a name anybody has.
        let mut by_mat = floptle_core::ObjectMaterials::default();
        by_mat.0.insert("Clothing".into(), floptle_core::Material::tinted([0.0, 0.0, 1.0]));
        for object in ["Torso#2", "RightArm#2", "LeftArm#2"] {
            match part_look_rule(Some(&by_mat), Some(object), Some("Clothing"), Some(&node), imported)
            {
                PartLook::Override(m) => assert_eq!(m.color, [0.0, 0.0, 1.0], "{object}"),
                _ => panic!("a material name must reach every part wearing it ({object})"),
            }
        }
        // …and reach no further than that.
        match part_look_rule(Some(&by_mat), Some("RightLeg#2"), Some("Pants"), Some(&node), imported)
        {
            PartLook::Node(_) => {}
            _ => panic!("`Pants` is a different material and keeps the node's"),
        }
        // The precise name still wins where both are present.
        let mut both = floptle_core::ObjectMaterials::default();
        both.0.insert("Clothing".into(), floptle_core::Material::tinted([0.0, 0.0, 1.0]));
        both.0.insert("Torso#2".into(), floptle_core::Material::tinted([1.0, 1.0, 0.0]));
        match part_look_rule(Some(&both), Some("Torso#2"), Some("Clothing"), None, imported) {
            PartLook::Override(m) => assert_eq!(m.color, [1.0, 1.0, 0.0], "object beats material"),
            _ => panic!("the object's own override is the most specific"),
        }

        // A model whose parts have neither name cannot be addressed per object,
        // and must not accidentally match somebody else's override.
        match part_look_rule(Some(&om), None, None, None, imported) {
            PartLook::Imported(c) => assert_eq!(c, imported),
            _ => panic!("no key, no override"),
        }
    }

    /// The project's sorting layers, so a rank means something.
    fn project() -> floptle_scene::ProjectConfigDoc {
        floptle_scene::ProjectConfigDoc {
            sorting_layers: vec!["Default".into(), "Ground".into(), "Characters".into()],
            ..Default::default()
        }
    }

    fn batch_on(world: &mut World, layer: &str, mode: Lit2D) -> Entity {
        let e = world.spawn();
        world.insert(e, Matter::SpriteBatch { size: 1.0 });
        world.insert(e, Sorting { layer: layer.into(), order: 0, ..Default::default() });
        world.insert(e, Lighting2D { mode, ..Default::default() });
        e
    }

    /// `floptle/0122`: a flat surface no live light can reach is not gathered at
    /// all. The mask already said it contributes nothing — honouring that in
    /// `fs_light` instead means instancing, uploading and rasterizing the whole
    /// flat scene a second time to throw it away.
    #[test]
    fn a_surface_no_light_reaches_is_not_gathered() {
        let mut world = World::default();
        let ground = batch_on(&mut world, "Ground", Lit2D::Auto);
        let chars = batch_on(&mut world, "Characters", Lit2D::Auto);
        let p = project();
        // One light, reaching rank 1 (Ground) only — the card's own repro.
        let reach = 1u64 << p.sorting_rank("Ground");

        let got = lit_2d_ranks(&world, &p, true, reach);
        assert_eq!(got.len(), 1, "a batch the light cannot reach was still filled");
        assert_eq!(got.get(&ground).copied(), Some((p.sorting_rank("Ground"), false)));
        assert!(!got.contains_key(&chars), "Characters is on no light's mask");
    }

    /// …and with nothing to reach, the world is not walked at all. This is the
    /// "no light placed costs nothing" property, and it has to hold for a scene
    /// full of flat matter — which is every 2D scene.
    #[test]
    fn no_light_placed_gathers_nothing() {
        let mut world = World::default();
        batch_on(&mut world, "Default", Lit2D::Auto);
        batch_on(&mut world, "Ground", Lit2D::Yes);
        assert!(lit_2d_ranks(&world, &project(), true, 0).is_empty());
    }

    /// An unrestricted light reaches every rank, so the filter correctly does
    /// nothing — the ordinary case of somebody dropping one light into a scene
    /// must not start dropping surfaces.
    #[test]
    fn an_unrestricted_light_still_lights_everything() {
        let mut world = World::default();
        batch_on(&mut world, "Default", Lit2D::Auto);
        batch_on(&mut world, "Ground", Lit2D::Auto);
        batch_on(&mut world, "Characters", Lit2D::Auto);
        assert_eq!(lit_2d_ranks(&world, &project(), true, u64::MAX).len(), 3);
    }

    /// A node that says `3d` stays off the path however far a light reaches —
    /// stating it is never re-decided, which is the whole contract of the flag,
    /// and it is the workaround two games are currently standing on.
    #[test]
    fn a_node_that_said_3d_is_never_gathered() {
        let mut world = World::default();
        batch_on(&mut world, "Ground", Lit2D::No);
        assert!(lit_2d_ranks(&world, &project(), true, u64::MAX).is_empty());
    }
}

/// `floptle/0144`: `water_draw` used to build its `MaterialParams` from
/// scratch and never look at the node's own `Material` — no shader, no
/// `retro: (exempt: true)`, no way to style it at all. These pin the overlay
/// rule: absent Material → today's exact numbers; present → its surface
/// params win, EXCEPT `alpha`, which only overrides when the material set one
/// (every unauthored `Material` defaults to `alpha = 1.0`, and a water volume
/// wearing one for its `retro` flag alone must not go opaque).
#[cfg(test)]
mod water_draw_tests {
    use super::*;

    /// A `Pool` water volume with the project's default numbers — same shape
    /// as `Matter::default_water()`, but with `frozen` controllable so the ice
    /// path can be pinned too.
    fn pool(frozen: bool) -> Matter {
        Matter::WaterVolume {
            kind: floptle_core::WaterKind::Pool,
            radius: 10.0,
            half_extents: [5.0, 2.0, 5.0],
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            frozen,
            tint: [0.10, 0.32, 0.38],
            visibility: 28.0,
        }
    }

    fn mesh_ids() -> Vec<MeshId> {
        // `water_draw` only ever indexes Cube/Sphere; the exact MeshId value
        // doesn't matter to these tests, only that a slot exists.
        vec![MeshId(0), MeshId(1), MeshId(2), MeshId(3)]
    }

    /// **The property the card asks to be pinned above all others**: with no
    /// Material component, water draws exactly as it always has.
    #[test]
    fn no_material_draws_unchanged() {
        let ids = mesh_ids();
        let (_, unfrozen) =
            water_draw(&pool(false), None, &Transform::IDENTITY, DVec3::ZERO, &ids, None).unwrap();
        assert_eq!(unfrozen.color[3], 0.55, "unfrozen water's alpha is untouched");
        assert_eq!(unfrozen.specular[3], 0.9, "unfrozen water's specular strength is untouched");
        assert_eq!(unfrozen.specular[0..3], [1.0, 1.0, 1.0], "unfrozen water's specular colour");
        assert_eq!(unfrozen.params[0], 96.0, "unfrozen water's shininess is untouched");

        let (_, frozen) =
            water_draw(&pool(true), None, &Transform::IDENTITY, DVec3::ZERO, &ids, None).unwrap();
        assert_eq!(frozen.color[3], 1.0, "frozen water (ice) is opaque, untouched");
        assert_eq!(frozen.specular[3], 0.15, "frozen water's specular strength is untouched");
        assert_eq!(frozen.params[0], 8.0, "frozen water's shininess is untouched");
    }

    /// A Material's specular/shininess win outright once it exists — the same
    /// "most specific wins, whole" rule `part_look_rule` states for meshes —
    /// but its default `alpha = 1.0` must NOT silently make the water opaque:
    /// a water volume wearing a Material purely for `retro: (exempt: true)`
    /// keeps its translucency.
    #[test]
    fn material_overlays_specular_but_alpha_needs_an_actual_value() {
        let ids = mesh_ids();
        // alpha left at Material::default()'s 1.0 — the untouched case.
        let m = Material {
            specular: [0.2, 0.4, 0.9],
            specular_strength: 0.5,
            shininess: 40.0,
            ..Material::default()
        };
        let (_, raw) = water_draw(
            &pool(false),
            Some(&m),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            None,
        )
        .unwrap();
        assert_eq!(raw.specular[0..3], [0.2, 0.4, 0.9], "material specular colour wins");
        assert_eq!(raw.specular[3], 0.5, "material specular strength wins");
        assert_eq!(raw.params[0], 40.0, "material shininess wins");
        assert_eq!(
            raw.color[3], 0.55,
            "an untouched Material.alpha (1.0) must not override the water's own translucency"
        );
    }

    /// An author who DOES dial in a specific alpha gets it.
    #[test]
    fn material_alpha_is_honoured_once_actually_set() {
        let ids = mesh_ids();
        let m = Material { alpha: 0.2, ..Material::default() };
        let (_, raw) = water_draw(
            &pool(false),
            Some(&m),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            None,
        )
        .unwrap();
        assert_eq!(raw.color[3], 0.2, "an explicit material alpha is honoured");
    }

    /// Criterion 3: frozen ice stays opaque whatever the material says.
    #[test]
    fn frozen_ignores_material_alpha() {
        let ids = mesh_ids();
        let m = Material { alpha: 0.2, ..Material::default() };
        let (_, raw) = water_draw(
            &pool(true),
            Some(&m),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            None,
        )
        .unwrap();
        assert_eq!(raw.color[3], 1.0, "frozen water is opaque ice regardless of the material");
    }

    /// Criterion 5, watched against the real surface-extras store: under a
    /// project with `retro_dither_alpha: true`, a WaterVolume with NO Material
    /// stays on the project's (dithered) neutral entry — matching every other
    /// undecorated surface — while one carrying `retro: (exempt: true)` lands
    /// on its own, distinct entry. `push_surface_extras` is the only thing
    /// that can answer this, so it needs a real (headless) `Raster`.
    #[test]
    fn retro_exempt_water_does_not_share_the_projects_dithered_neutral_entry() {
        let gpu = floptle_render::Gpu::headless(4, 4);
        // **A driver that cannot build the renderer is not a defect this test is
        // about.** `Raster::new` allocates a placeholder texture with a
        // reinterpretation view format (`upload_texture_mips`'s `view_formats`,
        // so a material's own picture can also be read raw for its surface
        // maps) — a capability CI's adapter does not have (the same class of
        // gap as "the raster pipeline cannot be built on OpenGL", already
        // documented in HANDOFF; wgpu's default uncaptured-error handler
        // panics, so without this the FIRST test to build a full `Raster` on a
        // headless device anywhere finds that out by crashing the test binary).
        // Same idiom `doctor.rs` uses to answer "can this machine render" at
        // all: install a sink instead of the default panic, and if the
        // pipeline could not be built, this test has nothing to say about a
        // machine that cannot ask it the question.
        let failed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = failed.clone();
        gpu.device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
            if let Ok(mut s) = sink.lock()
                && s.is_empty()
            {
                *s = e.to_string();
            }
        }));
        let mut raster = floptle_render::Raster::new(&gpu);
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        if let Ok(why) = failed.lock()
            && !why.is_empty()
        {
            eprintln!("skipped — this machine cannot build the raster pipeline:\n{why}");
            return;
        }
        raster.set_retro_defaults(floptle_core::Retro {
            dither_alpha: true,
            ..floptle_core::Retro::default()
        });
        let ids = mesh_ids();

        // No Material at all → index 0, same as every plain surface.
        let (_, plain) = water_draw(
            &pool(false),
            None,
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            Some(&mut raster),
        )
        .unwrap();
        assert_eq!(
            floptle_render::ext_index_of(&plain),
            0,
            "a materialless water volume stays on the project's own (dithered) neutral entry"
        );

        // A Material attached but touching nothing (not even `retro`) still
        // follows the project's dither, same as the materialless case above —
        // it just lands on its OWN entry to get there, because an attached
        // Material's other neutral values (e.g. `roughness = 0.5`) genuinely
        // differ from the GPU-wide neutral (`roughness = 1.0`) index 0 holds.
        // That is an existing, general property of `push_surface_extras` and
        // not specific to water; the comparison below is against THIS index,
        // not against 0, so the test isolates what `retro.exempt` changes.
        let plain_material = Material::default();
        let (_, plain_mat) = water_draw(
            &pool(false),
            Some(&plain_material),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            Some(&mut raster),
        )
        .unwrap();
        let dithered_index = floptle_render::ext_index_of(&plain_mat);

        // `retro: (exempt: true)`, otherwise identical — must land on a
        // DIFFERENT entry than the (still dithered) plain material above.
        let exempt = Material {
            retro: floptle_core::Retro { exempt: true, ..floptle_core::Retro::default() },
            ..Material::default()
        };
        let (_, raw) = water_draw(
            &pool(false),
            Some(&exempt),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            Some(&mut raster),
        )
        .unwrap();
        assert_ne!(
            floptle_render::ext_index_of(&raw),
            dithered_index,
            "retro: (exempt: true) must NOT receive the project's EXT_DITHER_ALPHA — it needs \
             a surface-extras entry distinct from an otherwise-identical dithered material"
        );
    }
}
