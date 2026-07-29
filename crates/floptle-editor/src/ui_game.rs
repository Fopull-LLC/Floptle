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
    Field,
    Tooltip,
}

/// Resolve each scrollbar's `target` NAME to the scroll view it drives, within
/// this layer. Same name-scoping rule as masks: first match in scene order.
fn layer_scrollbars(
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
        if let Some(sb) = &spec.scrollbar
            && let Some(&target) = by_name.get(sb.target.as_str())
        {
            out.push((*id, target));
        }
    }
    out
}

/// Slide an element by `d` design units, whatever its placement mode.
///
/// One function so every mover agrees: the Scene overlay drag, the ◫ UI tab's
/// canvas drag and nudge keys, and align/distribute. `Free` moves its position,
/// `Pin` its offset, `Stretch` its leading margins — in every case the element
/// ends up exactly `d` further along, and the placement mode the designer chose
/// survives the gesture.
pub(crate) fn nudge_place(place: &mut floptle_ui::Place, d: [f32; 2]) {
    match place {
        floptle_ui::Place::Free { pos } => {
            pos[0] += d[0];
            pos[1] += d[1];
        }
        floptle_ui::Place::Pin { offset, .. } => {
            offset[0] += d[0];
            offset[1] += d[1];
        }
        // Slide the whole anchored box by nudging the leading margins.
        floptle_ui::Place::Stretch { margin, .. } => {
            margin[0] += d[0];
            margin[1] += d[1];
        }
    }
}

/// Put an element AT a position in design units, whatever its placement mode.
///
/// The absolute twin of [`nudge_place`], for the one caller that knows where a
/// thing should be rather than how far to move it: the tooltip follower.
pub(crate) fn set_place(place: &mut floptle_ui::Place, at: [f32; 2]) {
    match place {
        floptle_ui::Place::Free { pos } => *pos = at,
        floptle_ui::Place::Pin { offset, .. } => *offset = at,
        floptle_ui::Place::Stretch { margin, .. } => {
            margin[0] = at[0];
            margin[1] = at[1];
        }
    }
}

