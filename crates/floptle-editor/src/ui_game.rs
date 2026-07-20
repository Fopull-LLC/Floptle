//! Game-UI editor integration (docs/ui-system-proposal.md, phase 1).
//!
//! - `gather_game_ui`: walk the scene for `UiLayer` nodes, build each layer's
//!   element tree, solve layout (CPU — cheap, readable), emit draw lists, and
//!   pre-register any image textures. Runs before the GPU borrows.
//! - `ui_inspector`: the Inspector section for UI layers/elements — plain
//!   properties, no imposed look (shape/image/text toggles map to the spec's
//!   Options).
//! - `add_ui_node`: the Add ⏵ UI menu's spawn (Empty node + UI components —
//!   the modular-components model, no new Matter variants).

use std::collections::HashMap;

use floptle_core::math::{Vec3, Vec4};
use floptle_core::{Entity, Matter, Parent, Transform};
use floptle_render::{Projection, RenderCamera};
use floptle_scene::MatterDoc;
use floptle_ui::{
    Align, Anchor, Dir, ElementSpec, ImageSpec, Justify, MaskSpec, Place, ShapeSpec, Size,
    SliderPart, SliderSpec, StackCfg, TextSpec, UiLayer,
};

use crate::Editor;

/// The camera the game is being viewed through while playing: the scene's
/// active `Camera` node, or the editor fly-cam if none is marked active. Used
/// to cast the pointer ray for world-space UI interaction.
fn play_camera(world: &floptle_core::World, fallback: RenderCamera) -> RenderCamera {
    let active = world
        .query::<Matter>()
        .find_map(|(e, m)| matches!(m, Matter::Camera { active: true, .. }).then_some(e));
    match active {
        Some(e) => {
            let fov_y = match world.get::<Matter>(e) {
                Some(Matter::Camera { fov_y, .. }) => *fov_y,
                _ => 60f32.to_radians(),
            };
            let wt = floptle_core::world_transform(world, e);
            RenderCamera::new(
                wt.translation,
                wt.rotation,
                Projection::Perspective { fov_y, near: 0.05, far: 4000.0 },
            )
        }
        None => fallback,
    }
}



/// What Add ⏵ UI creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddUi {
    Layer,
    Panel,
    Text,
    Image,
    Slider,
    Button,
    Scroll,
}

/// Resolve a layer's mask pairs `(mask id, target id)` in scene order: every
/// element with a MaskSpec claims its targets BY NAME within this layer (first
/// name match in scene order). Order in = order out, so "earliest mask wins"
/// in [`floptle_ui::draw_list`] means earliest in the scene.
fn layer_masks(
    world: &floptle_core::World,
    ents: &HashMap<u32, Entity>,
    roots: &[floptle_ui::Node],
) -> Vec<(u32, u32)> {
    fn walk(n: &floptle_ui::Node, out: &mut Vec<u32>) {
        out.push(n.id);
        for c in &n.children {
            walk(c, out);
        }
    }
    let mut ids = Vec::new();
    for r in roots {
        walk(r, &mut ids);
    }
    let mut by_name: HashMap<&str, u32> = HashMap::new();
    for id in &ids {
        if let Some(e) = ents.get(id)
            && let Some(n) = world.get::<floptle_core::Name>(*e)
        {
            by_name.entry(n.0.as_str()).or_insert(*id);
        }
    }
    let mut out = Vec::new();
    for id in &ids {
        let Some(e) = ents.get(id) else { continue };
        let Some(spec) = world.get::<ElementSpec>(*e) else { continue };
        if let Some(mask) = &spec.mask {
            for t in &mask.targets {
                if let Some(&tid) = by_name.get(t.as_str()) {
                    out.push((*id, tid));
                }
            }
        }
    }
    out
}

impl Editor {
    /// Register a project font with the UI renderer (reads the file once; the
    /// renderer remembers parse failures and falls back to the embedded font).
    pub(crate) fn ensure_ui_font(&mut self, path: &str) {
        let Some(uir) = self.ui_render.as_mut() else { return };
        if path.is_empty() || uir.has_font(path) {
            return;
        }
        let bytes = std::fs::read(std::path::Path::new(path)).unwrap_or_default();
        uir.ensure_font(path, &bytes);
    }

    /// Pre-register every font any UI text references (before the immutable
    /// renderer borrow the measure callback needs).
    fn ensure_ui_fonts(&mut self) {
        let fonts: Vec<String> = self
            .world
            .query::<ElementSpec>()
            .filter_map(|(_, s)| s.text.as_ref())
            .map(|t| t.font.clone())
            .filter(|f| !f.is_empty())
            .collect();
        for f in fonts {
            self.ensure_ui_font(&f);
        }
    }

