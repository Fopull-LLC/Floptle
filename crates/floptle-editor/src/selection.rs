//! Selection + direct manipulation: picking, selection set edits, the F-key
//! focus glide, tool switching, and applying gizmo drags to transforms.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::Shape;
use floptle_core::math::DVec3;
use floptle_core::math::Mat4;
use floptle_core::math::Quat;
use floptle_core::math::Vec2;
use floptle_core::math::Vec3;
use floptle_core::math::Vec4;
use floptle_core::transform::Transform;

/// Where a ray struck a node: see [`Editor::raycast_nodes`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct SceneHit {
    pub(crate) e: Entity,
    /// Distance along the (unit) ray.
    pub(crate) t: f32,
    /// The struck point, camera-relative.
    pub(crate) pos: Vec3,
    /// The struck surface's unit normal, in world orientation.
    pub(crate) normal: Vec3,
}

/// A local-space surface normal carried out through a node's inverse matrix
/// (the inverse transpose), so non-uniform scale keeps it perpendicular.
pub(crate) fn local_normal_to_world(m_inv: &Mat4, n: Vec3) -> Vec3 {
    let w = m_inv.transpose() * n.extend(0.0);
    w.truncate().normalize_or_zero()
}
use crate::gizmo::{SCALE_SENS, TRACKBALL_SENS, Tool, local_axis, ray_aabb, ray_box, ray_sphere};
use crate::viz::{cursor_ground, project};
use crate::{Editor, FocusAnim, snap_dvec3};
#[cfg(feature = "editor-ui")]
use crate::scene_hit;