/// A radius/border row: ONE drag value while all four entries agree, four when
/// they don't (or when you click ⋯ to split them).
///
/// The point is that the common case stays a single number — per-corner radii
/// exist for headers and tabs, not to make every panel a four-field chore. The
/// row auto-collapses again as soon as the four values match, so nothing is
/// left in a fiddly state by accident.
fn quad_row(
    ui: &mut egui::Ui,
    label: &str,
    v: &mut [f32; 4],
    parts: [&str; 4],
    max: f32,
    salt: &str,
) -> bool {
    let uniform = v[1] == v[0] && v[2] == v[0] && v[3] == v[0];
    let id = egui::Id::new((salt, label));
    // Sticky only while it is needed: a split row that becomes uniform again
    // stays split until you leave it, so typing four equal numbers doesn't
    // yank the fields out from under the cursor mid-edit.
    let mut split = ui.data(|d| d.get_temp::<bool>(id).unwrap_or(false)) || !uniform;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        if split {
            for (i, part) in parts.iter().enumerate() {
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut v[i])
                            .speed(0.5)
                            .range(0.0..=max)
                            .prefix(format!("{part} ")),
                    )
                    .changed();
            }
            if ui.small_button("=").on_hover_text("link all four again").clicked() {
                *v = [v[0]; 4];
                split = false;
                changed = true;
            }
        } else {
            let mut all = v[0];
            if ui
                .add(egui::DragValue::new(&mut all).speed(0.5).range(0.0..=max))
                .changed()
            {
                *v = [all; 4];
                changed = true;
            }
            if ui
                .small_button("⋯")
                .on_hover_text(format!("set {} separately", parts.join("/")))
                .clicked()
            {
                split = true;
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(id, split));
    changed
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
        if path.is_empty() {
            return;
        }
        let file = self.resolve_asset_path(path);
        let Some(uir) = self.ui_render.as_mut() else { return };
        if uir.has_font(path) {
            return;
        }
        let bytes = std::fs::read(&file).unwrap_or_default();
        uir.ensure_font(path, &bytes);
    }

    /// Pre-register every font any UI text references (before the immutable
    /// renderer borrow the measure callback needs).
    pub(crate) fn ensure_ui_fonts(&mut self) {
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

    /// Re-read the project's UI tokens and style sheets.
    ///
    /// Every `*.tokens.ron` and `*.uistyle.ron` anywhere under the project
    /// merges into one sheet — the same "many files, one namespace" rule
    /// materials and prefabs already follow. Load order is sorted by path so
    /// the winner of a name clash is stable between runs; clashes are recorded
    /// rather than silently resolved.
    ///
    /// A project with no style files gets empty ones and nothing changes. The
    /// engine ships neither.
    pub(crate) fn reload_ui_styles(&mut self) {
        let mut tokens = floptle_ui::Tokens::default();
        let mut sheet = floptle_ui::StyleSheet::default();
        let mut clashes = Vec::new();
        let files = Self::scan_ui_style_files(&self.project_root);
        for (path, _) in &files {
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.ends_with(".tokens.ron") {
                match floptle_ui::Tokens::parse(&text) {
                    Ok(t) => tokens.merge(t),
                    Err(e) => self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("ui tokens {name}: {e}"),
                        None,
                    ),
                }
            } else {
                match floptle_ui::StyleSheet::parse(&text) {
                    Ok(s) => clashes.extend(sheet.merge(s)),
                    Err(e) => self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("ui styles {name}: {e}"),
                        None,
                    ),
                }
            }
        }
        for name in &clashes {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("ui style \"{name}\" is defined in more than one sheet — the last one wins"),
                None,
            );
        }
        self.ui_tokens = tokens;
        self.ui_styles = sheet;
        self.ui_style_clashes = clashes;
        self.ui_style_files = files;
        // Transitions are keyed to the old resolved values; a token edit that
        // changes what "accent" means must not leave elements easing from a
        // colour that no longer exists.
        self.ui_style_rt.clear();
    }

    /// Every `*.tokens.ron` / `*.uistyle.ron` under the project, sorted, with
    /// its mtime — the load list AND the hot-reload signature.
    fn scan_ui_style_files(
        root: &std::path::Path,
    ) -> Vec<(std::path::PathBuf, Option<std::time::SystemTime>)> {
        fn walk(
            dir: &std::path::Path,
            out: &mut Vec<(std::path::PathBuf, Option<std::time::SystemTime>)>,
            depth: u32,
        ) {
            if depth > 8 {
                return;
            }
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for entry in rd.flatten() {
                let p = entry.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                if p.is_dir() {
                    // Skip the engine's own runtime dirs and anything hidden —
                    // `target/` alone would make this walk cost real time.
                    if name.starts_with('.') || name == "target" || name == "builds" {
                        continue;
                    }
                    walk(&p, out, depth + 1);
                } else if name.ends_with(".tokens.ron") || name.ends_with(".uistyle.ron") {
                    let m = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                    out.push((p, m));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out, 0);
        // Sorted so a name clash always resolves the same way between runs.
        out.sort();
        out
    }

    /// Hot reload: re-scan on a timer and reload when anything changed.
    ///
    /// The house mtime pattern (textures, prefabs, `.flsl`). Rate-limited
    /// because this walks directories rather than watching them — and the
    /// payoff is the loop the whole style system exists for: edit a token,
    /// watch every screen in the project repaint.
    pub(crate) fn poll_ui_styles(&mut self, now: f32) {
        // `Editor` derives Default, so both of these start at 0.0 — the first
        // frame must therefore be treated as "never scanned" rather than
        // "scanned just now", or the project's styles wouldn't load until half
        // a second in and the first frame would flash unstyled.
        if self.ui_style_poll > 0.0 && now - self.ui_style_poll < 0.5 {
            return;
        }
        self.ui_style_poll = now.max(f32::EPSILON);
        // Signature covers adds, deletes, renames and edits in one comparison.
        if Self::scan_ui_style_files(&self.project_root) != self.ui_style_files {
            self.reload_ui_styles();
        }
    }

    /// Resolve styles + advance transitions over a freshly-built layer tree.
    ///
    /// Runs on the Node COPIES, never the ECS — which is precisely why a
    /// play-time hover can't end up in a saved scene, and why this needs no
    /// cooperation from the play-snapshot machinery.
    ///
    /// EVERY pass that builds a tree must call this before laying it out. A
    /// style can set `pad`, `gap` and `text_size`, so an unstyled solve puts
    /// the rects somewhere other than where they are drawn — which as a hit
    /// test reads exactly like the mouse being offset from the cursor. The
    /// frame's `dt` is safe to hand to all of them (see `Editor::ui_style_dt`).
    fn style_layer(&mut self, roots: &mut [floptle_ui::Node]) {
        if self.ui_styles.styles.is_empty() {
            return;
        }
        let input = floptle_ui::StateInput {
            hovered: self.ui_hover,
            pressed: self.ui_active,
            focused: self.ui_focus,
        };
        let (sheet, tokens) = (&self.ui_styles, &self.ui_tokens);
        let dt = self.ui_style_dt;
        floptle_ui::apply_styles(roots, sheet, tokens, &input, &mut self.ui_style_rt, dt);
    }

    /// Every enabled UI layer `want` accepts, as a STYLED node tree, z-sorted
    /// (stable, so scene order breaks ties). Also returns the index→entity map
    /// the scrollbar and mask lookups need.
    ///
    /// The single place a layer tree gets built. It exists because there are
    /// three consumers — the hit test, the screen overlay and the world
    /// canvases — and while each rolled its own, one of them forgot to style:
    /// `pad`, `gap` and `text_size` all move rects, so the hit test was reading
    /// geometry that had never been on screen, which feels exactly like the
    /// cursor being offset from the mouse.
    #[allow(clippy::type_complexity)]
    fn ui_layer_trees(
        &mut self,
        want: impl Fn(&UiLayer) -> bool,
    ) -> (Vec<(Entity, UiLayer, Vec<floptle_ui::Node>)>, HashMap<u32, Entity>) {
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
            Some(floptle_ui::Node::with_children(e.index(), spec, children))
        }
        let mut out: Vec<(Entity, UiLayer, Vec<floptle_ui::Node>)> = Vec::new();
        for e in &order {
            let Some(layer) = self.world.get::<UiLayer>(*e).copied() else { continue };
            if !layer.enabled || !want(&layer) {
                continue;
            }
            let mut roots: Vec<_> = kids
                .get(&e.index())
                .map(|cs| cs.iter().filter_map(|c| build(&self.world, &kids, *c)).collect())
                .unwrap_or_default();
            floptle_ui::sort_roots(&mut roots);
            if roots.is_empty() {
                continue;
            }
            // BEFORE layout, always: a style can set padding, gap and text
            // size, all of which change what the solver measures.
            self.style_layer(&mut roots);
            out.push((*e, layer, roots));
        }
        out.sort_by_key(|(_, l, _)| l.z);
        (out, ents)
    }

    /// Solve every UI layer for this frame: (draw list, px-per-design-unit),
    /// z-sorted. Pre-resolves image textures into the registry (needs
    /// `&mut self`, so this runs BEFORE the draw core's field borrows).
    pub(crate) fn gather_game_ui(&mut self, viewport: [f32; 2]) -> Vec<(floptle_ui::DrawList, f32)> {
        if viewport[0] <= 1.0 || viewport[1] <= 1.0 {
            return Vec::new();
        }
        // World-space layers render in the scene, not as an overlay.
        let (layers, ents) = self.ui_layer_trees(|l| !l.is_world());
        if layers.is_empty() {
            return Vec::new();
        }
        let Some(uir) = self.ui_render.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        let mut textures: Vec<String> = Vec::new();
        for (_, layer, roots) in &layers {
            let scale = layer.scale_for(viewport);
            let design_vp = [viewport[0] / scale, viewport[1] / scale];
            let measure = |t: &TextSpec| uir.measure_spec(t);
            let mut placed = floptle_ui::solve(roots, design_vp, &measure);
            floptle_ui::place_scrollbars(roots, &mut placed, &layer_scrollbars(&self.world, &ents, roots));
            let masks = layer_masks(&self.world, &ents, roots);
            let dl = floptle_ui::draw_list_with(roots, &placed, &masks, self.ui_edit);
            for q in &dl.quads {
                if !q.texture.is_empty() {
                    textures.push(q.texture.clone());
                }
            }
            out.push((dl, scale));
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
        let (built, ents) = self.ui_layer_trees(|l| include_screen || l.is_world());
        let Some(uir) = self.ui_render.as_ref() else { return Vec::new() };
        let mut out = Vec::new();
        let mut textures: Vec<String> = Vec::new();
        for (e, layer, roots) in &built {
            let design_vp =
                [layer.design_height * window_aspect.max(0.1), layer.design_height];
            let measure = |t: &TextSpec| uir.measure_spec(t);
            let mut placed = floptle_ui::solve(roots, design_vp, &measure);
            floptle_ui::place_scrollbars(roots, &mut placed, &layer_scrollbars(&self.world, &ents, roots));
            let masks = layer_masks(&self.world, &ents, roots);
            let dl = floptle_ui::draw_list_with(roots, &placed, &masks, self.ui_edit);
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

    /// Build ONE layer's element tree, in draw order.
    ///
    /// The ◫ UI tab needs a single layer rather than the whole frame's worth,
    /// and needs it without the gather pass's z-sorting and texture
    /// registration — so this is the shared piece, not a fork of the pass.
    pub(crate) fn ui_layer_tree(&self, layer: Entity) -> Vec<floptle_ui::Node> {
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
            Some(floptle_ui::Node::with_children(e.index(), spec, children))
        }
        let mut roots: Vec<_> = kids
            .get(&layer.index())
            .map(|cs| cs.iter().filter_map(|c| build(&self.world, &kids, *c)).collect())
            .unwrap_or_default();
        floptle_ui::sort_roots(&mut roots);
        roots
    }

    /// Mask pairs for a tree the caller already built (see [`layer_masks`]).
    pub(crate) fn ui_layer_masks(&self, roots: &[floptle_ui::Node]) -> Vec<(u32, u32)> {
        let ents: HashMap<u32, Entity> =
            self.world.query::<Transform>().map(|(e, _)| (e.index(), e)).collect();
        layer_masks(&self.world, &ents, roots)
    }

    /// Scrollbar → scroll-view pairs for a tree the caller already built.
    pub(crate) fn ui_layer_scrollbars(&self, roots: &[floptle_ui::Node]) -> Vec<(u32, u32)> {
        let ents: HashMap<u32, Entity> =
            self.world.query::<Transform>().map(|(e, _)| (e.index(), e)).collect();
        layer_scrollbars(&self.world, &ents, roots)
    }

    /// The game view's size in physical pixels — the whole window when the game
    /// is what's on screen, else the docked Game tab's rect.
    ///
    /// Split out from [`Self::ui_pointer`] because the interact pass has to run
    /// with no pointer at all: a gamepad-only session (or an FPS camera with
    /// the cursor locked away) still needs its menu solved so navigation has
    /// rects to move between.
    fn ui_viewport(&self) -> Option<[f32; 2]> {
        let gpu = self.gpu.as_ref()?;
        if self.game_view() || self.player_mode {
            return Some([gpu.config.width as f32, gpu.config.height.max(1) as f32]);
        }
        let r = self.game_rect?;
        let ppp = self.egui.as_ref().map(|e| e.ctx.pixels_per_point()).unwrap_or(1.0);
        Some([r.width() * ppp, r.height() * ppp])
    }

    /// Pointer position + viewport (physical px, game-view space) for game-UI
    /// interaction. `None` when the cursor is hidden/locked (FPS look, game
    /// trap) or outside the game viewport.
    fn ui_pointer(&self) -> Option<([f32; 2], [f32; 2])> {
        if self.script_mouse_lock || self.game_trap {
            return None;
        }
        let cursor = self.cursor?;
        let size = self.ui_viewport()?;
        if self.game_view() || self.player_mode {
            // The UI draws over the whole window here.
            return Some(([cursor.x, cursor.y], size));
        }
        // Docked Game tab: viewport-local coordinates.
        let r = self.game_rect?;
        let ppp = self.egui.as_ref().map(|e| e.ctx.pixels_per_point()).unwrap_or(1.0);
        let p = [cursor.x - r.min.x * ppp, cursor.y - r.min.y * ppp];
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
            // Focus belongs to a running game. Clearing it on Stop means Play
            // never starts with a ring left over from the last session, and
            // means the editor's own arrow keys are never fighting a menu.
            self.ui_focus = None;
            self.ui_nav_repeat.clear();
            self.ui_submit_was = false;
            self.ui_cancel_was = false;
            self.ui_edit = None;
            self.ui_drag = None;
            self.ui_drag_report = None;
            self.ui_tip_hover = None;
            self.ui_text_ops.clear();
            self.input_typed.clear();
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
        // (element, [new offset_x, new offset]).
        let mut wheel_target: Option<(u32, [f32; 2])> = None;
        // Scroll views the pointer is inside, innermost last, with their travel
        // — what drag-to-scroll and scrollbar drags need.
        let mut scroll_hits: Vec<(u32, [f32; 2])> = Vec::new();
        // The pointer in the innermost hit scroll view's design units.
        let mut scroll_ptr: Option<[f32; 2]> = None;
        // (bar, target view, axis, track rect, pointer in design units).
        type BarHit = (u32, u32, usize, [f32; 4], [f32; 2]);
        let mut bar_hits: Vec<BarHit> = Vec::new();
        // Every `drop_target` the pointer is inside, in draw order. Kept apart
        // from `hover` because a drop target is usually the slot BEHIND the
        // item you are carrying it onto — taking only the topmost hit would
        // mean an inventory that never accepts anything.
        let mut drop_hits: Vec<u32> = Vec::new();
        // Per layer: (layer entity, design viewport, solved rects, tooltip
        // delay, pointer in design units) — what the tooltip pass needs.
        type TipLayer = (Entity, [f32; 2], Vec<floptle_ui::Placed>, f32, Option<[f32; 2]>);
        let mut tip_layers: Vec<TipLayer> = Vec::new();
        // The pointer in the last-solved layer's design units — the fallback
        // for gestures that wander off every element mid-drag.
        let mut last_ptr_design: Option<[f32; 2]> = None;
        // Solved screen-space rects in physical px, published to scripts after
        // this pass (`node:uiRect()`): a script can hit-test the mouse against
        // a panel's real position instead of guessing.
        let mut solved_rects: HashMap<u32, [f32; 4]> = HashMap::new();
        // Every layer's solved rects this frame, for the navigation pass.
        let mut nav_layers: Vec<(UiLayer, Vec<floptle_ui::Node>, Vec<floptle_ui::Placed>)> =
            Vec::new();
        let ptr_px = pointer.map(|(p, _)| p);
        if let Some(viewport) = self.ui_viewport()
            && viewport[0] > 1.0
            && viewport[1] > 1.0
        {
            // Every enabled layer, screen-space and world alike — the same
            // STYLED trees the draw passes lay out, which is the only reason a
            // click lands where the element looks like it is.
            let (layers, ents) = self.ui_layer_trees(|_| true);
            // Camera-relative pointer ray (for world-space panels). ADR-0015:
            // the world is offset to the camera, so the ray origin is ~0.
            let cam = play_camera(&self.world, self.camera.render_camera());
            let aspect = viewport[0] / viewport[1];
            let ray = ptr_px.map(|ptr| {
                let inv = cam.view_proj(aspect).inverse();
                let ndc = [ptr[0] / viewport[0] * 2.0 - 1.0, 1.0 - ptr[1] / viewport[1] * 2.0];
                let near = inv * Vec4::new(ndc[0], ndc[1], 0.0, 1.0);
                let far = inv * Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
                let ro = near.truncate() / near.w;
                ((far.truncate() / far.w - ro).normalize(), ro)
            });

            if let Some(uir) = self.ui_render.as_ref() {
                for (e, layer, roots) in &layers {
                    // Design viewport + the pointer's position within it +, for
                    // screen-space layers, the design→physical-pixel scale so
                    // solved rects can be published to scripts (`node:uiRect()`).
                    let (design_vp, ptr_design, screen_scale) = if layer.is_world() {
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
                        let pd = ray.and_then(|(rd, ro)| {
                            let denom = rd.dot(n);
                            if denom.abs() <= 1e-6 {
                                return None; // ray parallel to the panel
                            }
                            let t = (origin - ro).dot(n) / denom;
                            if t <= 0.0 {
                                return None; // panel is behind the camera
                            }
                            let hit = ro + rd * t;
                            let rel = hit - origin;
                            Some([
                                rel.dot(right) / right.length_squared(),
                                rel.dot(down) / down.length_squared(),
                            ])
                        });
                        (dvp, pd, None)
                    } else {
                        let scale = layer.scale_for(viewport);
                        (
                            [viewport[0] / scale, viewport[1] / scale],
                            ptr_px.map(|p| [p[0] / scale, p[1] / scale]),
                            Some(scale),
                        )
                    };
                    let measure = |t: &TextSpec| uir.measure_spec(t);
                    let mut placed = floptle_ui::solve(roots, design_vp, &measure);
                    let bars = layer_scrollbars(&self.world, &ents, roots);
                    floptle_ui::place_scrollbars(roots, &mut placed, &bars);
                    let bar_targets: HashMap<u32, u32> = bars.into_iter().collect();
                    nav_layers.push((*layer, roots.clone(), placed.clone()));
                    // Publish each screen-space element's SOLVED rect in physical
                    // pixels (design rect × scale) — `node:uiRect()` reads it.
                    if let Some(scale) = screen_scale {
                        for pl in &placed {
                            solved_rects.insert(
                                pl.id,
                                [
                                    pl.rect[0] * scale,
                                    pl.rect[1] * scale,
                                    pl.rect[2] * scale,
                                    pl.rect[3] * scale,
                                ],
                            );
                        }
                    }
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
                        // Everything the pointer can do something with. A
                        // tooltip counts: hovering IS the interaction.
                        if spec.button
                            || slider.is_some()
                            || spec.field.is_some()
                            || spec.draggable
                            || !spec.tooltip.is_empty()
                        {
                            items.push((pl.id, pl.rect, ptr_design, slider, clip));
                        }
                        if spec.drop_target
                            && ptr_design.is_some_and(|p| {
                                in_rect(&pl.rect, &p) && clip.is_none_or(|c| in_rect(&c, &p))
                            })
                        {
                            drop_hits.push(pl.id);
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
                            // The wheel drives Y when there's Y to drive, else
                            // X — so a horizontal strip of cards scrolls with
                            // an ordinary wheel and nobody has to know why.
                            // Shift forces X, the universal convention.
                            let shift = self.shift;
                            let sideways = shift || (max[1] <= 0.0 && max[0] > 0.0);
                            let d = wheel * sc.speed;
                            let next = if sideways {
                                [(sc.offset_x - d).clamp(0.0, max[0]), sc.offset]
                            } else {
                                [sc.offset_x, (sc.offset - d).clamp(0.0, max[1])]
                            };
                            wheel_target = Some((pl.id, next));
                        }
                        // Drag-to-scroll + scrollbar hit records, both of which
                        // need the travel and the pointer in THIS layer's units.
                        if spec.scroll.is_some()
                            && ptr_design.is_some_and(|p| {
                                in_rect(&pl.rect, &p) && clip.is_none_or(|c| in_rect(&c, &p))
                            })
                        {
                            scroll_hits
                                .push((pl.id, floptle_ui::scroll_max(roots, &placed, pl.id)));
                            scroll_ptr = ptr_design;
                        }
                        if let Some(sb) = spec.scrollbar.as_ref()
                            && let Some(p) = ptr_design
                            && in_rect(&pl.rect, &p)
                            && let Some(&target) = bar_targets.get(&pl.id)
                        {
                            let axis = match sb.axis {
                                Dir::Row => 0,
                                Dir::Column => 1,
                            };
                            bar_hits.push((pl.id, target, axis, pl.rect, p));
                        }
                    }
                    last_ptr_design = ptr_design.or(last_ptr_design);
                    tip_layers.push((*e, design_vp, placed, layer.tooltip_delay, ptr_design));
                }
            }
        }
        if let Some((id, next)) = wheel_target {
            let ent = self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == id);
            if let Some(e) = ent
                && let Some(spec) = self.world.get_mut::<ElementSpec>(e)
                && let Some(sc) = &mut spec.scroll
            {
                sc.offset_x = next[0];
                sc.offset = next[1];
            }
            self.input_scroll = 0.0;
            self.tick_scroll = 0.0;
        }
        // Keyboard / gamepad navigation, BEFORE the pointer pass so a pad press
        // and a mouse click land in the same queue in the same order.
        self.ui_navigate(&nav_layers, self.ui_frame_dt);
        let contains = |r: &[f32; 4], p: &[f32; 2]| {
            p[0] >= r[0] && p[1] >= r[1] && p[0] <= r[0] + r[2] && p[1] <= r[1] + r[3]
        };
        // Topmost interactive element under the pointer (per-item design
        // pointer), honoring scroll clips.
        let hit = items.iter().rev().find(|(_, rect, pd, _, clip)| {
            pd.is_some_and(|p| contains(rect, &p) && clip.is_none_or(|c| contains(&c, &p)))
        });
        let hover = hit.map(|(id, ..)| *id);
        // The pointer in the hovered element's own design units — what a drag
        // measures its travel in. Falls back to the last layer solved this
        // frame so a drag that wanders off every element still tracks.
        let hover_pd = hit.and_then(|(_, _, pd, ..)| *pd).or(last_ptr_design);
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
            // Clicking a focusable element focuses it, so a player who reaches
            // for the mouse mid-menu and then goes back to the pad carries on
            // from where they clicked rather than from where the ring was left.
            let focusable = nav_layers.iter().any(|(_, roots, _)| {
                fn has(ns: &[floptle_ui::Node], id: u32) -> bool {
                    ns.iter().any(|n| {
                        (n.id == id && (n.spec.focusable || n.spec.field.is_some()))
                            || has(&n.children, id)
                    })
                }
                has(roots, h)
            });
            if focusable {
                self.ui_focus_set(Some(h));
            }
            self.ui_events.push((h, "pressed"));
            // Clicking inside a field puts the caret where you clicked, not at
            // the end. Anything else and correcting a typo means retyping the
            // rest of the word.
            if self.ui_field_of(h).is_some()
                && let Some((_, rect, Some(pd), ..)) =
                    items.iter().find(|(id, ..)| *id == h).copied()
            {
                self.ui_sync_edit();
                self.ui_caret_at(h, rect, pd[0], self.shift);
            }
        }
        // Dragging inside a field extends the selection, the same gesture that
        // selects text everywhere else.
        if down
            && !pressed_edge
            && let Some(a) = self.ui_active
            && self.ui_field_of(a).is_some()
            && let Some((_, rect, Some(pd), ..)) = items.iter().find(|(id, ..)| *id == a).copied()
        {
            self.ui_caret_at(a, rect, pd[0], true);
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
                self.ui_toggle(a);
            }
        }
        // ---- scrollbar drags -------------------------------------------------
        // Grabbing anywhere on the track jumps to that position and keeps
        // tracking, which is what every scrollbar does and what makes a long
        // list usable at all.
        if pressed_edge && let Some(&(bar, ..)) = bar_hits.last() {
            self.ui_scroll_grab = Some(bar);
        }
        if !down {
            self.ui_scroll_grab = None;
            self.ui_scroll_drag = None;
        }
        if let Some(bar) = self.ui_scroll_grab
            && let Some(&(_, view, axis, track, p)) =
                bar_hits.iter().find(|(id, ..)| *id == bar)
        {
            let travel = scroll_hits
                .iter()
                .find(|(id, _)| *id == view)
                .map(|(_, m)| m[axis])
                .unwrap_or(0.0);
            if travel > 0.0 && track[axis + 2] > 0.0 {
                let t = ((p[axis] - track[axis]) / track[axis + 2]).clamp(0.0, 1.0);
                self.ui_set_scroll(view, axis, t * travel);
            }
        }
        // ---- drag the content itself ----------------------------------------
        if pressed_edge
            && self.ui_active.is_none()
            && let Some(&(view, _)) = scroll_hits.last()
            && self
                .world
                .query::<Transform>()
                .map(|(e, _)| e)
                .find(|e| e.index() == view)
                .and_then(|e| self.world.get::<ElementSpec>(e))
                .and_then(|s| s.scroll)
                .is_some_and(|sc| sc.drag)
            && let Some(p) = scroll_ptr
        {
            self.ui_scroll_drag = Some((view, p));
        }
        if down
            && let Some((view, last)) = self.ui_scroll_drag
            && let Some(p) = scroll_ptr
        {
            let travel = scroll_hits.iter().find(|(id, _)| *id == view).map(|(_, m)| *m);
            if let Some(max) = travel {
                let cur = self
                    .world
                    .query::<Transform>()
                    .map(|(e, _)| e)
                    .find(|e| e.index() == view)
                    .and_then(|e| self.world.get::<ElementSpec>(e))
                    .and_then(|s| s.scroll)
                    .map(|sc| [sc.offset_x, sc.offset])
                    .unwrap_or([0.0, 0.0]);
                for a in 0..2 {
                    if max[a] > 0.0 {
                        // Content follows the finger: dragging down reveals
                        // what's above, so the offset moves the other way.
                        self.ui_set_scroll(view, a, (cur[a] - (p[a] - last[a])).clamp(0.0, max[a]));
                    }
                }
            }
            self.ui_scroll_drag = Some((view, p));
        }
        if pointer.is_none() && !down {
            self.ui_active = None;
        }
        // ---- drag and drop ---------------------------------------------------
        // The drop target is the LAST `drop_target` under the pointer rather
        // than the topmost hit: the slot you are aiming at is usually behind
        // the item sitting in it.
        self.ui_drag_report = None;
        let drop_over = drop_hits.last().copied();
        self.ui_drag_step(drop_over, hover, hover_pd, pressed_edge, down);
        // ---- text fields -----------------------------------------------------
        self.ui_edit_text(self.ui_frame_dt);
        // ---- tooltips --------------------------------------------------------
        for (layer, design_vp, placed, delay, ptr) in tip_layers {
            self.ui_tooltips(layer, hover, ptr, design_vp, &placed, delay);
        }
        self.ui_tick_tooltip_timer(hover, self.ui_frame_dt);
        // Publish this frame's solved screen rects for `node:uiRect()` — fed
        // here (right before scripts run) so a script's mouse hit-test uses
        // the panel's ACTUAL rendered position.
        self.script_host.set_ui_rects(solved_rects);
        // …and the focus, so `node.focused` / `ui.focused()` read this frame's
        // truth rather than last frame's.
        self.script_host.set_ui_focus(self.ui_focus);
        self.script_host.set_ui_drag(self.ui_drag_report);
    }

    /// Write one axis of a scroll view's offset.
    fn ui_set_scroll(&mut self, view: u32, axis: usize, value: f32) {
        let ent = self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == view);
        if let Some(e) = ent
            && let Some(spec) = self.world.get_mut::<ElementSpec>(e)
            && let Some(sc) = &mut spec.scroll
        {
            if axis == 0 {
                sc.offset_x = value;
            } else {
                sc.offset = value;
            }
        }
        self.input_scroll = 0.0;
    }

    /// Apply toggle / radio-group behaviour to a clicked element.
    ///
    /// `selected` is already a first-class style state, so this needs no new
    /// look and no new hook — the element simply becomes selected, and the
    /// project's `selected` block says what that means. A group clears its
    /// mates within the same LAYER, so two screens can reuse a group name.
    fn ui_toggle(&mut self, clicked: u32) {
        let Some(ent) =
            self.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == clicked)
        else {
            return;
        };
        let Some(spec) = self.world.get::<ElementSpec>(ent) else { return };
        let group = spec.group.clone();
        let toggle = spec.toggle;
        if group.is_empty() {
            if toggle {
                let now = !spec.selected;
                if let Some(s) = self.world.get_mut::<ElementSpec>(ent) {
                    s.selected = now;
                }
            }
            return;
        }
        // Group-mates: same layer root, same group name.
        let layer = self.ui_layer_of(ent);
        let mates: Vec<Entity> = self
            .world
            .query::<ElementSpec>()
            .filter(|(e, s)| s.group == group && self.ui_layer_of(*e) == layer)
            .map(|(e, _)| e)
            .collect();
        for m in mates {
            if let Some(s) = self.world.get_mut::<ElementSpec>(m) {
                s.selected = m == ent;
            }
        }
    }

    /// The UiLayer entity an element belongs to (walking up `Parent`), so group
    /// names and element-name lookups are scoped to one screen.
    fn ui_layer_of(&self, mut e: Entity) -> Option<Entity> {
        for _ in 0..64 {
            if self.world.get::<UiLayer>(e).is_some() {
                return Some(e);
            }
            match self.world.get::<Parent>(e) {
                Some(p) => e = p.0,
                None => return None,
            }
        }
        None
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
                        radius: 10.0.into(),
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
                        radius: 8.0.into(),
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
                        radius: 8.0.into(),
                        ..Default::default()
                    }),
                    scroll: Some(floptle_ui::ScrollSpec::default()),
                    ..Default::default()
                }),
            ),
            AddUi::Field => (
                "Text Field",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [40.0, 40.0] },
                    size: [Size::Fixed(280.0), Size::Fixed(48.0)],
                    shape: Some(ShapeSpec {
                        fill: [0.09, 0.09, 0.11, 0.9],
                        radius: 6.0.into(),
                        ..Default::default()
                    }),
                    // Left-aligned with a little breathing room, because that
                    // is what a field is; everything else about the look is a
                    // style away.
                    text: Some(TextSpec { align: Align::Start, ..Default::default() }),
                    field: Some(floptle_ui::FieldSpec {
                        placeholder: "Type here".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            ),
            AddUi::Tooltip => (
                "Tooltip",
                None,
                Some(ElementSpec {
                    place: Place::Free { pos: [0.0, 0.0] },
                    size: [Size::Fit, Size::Fit],
                    stack: Some(floptle_ui::StackCfg { pad: 8.0, ..Default::default() }),
                    shape: Some(ShapeSpec {
                        fill: [0.06, 0.06, 0.08, 0.94],
                        radius: 4.0.into(),
                        ..Default::default()
                    }),
                    text: Some(TextSpec { size: 18.0, ..Default::default() }),
                    tooltip_box: true,
                    // Starts hidden; the engine shows it when something with a
                    // tooltip has been hovered long enough.
                    visible: false,
                    // On top of the screen it annotates, which is the one
                    // thing about a tooltip that is not a matter of taste.
                    order: 1000,
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
                            radius: 8.0.into(),
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
                            radius: 6.0.into(),
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
    #[allow(clippy::too_many_arguments)] // an Inspector section needs the project's context
    pub(crate) fn ui_inspector(
        world: &mut floptle_core::World,
        e: Entity,
        ui: &mut egui::Ui,
        asset_tree: &[crate::assets::AssetEntry],
        project_root: &std::path::Path,
        texture_settings: &std::collections::HashMap<String, crate::assets::TexSetting>,
        ui_flsl_cache: &crate::shaders::UiFlslCache,
        styles: &floptle_ui::StyleSheet,
    ) -> bool {
        let mut changed = false;
        if let Some(mut layer) = world.get::<UiLayer>(e).copied() {
            use floptle_ui::{UiScaleMode, UiSpace};
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
            // ---- canvas scaler: how design units map to the window ----------
            ui.horizontal(|ui| {
                ui.label("scale mode");
                let (label, tip) = match layer.scale_mode {
                    UiScaleMode::MatchHeight => ("match height", "reference height spans the window height; width follows the aspect (the classic default)"),
                    UiScaleMode::MatchWidth => ("match width", "reference width spans the window width; height follows the aspect"),
                    UiScaleMode::Blend => ("blend W/H", "blend match-width and match-height by the slider below — the responsive middle ground"),
                    UiScaleMode::Expand => ("expand (fit)", "fit the whole reference INSIDE the window — never crops, may leave margins"),
                    UiScaleMode::Shrink => ("shrink (fill)", "fill the window with the reference — no margins, may crop the edges"),
                    UiScaleMode::ConstantPixels => ("constant px", "1 design unit = 1 pixel; the UI never rescales with the window"),
                };
                egui::ComboBox::from_id_salt(("ui_scale_mode", e))
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (v, l, t) in [
                            (UiScaleMode::MatchHeight, "match height", "reference height spans the window height; width follows the aspect (the classic default)"),
                            (UiScaleMode::MatchWidth, "match width", "reference width spans the window width; height follows the aspect"),
                            (UiScaleMode::Blend, "blend W/H", "blend match-width and match-height by the slider below"),
                            (UiScaleMode::Expand, "expand (fit)", "fit the whole reference INSIDE the window — never crops, may leave margins"),
                            (UiScaleMode::Shrink, "shrink (fill)", "fill the window with the reference — no margins, may crop"),
                            (UiScaleMode::ConstantPixels, "constant px", "1 design unit = 1 pixel; never rescales"),
                        ] {
                            changed |= ui.selectable_value(&mut layer.scale_mode, v, l).on_hover_text(t).changed();
                        }
                    })
                    .response
                    .on_hover_text(tip);
            });
            ui.horizontal(|ui| {
                ui.label("reference");
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.reference_width).range(100.0..=8192.0).prefix("W "))
                    .on_hover_text("reference WIDTH in design units — the width you author against (used by every mode except match-height)")
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.design_height).range(100.0..=4320.0).prefix("H "))
                    .on_hover_text("reference HEIGHT in design units — the height you author against. Element positions/sizes are in these units.")
                    .changed();
            });
            if layer.scale_mode == UiScaleMode::Blend {
                ui.horizontal(|ui| {
                    ui.label("match");
                    changed |= ui
                        .add(egui::Slider::new(&mut layer.match_wh, 0.0..=1.0).text("W↔H"))
                        .on_hover_text("0 = match width, 1 = match height, 0.5 = balance both")
                        .changed();
                });
            }
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
            // Navigation feel + tooltip dwell, per layer — a fast action menu
            // and a long settings list genuinely want different numbers, and
            // guessing one for both is how a menu ends up feeling wrong
            // without anyone being able to say why.
            ui.horizontal(|ui| {
                ui.label("nav hold").on_hover_text(
                    "seconds a direction is held before it starts repeating, then seconds \
                     between repeats",
                );
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.nav_delay).speed(0.01).range(0.0..=2.0).prefix("wait "))
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.nav_repeat).speed(0.01).range(0.01..=1.0).prefix("every "))
                    .changed();
                changed |= ui
                    .checkbox(&mut layer.nav_wrap, "wrap")
                    .on_hover_text(
                        "running off the end comes back on the other side. Right for a short \
                         menu, wrong for a long inventory — which is why it asks.",
                    )
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("tooltip delay").on_hover_text(
                    "seconds of hovering before this layer's tooltip element appears",
                );
                changed |= ui
                    .add(egui::DragValue::new(&mut layer.tooltip_delay).speed(0.02).range(0.0..=5.0).suffix(" s"))
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
        // --- placement (Free / Pin / Stretch) ---
        ui.horizontal(|ui| {
            ui.label("placement");
            let cur = match spec.place {
                Place::Free { .. } => "free",
                Place::Pin { .. } => "pin",
                Place::Stretch { .. } => "stretch",
            };
            egui::ComboBox::from_id_salt(("ui_place_mode", e.index()))
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(cur == "free", "free").on_hover_text("a fixed position from the parent's top-left").clicked()
                        && !matches!(spec.place, Place::Free { .. }) {
                        spec.place = Place::Free { pos: [40.0, 40.0] };
                        c = true;
                    }
                    if ui.selectable_label(cur == "pin", "pin").on_hover_text("stick to one of 9 parent points + an offset (HUD corners)").clicked()
                        && !matches!(spec.place, Place::Pin { .. }) {
                        spec.place = Place::Pin { anchor: Anchor::TopLeft, offset: [0.0, 0.0] };
                        c = true;
                    }
                    if ui.selectable_label(cur == "stretch", "stretch").on_hover_text("anchor to a box between two parent fractions and STRETCH with it — the responsive mode").clicked()
                        && !matches!(spec.place, Place::Stretch { .. }) {
                        spec.place = Place::fill(16.0);
                        c = true;
                    }
                });
        });
        // Stretch quick-presets: the shapes designers actually reach for.
        if matches!(spec.place, Place::Stretch { .. }) {
            ui.horizontal(|ui| {
                ui.small("fill:");
                let presets: [(&str, [f32; 2], [f32; 2]); 5] = [
                    ("all", [0.0, 0.0], [1.0, 1.0]),
                    ("top", [0.0, 0.0], [1.0, 0.0]),
                    ("bottom", [0.0, 1.0], [1.0, 1.0]),
                    ("left", [0.0, 0.0], [0.0, 1.0]),
                    ("right", [1.0, 0.0], [1.0, 1.0]),
                ];
                for (lbl, mn, mx) in presets {
                    if ui.small_button(lbl).clicked()
                        && let Place::Stretch { min, max, .. } = &mut spec.place {
                        *min = mn;
                        *max = mx;
                        c = true;
                    }
                }
            });
        }
        match &mut spec.place {
            Place::Free { pos } => {
                ui.horizontal(|ui| {
                    ui.label("pos");
                    c |= ui.add(egui::DragValue::new(&mut pos[0]).speed(1.0)).changed();
                    c |= ui.add(egui::DragValue::new(&mut pos[1]).speed(1.0)).changed();
                });
            }
            Place::Stretch { min, max, margin } => {
                ui.horizontal(|ui| {
                    ui.label("anchor min");
                    c |= ui.add(egui::DragValue::new(&mut min[0]).speed(0.01).range(0.0..=1.0).prefix("x ")).changed();
                    c |= ui.add(egui::DragValue::new(&mut min[1]).speed(0.01).range(0.0..=1.0).prefix("y ")).changed();
                });
                ui.horizontal(|ui| {
                    ui.label("anchor max");
                    c |= ui.add(egui::DragValue::new(&mut max[0]).speed(0.01).range(0.0..=1.0).prefix("x ")).changed();
                    c |= ui.add(egui::DragValue::new(&mut max[1]).speed(0.01).range(0.0..=1.0).prefix("y ")).changed();
                });
                ui.horizontal(|ui| {
                    ui.label("margin");
                    c |= ui.add(egui::DragValue::new(&mut margin[0]).speed(1.0).prefix("L ")).changed();
                    c |= ui.add(egui::DragValue::new(&mut margin[1]).speed(1.0).prefix("T ")).changed();
                    c |= ui.add(egui::DragValue::new(&mut margin[2]).speed(1.0).prefix("R ")).changed();
                    c |= ui.add(egui::DragValue::new(&mut margin[3]).speed(1.0).prefix("B ")).changed();
                });
                ui.small("axes where max>min stretch (size ignored there); equal = a point anchor keeping its size");
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
        // --- min/max size clamps (0 = unbounded) ---
        ui.horizontal(|ui| {
            ui.label("min size").on_hover_text("floor on the resolved size (design units, 0 = none) — keeps %/fit/stretch from collapsing");
            c |= ui.add(egui::DragValue::new(&mut spec.min_size[0]).speed(1.0).range(0.0..=8192.0).prefix("W ")).changed();
            c |= ui.add(egui::DragValue::new(&mut spec.min_size[1]).speed(1.0).range(0.0..=8192.0).prefix("H ")).changed();
        });
        ui.horizontal(|ui| {
            ui.label("max size").on_hover_text("cap on the resolved size (design units, 0 = none) — keeps it from ballooning on huge/ultrawide screens");
            c |= ui.add(egui::DragValue::new(&mut spec.max_size[0]).speed(1.0).range(0.0..=8192.0).prefix("W ")).changed();
            c |= ui.add(egui::DragValue::new(&mut spec.max_size[1]).speed(1.0).range(0.0..=8192.0).prefix("H ")).changed();
        });
        ui.horizontal(|ui| {
            c |= ui
                .checkbox(&mut spec.toggle, "toggle")
                .on_hover_text(
                    "clicking flips `selected` — a checkbox, a mute button, a filter chip. \
                     What ON looks like is your style's `selected` block.",
                )
                .changed();
            ui.label("group").on_hover_text(
                "radio behaviour: clicking selects this and deselects everything else with \
                 the same group name in this layer. Tabs, difficulty pickers, weapon slots. \
                 Empty = not a group.",
            );
            c |= ui.text_edit_singleline(&mut spec.group).changed();
        });
        ui.horizontal(|ui| {
            c |= ui
                .checkbox(&mut spec.focusable, "focusable")
                .on_hover_text(
                    "reachable by keyboard and gamepad: a direction press can move the focus \
                     here, and a submit press fires this element's `clicked` hook. What focus \
                     LOOKS like is your style's `focus` block — the engine draws no ring.",
                )
                .changed();
            if spec.focusable {
                let has_nav = spec.nav.is_some();
                if ui
                    .selectable_label(has_nav, "nav ⏵")
                    .on_hover_text(
                        "name the element each direction goes to, when the geometry gets it \
                         wrong (a grid that wraps, a Back button reachable from anywhere). \
                         Blank = work it out from the solved rects.",
                    )
                    .clicked()
                {
                    spec.nav = if has_nav { None } else { Some(Default::default()) };
                    c = true;
                }
            }
        });
        if spec.focusable && let Some(nav) = spec.nav.as_mut() {
            ui.indent("ui_nav", |ui| {
                for (label, field) in [
                    ("up", &mut nav.up),
                    ("down", &mut nav.down),
                    ("left", &mut nav.left),
                    ("right", &mut nav.right),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        c |= ui.text_edit_singleline(field).changed();
                    });
                }
            });
        }
        // --- drag & drop -----------------------------------------------------
        ui.horizontal(|ui| {
            c |= ui
                .checkbox(&mut spec.draggable, "draggable")
                .on_hover_text(
                    "can be picked up: fires `dragStart` / `dragMove` / `dropped` (or \
                     `dragCancel`). The engine does NOT move it and draws no ghost — what a \
                     drag looks like is your script's, because a card that tilts and an item \
                     that snaps to a grid are both drags.",
                )
                .changed();
            c |= ui
                .checkbox(&mut spec.drop_target, "drop target")
                .on_hover_text(
                    "can receive one: `dragEnter` / `dragOver` / `dragLeave` / `dropped`. \
                     Read `ui.dragging()` in the hook to see what arrived.",
                )
                .changed();
        });
        // --- repeater --------------------------------------------------------
        let mut has_rep = spec.repeater.is_some();
        if ui
            .checkbox(&mut has_rep, "repeat a row")
            .on_hover_text(
                "keep this element's children matching `count`, one copy of a prefab each. \
                 The engine spawns and destroys only the difference, so a list that gains \
                 a row keeps the others' state. Rows read `node.index`. Runs during Play.",
            )
            .changed()
        {
            spec.repeater = has_rep.then(floptle_ui::RepeatSpec::default);
            c = true;
        }
        if let Some(r) = &mut spec.repeater {
            ui.indent("ui_repeat", |ui| {
                ui.horizontal(|ui| {
                    ui.label("row prefab").on_hover_text("the same name `spawn()` takes");
                    c |= ui.text_edit_singleline(&mut r.template).changed();
                });
                ui.horizontal(|ui| {
                    ui.label("count").on_hover_text(
                        "usually driven from Lua — `ui.bind(list, \"count\", function() \
                         return #items end)`",
                    );
                    c |= ui.add(egui::DragValue::new(&mut r.count).speed(0.2).range(0..=4096)).changed();
                });
            });
        }
        // --- tooltip ---------------------------------------------------------
        ui.horizontal(|ui| {
            ui.label("tooltip").on_hover_text(
                "shown after a moment's hover, in this layer's tooltip element. Empty = none.",
            );
            c |= ui.text_edit_singleline(&mut spec.tooltip).changed();
        });
        c |= ui
            .checkbox(&mut spec.tooltip_box, "is this layer's tooltip")
            .on_hover_text(
                "this element IS the tooltip: the engine hides it when nothing is hovered, \
                 writes the hovered element's text into its first label, and moves it to \
                 follow the pointer. What it looks like is entirely yours.",
            )
            .changed();
        ui.horizontal(|ui| {
            ui.label("depth").on_hover_text(
                "sort key among siblings — lower draws first (further back), and inside a stack \
                 lower comes first in the flow. Ties keep scene order. The ◫ UI tab's outline \
                 drag writes this.",
            );
            c |= ui.add(egui::DragValue::new(&mut spec.order).speed(0.2)).changed();
        });
        ui.horizontal(|ui| {
            c |= ui.checkbox(&mut spec.visible, "visible").changed();
            ui.label("opacity");
            c |= ui
                .add(egui::Slider::new(&mut spec.opacity, 0.0..=1.0))
                .on_hover_text("multiplies this element AND its children — fade a whole menu with one number")
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("group tint")
                .on_hover_text("multiplies every colour in this subtree — damage flashes, disabled washes");
            c |= ui.color_edit_button_rgba_unmultiplied(&mut spec.tint).changed();
            if spec.tint != [1.0; 4] && ui.small_button("reset").clicked() {
                spec.tint = [1.0; 4];
                c = true;
            }
        });
        // --- transform (visual only — layout never sees it) ---
        ui.horizontal(|ui| {
            ui.label("rotate");
            c |= ui
                .add(egui::DragValue::new(&mut spec.rotation).speed(0.5).suffix("°"))
                .changed();
            ui.label("scale");
            c |= ui
                .add(egui::DragValue::new(&mut spec.scale[0]).speed(0.01).range(0.01..=8.0).prefix("x "))
                .changed();
            c |= ui
                .add(egui::DragValue::new(&mut spec.scale[1]).speed(0.01).range(0.01..=8.0).prefix("y "))
                .changed();
        })
        .response
        .on_hover_text(
            "visual only — the element keeps its layout rect, so a hover pop or a press dip \
             can never shove its siblings around",
        );
        if spec.rotation != 0.0 || spec.scale != [1.0, 1.0] {
            ui.horizontal(|ui| {
                ui.label("  pivot");
                c |= ui
                    .add(egui::DragValue::new(&mut spec.pivot[0]).speed(0.01).range(-2.0..=3.0).prefix("x "))
                    .changed();
                c |= ui
                    .add(egui::DragValue::new(&mut spec.pivot[1]).speed(0.01).range(-2.0..=3.0).prefix("y "))
                    .changed();
                if ui.small_button("reset").clicked() {
                    spec.rotation = 0.0;
                    spec.scale = [1.0, 1.0];
                    spec.pivot = [0.5, 0.5];
                    c = true;
                }
            })
            .response
            .on_hover_text("fraction of the element's own rect — 0.5, 0.5 is its centre");
        }
        c |= ui
            .checkbox(&mut spec.button, "button (clickable)")
            .on_hover_text(
                "the pointer can hover/press/click this element — its scripts get                  hoverStart / hoverEnd / pressed / released / clicked hooks.",
            )
            .changed();
        ui.horizontal(|ui| {
            c |= ui
                .checkbox(&mut spec.disabled, "disabled")
                .on_hover_text("stops responding and picks the style's `disabled` block")
                .changed();
            c |= ui
                .checkbox(&mut spec.selected, "selected")
                .on_hover_text("\"this is the current one\" — a menu cursor, a chosen tab")
                .changed();
        });
        // --- style (at most one; the element's own properties still win) ---
        ui.horizontal(|ui| {
            ui.label("style");
            let current = if spec.style.is_empty() { "(none)" } else { spec.style.as_str() };
            egui::ComboBox::from_id_salt(("ui_style_pick", e.index()))
                .selected_text(current)
                .width(180.0)
                .show_ui(ui, |ui| {
                    let mut none = String::new();
                    if ui.selectable_value(&mut none, String::new(), "(none)").clicked() {
                        spec.style.clear();
                        c = true;
                    }
                    for name in styles.styles.keys() {
                        if ui.selectable_label(&spec.style == name, name).clicked() {
                            spec.style = name.clone();
                            c = true;
                        }
                    }
                });
            if !spec.style.is_empty() && styles.get(&spec.style).is_none() {
                ui.colored_label(egui::Color32::from_rgb(255, 140, 90), "⚠ missing")
                    .on_hover_text(
                        "no style by that name in any .uistyle.ron — the element keeps \
                         its authored look",
                    );
            }
        })
        .response
        .on_hover_text(
            "one named style from the project's .uistyle.ron files. Whatever the style \
             doesn't set stays exactly as authored here — no cascade, no specificity.",
        );
        if styles.styles.is_empty() {
            ui.small("no .uistyle.ron in this project yet — styles are how hover/pressed stop being per-button Lua");
        }
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
                // A gradient is one checkbox away from any flat fill, because
                // "this panel reads as a slab" should be a ten-second fix and
                // not a reason to go write a `stage ui` shader.
                let mut on = s.gradient.is_some();
                if ui
                    .checkbox(&mut on, "gradient")
                    .on_hover_text("fade this fill into a second colour")
                    .changed()
                {
                    s.gradient = on.then(|| floptle_ui::Gradient {
                        // Start from the fill, darkened: the element keeps the
                        // look it already had at one end, so ticking the box
                        // never throws away what you had.
                        to: [s.fill[0] * 0.55, s.fill[1] * 0.55, s.fill[2] * 0.55, s.fill[3]],
                        ..Default::default()
                    });
                    c = true;
                }
            });
            if let Some(g) = &mut s.gradient {
                ui.horizontal(|ui| {
                    ui.label("  to");
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut g.to).changed();
                    egui::ComboBox::from_id_salt("ui_grad_kind")
                        .selected_text(match g.kind {
                            floptle_ui::GradientKind::Linear => "linear",
                            floptle_ui::GradientKind::Radial => "radial",
                            floptle_ui::GradientKind::Angular => "angular",
                        })
                        .show_ui(ui, |ui| {
                            for (k, label) in [
                                (floptle_ui::GradientKind::Linear, "linear"),
                                (floptle_ui::GradientKind::Radial, "radial"),
                                (floptle_ui::GradientKind::Angular, "angular"),
                            ] {
                                c |= ui.selectable_value(&mut g.kind, k, label).changed();
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("  angle");
                    c |= ui
                        .add(egui::DragValue::new(&mut g.angle).speed(1.0).suffix("°"))
                        .changed();
                    ui.label("mid");
                    c |= ui
                        .add(egui::DragValue::new(&mut g.mid).speed(0.01).range(0.0..=1.0))
                        .on_hover_text("where the two colours meet")
                        .changed();
                    if g.kind == floptle_ui::GradientKind::Radial {
                        ui.label("extent");
                        c |= ui
                            .add(egui::DragValue::new(&mut g.radius).speed(0.02).range(0.01..=4.0))
                            .changed();
                    }
                });
            }
            c |= quad_row(ui, "radius", &mut s.radius.0, ["TL", "TR", "BR", "BL"], 512.0, "ui_r");
            ui.horizontal(|ui| {
                ui.label("border colour");
                c |= ui.color_edit_button_rgba_unmultiplied(&mut s.border_color).changed();
            });
            c |= quad_row(ui, "border", &mut s.border.0, ["L", "T", "R", "B"], 64.0, "ui_b");
            // Soft shadow — behind the rect, or inside it.
            let mut has_shadow = s.shadow.is_some();
            if ui
                .checkbox(&mut has_shadow, "shadow")
                .on_hover_text("a soft shadow behind the panel (or inside it, for a recess)")
                .changed()
            {
                s.shadow = has_shadow.then(floptle_ui::ShadowSpec::default);
                c = true;
            }
            if let Some(sh) = &mut s.shadow {
                ui.horizontal(|ui| {
                    ui.label("  color");
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut sh.color).changed();
                    ui.label("blur");
                    c |= ui.add(egui::DragValue::new(&mut sh.blur).speed(0.5).range(0.0..=128.0)).changed();
                    c |= ui
                        .checkbox(&mut sh.inset, "inset")
                        .on_hover_text("draw it inside the shape — a recessed well")
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("  offset");
                    c |= ui.add(egui::DragValue::new(&mut sh.offset[0]).speed(0.5).prefix("x ")).changed();
                    c |= ui.add(egui::DragValue::new(&mut sh.offset[1]).speed(0.5).prefix("y ")).changed();
                    ui.label("spread");
                    c |= ui.add(egui::DragValue::new(&mut sh.spread).speed(0.5).range(0.0..=128.0)).changed();
                });
            }
            // Glow — light spilling out from under the element.
            let mut has_glow = s.glow.is_some();
            if ui
                .checkbox(&mut has_glow, "glow")
                .on_hover_text("an additive bloom around the element")
                .changed()
            {
                s.glow = has_glow.then(floptle_ui::GlowSpec::default);
                c = true;
            }
            if let Some(g) = &mut s.glow {
                ui.horizontal(|ui| {
                    ui.label("  color");
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut g.color).changed();
                    ui.label("radius");
                    c |= ui.add(egui::DragValue::new(&mut g.radius).speed(0.5).range(0.0..=128.0)).changed();
                    ui.label("spread");
                    c |= ui.add(egui::DragValue::new(&mut g.spread).speed(0.5).range(0.0..=128.0)).changed();
                });
            }
            // Grain — the cheapest thing on this panel, and often the one that
            // stops a screen looking machine-made.
            let mut has_grain = s.grain.is_some();
            if ui
                .checkbox(&mut has_grain, "grain")
                .on_hover_text("a little noise over the fill — kills the plastic look")
                .changed()
            {
                s.grain = has_grain.then(floptle_ui::GrainSpec::default);
                c = true;
            }
            if let Some(g) = &mut s.grain {
                ui.horizontal(|ui| {
                    ui.label("  amount");
                    c |= ui
                        .add(egui::DragValue::new(&mut g.amount).speed(0.005).range(0.0..=1.0))
                        .changed();
                    ui.label("cell");
                    c |= ui
                        .add(egui::DragValue::new(&mut g.scale).speed(0.1).range(1.0..=32.0))
                        .on_hover_text("noise cell size in px — higher is chunkier")
                        .changed();
                });
            }
            ui.horizontal(|ui| {
                ui.label("blend");
                egui::ComboBox::from_id_salt("ui_blend")
                    .selected_text(s.blend.label())
                    .show_ui(ui, |ui| {
                        for b in floptle_ui::Blend::ALL {
                            c |= ui.selectable_value(&mut s.blend, b, b.label()).changed();
                        }
                    });
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
                    project_root,
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
            ui.horizontal(|ui| {
                ui.label("tracking");
                c |= ui
                    .add(egui::DragValue::new(&mut t.tracking).speed(0.05).range(-8.0..=32.0))
                    .on_hover_text("letter spacing — wide tracking is what makes a title look set")
                    .changed();
                ui.label("line");
                c |= ui
                    .add(egui::DragValue::new(&mut t.line_height).speed(0.02).range(0.0..=4.0))
                    .on_hover_text("line height multiplier (0 = the font's own metrics)")
                    .changed();
                egui::ComboBox::from_id_salt("ui_case")
                    .selected_text(match t.case {
                        floptle_ui::Case::AsIs => "as-is",
                        floptle_ui::Case::Upper => "UPPER",
                        floptle_ui::Case::Lower => "lower",
                        floptle_ui::Case::Title => "Title",
                    })
                    .show_ui(ui, |ui| {
                        for (v, l) in [
                            (floptle_ui::Case::AsIs, "as-is"),
                            (floptle_ui::Case::Upper, "UPPER"),
                            (floptle_ui::Case::Lower, "lower"),
                            (floptle_ui::Case::Title, "Title"),
                        ] {
                            c |= ui.selectable_value(&mut t.case, v, l).changed();
                        }
                    });
            });
            ui.horizontal(|ui| {
                c |= ui
                    .checkbox(&mut t.wrap, "wrap")
                    .on_hover_text("break lines at the element's width")
                    .changed();
                ui.label("max lines");
                c |= ui
                    .add(egui::DragValue::new(&mut t.max_lines).speed(0.2).range(0..=64))
                    .on_hover_text("0 = unlimited")
                    .changed();
                egui::ComboBox::from_id_salt("ui_overflow")
                    .selected_text(match t.overflow {
                        floptle_ui::Overflow::Show => "show",
                        floptle_ui::Overflow::Clip => "clip",
                        floptle_ui::Overflow::Ellipsis => "ellipsis",
                    })
                    .show_ui(ui, |ui| {
                        for (v, l) in [
                            (floptle_ui::Overflow::Show, "show"),
                            (floptle_ui::Overflow::Clip, "clip"),
                            (floptle_ui::Overflow::Ellipsis, "ellipsis"),
                        ] {
                            c |= ui.selectable_value(&mut t.overflow, v, l).changed();
                        }
                    });
            });
            // Outline and shadow: what lets a label survive an arbitrary
            // background without a panel behind it.
            let mut has_stroke = t.stroke.is_some();
            if ui
                .checkbox(&mut has_stroke, "outline")
                .on_hover_text("an outline around the glyphs — legibility over anything")
                .changed()
            {
                t.stroke = has_stroke.then(floptle_ui::TextStroke::default);
                c = true;
            }
            if let Some(st) = &mut t.stroke {
                ui.horizontal(|ui| {
                    ui.label("  color");
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut st.color).changed();
                    ui.label("width");
                    c |= ui
                        .add(egui::DragValue::new(&mut st.width).speed(0.1).range(0.0..=8.0))
                        .changed();
                });
            }
            let mut has_tsh = t.shadow.is_some();
            if ui.checkbox(&mut has_tsh, "text shadow").changed() {
                t.shadow = has_tsh.then(floptle_ui::TextShadow::default);
                c = true;
            }
            if let Some(sh) = &mut t.shadow {
                ui.horizontal(|ui| {
                    ui.label("  color");
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut sh.color).changed();
                    c |= ui.add(egui::DragValue::new(&mut sh.offset[0]).speed(0.2).prefix("x ")).changed();
                    c |= ui.add(egui::DragValue::new(&mut sh.offset[1]).speed(0.2).prefix("y ")).changed();
                });
            }
        }
        // --- text field ---
        // Lives under `text` because a field's VALUE is its text: everything
        // above (font, alignment, tracking, stroke, the style's `text_color`)
        // applies unchanged, and a script reads it the way it reads any label.
        let mut has_field = spec.field.is_some();
        if ui
            .checkbox(&mut has_field, "editable (text field)")
            .on_hover_text(
                "the player can type into this element; the value IS its text above. \
                 Implicitly focusable. Fires `changed` and `submitted`.",
            )
            .changed()
        {
            spec.field = has_field.then(floptle_ui::FieldSpec::default);
            // A field with no text has nothing to edit and nothing to draw.
            if spec.field.is_some() {
                spec.text.get_or_insert_with(TextSpec::default);
            }
            c = true;
        }
        if let Some(f) = &mut spec.field {
            ui.indent("ui_field", |ui| {
                ui.horizontal(|ui| {
                    ui.label("placeholder")
                        .on_hover_text("shown while empty — never submits, never reads back as a value");
                    c |= ui.text_edit_singleline(&mut f.placeholder).changed();
                });
                ui.horizontal(|ui| {
                    ui.label("max").on_hover_text("cap in CHARACTERS (0 = none)");
                    c |= ui
                        .add(egui::DragValue::new(&mut f.max_len).speed(0.2).range(0..=1024))
                        .changed();
                    c |= ui
                        .checkbox(&mut f.numeric, "numeric")
                        .on_hover_text("digits, one leading -, one .")
                        .changed();
                    c |= ui
                        .checkbox(&mut f.upper, "UPPER")
                        .on_hover_text("shout as you type — lobby codes, initials, licence keys")
                        .changed();
                });
                ui.horizontal(|ui| {
                    c |= ui
                        .checkbox(&mut f.mask, "mask")
                        .on_hover_text(
                            "draw every character as a dot. Copy and cut are refused while \
                             this is on — a password field that fills the clipboard is a bug.",
                        )
                        .changed();
                    if f.mask {
                        let mut s = f.mask_char.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut s).desired_width(24.0))
                            .changed()
                            && let Some(ch) = s.chars().next()
                        {
                            f.mask_char = ch;
                            c = true;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("caret");
                    c |= ui.add(egui::DragValue::new(&mut f.caret_width).speed(0.1).range(0.5..=16.0)).changed();
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut f.caret_color).changed();
                    ui.label("sel");
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut f.selection_color).changed();
                    ui.label("hint");
                    c |= ui.color_edit_button_rgba_unmultiplied(&mut f.placeholder_color).changed();
                })
                .response
                .on_hover_text(
                    "leave a colour fully transparent and it follows the text colour \
                     (caret: as-is, selection: 30%, placeholder: 45%). Derived from the design \
                     you already made, rather than picked by the engine.",
                );
            });
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
                    project_root,
                    &current,
                    Some("(none)"),
                    asset_tree,
                    crate::assets::is_texture,
                    170.0,
                ) {
                    let new = pick.unwrap_or_default();
                    // Inherit the texture's spritesheet grid (set in its asset
                    // settings) so a picked sheet slices without extra steps.
                    let (sc, sr) =
                        crate::assets::tex_setting(texture_settings, project_root, &new).sheet();
                    img.cols = sc;
                    img.rows = sr;
                    img.cell = 0;
                    img.texture = new;
                    c = true;
                }
            });
            // --- spritesheet cell picker (when the texture is a sheet) ---
            let (sc, sr) =
                crate::assets::tex_setting(texture_settings, project_root, &img.texture).sheet();
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
                ui.label("fit");
                egui::ComboBox::from_id_salt("ui_img_fit")
                    .selected_text(match img.fit {
                        floptle_ui::ImageFit::Stretch => "stretch",
                        floptle_ui::ImageFit::Contain => "contain",
                        floptle_ui::ImageFit::Cover => "cover",
                    })
                    .show_ui(ui, |ui| {
                        for (v, l, tip) in [
                            (floptle_ui::ImageFit::Stretch, "stretch", "fill the rect, ignore aspect"),
                            (floptle_ui::ImageFit::Contain, "contain", "fit inside, letterboxed"),
                            (floptle_ui::ImageFit::Cover, "cover", "fill the rect, crop the overflow"),
                        ] {
                            c |= ui.selectable_value(&mut img.fit, v, l).on_hover_text(tip).changed();
                        }
                    });
            });
            // 9-slice: the thing that makes YOUR panel art usable at any size.
            let mut sliced = img.slice.iter().any(|v| *v > 0.0);
            if ui
                .checkbox(&mut sliced, "9-slice")
                .on_hover_text(
                    "keep the corners unstretched and stretch only the edges and middle — \
                     how one small frame texture dresses a panel at any size",
                )
                .changed()
            {
                // A sensible starting frame beats four zeroes: ticking the box
                // should show you the effect, not nothing.
                img.slice = if sliced { [0.25; 4] } else { [0.0; 4] };
                c = true;
            }
            if sliced {
                c |= quad_row(
                    ui,
                    "  insets",
                    &mut img.slice,
                    ["L", "T", "R", "B"],
                    0.49,
                    "ui_slice",
                );
                ui.small("fractions of the image — 0.25 means the outer quarter is the frame");
            }
            ui.horizontal(|ui| {
                ui.label("tiling");
                c |= ui
                    .add(egui::DragValue::new(&mut img.tiling[0]).speed(0.05).range(0.01..=64.0).prefix("x "))
                    .changed();
                c |= ui
                    .add(egui::DragValue::new(&mut img.tiling[1]).speed(0.05).range(0.01..=64.0).prefix("y "))
                    .changed();
                ui.label("offset");
                c |= ui.add(egui::DragValue::new(&mut img.offset[0]).speed(0.01).prefix("u ")).changed();
                c |= ui.add(egui::DragValue::new(&mut img.offset[1]).speed(0.01).prefix("v ")).changed();
            })
            .response
            .on_hover_text("repeat the image across the rect; animate the offset to scroll it");
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
        // --- ✨ effect (a `stage ui` .flsl face drawn over the shape) ---
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("✨ effect");
            // One-click built-in effects: pick one and it assigns the shader +
            // resets params to that effect's defaults. "Custom…" keeps whatever is
            // set (use the picker below); "None" removes the shader.
            let cur_name = crate::ui_shader_lib::effect_label(&spec.shader);
            egui::ComboBox::from_id_salt(("ui_effect", e.index()))
                .selected_text(cur_name)
                .width(150.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(spec.shader.is_empty(), "None").clicked()
                        && !spec.shader.is_empty()
                    {
                        spec.shader.clear();
                        spec.shader_params.clear();
                        c = true;
                    }
                    for (label, stem, _) in crate::ui_shader_lib::UI_EFFECTS {
                        let path = crate::ui_shader_lib::effect_path(stem);
                        if ui.selectable_label(spec.shader == path, *label).clicked()
                            && spec.shader != path
                        {
                            spec.shader = path;
                            spec.shader_params.clear(); // fall back to the effect's defaults
                            c = true;
                        }
                    }
                })
                .response
                .on_hover_text(
                    "built-in procedural effects (outline, gloss, glow, wobble, …). \
                     They draw over the element's shape and follow its rounded corners. \
                     Pick 'Custom' below to point at your own .flsl.",
                );
        });
        // Custom .flsl picker (any stage-ui shader in your assets).
        ui.horizontal(|ui| {
            ui.label("  shader");
            let current = if spec.shader.is_empty() {
                "(none)".to_string()
            } else {
                spec.shader.rsplit('/').next().unwrap_or(&spec.shader).to_string()
            };
            if let Some(pick) = crate::ui_widgets::asset_picker(
                ui,
                egui::Id::new(("ui_shader_pick", e.index())),
                project_root,
                &current,
                Some("(none)"),
                asset_tree,
                crate::assets::is_shader,
                150.0,
            ) {
                spec.shader = pick.unwrap_or_default();
                spec.shader_params.clear();
                c = true;
            }
        });
        // Live params for the assigned shader (compile error surfaced in red).
        if !spec.shader.is_empty() {
            if let Some(entry) = ui_flsl_cache.get(&spec.shader) {
                if let Some(err) = &entry.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 120, 110), format!("⚠ {err}"));
                }
                if let Some((compiled, _)) = &entry.compiled {
                    if compiled.uniforms.is_empty() {
                        ui.small("this shader exposes no parameters");
                    } else {
                        egui::Grid::new(("ui_shader_params", e.index()))
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                c |= crate::inspector::shader_uniform_rows(
                                    ui,
                                    &compiled.uniforms,
                                    &mut spec.shader_params,
                                );
                            });
                    }
                }
            } else {
                ui.small("compiling…");
            }
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
                c |= ui
                    .checkbox(&mut sc.drag, "drag to scroll")
                    .on_hover_text(
                        "dragging the background pans the content. Off by default: in a view \
                         full of buttons a drag that scrolled would fight every press",
                    )
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label("offset").on_hover_text(
                    "current scroll position in design units. Both axes scroll — the wheel \
                     drives whichever one has travel (shift forces sideways)",
                );
                c |= ui.add(egui::DragValue::new(&mut sc.offset_x).prefix("x ")).changed();
                c |= ui.add(egui::DragValue::new(&mut sc.offset).prefix("y ")).changed();
            });
        }
        // --- scrollbar (drives a named scroll view; your two elements) ---
        let mut has = spec.scrollbar.is_some();
        if ui
            .checkbox(&mut has, "scrollbar")
            .on_hover_text(
                "this element becomes a scrollbar TRACK for a named scroll view, and its \
                 `part: Handle` child becomes the thumb — sized to how much of the content \
                 is visible. The engine draws no scrollbar of its own; these are your two \
                 elements, styled however you like",
            )
            .changed()
        {
            spec.scrollbar = has.then(floptle_ui::ScrollBar::default);
            c = true;
        }
        if let Some(sb) = &mut spec.scrollbar {
            ui.horizontal(|ui| {
                ui.label("drives");
                c |= ui
                    .text_edit_singleline(&mut sb.target)
                    .on_hover_text("the scroll view's node name, within this layer")
                    .changed();
                let vertical = sb.axis == floptle_ui::Dir::Column;
                let mut v = vertical;
                if ui.selectable_label(v, "↕").on_hover_text("vertical").clicked() {
                    v = true;
                }
                if ui.selectable_label(!v, "↔").on_hover_text("horizontal").clicked() {
                    v = false;
                }
                if v != vertical {
                    sb.axis =
                        if v { floptle_ui::Dir::Column } else { floptle_ui::Dir::Row };
                    c = true;
                }
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