    /// Solve every UI layer for this frame: (draw list, px-per-design-unit),
    /// z-sorted. Pre-resolves image textures into the registry (needs
    /// `&mut self`, so this runs BEFORE the draw core's field borrows).
    pub(crate) fn gather_game_ui(&mut self, viewport: [f32; 2]) -> Vec<(floptle_ui::DrawList, f32)> {
        if viewport[0] <= 1.0 || viewport[1] <= 1.0 {
            return Vec::new();
        }
        self.ensure_ui_fonts();
        // Scene order + children map (node order = deterministic draw order).
        let order: Vec<Entity> = self.world.query::<Transform>().map(|(e, _)| e).collect();
        let ents: HashMap<u32, Entity> = order.iter().map(|e| (e.index(), *e)).collect();
        let mut kids: HashMap<u32, Vec<Entity>> = HashMap::new();
        for e in &order {
            if let Some(p) = self.world.get::<Parent>(*e) {
                kids.entry(p.0.index()).or_default().push(*e);
            }
        }
        fn build(
            world: &floptle_core::World,
            kids: &HashMap<u32, Vec<Entity>>,
            e: Entity,
        ) -> Option<floptle_ui::Node> {
            let spec = world.get::<ElementSpec>(e)?.clone();
            let children = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(world, kids, *c)).collect())
                .unwrap_or_default();
            Some(floptle_ui::Node { id: e.index(), spec, children })
        }
        let mut layers: Vec<(i32, Vec<floptle_ui::Node>, f32)> = Vec::new();
        for e in &order {
            let Some(layer) = self.world.get::<UiLayer>(*e).copied() else { continue };
            if !layer.enabled || layer.is_world() {
                continue; // world-space layers render in the scene, not as an overlay
            }
            let scale = (viewport[1] / layer.design_height.max(1.0)).max(0.01);
            let roots: Vec<_> = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(&self.world, &kids, *c)).collect())
                .unwrap_or_default();
            if !roots.is_empty() {
                layers.push((layer.z, roots, scale));
            }
        }
        if layers.is_empty() {
            return Vec::new();
        }
        layers.sort_by_key(|(z, ..)| *z);
        let Some(uir) = self.ui_render.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        let mut textures: Vec<String> = Vec::new();
        for (_, roots, scale) in &layers {
            let design_vp = [viewport[0] / scale, viewport[1] / scale];
            let measure = |t: &TextSpec| uir.measure_spec(t);
            let placed = floptle_ui::solve(roots, design_vp, &measure);
            let masks = layer_masks(&self.world, &ents, roots);
            let dl = floptle_ui::draw_list(roots, &placed, &masks);
            for q in &dl.quads {
                if !q.texture.is_empty() {
                    textures.push(q.texture.clone());
                }
            }
            out.push((dl, *scale));
        }
        for t in textures {
            let _ = self.ensure_texture(&t);
        }
        out
    }

    /// UI layers rendered as WORLD CANVASES — a flat quad at each layer node's
    /// transform: origin = translation (canvas top-left), plane axes from its
    /// rotation, `canvas_scale` world units per design unit. Returns per layer:
    /// (draw list, solved rects in design units, origin, right, down, design_vp).
    ///
    /// `include_screen` picks which layers qualify:
    /// - `true` (Scene authoring view): EVERY enabled layer, so a screen-space
    ///   layer still shows as a movable hologram you can arrange.
    /// - `false` (in-game): only [`UiSpace::World`] layers — screen-space ones
    ///   are drawn as the flat overlay instead.
    #[allow(clippy::type_complexity)]
    pub(crate) fn gather_ui_world(
        &mut self,
        window_aspect: f32,
        include_screen: bool,
    ) -> Vec<(floptle_ui::DrawList, Vec<floptle_ui::Placed>, [f64; 3], [f32; 3], [f32; 3], [f32; 2])>
    {
        self.ensure_ui_fonts();
        let order: Vec<Entity> = self.world.query::<Transform>().map(|(e, _)| e).collect();
        let ents: HashMap<u32, Entity> = order.iter().map(|e| (e.index(), *e)).collect();
        let mut kids: HashMap<u32, Vec<Entity>> = HashMap::new();
        for e in &order {
            if let Some(p) = self.world.get::<Parent>(*e) {
                kids.entry(p.0.index()).or_default().push(*e);
            }
        }
        fn build(
            world: &floptle_core::World,
            kids: &HashMap<u32, Vec<Entity>>,
            e: Entity,
        ) -> Option<floptle_ui::Node> {
            let spec = world.get::<ElementSpec>(e)?.clone();
            let children = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(world, kids, *c)).collect())
                .unwrap_or_default();
            Some(floptle_ui::Node { id: e.index(), spec, children })
        }
        let Some(uir) = self.ui_render.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        let mut textures: Vec<String> = Vec::new();
        for e in &order {
            let Some(layer) = self.world.get::<UiLayer>(*e).copied() else { continue };
            if !layer.enabled || (!include_screen && !layer.is_world()) {
                continue;
            }
            let roots: Vec<_> = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(&self.world, &kids, *c)).collect())
                .unwrap_or_default();
            if roots.is_empty() {
                continue;
            }
            let design_vp =
                [layer.design_height * window_aspect.max(0.1), layer.design_height];
            let measure = |t: &TextSpec| uir.measure_spec(t);
            let placed = floptle_ui::solve(&roots, design_vp, &measure);
            let masks = layer_masks(&self.world, &ents, &roots);
            let dl = floptle_ui::draw_list(&roots, &placed, &masks);
            for q in &dl.quads {
                if !q.texture.is_empty() {
                    textures.push(q.texture.clone());
                }
            }
            let wt = floptle_core::world_transform(&self.world, *e);
            let ws = layer.canvas_scale.max(0.0001);
            let right = wt.rotation * floptle_core::math::Vec3::X * ws;
            let down = wt.rotation * (-floptle_core::math::Vec3::Y) * ws;
            out.push((
                dl,
                placed,
                [wt.translation.x, wt.translation.y, wt.translation.z],
                [right.x, right.y, right.z],
                [down.x, down.y, down.z],
                design_vp,
            ));
        }
        for t in textures {
            let _ = self.ensure_texture(&t);
        }
        out
    }

    /// Pointer position + viewport (physical px, game-view space) for game-UI
    /// interaction. `None` when the cursor is hidden/locked (FPS look, game
    /// trap) or outside the game viewport.
    fn ui_pointer(&self) -> Option<([f32; 2], [f32; 2])> {
        if self.script_mouse_lock || self.game_trap {
            return None;
        }
        let cursor = self.cursor?;
        let gpu = self.gpu.as_ref()?;
        let win = [gpu.config.width as f32, gpu.config.height.max(1) as f32];
        if self.game_view() || self.player_mode {
            // The UI draws over the whole window here.
            return Some(([cursor.x, cursor.y], win));
        }
        // Docked Game tab: viewport-local coordinates.
        let r = self.game_rect?;
        let ppp = self.egui.as_ref().map(|e| e.ctx.pixels_per_point()).unwrap_or(1.0);
        let p = [cursor.x - r.min.x * ppp, cursor.y - r.min.y * ppp];
        let size = [r.width() * ppp, r.height() * ppp];
        if p[0] < 0.0 || p[1] < 0.0 || p[0] > size[0] || p[1] > size[1] {
            return None;
        }
        Some((p, size))
    }

    /// The game-UI interaction pass (buttons + draggable sliders), run each
    /// frame while playing, BEFORE the scripts (so a slider's new value is
    /// visible to this frame's `update`). Detected hook events land in
    /// `self.ui_events`, dispatched to Lua after the script run.
    pub(crate) fn ui_interact(&mut self) {
        self.ui_events.clear();
        let down = self.input_buttons[0];
        // Edges come from banked EVENTS (never missed, even when a whole click
        // fits inside one slow frame) OR the sampled state transition.
        let pressed_edge = std::mem::take(&mut self.ui_lmb_pressed_evt) || (down && !self.ui_lmb_was);
        let released_edge =
            std::mem::take(&mut self.ui_lmb_released_evt) || (!down && self.ui_lmb_was);
        self.ui_lmb_was = down;
        if !self.playing {
            self.ui_hover = None;
            self.ui_active = None;
            // Don't let edit-mode clicks bank up and fire as phantom presses
            // on the first playing frame.
            self.ui_lmb_pressed_evt = false;
            self.ui_lmb_released_evt = false;
            return;
        }
        let pointer = self.ui_pointer();
        // Collect every interactive element in draw order (later = on top). Each
        // item carries the pointer's position IN THAT LAYER'S design units, so
        // screen-space (pointer px / scale) and world-space (camera ray → panel
        // plane) hit-test through one uniform `contains`: (id, rect, pointer
        // design-units or None if off-panel, slider spec).
        // (id, rect design-units, pointer in design-units or None if off-panel,
        //  slider, scroll-clip rect if inside a scroll view).
        type InteractItem =
            (u32, [f32; 4], Option<[f32; 2]>, Option<SliderSpec>, Option<[f32; 4]>);
        let mut items: Vec<InteractItem> = Vec::new();
        // The topmost scroll view under the pointer this frame → (entity id,
        // clamped new offset). Applied after the borrows drop; consumes the
        // wheel so gameplay zoom never fights a menu scroll.
        let wheel = self.input_scroll;
        let mut wheel_target: Option<(u32, f32)> = None;
        if let Some((ptr_px, viewport)) = pointer
            && viewport[0] > 1.0
            && viewport[1] > 1.0
        {
            self.ensure_ui_fonts();
            let order: Vec<Entity> = self.world.query::<Transform>().map(|(e, _)| e).collect();
            let mut kids: HashMap<u32, Vec<Entity>> = HashMap::new();
            for e in &order {
                if let Some(p) = self.world.get::<Parent>(*e) {
                    kids.entry(p.0.index()).or_default().push(*e);
                }
            }
            fn build(
                world: &floptle_core::World,
                kids: &HashMap<u32, Vec<Entity>>,
                e: Entity,
            ) -> Option<floptle_ui::Node> {
                let spec = world.get::<ElementSpec>(e)?.clone();
                let children = kids
                    .get(&e.index())
                    .map(|cs| cs.iter().filter_map(|c| build(world, kids, *c)).collect())
                    .unwrap_or_default();
                Some(floptle_ui::Node { id: e.index(), spec, children })
            }
            // Camera-relative pointer ray (for world-space panels). ADR-0015:
            // the world is offset to the camera, so the ray origin is ~0.
            let cam = play_camera(&self.world, self.camera.render_camera());
            let aspect = viewport[0] / viewport[1];
            let inv = cam.view_proj(aspect).inverse();
            let ndc = [ptr_px[0] / viewport[0] * 2.0 - 1.0, 1.0 - ptr_px[1] / viewport[1] * 2.0];
            let near = inv * Vec4::new(ndc[0], ndc[1], 0.0, 1.0);
            let far = inv * Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
            let ro = near.truncate() / near.w;
            let rd = (far.truncate() / far.w - ro).normalize();

            // (z, roots, layer) in draw order.
            let mut layers: Vec<(i32, Vec<floptle_ui::Node>, UiLayer, Entity)> = Vec::new();
            for e in &order {
                let Some(layer) = self.world.get::<UiLayer>(*e).copied() else { continue };
                if !layer.enabled {
                    continue;
                }
                let roots: Vec<_> = kids
                    .get(&e.index())
                    .map(|cs| cs.iter().filter_map(|c| build(&self.world, &kids, *c)).collect())
                    .unwrap_or_default();
                if !roots.is_empty() {
                    layers.push((layer.z, roots, layer, *e));
                }
            }
            layers.sort_by_key(|(z, ..)| *z);
            if let Some(uir) = self.ui_render.as_ref() {
                for (_, roots, layer, e) in &layers {
                    // Design viewport + the pointer's position within it.
                    let (design_vp, ptr_design) = if layer.is_world() {
                        // Ray → panel plane; design coords along right/down axes.
                        let dh = layer.design_height;
                        let dvp = [dh * aspect.max(0.1), dh];
                        let wt = floptle_core::world_transform(&self.world, *e);
                        let ws = layer.canvas_scale.max(0.0001);
                        let right = wt.rotation * Vec3::X * ws;
                        let down = wt.rotation * (-Vec3::Y) * ws;
                        let origin = Vec3::new(
                            (wt.translation.x - cam.world_position.x) as f32,
                            (wt.translation.y - cam.world_position.y) as f32,
                            (wt.translation.z - cam.world_position.z) as f32,
                        );
                        let n = right.cross(down);
                        let denom = rd.dot(n);
                        let pd = if denom.abs() > 1e-6 {
                            let t = (origin - ro).dot(n) / denom;
                            if t > 0.0 {
                                let hit = ro + rd * t;
                                let rel = hit - origin;
                                Some([
                                    rel.dot(right) / right.length_squared(),
                                    rel.dot(down) / down.length_squared(),
                                ])
                            } else {
                                None // panel is behind the camera
                            }
                        } else {
                            None // ray parallel to the panel
                        };
                        (dvp, pd)
                    } else {
                        let scale = (viewport[1] / layer.design_height.max(1.0)).max(0.01);
                        (
                            [viewport[0] / scale, viewport[1] / scale],
                            Some([ptr_px[0] / scale, ptr_px[1] / scale]),
                        )
                    };
                    let measure = |t: &TextSpec| uir.measure_spec(t);
                    let placed = floptle_ui::solve(roots, design_vp, &measure);
                    fn specs<'a>(n: &'a floptle_ui::Node, m: &mut HashMap<u32, &'a ElementSpec>) {
                        m.insert(n.id, &n.spec);
                        for c in &n.children {
                            specs(c, m);
                        }
                    }
                    let mut spec_of = HashMap::new();
                    for r in roots {
                        specs(r, &mut spec_of);
                    }
                    let in_rect = |r: &[f32; 4], p: &[f32; 2]| {
                        p[0] >= r[0] && p[1] >= r[1] && p[0] <= r[0] + r[2] && p[1] <= r[1] + r[3]
                    };
                    // Elements inside a scroll view hit-test through its clip:
                    // a row scrolled out of the view must not hover or click.
                    let clips = floptle_ui::scroll_clips(roots, &placed);
                    for pl in &placed {
                        let Some(spec) = spec_of.get(&pl.id) else { continue };
                        let clip = clips.get(&pl.id).map(|c| c.rect);
                        let slider = spec.slider.filter(|s| s.interact);
                        if spec.button || slider.is_some() {
                            items.push((pl.id, pl.rect, ptr_design, slider, clip));
                        }
                        // Wheel over a scroll view (respecting its own clip if
                        // nested): later layers/elements are on top, so the
                        // LAST match wins.
                        if wheel != 0.0
                            && let Some(sc) = spec.scroll
                            && ptr_design.is_some_and(|p| {
                                in_rect(&pl.rect, &p)
                                    && clip.is_none_or(|c| in_rect(&c, &p))
                            })
                        {
                            let max = floptle_ui::scroll_max(roots, &placed, pl.id);
                            let next = (sc.offset - wheel * sc.speed).clamp(0.0, max);
                            wheel_target = Some((pl.id, next));
                        }
                    }
                }
            }
        }
        if let Some((id, next)) = wheel_target {
            let ent = self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == id);
            if let Some(e) = ent
                && let Some(spec) = self.world.get_mut::<ElementSpec>(e)
                && let Some(sc) = &mut spec.scroll
            {
                sc.offset = next;
            }
            self.input_scroll = 0.0;
            self.tick_scroll = 0.0;
        }
        let contains = |r: &[f32; 4], p: &[f32; 2]| {
            p[0] >= r[0] && p[1] >= r[1] && p[0] <= r[0] + r[2] && p[1] <= r[1] + r[3]
        };
        // Topmost interactive element under the pointer (per-item design
        // pointer), honoring scroll clips.
        let hover = items
            .iter()
            .rev()
            .find(|(_, rect, pd, _, clip)| {
                pd.is_some_and(|p| contains(rect, &p) && clip.is_none_or(|c| contains(&c, &p)))
            })
            .map(|(id, ..)| *id);
        if hover != self.ui_hover {
            if let Some(old) = self.ui_hover {
                self.ui_events.push((old, "hoverEnd"));
            }
            if let Some(new) = hover {
                self.ui_events.push((new, "hoverStart"));
            }
            self.ui_hover = hover;
        }
        if pressed_edge && let Some(h) = hover {
            self.ui_active = Some(h);
            self.ui_events.push((h, "pressed"));
        }
        // A grabbed interactive slider follows the pointer while held —
        // even when it wanders off the track (normal drag feel). The pointer is
        // already in the panel's design units (screen or world) from gathering.
        if down
            && self.ui_active.is_some()
            && let Some((id, rect, Some(pd), Some(s), _)) = items
                .iter()
                .find(|(id, ..)| Some(*id) == self.ui_active)
                .copied()
        {
            let axis = match s.dir {
                Dir::Row => 0,
                Dir::Column => 1,
            };
            let mut t = ((pd[axis] - rect[axis]) / rect[axis + 2].max(1e-3)).clamp(0.0, 1.0);
            if s.flip {
                t = 1.0 - t;
            }
            let value = s.min + t * (s.max - s.min);
            let ent = self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == id);
            if let Some(e) = ent
                && let Some(spec) = self.world.get_mut::<ElementSpec>(e)
                && let Some(sl) = &mut spec.slider
            {
                sl.value = value;
            }
        }
        if released_edge && let Some(a) = self.ui_active.take() {
            self.ui_events.push((a, "released"));
            if hover == Some(a) {
                self.ui_events.push((a, "clicked"));
            }
        }
        if pointer.is_none() && !down {
            self.ui_active = None;
        }
    }

    /// Add ⏵ UI: an Empty node carrying the UI components. Elements land
    /// under the selected node (so building a screen is: add a Layer, keep
    /// adding elements inside it).
    pub(crate) fn add_ui_node(&mut self, what: AddUi) {
        let (name, layer, spec): (&str, Option<UiLayer>, Option<ElementSpec>) = match what {
            AddUi::Layer => ("UI Layer", Some(UiLayer::default()), None),
            AddUi::Panel => (
                "Panel",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(240.0), Size::Fixed(140.0)],
                    shape: Some(ShapeSpec { fill: [0.12, 0.12, 0.14, 0.85], ..Default::default() }),
                    ..Default::default()
                }),
            ),
            AddUi::Text => (
                "Text",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    text: Some(TextSpec { text: "Text".into(), ..Default::default() }),
                    ..Default::default()
                }),
            ),
            AddUi::Image => (
                "Image",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(128.0), Size::Fixed(128.0)],
                    image: Some(ImageSpec::default()),
                    ..Default::default()
                }),
            ),
            AddUi::Button => (
                "Button",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(200.0), Size::Fixed(56.0)],
                    shape: Some(ShapeSpec {
                        fill: [0.16, 0.16, 0.19, 0.95],
                        radius: 10.0,
                        ..Default::default()
                    }),
                    text: Some(TextSpec { text: "Button".into(), align: Align::Center, ..Default::default() }),
                    button: true,
                    ..Default::default()
                }),
            ),
            // The track. Its Fill/Handle children are spawned below — they are
            // ordinary elements the designer retextures/moves/resizes freely.
            AddUi::Slider => (
                "Slider",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(320.0), Size::Fixed(28.0)],
                    shape: Some(ShapeSpec {
                        fill: [0.13, 0.13, 0.15, 0.9],
                        radius: 8.0,
                        ..Default::default()
                    }),
                    slider: Some(SliderSpec::default()),
                    ..Default::default()
                }),
            ),
            AddUi::Scroll => (
                "Scroll View",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(280.0), Size::Fixed(200.0)],
                    shape: Some(ShapeSpec {
                        fill: [0.1, 0.1, 0.12, 0.85],
                        radius: 8.0,
                        ..Default::default()
                    }),
                    scroll: Some(floptle_ui::ScrollSpec::default()),
                    ..Default::default()
                }),
            ),
        };
        self.add_node(name, MatterDoc::Empty);
        // add_node selects what it created — attach the components there.
        let Some(&e) = self.selection.first() else { return };
        if let Some(l) = layer {
            self.world.insert(e, l);
        }
        if let Some(s) = spec {
            self.world.insert(e, s);
        }
        if what == AddUi::Slider {
            // Plain-shape parts, no imposed look — swap in your own textures.
            let parts: [(&str, ElementSpec); 2] = [
                (
                    "Fill",
                    ElementSpec {
                        part: Some(SliderPart::Fill),
                        place: Place::Free { pos: [0.0, 0.0] },
                        size: [Size::Pct(1.0), Size::Pct(1.0)],
                        shape: Some(ShapeSpec {
                            fill: [0.85, 0.87, 0.9, 1.0],
                            radius: 8.0,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ),
                (
                    "Handle",
                    ElementSpec {
                        part: Some(SliderPart::Handle),
                        place: Place::Pin { anchor: Anchor::Left, offset: [0.0, 0.0] },
                        size: [Size::Fixed(16.0), Size::Fixed(36.0)],
                        shape: Some(ShapeSpec {
                            fill: [1.0, 1.0, 1.0, 1.0],
                            radius: 6.0,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ),
            ];
            for (pname, pspec) in parts {
                let c = self.world.spawn();
                self.world.insert(c, floptle_core::Transform::IDENTITY);
                self.world.insert(c, floptle_core::Name(pname.into()));
                self.world.insert(c, MatterDoc::Empty.to_matter());
                self.world.insert(c, Parent(e));
                self.world.insert(c, pspec);
            }
        }
    }

    /// The Inspector's UI section: shown for nodes carrying UiLayer and/or
    /// ElementSpec. Returns true when something changed (undo coalescing).
    pub(crate) fn ui_inspector(
        world: &mut floptle_core::World,
        e: Entity,
        ui: &mut egui::Ui,
        asset_tree: &[crate::assets::AssetEntry],
        texture_settings: &std::collections::HashMap<String, crate::assets::TexSetting>,
    ) -> bool {
        let mut changed = false;
        if let Some(mut layer) = world.get::<UiLayer>(e).copied() {
            use floptle_ui::UiSpace;
            ui.separator();
            ui.label("🖼 UI Layer");
            // ---- screen vs world space ----------------------------------
            ui.horizontal(|ui| {
                ui.label("space");
                egui::ComboBox::from_id_salt(("ui_space", e))
                    .selected_text(match layer.space {
                        UiSpace::Screen => "Screen",
                        UiSpace::World => "World",
                    })
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut layer.space, UiSpace::Screen, "Screen")
                            .on_hover_text("a flat overlay that fills the window (HUD, menus)")
                            .changed();
                        changed |= ui
                            .selectable_value(&mut layer.space, UiSpace::World, "World")
                            .on_hover_text(
                                "a flat panel inside the 3D world at this node's transform \
                                 (diegetic screens, in-world signage) — move/rotate the node \
                                 to place it, scale it with 'canvas size' below",
                            )
                            .changed();
                    });
            });
            ui.small(match layer.space {
                UiSpace::Screen => "screen-space overlay — in game it fills the window",
                UiSpace::World => "world-space panel — lives in the scene at this node",
            });
            ui.horizontal(|ui| {
                changed |= ui
                    .checkbox(&mut layer.enabled, "enabled")
                    .on_hover_text("master switch: an off layer draws nothing")
                    .changed();
                ui.label("z");
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.z))
                    .on_hover_text("layers draw lowest z first")
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("design height");
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.design_height).range(100.0..=4320.0))
                    .on_hover_text(
                        "resolution independence: this many design units ALWAYS span the                          window height, on any monitor — bigger number = smaller-looking                          UI. Width follows the window's aspect. Element positions/sizes                          are in these units.",
                    )
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("canvas size");
                changed |= ui
                    .add(
                        egui::Slider::new(&mut layer.canvas_scale, 0.001..=0.1)
                            .logarithmic(true),
                    )
                    .on_hover_text(if layer.is_world() {
                        "how big this world panel stands in the scene (world units per \
                         design unit). Move/rotate the node to place it."
                    } else {
                        "size of the Scene-view authoring hologram (world units per design \
                         unit). Screen-space gameplay is unaffected; switch 'space' to World \
                         to make this the real in-game size."
                    })
                    .changed();
            });
            if changed {
                world.insert(e, layer);
            }
        }
        let Some(mut spec) = world.get::<ElementSpec>(e).cloned() else {
            return changed;
        };
        let mut c = false;
        ui.separator();
        ui.label("▭ UI Element");
        // --- placement ---
        let mut pinned = matches!(spec.place, Place::Pin { .. });
        ui.horizontal(|ui| {
            if ui.checkbox(&mut pinned, "pin to edge").on_hover_text("stick to a corner/edge preset instead of a free position").changed() {
                spec.place = if pinned {
                    Place::Pin { anchor: Anchor::TopLeft, offset: [0.0, 0.0] }
                } else {
                    Place::Free { pos: [40.0, 40.0] }
                };
                c = true;
            }
        });
        match &mut spec.place {
            Place::Free { pos } => {
                ui.horizontal(|ui| {
                    ui.label("pos");
                    c |= ui.add(egui::DragValue::new(&mut pos[0]).speed(1.0)).changed();
                    c |= ui.add(egui::DragValue::new(&mut pos[1]).speed(1.0)).changed();
                });
            }
            Place::Pin { anchor, offset } => {
                ui.horizontal(|ui| {
                    ui.label("anchor");
                    egui::ComboBox::from_id_salt(("ui_anchor", e.index()))
                        .selected_text(format!("{anchor:?}"))
                        .show_ui(ui, |ui| {
                            for a in [
                                Anchor::TopLeft, Anchor::Top, Anchor::TopRight,
                                Anchor::Left, Anchor::Center, Anchor::Right,
                                Anchor::BottomLeft, Anchor::Bottom, Anchor::BottomRight,
                            ] {
                                c |= ui.selectable_value(anchor, a, format!("{a:?}")).changed();
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("offset");
                    c |= ui.add(egui::DragValue::new(&mut offset[0]).speed(1.0)).changed();
                    c |= ui.add(egui::DragValue::new(&mut offset[1]).speed(1.0)).changed();
                });
            }
        }
        // --- size (Fixed/Pct simplified to a number + % toggle; Fit/Grow via menu) ---
        for (axis, label) in [(0usize, "width"), (1usize, "height")] {
            ui.horizontal(|ui| {
                ui.label(label);
                let current = spec.size[axis];
                let kind = match current {
                    Size::Fixed(_) => "px",
                    Size::Pct(_) => "%",
                    Size::Fit => "fit",
                    Size::Grow(_) => "grow",
                };
                egui::ComboBox::from_id_salt(("ui_size", e.index(), axis))
                    .selected_text(kind)
                    .width(56.0)
                    .show_ui(ui, |ui| {
                        for (k, v) in [
                            ("px", Size::Fixed(100.0)),
                            ("%", Size::Pct(0.5)),
                            ("fit", Size::Fit),
                            ("grow", Size::Grow(1.0)),
                        ] {
                            if ui.selectable_label(kind == k, k).clicked()
                                && std::mem::discriminant(&spec.size[axis])
                                    != std::mem::discriminant(&v)
                            {
                                spec.size[axis] = v;
                                c = true;
                            }
                        }
                    });
                match &mut spec.size[axis] {
                    Size::Fixed(v) => c |= ui.add(egui::DragValue::new(v).speed(1.0)).changed(),
                    Size::Pct(v) => {
                        c |= ui.add(egui::DragValue::new(v).speed(0.01).range(0.0..=1.0)).changed()
                    }
                    Size::Grow(v) => c |= ui.add(egui::DragValue::new(v).speed(0.1)).changed(),
                    Size::Fit => {}
                }
            });
        }
        ui.horizontal(|ui| {
            c |= ui.checkbox(&mut spec.visible, "visible").changed();
            ui.label("opacity");
            c |= ui.add(egui::Slider::new(&mut spec.opacity, 0.0..=1.0)).changed();
        });
        c |= ui
            .checkbox(&mut spec.button, "button (clickable)")
            .on_hover_text(
                "the pointer can hover/press/click this element — its scripts get                  hoverStart / hoverEnd / pressed / released / clicked hooks. Style                  the states in Lua (no imposed look).",
            )
            .changed();
        // --- stack (opt-in flow) ---
        let mut has_stack = spec.stack.is_some();
        if ui
            .checkbox(&mut has_stack, "stack children")
            .on_hover_text("opt-in auto-layout: children flow in a row/column with gap + padding")
            .changed()
        {
            spec.stack = has_stack.then(StackCfg::default);
            c = true;
        }
        if let Some(s) = &mut spec.stack {
            ui.horizontal(|ui| {
                c |= ui.selectable_value(&mut s.dir, Dir::Row, "row").changed();
                c |= ui.selectable_value(&mut s.dir, Dir::Column, "column").changed();
                ui.label("gap");
                c |= ui.add(egui::DragValue::new(&mut s.gap).speed(0.5)).changed();
                ui.label("pad");
                c |= ui.add(egui::DragValue::new(&mut s.pad).speed(0.5)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("align");
                for (v, l) in [(Align::Start, "start"), (Align::Center, "center"), (Align::End, "end"), (Align::Stretch, "stretch")] {
                    c |= ui.selectable_value(&mut s.align, v, l).changed();
                }
            });
            ui.horizontal(|ui| {
                ui.label("justify");
                for (v, l) in [(Justify::Start, "start"), (Justify::Center, "center"), (Justify::End, "end"), (Justify::SpaceBetween, "between")] {
                    c |= ui.selectable_value(&mut s.justify, v, l).changed();
                }
            });
        }
        // --- shape ---
        let mut has = spec.shape.is_some();
        if ui.checkbox(&mut has, "shape").changed() {
            spec.shape = has.then(ShapeSpec::default);
            c = true;
        }
        if let Some(s) = &mut spec.shape {
            ui.horizontal(|ui| {
                ui.label("fill");
                c |= ui.color_edit_button_rgba_unmultiplied(&mut s.fill).changed();
                ui.label("radius");
                c |= ui.add(egui::DragValue::new(&mut s.radius).speed(0.5).range(0.0..=512.0)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("border");
                c |= ui.add(egui::DragValue::new(&mut s.border).speed(0.5).range(0.0..=64.0)).changed();
                c |= ui.color_edit_button_rgba_unmultiplied(&mut s.border_color).changed();
            });
        }
        // --- text ---
        let mut has = spec.text.is_some();
        if ui.checkbox(&mut has, "text").changed() {
            spec.text = has.then(TextSpec::default);
            c = true;
        }
        if let Some(t) = &mut spec.text {
            c |= ui.text_edit_singleline(&mut t.text).changed();
            ui.horizontal(|ui| {
                ui.label("size");
                ui.add_enabled_ui(!t.fit, |ui| {
                    c |= ui
                        .add(egui::DragValue::new(&mut t.size).speed(0.5).range(4.0..=256.0))
                        .changed();
                });
                c |= ui
                    .checkbox(&mut t.fit, "fit")
                    .on_hover_text(
                        "dynamic sizing: the text scales to fill the element's rect                          (largest size that fits) — size is ignored",
                    )
                    .changed();
                c |= ui.color_edit_button_rgba_unmultiplied(&mut t.color).changed();
            });
            ui.horizontal(|ui| {
                for (v, l) in [(Align::Start, "left"), (Align::Center, "center"), (Align::End, "right")] {
                    c |= ui.selectable_value(&mut t.align, v, l).changed();
                }
                ui.separator();
                for (v, l) in [(Align::Start, "top"), (Align::Center, "middle"), (Align::End, "bottom")] {
                    c |= ui.selectable_value(&mut t.valign, v, l).changed();
                }
            });
            ui.horizontal(|ui| {
                ui.label("font");
                let current = if t.font.is_empty() {
                    "(default)".to_string()
                } else {
                    t.font.rsplit('/').next().unwrap_or(&t.font).to_string()
                };
                if let Some(pick) = crate::ui_widgets::asset_picker(
                    ui,
                    egui::Id::new(("ui_font_pick", e.index())),
                    &current,
                    Some("(default)"),
                    asset_tree,
                    crate::assets::is_font,
                    170.0,
                ) {
                    t.font = pick.unwrap_or_default();
                    c = true;
                }
            })
            .response
            .on_hover_text("any .ttf/.otf in your assets — drop font files into the project and they appear here");
        }
        // --- image ---
        let mut has = spec.image.is_some();
        if ui.checkbox(&mut has, "image").on_hover_text("any texture from your assets — the engine ships no UI art").changed() {
            spec.image = has.then(ImageSpec::default);
            c = true;
        }
        if let Some(img) = &mut spec.image {
            ui.horizontal(|ui| {
                ui.label("texture");
                let current = if img.texture.is_empty() {
                    "(none)".to_string()
                } else {
                    img.texture.rsplit('/').next().unwrap_or(&img.texture).to_string()
                };
                if let Some(pick) = crate::ui_widgets::asset_picker(
                    ui,
                    egui::Id::new(("ui_tex_pick", e.index())),
                    &current,
                    Some("(none)"),
                    asset_tree,
                    crate::assets::is_texture,
                    170.0,
                ) {
                    let new = pick.unwrap_or_default();
                    // Inherit the texture's spritesheet grid (set in its asset
                    // settings) so a picked sheet slices without extra steps.
                    let (sc, sr) = texture_settings.get(&new).map(|t| t.sheet()).unwrap_or((1, 1));
                    img.cols = sc;
                    img.rows = sr;
                    img.cell = 0;
                    img.texture = new;
                    c = true;
                }
            });
            // --- spritesheet cell picker (when the texture is a sheet) ---
            let (sc, sr) =
                texture_settings.get(&img.texture).map(|t| t.sheet()).unwrap_or((1, 1));
            // Keep the image's grid in sync if the asset's split changed.
            if (img.cols, img.rows) != (sc, sr) {
                img.cols = sc;
                img.rows = sr;
                img.cell = img.cell.min((sc * sr).saturating_sub(1));
                c = true;
            }
            if sc * sr > 1 {
                ui.label(format!("sprite cell ({}×{} sheet)", sc, sr));
                if let Some(sheet) = crate::ui_widgets::asset_thumb(ui, &img.texture, 256) {
                    // A clickable grid of the sheet's cells; the current one is ringed.
                    let cell_px = (240.0 / sc as f32).clamp(16.0, 48.0);
                    egui::ScrollArea::vertical().max_height(180.0).id_salt(("cells", e.index())).show(ui, |ui| {
                        for r in 0..sr {
                            ui.horizontal(|ui| {
                                for cc in 0..sc {
                                    let idx = r * sc + cc;
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(cell_px, cell_px),
                                        egui::Sense::click(),
                                    );
                                    let uv = egui::Rect::from_min_max(
                                        egui::pos2(cc as f32 / sc as f32, r as f32 / sr as f32),
                                        egui::pos2((cc + 1) as f32 / sc as f32, (r + 1) as f32 / sr as f32),
                                    );
                                    egui::Image::new(&sheet).uv(uv).paint_at(ui, rect);
                                    let ring = if img.cell == idx {
                                        egui::Color32::from_rgb(255, 200, 60)
                                    } else if resp.hovered() {
                                        egui::Color32::from_gray(200)
                                    } else {
                                        egui::Color32::from_gray(90)
                                    };
                                    ui.painter().rect_stroke(
                                        rect,
                                        1.0,
                                        egui::Stroke::new(if img.cell == idx { 2.0 } else { 1.0 }, ring),
                                        egui::StrokeKind::Inside,
                                    );
                                    if resp.on_hover_text(format!("cell {idx}")).clicked() {
                                        img.cell = idx;
                                        c = true;
                                    }
                                }
                            });
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.label("cell");
                    let mut cell = img.cell;
                    if ui.add(egui::DragValue::new(&mut cell).range(0..=(sc * sr - 1))).changed() {
                        img.cell = cell;
                        c = true;
                    }
                    ui.small("animate this (stepped property track) for sprite animation");
                });
            }
            ui.horizontal(|ui| {
                ui.label("tint");
                c |= ui.color_edit_button_rgba_unmultiplied(&mut img.tint).changed();
            });
        }
        // --- slider (value-driven bar: this element is the track) ---
        let mut has = spec.slider.is_some();
        if ui
            .checkbox(&mut has, "slider")
            .on_hover_text(
                "value-driven bar (health, progress…): child elements marked as                  Fill scale with the value, Handle children ride its position —                  the parts stay ordinary elements you retexture and arrange freely",
            )
            .changed()
        {
            spec.slider = has.then(SliderSpec::default);
            c = true;
        }
        if let Some(s) = &mut spec.slider {
            ui.horizontal(|ui| {
                ui.label("value");
                let lo = s.min.min(s.max);
                let hi = s.max.max(s.min);
                c |= ui.add(egui::Slider::new(&mut s.value, lo..=hi)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("min");
                c |= ui.add(egui::DragValue::new(&mut s.min).speed(1.0)).changed();
                ui.label("max");
                c |= ui.add(egui::DragValue::new(&mut s.max).speed(1.0)).changed();
                c |= ui.selectable_value(&mut s.dir, Dir::Row, "↔").on_hover_text("horizontal").changed();
                c |= ui.selectable_value(&mut s.dir, Dir::Column, "↕").on_hover_text("vertical").changed();
                c |= ui
                    .checkbox(&mut s.flip, "flip")
                    .on_hover_text("the handle rides from the far end back toward the start")
                    .changed();
                c |= ui
                    .checkbox(&mut s.interact, "draggable")
                    .on_hover_text(
                        "the player can click/drag the track to set the value (settings                          sliders); off = display-only (health bars)",
                    )
                    .changed();
            });
        }
        // --- slider part (role under a slider parent) ---
        if world
            .get::<Parent>(e)
            .and_then(|p| world.get::<ElementSpec>(p.0))
            .is_some_and(|ps| ps.slider.is_some())
        {
            ui.horizontal(|ui| {
                ui.label("slider part");
                let cur = match spec.part {
                    None => "none",
                    Some(SliderPart::Fill) => "fill",
                    Some(SliderPart::Handle) => "handle",
                };
                egui::ComboBox::from_id_salt(("ui_part", e.index()))
                    .selected_text(cur)
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for (label, v) in [
                            ("none", None),
                            ("fill", Some(SliderPart::Fill)),
                            ("handle", Some(SliderPart::Handle)),
                        ] {
                            if ui.selectable_label(cur == label, label).clicked() && spec.part != v
                            {
                                spec.part = v;
                                c = true;
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "fill scales with the parent slider's value; handle rides its                          position — its authored size is the full-value size",
                    );
            });
        }
        // --- scroll view (children shift by the wheel + clip to this rect) ---
        let mut has = spec.scroll.is_some();
        if ui
            .checkbox(&mut has, "scroll view")
            .on_hover_text(
                "children keep their authored layout but scroll vertically with the                  wheel (clipped to this element's rect) — put more content inside                  than fits and it just works; scripts read/write UiElement.scrollY",
            )
            .changed()
        {
            spec.scroll = has.then(floptle_ui::ScrollSpec::default);
            c = true;
        }
        if let Some(sc) = &mut spec.scroll {
            ui.horizontal(|ui| {
                ui.label("wheel speed");
                c |= ui
                    .add(egui::DragValue::new(&mut sc.speed).range(4.0..=400.0))
                    .on_hover_text("design units per wheel notch")
                    .changed();
            });
        }
        // --- mask (clip other elements to this element's rounded rect) ---
        let mut has = spec.mask.is_some();
        if ui
            .checkbox(&mut has, "mask")
            .on_hover_text(
                "clip the chosen elements (and everything inside them) to this                  element's rounded rect — pick targets by node name below",
            )
            .changed()
        {
            spec.mask = has.then(MaskSpec::default);
            c = true;
        }
        if let Some(mask) = &mut spec.mask {
            // Candidates: every UI element node's name (this element excluded —
            // masking yourself is targeting your own name, allowed via Other).
            let mut names: Vec<String> = world
                .query::<ElementSpec>()
                .filter_map(|(oe, _)| world.get::<floptle_core::Name>(oe).map(|n| n.0.clone()))
                .collect();
            names.sort();
            names.dedup();
            let mut remove: Option<usize> = None;
            for (i, target) in mask.targets.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    if ui.button("✖").on_hover_text("remove this target").clicked() {
                        remove = Some(i);
                    }
                    if let Some(pick) = crate::ui_widgets::searchable_picker(
                        ui,
                        egui::Id::new(("ui_mask_target", e.index(), i)),
                        if target.is_empty() { "(pick element)" } else { target },
                        None,
                        &names,
                        170.0,
                    ) {
                        *target = pick.unwrap_or_default();
                        c = true;
                    }
                    // Conflict: the FIRST mask in scene order claiming a name
                    // wins — warn when that isn't this one.
                    if !target.is_empty() {
                        let winner = world
                            .query::<ElementSpec>()
                            .find(|(_, os)| {
                                os.mask.as_ref().is_some_and(|m| m.targets.contains(target))
                            })
                            .map(|(oe, _)| oe);
                        if let Some(w) = winner
                            && w != e
                        {
                            let wname = world
                                .get::<floptle_core::Name>(w)
                                .map(|n| n.0.clone())
                                .unwrap_or_default();
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "⚠",
                            )
                            .on_hover_text(format!(
                                "'{wname}' (earlier in the scene) also masks this element                                  — the earliest mask wins"
                            ));
                        }
                    }
                });
            }
            if let Some(i) = remove {
                mask.targets.remove(i);
                c = true;
            }
            if ui.button("✚ add target").clicked() {
                mask.targets.push(String::new());
                c = true;
            }
        }
        if c {
            world.insert(e, spec);
        }
        changed || c
    }
}