impl Editor {
    /// Switch the active tool and cancel any in-progress gizmo drag.
    pub(crate) fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.grabbed = None;
        self.drag = None;
        // Selecting a brush tool focuses its settings so the controls are at hand.
        #[cfg(feature = "editor-ui")]
        {
            if tool == Tool::Sculpt {
                self.focus_terrain();
            }
            if tool == Tool::Tiles {
                self.focus_tiles();
            }
        }
        #[cfg(feature = "editor-ui")]
        if tool == Tool::Paint {
            self.focus_paint();
        }
        #[cfg(feature = "editor-ui")]
        if tool == Tool::MapEdit {
            self.focus_map();
        }
        if tool != Tool::MapEdit {
            // Leaving map mode drops the sub-object selection + any box drag,
            // and abandons a half-drawn shape.
            self.map_sel = None;
            self.map_box = None;
            self.map_draw = None;
            self.map_arm = None;
            self.map_knife_on = false;
            self.map_knife = None;
        }
    }

    // ---- selection ----------------------------------------------------------
    /// The entity the gizmo + inspector act on (the most recently selected).
    pub(crate) fn primary(&self) -> Option<Entity> {
        self.selection.last().copied()
    }

    /// **The one gate every selection gesture passes through.**
    ///
    /// A locked selection ignores viewport picks, Hierarchy clicks, select-all
    /// and the arrow-key step — see [`Editor::selection_locked`]. It
    /// does not gate the world changing underneath: undo/redo
    /// restoring what was selected at the time, a scene switch dropping a dead
    /// entity, an extension setting the selection through its own API. Those
    /// are not somebody clicking, and a lock that swallowed them would leave
    /// the Inspector pointing at a node that no longer exists.
    fn selection_gesture_blocked(&self) -> bool {
        self.selection_locked
    }

    /// Toggle the selection lock. Refuses to lock an empty selection: the only
    /// switch is on the Inspector's name row, which is drawn for the selected
    /// node, so a lock held over nothing would hide its own release.
    pub(crate) fn toggle_selection_lock(&mut self) {
        if self.selection_locked {
            self.selection_locked = false;
        } else if !self.selection.is_empty() {
            self.selection_locked = true;
        }
    }

    /// Release a lock the world has emptied — the locked node deleted, or the
    /// scene switched out from under it. Called once a frame.
    ///
    /// Without this the editor can reach a state with no way out: locked, with
    /// nothing selected, so the Inspector draws no name row and the lock's own
    /// switch is not on screen.
    pub(crate) fn enforce_selection_lock(&mut self) {
        if self.selection_locked && self.selection.is_empty() {
            self.selection_locked = false;
        }
    }

    /// Select a bone (or model object) of `mesh` — the one rule, shared by the
    /// three places a bone can be clicked: the viewport rig, the Hierarchy's
    /// bone rows, and the Inspector's Objects & Rig lists.
    ///
    /// **A held selection does not hold the RIG.** Locking exists so you can
    /// work on one model without a stray click taking you somewhere else, and
    /// posing is exactly when you want that — but the lock also swallowed every
    /// bone click, which made it useless for the one job it was most wanted for.
    /// A bone is part of the model the lock is holding, not a way out of it, so
    /// clicking one is not the gesture the lock exists to refuse. A bone
    /// belonging to some other model still is, and is still refused.
    ///
    /// Unlocked, a bone selection replaces the node selection — the two are
    /// mutually exclusive and the Inspector switches to the bone editor. Locked,
    /// the model stays selected, because it must: the lock has nothing to hold
    /// otherwise (`enforce_selection_lock` would release it a frame later) and
    /// the rig is only drawn for a mesh that is selected, so clearing it would
    /// take the bones off the screen the moment you picked one.
    ///
    /// Returns whether the bone was taken.
    pub(crate) fn select_bone(&mut self, mesh: Entity, idx: usize) -> bool {
        if !select_bone_into(
            &mut self.selection,
            &mut self.bone_selection,
            self.selection_locked,
            mesh,
            idx,
        ) {
            return false;
        }
        self.selected_asset = None;
        true
    }

    /// Clear the selection as a gesture — clicking empty space in a viewport.
    /// Held by the lock, unlike a bare `selection.clear()`, which is what the
    /// paths that answer to the world (scene switch, delete) still use.
    pub(crate) fn clear_selection(&mut self) {
        if self.selection_gesture_blocked() {
            return;
        }
        self.selection.clear();
    }

    pub(crate) fn select_single(&mut self, e: Entity) {
        if self.selection_gesture_blocked() {
            return;
        }
        self.selection.clear();
        self.selection.push(e);
        // Picking a scene node drops any particle-track and bone selection, so the
        // Inspector reverts from the track/bone editor to this node.
        #[cfg(feature = "editor-ui")]
        {
            self.vfx_ui.sel_track = None;
        }
        self.bone_selection = None;
    }

    pub(crate) fn select_toggle(&mut self, e: Entity) {
        if self.selection_gesture_blocked() {
            return;
        }
        if let Some(i) = self.selection.iter().position(|&x| x == e) {
            self.selection.remove(i);
        } else {
            self.selection.push(e);
        }
        #[cfg(feature = "editor-ui")]
        {
            self.vfx_ui.sel_track = None;
        }
    }

    pub(crate) fn select_all(&mut self) {
        if self.selection_gesture_blocked() {
            return;
        }
        self.selection = self.world.query::<Matter>().map(|(e, _)| e).collect();
    }

    /// Who an Inspector button acts on: the whole selection when `e` is the node
    /// the panel was showing, otherwise just `e`.
    ///
    /// Every ✚ / ✖ component button hands back the node it was drawn for, which
    /// is the primary. With twelve crates selected, "add a rigid body" plainly
    /// means twelve rigid bodies — but a button drawn for some *other* node (an
    /// asset row, a bone) still gets exactly the node it named.
    pub(crate) fn selected_group(&self, e: Entity) -> Vec<Entity> {
        if self.selection.len() > 1 && self.selection.contains(&e) {
            self.selection.clone()
        } else {
            vec![e]
        }
    }

    /// Selected entities that are real Matter nodes (excludes the Lighting node).
    pub(crate) fn selected_matter(&self) -> Vec<Entity> {
        self.selection.iter().copied().filter(|&e| self.world.get::<Matter>(e).is_some()).collect()
    }

    /// True when the cursor is over the Scene viewport tab and not under a popup —
    /// the gate for viewport picking, gizmo grabs and camera look. egui_dock keeps
    /// the side panels in the background layer, so `is_pointer_over_egui` alone
    /// can't separate them from the viewport; the Scene-tab rect is what does.
    pub(crate) fn cursor_over_scene(&self) -> bool {
        #[cfg(feature = "editor-ui")]
        {
            self.egui
                .as_ref()
                .is_some_and(|eg| scene_hit(&eg.ctx, self.cursor, self.scene_rect))
        }
        // A build has no Scene viewport for the cursor to be over.
        #[cfg(not(feature = "editor-ui"))]
        false
    }

    /// True when the cursor is over the Game viewport rect (and not under a popup) —
    /// the gate for trapping the cursor into the Game view on click.
    pub(crate) fn cursor_over_game(&self) -> bool {
        #[cfg(feature = "editor-ui")]
        {
            self.egui
                .as_ref()
                .is_some_and(|eg| scene_hit(&eg.ctx, self.cursor, self.game_rect))
        }
        // The whole window is the game; "over it" is not a question here.
        #[cfg(not(feature = "editor-ui"))]
        false
    }

    /// A mouse button went down over the Scene view, the Game view, or the
    /// bare window: whatever text field was being typed into is done, and
    /// the keys that follow reach the viewport.
    ///
    /// The viewports take their presses straight from the window, so egui
    /// never sees one and keeps the Inspector field focused; the fly keys
    /// and shortcuts typed after a right-click into the scene land in the
    /// field. Every button counts: a right-click to look around is the
    /// usual first move, and the intent is the same.
    pub(crate) fn viewport_press_ends_typing(&self) {
        #[cfg(feature = "editor-ui")]
        if let Some(eg) = self.egui.as_ref() {
            let over_viewport = self.cursor_over_scene() || self.cursor_over_game();
            end_typing_on_press(&eg.ctx, over_viewport);
        }
    }

    /// The world point under the cursor — its ray's hit on the ground plane (y=0),
    /// or ~6 units in front of the camera if the ray doesn't meet the ground. Used to
    /// place a dropped asset where the cursor is.
    pub(crate) fn cursor_world(&self) -> DVec3 {
        let cam = self.camera.render_camera();
        let Some(gpu) = self.gpu.as_ref() else {
            return cam.world_position + (cam.rotation * Vec3::NEG_Z * 6.0).as_dvec3();
        };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let inv = cam.view_proj(w / h).inverse();
        cursor_ground(cam.world_position, cam.rotation, inv, w, h, self.cursor)
    }

    /// Move the selection up (-1) / down (+1) through the hierarchy (arrow keys).
    pub(crate) fn step_selection(&mut self, delta: i32) {
        if self.selection_gesture_blocked() {
            return;
        }
        let order: Vec<Entity> = self.world.query::<Matter>().map(|(e, _)| e).collect();
        if order.is_empty() {
            return;
        }
        let cur = self.selection.last().and_then(|s| order.iter().position(|e| e == s));
        let next = match cur {
            Some(i) => (i as i32 + delta).clamp(0, order.len() as i32 - 1) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        self.select_single(order[next]);
    }

    /// Track a mouse button for the script `input` API (edge + held) — the press
    /// edge banks in both the per-frame set (`update`) and the per-tick accumulator
    /// (`fixedUpdate`).
    pub(crate) fn track_mouse_button(&mut self, i: usize, pressed: bool) {
        // The action layer tracks all five buttons, not just the first three.
        self.note_action_button(i, pressed);
        if i < 3 {
            if pressed && !self.input_buttons[i] {
                self.input_buttons_pressed[i] = true;
                self.tick_buttons_pressed[i] = true;
            }
            // Game-UI edges bank as events, not sampled state: at a low frame
            // rate a quick click's press and release land inside one frame's
            // event batch, and a sampled edge (down && !was) misses it — the
            // player "can't click" buttons exactly when the game struggles.
            if i == 0 {
                if pressed {
                    self.ui_lmb_pressed_evt = true;
                } else {
                    self.ui_lmb_released_evt = true;
                }
            }
            self.input_buttons[i] = pressed;
        }
    }

    /// Toggle the selected folder's open/closed state in the Hierarchy (Enter key).
    pub(crate) fn toggle_folder_selected(&mut self) {
        let Some(e) = self.selection.last().copied() else { return };
        if matches!(self.world.get::<Matter>(e), Some(Matter::Empty))
            && !self.collapsed.remove(&e) {
                self.collapsed.insert(e);
            }
    }

    /// Frame the selected object in the viewport (the F key): keep the view angle,
    /// move the camera so the object is centered at a size-appropriate distance.
    ///
    /// Reads the node's world placement, not its local `Transform`. A local
    /// translation is an offset from a parent, so framing it would fly `F` on a
    /// door inside a building to wherever `(0.4, 0, 1.2)` happens to be in the
    /// world — usually the origin — and look like the key had missed.
    pub(crate) fn focus_selected(&mut self) {
        let Some(e) = self.selection.last().copied() else { return };
        if self.world.get::<Transform>(e).is_none() {
            return; // nothing placeable to frame
        }
        let wt = floptle_core::world_transform(&self.world, e);
        let target = wt.translation;
        // The world scale too, for the same reason: a node scaled by its parent
        // is that much bigger on screen, and framing it by its local scale sits
        // the camera at the wrong distance by exactly the parent's factor.
        let scale = wt.scale.abs().max_element() as f64;
        let base = match self.world.get::<Matter>(e) {
            Some(Matter::Mesh { asset_path }) => {
                self.mesh_registry.get(asset_path).map(|a| a.size as f64).unwrap_or(1.0)
            }
            Some(Matter::Blob { scale: s }) => *s as f64,
            _ => 1.0,
        };
        let radius = (base * scale).max(0.3);
        self.focus_point(target, (radius * 3.0 + 2.0).clamp(2.5, 80.0));
    }

    /// Glide the camera until `target` sits `distance` straight ahead.
    ///
    /// Split out of [`Self::focus_selected`] because `ed.lookAt` needs exactly
    /// this and nothing else: a package pointing at a place must move the
    /// camera the same way the `F` key does — same easing, same kept view
    /// angle — or "show me" from a tool feels like a different editor from
    /// "show me" from the keyboard.
    pub(crate) fn focus_point(&mut self, target: DVec3, distance: f64) {
        // Keep the current view direction; glide the position so the target ends up
        // `distance` straight ahead. The eased move runs in the per-frame update.
        let forward = (self.camera.rotation() * Vec3::NEG_Z).as_dvec3();
        let dest = target - forward * distance;
        self.focus_anim = Some(FocusAnim { from: self.camera.position, to: dest, t: 0.0 });
    }

    /// The viewport ray under `cursor` (physical px), camera-relative: origin and
    /// unit direction. `None` before the GPU exists.
    pub(crate) fn cursor_ray(&self, cursor: Vec2) -> Option<(Vec3, Vec3)> {
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let inv = cam.view_proj(w / h).inverse();
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro).normalize();
        ro.is_finite().then_some(())?;
        rd.is_finite().then_some((ro, rd))
    }

    /// Pick the nearest selectable entity under a viewport cursor (physical px).
    /// `None` = empty space. See [`Self::raycast_nodes`].
    pub(crate) fn pick(&mut self, cursor: Vec2) -> Option<Entity> {
        let (ro, rd) = self.cursor_ray(cursor)?;
        self.raycast_nodes(ro, rd, |_| true).map(|h| h.e)
    }

    /// The nearest node a camera-relative ray strikes, where it strikes it, and
    /// the surface's facing there. Nodes `keep` refuses are passed through.
    ///
    /// Models, primitives and map meshes are tested against their triangles, so
    /// what is picked is what is drawn under the cursor: a click on a crate
    /// standing on a large floor model selects the crate, and a click through
    /// the empty corner of a model's bounds reaches whatever is behind it.
    /// Everything else is tested against its drawn extent. Every candidate is
    /// ranked by the distance to its hit, never to the bounds that admitted it.
    pub(crate) fn raycast_nodes(
        &mut self,
        ro: Vec3,
        rd: Vec3,
        keep: impl Fn(Entity) -> bool,
    ) -> Option<SceneHit> {
        let cam = self.camera.render_camera();
        // Where each node's picture actually is this frame. Clicking has to test
        // against what is on screen: a sprite on a parallax layer is drawn a long
        // way from its own transform, so without this the visible sprite picks
        // nothing and empty space picks it.
        let draws = crate::sprite2d::draw_offsets(&self.world, &self.project, cam.world_position);
        // Nodes whose triangles decide the hit: a bounds test admits them, then
        // the exact cast below settles it.
        let mut exact: Vec<(Entity, Mat4)> = Vec::new();
        let mut best: Option<SceneHit> = None;
        let offer = |best: &mut Option<SceneHit>, hit: SceneHit| {
            if best.as_ref().is_none_or(|b| hit.t < b.t) {
                *best = Some(hit);
            }
        };
        let flat = |e: Entity, t: f32| SceneHit { e, t, pos: ro + rd * t, normal: -rd };
        for (e, m) in self.world.query::<Matter>() {
            if !keep(e) {
                continue;
            }
            // Ray-test against the node's world placement (so parented nodes pick).
            let mut t = floptle_core::world_transform(&self.world, e);
            t.translation += draws.get(&e).copied().unwrap_or_default();
            let model = t.render_matrix(cam.world_position);
            let m_inv = model.inverse();
            if !m_inv.is_finite() {
                continue;
            }
            // The ray in the object's local frame. Unnormalized, so the same `t`
            // is valid in both spaces and hits stay comparable.
            let ro_l = (m_inv * ro.extend(1.0)).truncate();
            let rd_l = (m_inv * rd.extend(0.0)).truncate();
            match m {
                Matter::Primitive { shape, .. } => {
                    // Bounds that contain every primitive shape; the triangles decide.
                    let admit = match shape {
                        Shape::Sphere => ray_sphere(ro_l, rd_l, Vec3::ZERO, 0.85),
                        _ => ray_aabb(ro_l, rd_l, 1.05),
                    };
                    if admit.is_some() {
                        exact.push((e, model));
                    }
                }
                Matter::Blob { scale } => {
                    let center = (t.translation - cam.world_position).as_vec3();
                    if let Some(th) = ray_sphere(ro, rd, center, 0.85 * scale * t.scale.x) {
                        offer(&mut best, flat(e, th));
                    }
                }
                Matter::FieldShape { radius } => {
                    // Pick by the authored bounding sphere (the shape lives inside it).
                    let center = (t.translation - cam.world_position).as_vec3();
                    if let Some(th) = ray_sphere(ro, rd, center, (radius * t.scale.x).max(0.1)) {
                        offer(&mut best, flat(e, th));
                    }
                }
                Matter::Mesh { asset_path } => {
                    // `size` is the longest edge of the model's bounds, centred on
                    // the origin; a sphere through the bounds' corners contains it.
                    let half = self.mesh_registry.get(asset_path).map(|a| a.size * 0.5).unwrap_or(1.0);
                    if ray_sphere(ro_l, rd_l, Vec3::ZERO, half * 1.75).is_some() {
                        exact.push((e, model));
                    }
                }
                Matter::MapMesh { id } => {
                    // Exact face raycast (the kernel keeps CPU geometry).
                    if let Some(h) = self
                        .maps
                        .meshes
                        .get(id)
                        .and_then(|mesh| floptle_map::raycast(mesh, ro_l, rd_l, f32::MAX))
                    {
                        let normal = local_normal_to_world(&m_inv, h.normal);
                        offer(&mut best, SceneHit { e, t: h.t, pos: ro + rd * h.t, normal });
                    }
                }
                // A flat grid in the node's XY plane: pick it as a thin box, so
                // clicking the floor of a 2D room selects the map rather than
                // requiring the Hierarchy.
                Matter::Tilemap { cols, rows, tile, .. } => {
                    let half = Vec3::new(
                        (*cols as f32 * tile * 0.5).max(0.01),
                        (*rows as f32 * tile * 0.5).max(0.01),
                        tile * 0.1,
                    );
                    if let Some(th) = ray_box(ro_l, rd_l, half) {
                        let normal = local_normal_to_world(&m_inv, Vec3::Z);
                        offer(&mut best, SceneHit { e, t: th, pos: ro + rd * th, normal });
                    }
                }
                // Its sprites are this frame's, and picking one would select the
                // batch anyway — so pick the batch's own origin.
                Matter::SpriteBatch { size } => {
                    let center = (t.translation - cam.world_position).as_vec3();
                    if let Some(th) = ray_sphere(ro, rd, center, (size * t.scale.max_element()).max(0.1)) {
                        offer(&mut best, flat(e, th));
                    }
                }
                // One sprite is a quad, and clicking it should feel like
                // clicking the picture — so pick against a sphere around where
                // the picture is, at the size it is actually drawn.
                //
                // Two things that are easy to leave out and both make a sprite
                // unclickable where it can plainly be seen: a `ppu` sprite's
                // size comes from its texture, not from `size`; and the pivot
                // moves the quad off the origin, so a feet-pivoted character is
                // drawn entirely above the point a naive sphere is centred on.
                Matter::Sprite { ppu, size, pivot, .. } => {
                    let mat = self.world.get::<floptle_core::Material>(e);
                    let px = mat
                        .and_then(|m| m.texture.as_deref())
                        .and_then(|p| self.texture_registry.get(p).copied())
                        .and_then(|id| self.raster.as_ref().and_then(|r| r.texture_size(id)));
                    let (w, h) = crate::sprite2d::sprite_world_size(*ppu, *size, mat, px);
                    let s = t.scale;
                    let (w, h) = (w * s.x.abs(), h * s.y.abs());
                    // The quad's centre, from the pivot, in the node's own frame.
                    let off = t.rotation
                        * floptle_core::math::Vec3::new(
                            (0.5 - pivot[0]) * w,
                            (0.5 - pivot[1]) * h,
                            0.0,
                        );
                    let center = (t.translation - cam.world_position).as_vec3() + off;
                    if let Some(th) = ray_sphere(ro, rd, center, (w.max(h) * 0.5).max(0.05)) {
                        offer(&mut best, flat(e, th));
                    }
                }
                // no mesh — select via the hierarchy.
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
        for (e, model) in exact {
            if let Some(hit) = self.raycast_node_triangles(e, &model, ro, rd) {
                offer(&mut best, hit);
            }
        }
        best
    }

    /// The camera-relative ray against one node's own triangles (model or
    /// primitive), placed by `model`. A rigged model whose triangles miss falls
    /// back to its tight bounds, because its pose on screen is not the bind pose
    /// the triangles are stored in.
    fn raycast_node_triangles(&mut self, e: Entity, model: &Mat4, ro: Vec3, rd: Vec3) -> Option<SceneHit> {
        let key = self.ensure_paint_mesh_pub(e)?;
        let m_inv = model.inverse();
        let ro_l = (m_inv * ro.extend(1.0)).truncate();
        let rd_l = (m_inv * rd.extend(0.0)).truncate();
        let len = rd_l.length();
        if len < 1e-9 {
            return None;
        }
        // The cache walks a grid in local units, so it takes a unit direction;
        // dividing by `len` turns its distance back into the world ray's.
        if let Some(h) = self.paint_meshes.raycast(&key, ro_l, rd_l / len, 1e7) {
            let t = h.t / len;
            let normal = local_normal_to_world(&m_inv, h.normal);
            return Some(SceneHit { e, t, pos: ro + rd * t, normal });
        }
        let rigged = match self.world.get::<Matter>(e) {
            Some(Matter::Mesh { asset_path }) => {
                self.mesh_registry.get(asset_path).is_some_and(|a| a.rig.is_some())
            }
            _ => false,
        };
        if !rigged {
            return None;
        }
        let (min, max) = self.paint_meshes.bounds(&key)?;
        let (c, half) = ((min + max) * 0.5, (max - min) * 0.5);
        let t = ray_box(ro_l - c, rd_l, half)?;
        Some(SceneHit { e, t, pos: ro + rd * t, normal: -rd })
    }

    /// Apply a gizmo drag for the grabbed handle, as an absolute transform from the
    /// start-of-drag snapshot (no per-event accumulation ⏵ no drift).
    pub(crate) fn gizmo_drag(&mut self) {
        let (Some(drag), Some(cursor)) = (self.drag, self.cursor) else {
            return;
        };
        // A bone drag has no ECS selection (the bone isn't an entity) — it acts on the
        // drag's mesh entity. An entity drag acts on the current primary selection.
        let e = if drag.bone.is_some() { drag.entity } else { self.primary().unwrap_or(drag.entity) };
        // The snapshot must belong to the still-selected object (guards against the
        // selection changing mid-drag and applying the wrong object's transform).
        if drag.entity != e {
            self.grabbed = None;
            self.drag = None;
            return;
        }
        let handle = drag.handle;
        let (w, h) = self
            .gpu
            .as_ref()
            .map(|g| (g.config.width as f32, g.config.height.max(1) as f32))
            .unwrap_or((1280.0, 720.0));
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let cam_world = cam.world_position;
        let start = drag.start_xf;
        let cursor_delta = cursor - drag.cursor_start;
        let (snap, step) = (self.grid.snap, self.grid.size as f64);
        // A sub-object drag snaps the distance travelled, not the resulting
        // world point: the gizmo sits on a selection centroid that is rarely on
        // a grid line, and on a normal-aligned (diagonal) axis, snapping the
        // point would quantize the move off its own axis and slide the face
        // sideways.
        let sub_object = self.map_drag.is_some();

        match self.gizmo_tool() {
            Tool::Move | Tool::MapEdit => {
                if let Some(i) = handle.axis_index() {
                    let dir = local_axis(start.rotation, i);
                    // Project the axis (a 1-unit step) to screen; the move distance is
                    // the cursor delta projected onto that screen direction.
                    let (Some(s0), Some(s1)) = (
                        project(start.translation, cam_world, vp, w, h),
                        project(start.translation + dir.as_dvec3(), cam_world, vp, w, h),
                    ) else {
                        return;
                    };
                    let sdir = s1 - s0;
                    let len2 = sdir.length_squared();
                    if len2 < 1e-6 {
                        return; // axis points (almost) straight at the camera
                    }
                    let mut units = cursor_delta.dot(sdir) / len2;
                    if snap && sub_object {
                        let s = step as f32;
                        units = (units / s).round() * s;
                    }
                    let mut p = start.translation + (dir * units).as_dvec3();
                    if snap && !sub_object {
                        p = snap_dvec3(p, step);
                    }
                    let xf = Transform { translation: p, ..start };
                    self.set_world_transform(e, xf);
                    self.apply_group_transform(start, xf);
                } else {
                    // Center handle: free move in the camera plane.
                    let rot = cam.rotation;
                    let right = rot * Vec3::X;
                    let up = rot * Vec3::Y;
                    let dist = (start.translation - cam_world).length().max(0.1) as f32;
                    let wpp = 2.0 * dist * (30f32.to_radians()).tan() / h;
                    let mut mv = right * (cursor_delta.x * wpp) - up * (cursor_delta.y * wpp);
                    if snap && sub_object {
                        let s = step as f32;
                        mv = (mv / s).round() * s;
                    }
                    let mut p = start.translation + mv.as_dvec3();
                    if snap && !sub_object {
                        p = snap_dvec3(p, step);
                    }
                    let xf = Transform { translation: p, ..start };
                    self.set_world_transform(e, xf);
                    self.apply_group_transform(start, xf);
                }
            }
            Tool::Rotate => {
                if let Some(i) = handle.axis_index() {
                    // Rotate about the object's local axis (in world space).
                    let dir = local_axis(start.rotation, i);
                    let Some(center) = project(start.translation, cam_world, vp, w, h) else {
                        return;
                    };
                    let v1 = drag.cursor_start - center;
                    let v2 = cursor - center;
                    if v1.length_squared() < 1.0 || v2.length_squared() < 1.0 {
                        return;
                    }
                    let mut angle = (v1.x * v2.y - v1.y * v2.x).atan2(v1.x * v2.x + v1.y * v2.y);
                    // Screen-y points down; flip when the axis faces toward the camera
                    // so a drag always spins the visible way.
                    if dir.dot((start.translation - cam_world).as_vec3()) < 0.0 {
                        angle = -angle;
                    }
                    let rot = (Quat::from_axis_angle(dir, angle) * start.rotation).normalize();
                    let xf = Transform { rotation: rot, ..start };
                    self.set_world_transform(e, xf);
                    self.apply_group_transform(start, xf);
                } else {
                    // Center handle: free / trackball rotate about the camera axes —
                    // drag horizontally to spin about camera-up, vertically about
                    // camera-right.
                    let cam_right = cam.rotation * Vec3::X;
                    let cam_up = cam.rotation * Vec3::Y;
                    let q = Quat::from_axis_angle(cam_up, cursor_delta.x * TRACKBALL_SENS)
                        * Quat::from_axis_angle(cam_right, cursor_delta.y * TRACKBALL_SENS);
                    let rot = (q * start.rotation).normalize();
                    let xf = Transform { rotation: rot, ..start };
                    self.set_world_transform(e, xf);
                    self.apply_group_transform(start, xf);
                }
            }
            Tool::Scale => {
                if let Some(i) = handle.axis_index() {
                    let dir = local_axis(start.rotation, i);
                    let (Some(s0), Some(s1)) = (
                        project(start.translation, cam_world, vp, w, h),
                        project(start.translation + dir.as_dvec3(), cam_world, vp, w, h),
                    ) else {
                        return;
                    };
                    let n = (s1 - s0).normalize_or_zero();
                    let factor = 1.0 + cursor_delta.dot(n) * SCALE_SENS;
                    let mut sc = start.scale;
                    // Floor the magnitude, keep the sign — a mirrored (negative-scale)
                    // node must stay mirrored (a bare `.max(0.01)` snaps -1 to +0.01).
                    let s = start.scale[i] * factor;
                    sc[i] = s.abs().max(0.01).copysign(if s == 0.0 { start.scale[i] } else { s });
                    let xf = Transform { scale: sc, ..start };
                    self.set_world_transform(e, xf);
                    self.apply_group_transform(start, xf);
                } else {
                    // Center handle: uniform scale by the cursor's distance ratio.
                    let Some(center) = project(start.translation, cam_world, vp, w, h) else {
                        return;
                    };
                    let d0 = (drag.cursor_start - center).length().max(1.0);
                    let d1 = (cursor - center).length();
                    let factor = (d1 / d0).max(0.01);
                    // Per-component magnitude floor that keeps each axis's sign, so a
                    // mirrored node can be uniformly scaled without losing its mirror.
                    let f = |v: f32| v.abs().max(0.01).copysign(v);
                    let s = start.scale * factor;
                    let sc = Vec3::new(f(s.x), f(s.y), f(s.z));
                    let xf = Transform { scale: sc, ..start };
                    self.set_world_transform(e, xf);
                    self.apply_group_transform(start, xf);
                }
            }
            Tool::Rect => {
                // Face push/pull: the dragged face follows the cursor along its
                // outward normal; the opposite face stays put (scale + recenter
                // in one gesture — pull a cube into a floor without offset math).
                let Some(i) = handle.axis_index() else { return };
                let outward = local_axis(start.rotation, i) * handle.sign();
                let (Some(s0), Some(s1)) = (
                    project(start.translation, cam_world, vp, w, h),
                    project(start.translation + outward.as_dvec3(), cam_world, vp, w, h),
                ) else {
                    return;
                };
                let sdir = s1 - s0;
                let len2 = sdir.length_squared();
                if len2 < 1e-6 {
                    return; // face normal points (almost) straight at the camera
                }
                let mut units = cursor_delta.dot(sdir) / len2;
                if snap {
                    units = (units / step as f32).round() * step as f32;
                }
                let Some(base) = rect_base_half(&self.world, &self.mesh_registry, e) else {
                    return;
                };
                let h0 = (base[i] * start.scale[i].abs()).max(1e-3);
                // New full extent along the axis; keep it positive.
                let extent = (2.0 * h0 + units).max(0.02 * h0);
                let applied = extent - 2.0 * h0;
                let mut sc = start.scale;
                sc[i] = start.scale[i] * (extent / (2.0 * h0));
                let p = start.translation + (outward * (applied * 0.5)).as_dvec3();
                let xf = Transform { translation: p, scale: sc, ..start };
                self.set_world_transform(e, xf);
                self.apply_group_transform(start, xf);
            }
            Tool::Select | Tool::Sculpt | Tool::Paint | Tool::Tiles => {}
        }
    }

    /// Mirror a gizmo drag onto the rest of a multi-selection: whatever delta the
    /// drag applied to the primary (`start` → `new`) is applied to every entity in
    /// `drag_group`, relative to the primary's start frame — so a group Move slides
    /// everything together, a group Rotate orbits the others around the primary,
    /// and a group Scale scales their offsets too. No-op for single selections
    /// and bone drags (the group is only populated on an entity grab).
    pub(crate) fn apply_group_transform(&mut self, start: Transform, new: Transform) {
        if self.drag_group.is_empty() {
            return;
        }
        let dq = (new.rotation * start.rotation.inverse()).normalize();
        // Per-axis scale ratio in the primary's local frame; guard divisions by a
        // (floored/zero) start scale.
        let safe = |n: f32, d: f32| if d.abs() > 1e-6 { n / d } else { 1.0 };
        let ds = Vec3::new(
            safe(new.scale.x, start.scale.x),
            safe(new.scale.y, start.scale.y),
            safe(new.scale.z, start.scale.z),
        );
        let group = std::mem::take(&mut self.drag_group);
        for &(e, s) in &group {
            // Offset from the primary, scaled in the primary's start frame, then
            // re-rotated by the drag's rotation delta.
            let rel = (s.translation - start.translation).as_vec3();
            let rel = new.rotation * ((start.rotation.inverse() * rel) * ds);
            let xf = Transform {
                translation: new.translation + rel.as_dvec3(),
                rotation: (dq * s.rotation).normalize(),
                scale: s.scale * ds,
            };
            self.set_world_transform(e, xf);
        }
        self.drag_group = group;
    }

    /// Write `world_xf` (an absolute transform) to `e`, converting it back to the
    /// node's *local* transform when it has a parent (so dragging a child's gizmo
    /// edits its local placement, and parents still carry it).
    pub(crate) fn set_world_transform(&mut self, e: Entity, world_xf: Transform) {
        // A Map-tool drag targets sub-objects (verts/edges/faces), not the
        // node: translate the snapshot verts by the gizmo's world delta and
        // never touch the Transform. Route before everything else.
        if self.map_drag.is_some() {
            if let Some(d) = self.drag {
                self.map_apply_drag(d.start_xf, world_xf);
            }
            return;
        }
        // A gizmo drag on an armature bone / model object (not an ECS entity):
        // in pivot-edit mode it moves the object's rotation pivot; otherwise it poses
        // the bone into the open clip. Route it before any Transform write.
        #[cfg(feature = "editor-ui")]
        if let Some(idx) = self.drag.and_then(|d| (d.entity == e).then_some(d.bone).flatten()) {
            if self.pivot_edit {
                self.set_bone_pivot(e, idx, world_xf);
            } else {
                self.set_bone_world(e, idx, world_xf);
            }
            return;
        }
        // A bone-attached node's Transform is regenerated from BoneAttach.offset every
        // frame by resolve_attachments, so writing Transform here would be clobbered next
        // frame (the node snaps back onto the bone). Edit the offset instead — bone-local,
        // via the componentwise TRS inverse (so a mirrored mesh's negative scale stays on
        // the right axis; a matrix decomposition would pin it to X and pop the node).
        if let Some(a) = self.world.get::<floptle_core::BoneAttach>(e).cloned()
            && let Some(bone_world) = crate::anim::bone_world_transform(
                &self.anim,
                &self.world,
                &self.mesh_registry,
                a.target,
                &a.bone,
            )
        {
            let offset = bone_world.inv_mul(&world_xf);
            // Guard against a degenerate bone frame (e.g. a zero-scale mesh) —
            // writing a NaN offset would corrupt the attachment.
            if offset.translation.is_finite()
                && offset.scale.is_finite()
                && let Some(at) = self.world.get_mut::<floptle_core::BoneAttach>(e)
            {
                at.offset = offset;
            }
            return;
        }
        let local = match self.world.get::<floptle_core::Parent>(e).copied() {
            None => world_xf,
            Some(floptle_core::Parent(p)) => {
                // Componentwise TRS inverse of the scene graph's own composition —
                // exact for mirrored (negative-scale) parents, unlike a matrix
                // inverse + decomposition.
                let pw = floptle_core::world_transform(&self.world, p);
                pw.inv_mul(&world_xf)
            }
        };
        if let Some(t) = self.world.get_mut::<Transform>(e) {
            *t = local;
        }
    }

    /// The transform gizmo's target when an armature bone is selected in the
    /// Hierarchy: `(mesh entity, bone index, the bone's world Transform in scene
    /// space)`. Bones aren't ECS entities, so the gizmo is driven off this instead
    /// of a `Transform` component. `None` unless a bone on a rigged mesh is selected.
    pub(crate) fn bone_gizmo_target(&self) -> Option<(Entity, usize, Transform)> {
        let (mesh, idx) = self.bone_selection?;
        let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh) else {
            return None;
        };
        let rig = self.mesh_registry.get(asset_path)?.rig.as_ref()?;
        // Live pose if animating, else the rest pose — same source as the bone
        // Inspector and `bone_world_matrix` (offset already baked into `poses`).
        let bone_local = self
            .anim
            .poses
            .get(&mesh)
            .and_then(|p| p.get(idx))
            .or_else(|| rig.rest_world.get(idx))
            .copied()
            .unwrap_or(Mat4::IDENTITY);
        // Sit the gizmo at the object's pivot (its joint), not the node origin — for a
        // baked object the origin is at the model root (the feet), which is exactly the
        // problem. `bone_local · T(pivot)` places it at the pivot with the node's orientation.
        let pivot = rig.skeleton.nodes.get(idx).map(|n| n.pivot).unwrap_or(Vec3::ZERO);
        let world_m = floptle_core::world_transform(&self.world, mesh).world_matrix()
            * (bone_local * Mat4::from_translation(pivot)).as_dmat4();
        Some((mesh, idx, Transform::from_matrix(world_m)))
    }

    /// Apply a gizmo drag to an armature bone: convert the desired world transform
    /// back to the bone's local pose (relative to its parent bone) and auto-key it
    /// into the open clip at the playhead — exactly what the bone Inspector's numeric
    /// editor writes, so posing a bone with the gizmo == keying it.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn set_bone_world(&mut self, mesh: Entity, idx: usize, world_xf: Transform) {
        let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned() else {
            return;
        };
        // Pull everything off the rig, then drop the borrow before touching anim_ui.
        let (parent, parent_local, offset, bone_name, anchor) = {
            let Some(rig) = self.mesh_registry.get(&asset_path).and_then(|m| m.rig.as_ref())
            else {
                return;
            };
            let parent = rig.skeleton.nodes.get(idx).and_then(|n| n.parent);
            let pw = match parent {
                Some(p) => self
                    .anim
                    .poses
                    .get(&mesh)
                    .and_then(|ps| ps.get(p))
                    .or_else(|| rig.rest_world.get(p))
                    .copied()
                    .unwrap_or(Mat4::IDENTITY),
                None => Mat4::IDENTITY,
            };
            (
                parent,
                pw,
                rig.offset,
                rig.skeleton.nodes.get(idx).map(|n| n.name.clone()),
                rig.skeleton.nodes.get(idx).map(|n| n.pivot_anchor()).unwrap_or(Vec3::ZERO),
            )
        };
        let Some(bone_name) = bone_name else { return };
        let mesh_world = floptle_core::world_transform(&self.world, mesh).world_matrix();
        // Parent frame in scene space. bone_scene = mesh_world · poses[bone] and
        // poses[bone] = poses[parent] · local (the offset cancels), so the parent
        // scene frame is mesh_world · poses[parent], or mesh_world · offset at a root.
        let parent_scene = match parent {
            Some(_) => mesh_world * parent_local.as_dmat4(),
            None => mesh_world * offset.as_dmat4(),
        };
        // The gizmo sits at the pivot, so `parent_scene⁻¹ · world` = T(t + anchor)·R·S
        // (see `TransformTRS::matrix_about_rest`); its translation is `t + anchor`, so
        // the node's pose translation is that minus the pivot anchor — the pivot mapped
        // into parent space by the rest rotation/scale, which is the offset the forward
        // composition actually added. Subtracting the raw pivot instead would drift any
        // node whose rest is rotated. (pivot = 0 → anchor = 0 → unchanged.)
        let local_m = parent_scene.inverse() * world_xf.world_matrix();
        if !local_m.is_finite() {
            return;
        }
        let (s, r, t) = local_m.to_scale_rotation_translation();
        let trs = floptle_anim::TransformTRS {
            t: t.as_vec3() - anchor,
            r: r.as_quat(),
            s: s.as_vec3(),
        };
        // Auto-key at the playhead — same gate as bone_inspector_ui (this mesh must be
        // the Animating tab's target with a clip open, since channels bind by name).
        #[cfg(feature = "editor-ui")]
        if self.anim_ui.target == Some(mesh) && self.anim_ui.clip_doc.is_some() {
            // One undo step per drag gesture (snapshot_clip is a no-op once dirty).
            crate::anim_ui::snapshot_clip(&mut self.anim_ui);
            let ph = self.anim_ui.playhead;
            if let Some((_, doc)) = self.anim_ui.clip_doc.as_mut() {
                crate::anim_ui::write_key(doc, &bone_name, ph, &trs);
            }
            self.anim_ui.clip_dirty = true;
        }
    }

    /// Move an object/bone's rotation pivot to the dragged gizmo position (pivot-edit
    /// mode). The gizmo sits at the pivot in the node's rest frame, so the node-local
    /// pivot point is `(mesh_world · rest_world[idx])⁻¹ · gizmo_world`. Applied live +
    /// persisted to the `.rig.ron` sidecar.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn set_bone_pivot(&mut self, mesh: Entity, idx: usize, world_xf: Transform) {
        let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned() else {
            return;
        };
        let mesh_world = floptle_core::world_transform(&self.world, mesh).world_matrix();
        let (rest_world_i, node_name) = {
            let Some(rig) = self.mesh_registry.get(&asset_path).and_then(|m| m.rig.as_ref())
            else {
                return;
            };
            (
                rig.rest_world.get(idx).copied().unwrap_or(Mat4::IDENTITY),
                rig.skeleton.nodes.get(idx).map(|n| n.name.clone()),
            )
        };
        let Some(node_name) = node_name else { return };
        let base = mesh_world * rest_world_i.as_dmat4();
        let local = base.inverse() * world_xf.world_matrix();
        if !local.is_finite() {
            return;
        }
        let p = local.to_scale_rotation_translation().2.as_vec3();
        self.apply_object_pivot(mesh, &node_name, p);
    }

    /// Set an object/bone's rotation pivot (node-local) live on the shared rig and
    /// persist it to the model's `.rig.ron` sidecar. Shared by the pivot-drag gizmo
    /// and the Inspector's numeric pivot fields.
    pub(crate) fn apply_object_pivot(&mut self, mesh: Entity, node_name: &str, pivot: Vec3) {
        let Some(Matter::Mesh { asset_path }) = self.world.get::<Matter>(mesh).cloned() else {
            return;
        };
        if let Some(rig) = self.mesh_registry.get_mut(&asset_path).and_then(|m| m.rig.as_mut())
            && let Some(i) = rig.skeleton.index_of(node_name)
        {
            rig.skeleton.nodes[i].pivot = pivot;
        }
        let abs = self.resolve_asset_path(&asset_path);
        let mut ov = crate::rig_overrides::RigOverrides::load(&abs);
        ov.pivot.insert(node_name.to_string(), [pivot.x, pivot.y, pivot.z]);
        let _ = ov.save(&abs);
    }
}

/// Surrender egui's focused widget for a press it never sees.
///
/// Fires when the press is over a viewport, or outside egui altogether.
/// `is_pointer_over_egui` alone cannot decide: the docked Scene tab is
/// egui's own central area, so over the viewport it answers true. Returns
/// whether a widget lost focus.
#[cfg(feature = "editor-ui")]
pub(crate) fn end_typing_on_press(ctx: &egui::Context, over_viewport: bool) -> bool {
    if !over_viewport && ctx.is_pointer_over_egui() {
        return false;
    }
    match ctx.memory(|m| m.focused()) {
        Some(id) => {
            ctx.memory_mut(|m| m.surrender_focus(id));
            true
        }
        None => false,
    }
}

/// [`Editor::select_bone`]'s rule, as a free function — the Hierarchy tree draws
/// from borrowed field references rather than from `&mut Editor`, and the rule
/// has to be the same one in both places or a bone would be clickable from one
/// panel and not the other.
pub(crate) fn select_bone_into(
    selection: &mut Vec<Entity>,
    bone_selection: &mut Option<(Entity, usize)>,
    locked: bool,
    mesh: Entity,
    idx: usize,
) -> bool {
    if locked {
        // Only a bone of a model the lock is already holding.
        if !selection.contains(&mesh) {
            return false;
        }
        *bone_selection = Some((mesh, idx));
        return true;
    }
    *bone_selection = Some((mesh, idx));
    selection.clear();
    true
}

/// The object's local bounds half-extents (pre-`Transform.scale`) for the Rect
/// tool's face handles — mirrors [`Editor::pick`]'s primitive sizes. `None` =
/// the Rect tool has no box for this matter (Empty, lights, UI elements — the
/// Scene tab gives those their own 2D handles).
pub(crate) fn rect_base_half(
    world: &floptle_core::World,
    mesh_registry: &std::collections::HashMap<String, crate::MeshAsset>,
    e: Entity,
) -> Option<Vec3> {
    // A UI element's rect is edited by the Scene-tab handles, not the 3D box.
    if world.get::<floptle_ui::ElementSpec>(e).is_some() {
        return None;
    }
    match world.get::<Matter>(e)? {
        Matter::Primitive { shape, .. } => Some(match shape {
            Shape::Cube => Vec3::splat(0.7),
            Shape::Plane => Vec3::new(0.7, 0.7, 0.02),
            Shape::Sphere => Vec3::splat(0.85),
            Shape::Capsule => Vec3::new(0.5, 1.0, 0.5),
        }),
        Matter::Blob { scale } => Some(Vec3::splat(0.85 * *scale)),
        Matter::Mesh { asset_path } => {
            let r = mesh_registry.get(asset_path).map(|a| a.size * 0.5).unwrap_or(1.0);
            Some(Vec3::splat(r.max(0.05)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;

    /// A child node's `Transform.translation` is an offset from its parent.
    /// Framing on it flies the camera to wherever that offset happens to land in
    /// world space — for a door at `(0.4, 0, 1.2)` inside a building parked a
    /// kilometre away, that is the origin, and it reads as the `F` key having
    /// done nothing rather than as it having gone somewhere wrong.
    ///
    /// Every unparented node works either way, which is exactly why this lasted.
    #[test]
    fn framing_a_parented_node_goes_where_the_node_actually_is() {
        let mut ed = Editor::default();
        let parent = ed.world.spawn();
        ed.world.insert(parent, floptle_core::Name("Building".into()));
        ed.world.insert(
            parent,
            Transform { translation: DVec3::new(1000.0, 0.0, -500.0), ..Transform::IDENTITY },
        );
        let child = ed.world.spawn();
        ed.world.insert(child, floptle_core::Name("Door".into()));
        ed.world.insert(child, floptle_core::Parent(parent));
        ed.world
            .insert(child, Transform { translation: DVec3::new(0.4, 0.0, 1.2), ..Transform::IDENTITY });

        ed.selection = vec![child];
        ed.focus_selected();

        let anim = ed.focus_anim.expect("F should have started a move");
        let world = floptle_core::world_transform(&ed.world, child).translation;
        assert!(
            world.length() > 1000.0,
            "the fixture is wrong — the door has to be far from the origin for \
             this to be able to fail"
        );
        // The claim: the door ends up straight ahead of where the camera stopped.
        // Framing on the local offset would leave the camera near the origin,
        // pointing at a door a kilometre away.
        let forward = (ed.camera.rotation() * Vec3::NEG_Z).as_dvec3();
        let to_door = world - anim.to;
        assert!(
            to_door.normalize().dot(forward) > 0.999,
            "the door is not straight ahead of where the camera stopped: {to_door:?}"
        );
    }

    /// The unparented case, unchanged — the one every scene in every test has.
    #[test]
    fn framing_a_root_node_still_lands_on_it() {
        let mut ed = Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Name("Crate".into()));
        ed.world
            .insert(e, Transform { translation: DVec3::new(3.0, 1.0, -2.0), ..Transform::IDENTITY });
        ed.selection = vec![e];
        ed.focus_selected();

        let anim = ed.focus_anim.expect("F should have started a move");
        let forward = (ed.camera.rotation() * Vec3::NEG_Z).as_dvec3();
        let to_crate = DVec3::new(3.0, 1.0, -2.0) - anim.to;
        assert!(to_crate.normalize().dot(forward) > 0.999, "{to_crate:?}");
    }

    // ---- the selection lock -------------------------------------------------
    //
    // The lock exists so you can edit one node's fields while clicking around
    // the rest of the scene. Every test here is about one of the two ways that
    // can go wrong: a gesture getting through, or the lock stranding the editor
    // with no way to release it.

    /// A node with a name and a transform — the shape the Inspector draws for.
    fn locked_node(ed: &mut Editor, name: &str) -> Entity {
        let e = ed.world.spawn();
        ed.world.insert(e, floptle_core::Name(name.into()));
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(e, Matter::Empty);
        e
    }

    #[test]
    fn a_held_selection_ignores_every_gesture() {
        let mut ed = Editor::default();
        let a = locked_node(&mut ed, "A");
        let b = locked_node(&mut ed, "B");
        ed.select_single(a);
        ed.toggle_selection_lock();
        assert!(ed.selection_locked, "a non-empty selection locks");

        ed.select_single(b);
        assert_eq!(ed.selection, vec![a], "a viewport pick must not move the selection");
        ed.select_toggle(b);
        assert_eq!(ed.selection, vec![a], "ctrl-click must not add to it");
        ed.select_all();
        assert_eq!(ed.selection, vec![a], "select-all must not replace it");
        ed.clear_selection();
        assert_eq!(ed.selection, vec![a], "clicking empty space must not clear it");
        ed.step_selection(1);
        assert_eq!(ed.selection, vec![a], "the arrow keys must not step off it");

        ed.toggle_selection_lock();
        assert!(!ed.selection_locked);
        ed.select_single(b);
        assert_eq!(ed.selection, vec![b], "released, the same gesture works again");
    }

    /// **A held selection holds the scene, not the rig.**
    ///
    /// Locking is most wanted for exactly the job it used to make impossible:
    /// posing one model without a stray click taking you off it. Every bone
    /// click was swallowed with everything else, so the lock and the Animating
    /// tab could not be used together at all.
    ///
    /// A bone of the held model is part of that model, not a way out of it. A
    /// bone of some other model still is, and is still refused — otherwise the
    /// lock leaks through the one panel that lists every rig in the scene.
    #[test]
    fn a_held_selection_still_lets_you_pick_the_model_s_own_bones() {
        let mut ed = Editor::default();
        let mine = locked_node(&mut ed, "Knight");
        let other = locked_node(&mut ed, "Horse");
        ed.select_single(mine);
        ed.toggle_selection_lock();
        assert!(ed.selection_locked);

        assert!(ed.select_bone(mine, 3), "a bone of the held model was refused");
        assert_eq!(ed.bone_selection, Some((mine, 3)));
        // The model stays selected. It has to: the lock has nothing to hold
        // otherwise (`enforce_selection_lock` would release it next frame) and
        // the rig is only drawn for a mesh that is selected, so clearing it
        // would take the bones off the screen the instant one was picked.
        assert_eq!(ed.selection, vec![mine], "picking a bone dropped the held model");
        ed.enforce_selection_lock();
        assert!(ed.selection_locked, "the lock released itself after a bone pick");

        // Another bone of the same model: fine.
        assert!(ed.select_bone(mine, 7));
        assert_eq!(ed.bone_selection, Some((mine, 7)));
        // Another model's bone: refused, and nothing moves.
        assert!(!ed.select_bone(other, 1), "the lock leaked through another model's rig");
        assert_eq!(ed.bone_selection, Some((mine, 7)));
        assert_eq!(ed.selection, vec![mine]);
    }

    /// Released, a bone pick behaves as it always did: the bone replaces the
    /// node selection, because the two are mutually exclusive and the Inspector
    /// switches to the bone editor.
    #[test]
    fn an_unheld_bone_pick_still_replaces_the_node_selection() {
        let mut ed = Editor::default();
        let a = locked_node(&mut ed, "A");
        let b = locked_node(&mut ed, "B");
        ed.select_single(a);
        assert!(ed.select_bone(b, 2), "an unheld selection refuses nothing");
        assert_eq!(ed.bone_selection, Some((b, 2)));
        assert!(ed.selection.is_empty(), "a bone and a node selection are exclusive");
    }

    /// The trap this guards: the lock's only switch is the Inspector's name
    /// row, which is drawn for the selected node. Locked with nothing selected,
    /// there is no row, so there is no way back.
    #[test]
    fn an_empty_selection_cannot_be_locked() {
        let mut ed = Editor::default();
        ed.toggle_selection_lock();
        assert!(!ed.selection_locked, "there would be no name row to unlock from");
    }

    /// …and the same trap from the other side: locked, then the world takes the
    /// node away (deleted, or the scene switched).
    #[test]
    fn a_lock_the_world_empties_releases_itself() {
        let mut ed = Editor::default();
        let a = locked_node(&mut ed, "A");
        ed.select_single(a);
        ed.toggle_selection_lock();

        // Not a gesture — this is the scene changing underneath, which is the
        // one thing the lock does not hold back.
        ed.selection.clear();
        ed.enforce_selection_lock();
        assert!(!ed.selection_locked, "a lock over nothing must release itself");
    }

    /// The lock holds back clicks, not the world. Undo restoring what was
    /// selected at the time is not somebody clicking, and a lock that swallowed
    /// it would leave the Inspector pointing at a node that is not there.
    #[test]
    fn the_lock_does_not_hold_back_the_world_itself() {
        let mut ed = Editor::default();
        let a = locked_node(&mut ed, "A");
        let b = locked_node(&mut ed, "B");
        ed.select_single(a);
        ed.toggle_selection_lock();

        ed.selection = vec![b];
        assert_eq!(ed.selection, vec![b], "history and scene paths still write directly");
        ed.enforce_selection_lock();
        assert!(ed.selection_locked, "and a non-empty result keeps the lock");
    }
}

#[cfg(all(test, feature = "editor-ui"))]
mod press_tests {
    use super::end_typing_on_press;

    /// One frame of a full-window panel holding a focused text field, with
    /// the pointer parked over it (so egui counts the pointer as over
    /// itself, exactly as it does over the docked Scene tab) and `typed`
    /// delivered as keyboard text.
    fn frame(ctx: &egui::Context, text: &mut String, typed: &str, focus: bool) {
        let mut input = crate::icons::test_input();
        input.events.push(egui::Event::PointerMoved(egui::pos2(400.0, 300.0)));
        if !typed.is_empty() {
            input.events.push(egui::Event::Text(typed.to_string()));
        }
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let resp = ui.add(egui::TextEdit::singleline(text).id_salt("field"));
                if focus {
                    resp.request_focus();
                }
            });
        });
    }

    /// The report: a number typed into the Inspector, a right-click into the
    /// Scene view, and the fly keys typed next still land in the field. Over
    /// the viewport egui says the pointer is over egui — the Scene tab is
    /// its central area — so the press must be trusted over that answer.
    #[test]
    fn a_press_over_the_viewport_takes_the_keyboard_off_the_field() {
        let ctx = crate::icons::test_context();
        let mut text = String::from("12");
        frame(&ctx, &mut text, "", true);
        frame(&ctx, &mut text, "", false);
        assert!(ctx.text_edit_focused(), "the field never took focus, so nothing is under test");
        assert!(ctx.is_pointer_over_egui(), "the pointer must read as over egui, as it does over the docked Scene tab");

        assert!(end_typing_on_press(&ctx, true));
        assert!(ctx.memory(|m| m.focused()).is_none());
        frame(&ctx, &mut text, "w", false);
        assert_eq!(text, "12", "a fly key typed after the press still went into the field");
        assert!(!ctx.text_edit_focused());
    }

    /// The same press over a panel is egui's to handle: the field keeps
    /// focus and keeps taking the letters. A control for the test above.
    #[test]
    fn a_press_over_a_panel_leaves_the_field_alone() {
        let ctx = crate::icons::test_context();
        let mut text = String::from("12");
        frame(&ctx, &mut text, "", true);
        frame(&ctx, &mut text, "", false);
        assert!(ctx.text_edit_focused());

        assert!(!end_typing_on_press(&ctx, false));
        frame(&ctx, &mut text, "3", false);
        assert_eq!(text, "123");
        assert!(ctx.text_edit_focused());
    }

    /// Nothing focused: nothing to surrender, and no panic.
    #[test]
    fn a_press_with_nothing_focused_is_a_no_op() {
        let ctx = crate::icons::test_context();
        let mut text = String::new();
        frame(&ctx, &mut text, "", false);
        assert!(!end_typing_on_press(&ctx, true));
    }
}

#[cfg(test)]
mod pick_tests {
    use super::*;
    use floptle_render::{MeshData, Vertex};

    /// A 100 × 100 floor model: two triangles at y = 0, the kind of ground or
    /// level piece a scene is built on.
    fn floor_quad() -> MeshData {
        let v = |x: f32, z: f32| Vertex { pos: [x, 0.0, z], normal: [0.0, 1.0, 0.0], uv: [0.0, 0.0] };
        MeshData {
            vertices: vec![v(-50.0, -50.0), v(50.0, -50.0), v(50.0, 50.0), v(-50.0, 50.0)],
            indices: vec![0, 2, 1, 0, 3, 2],
            colors: None,
        }
    }

    fn put(ed: &mut Editor, m: Matter, at: [f64; 3], scale: f32) -> Entity {
        let e = ed.world.spawn();
        ed.world.insert(e, m);
        ed.world.insert(
            e,
            Transform { translation: DVec3::from(at), scale: Vec3::splat(scale), ..Transform::IDENTITY },
        );
        e
    }

    fn cube() -> Matter {
        Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] }
    }

    /// **What is picked is what is drawn under the cursor.** Every case here
    /// picked the wrong node while models were tested against a sphere sized by
    /// their longest edge and any hit ranked by where its bounds began.
    #[test]
    fn a_click_selects_the_surface_under_the_cursor_not_the_biggest_bounds() {
        let mut ed = Editor::default();
        ed.mesh_registry.insert(
            "floor.glb".into(),
            crate::MeshAsset { parts: Vec::new(), part_meta: Vec::new(), tex_filter: None, size: 100.0, rig: None },
        );
        ed.paint_meshes.get_or_build("floor.glb", || vec![floor_quad()]);
        let floor = put(&mut ed, Matter::Mesh { asset_path: "floor.glb".into() }, [0.0; 3], 1.0);
        let crate_ = put(&mut ed, cube(), [10.0, 0.7, 0.0], 1.0);
        let far = put(&mut ed, cube(), [0.0, 5.0, -80.0], 1.0);

        // A crate standing on the floor, clicked from well outside the floor's
        // bounding sphere: the crate, and its top face.
        // Rays are camera-relative, as the viewport casts them.
        let cam = ed.camera.render_camera().world_position.as_vec3();
        let eye = Vec3::new(10.0, 60.0, 60.0);
        let rd = (Vec3::new(10.0, 1.4, 0.0) - eye).normalize();
        let hit = ed.raycast_nodes(eye - cam, rd, |_| true).expect("the crate is under the cursor");
        assert_eq!(hit.e, crate_);
        assert!((hit.pos.y + cam.y - 1.4).abs() < 1e-3 && hit.normal.y > 0.99, "{hit:?}");

        // The floor itself, where nothing stands on it.
        let rd = (Vec3::new(-20.0, 0.0, 10.0) - eye).normalize();
        assert_eq!(ed.raycast_nodes(eye - cam, rd, |_| true).map(|h| h.e), Some(floor));

        // Level with the floor, through its bounds but over its surface: the
        // node behind it.
        let eye = Vec3::new(0.0, 5.0, 80.0);
        assert_eq!(ed.raycast_nodes(eye - cam, Vec3::NEG_Z, |_| true).map(|h| h.e), Some(far));

        // Standing inside a room box: the crate in front, not the room.
        let mut ed = Editor::default();
        put(&mut ed, cube(), [0.0; 3], 30.0);
        let near = put(&mut ed, cube(), [0.0, 0.0, -5.0], 1.0);
        assert_eq!(ed.raycast_nodes(-cam, Vec3::NEG_Z, |_| true).map(|h| h.e), Some(near));

        // And a node `keep` refuses is looked through.
        let hit = ed.raycast_nodes(-cam, Vec3::NEG_Z, |e| e != near).expect("the room's wall");
        assert!((hit.t - 21.0).abs() < 1e-3, "{hit:?}");
    }
}
