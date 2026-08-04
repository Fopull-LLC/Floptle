//! The ◫ UI tab — the authoring surface for game UI
//! (docs/ui-system-2-proposal.md, phase C).
//!
//! Every other subsystem in the engine has a tab of its own; UI didn't, and it
//! shows in real projects: 53 elements hand-placed at typed pixel offsets,
//! centring arithmetic in Lua, z-order that could only be changed by deleting
//! and re-adding a node. Arrangement *is* the work in UI, and arrangement had
//! no tool.
//!
//! What this tab is:
//!
//! - **The real render.** The canvas is the shipping GPU pipeline drawing the
//!   selected layer into an offscreen target — not an egui approximation. A
//!   gradient, a `stage ui` shader and a 9-slice look here exactly as they look
//!   in the game, because they *are* the game's render.
//! - **Design-unit truth.** Everything the canvas reports (rulers, guides,
//!   readouts, nudges) is in the layer's design units, so numbers here mean the
//!   same thing as numbers in the Inspector and in Lua.
//! - **Opinion-free.** The tab imposes no look, ships no theme, and never
//!   writes a style you didn't ask for. Its defaults (snap step, resolution
//!   list) come from *your* project's tokens where a project has them.
//!
//! The Scene viewport's UI overlay stays: a world-space canvas genuinely
//! belongs in the 3D view. This is where flat screens get built.

use std::collections::{BTreeMap, HashMap, HashSet};

use floptle_core::{Entity, Parent, Transform};
use floptle_ui::{ElementSpec, Place, Placed, UiLayer, UiState};

/// Vertical guides (x) and horizontal guides (y), in design units.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Guides {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
}

/// A copied style: the name when the source had one, else the raw look lifted
/// off the element (so "copy style" works even in a project with no sheet).
#[derive(Clone)]
pub(crate) enum StyleClip {
    Named(String),
    Look(Box<ElementSpec>),
}

/// What the pointer is currently dragging on the canvas.
#[derive(Clone, Default)]
pub(crate) enum Drag {
    #[default]
    None,
    /// Moving elements. `applied` is how much of the gesture has already been
    /// committed, so each frame emits only the remainder — that's what lets a
    /// snapped drag hold still while the pointer keeps travelling.
    Move {
        applied: [f32; 2],
        start: egui::Pos2,
    },
    /// Re-ordering inside a Stack: the parent, and where the caret currently
    /// sits among its children.
    Reorder {
        parent: u32,
        at: usize,
    },
    /// Resizing the single selected element from one handle.
    Resize {
        id: u32,
        hx: i8,
        hy: i8,
    },
    /// Rubber-band selection from `start` (canvas points).
    Marquee {
        start: egui::Pos2,
        add: bool,
    },
    /// Dragging a guide: which axis, and its index (`None` = a fresh one being
    /// pulled off the ruler).
    Guide {
        vertical: bool,
        idx: Option<usize>,
    },
}

/// Preview resolutions. The point of the list is that `Pin` and `Stretch` only
/// pay off when you can *see* them pay off — a layout that survives 21:9 and a
/// phone is the whole reason those placements exist.
pub(crate) const RES_PRESETS: &[(&str, [f32; 2])] = &[
    ("Reference", [0.0, 0.0]),
    ("1920 × 1080 · 16:9", [1920.0, 1080.0]),
    ("1280 × 720 · 16:9", [1280.0, 720.0]),
    ("2560 × 1080 · 21:9", [2560.0, 1080.0]),
    ("1024 × 768 · 4:3", [1024.0, 768.0]),
    ("1080 × 1080 · 1:1", [1080.0, 1080.0]),
    ("844 × 390 · phone", [844.0, 390.0]),
    ("390 × 844 · phone ⟂", [390.0, 844.0]),
    ("Custom", [0.0, 0.0]),
];

pub(crate) struct UiDesignState {
    /// The layer being edited (entity index); `None` picks the first in scene.
    pub layer: Option<u32>,
    pub zoom: f32,
    pub pan: egui::Vec2,
    /// Set to re-fit the canvas on the next draw. Fitting is on DEMAND, never
    /// automatic: a canvas that re-frames itself while you work is exactly the
    /// "things move on their own" failure this editor tries not to have.
    pub want_fit: bool,
    pub res: usize,
    pub custom_res: [f32; 2],
    /// What the canvas clears to behind the layer. A colour, not a checker or a
    /// theme: you design against your game's actual background, and the tool
    /// stays neutral about whether your UI is light or dark.
    pub backdrop: [f32; 3],
    pub snap: bool,
    /// Snap step in design units; 0 = take it from the project's spacing tokens.
    pub snap_grid: f32,
    pub snap_guides: bool,
    pub snap_siblings: bool,
    pub show_grid: bool,
    pub rulers: bool,
    pub outlines: bool,
    /// Overlay the navigation graph: which elements are focusable, and where
    /// each direction leads from the selected one. Checking a gamepad path by
    /// launching the game and pressing down forty times is not checking it.
    pub show_nav: bool,
    pub outline_panel: bool,
    /// Force a style state on the whole layer so states can be *designed*
    /// rather than discovered at runtime.
    pub state: Option<UiState>,
    /// Guides per layer entity index (design units).
    pub guides: BTreeMap<u32, Guides>,
    pub guides_dirty: bool,
    /// Authoring-only: locked elements ignore canvas picking. Never saved into
    /// the scene — it's a property of how you're working, not of the game.
    pub locked: HashSet<u32>,
    pub style_clip: Option<StyleClip>,
    /// Inline text editing: (element, buffer).
    pub text_edit: Option<(u32, String)>,
    pub drag: Drag,
    /// "Make this a style" dialog: (target element, name, sheet index).
    pub make_style: Option<(u32, String, usize)>,
    /// Style sheets found in the project (path, display name) — the dialog's
    /// destination picker. Refreshed when the dialog opens.
    pub sheets: Vec<(std::path::PathBuf, String)>,

    // ---- filled by the render pass, read by the tab (one frame behind, the
    // same contract the Game viewport uses) ----
    /// The canvas rect the tab drew at, so the next frame's render can size its
    /// target to it.
    pub rect: Option<egui::Rect>,
    pub tex: Option<egui::TextureId>,
    /// Design-space viewport the layer was solved at.
    pub design_vp: [f32; 2],
    /// Physical pixels per design unit in `tex`.
    pub render_scale: f32,
    pub placed: Vec<Placed>,
    /// The layer actually rendered (may differ from `layer` on the first frame).
    pub rendered_layer: Option<u32>,
    /// Set by the tab each frame it draws; the render pass reads and clears it.
    pub tab_visible: bool,
}

impl Default for UiDesignState {
    fn default() -> Self {
        UiDesignState {
            layer: None,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            want_fit: true,
            res: 0,
            custom_res: [1280.0, 720.0],
            backdrop: [0.07, 0.075, 0.09],
            snap: true,
            snap_grid: 0.0,
            snap_guides: true,
            snap_siblings: true,
            show_grid: false,
            rulers: true,
            outlines: true,
            show_nav: false,
            outline_panel: true,
            state: None,
            guides: BTreeMap::new(),
            guides_dirty: false,
            locked: HashSet::new(),
            style_clip: None,
            text_edit: None,
            drag: Drag::None,
            make_style: None,
            sheets: Vec::new(),
            rect: None,
            tex: None,
            design_vp: [1280.0, 720.0],
            render_scale: 1.0,
            placed: Vec::new(),
            rendered_layer: None,
            tab_visible: false,
        }
    }
}

impl UiDesignState {
    /// The preview resolution in physical pixels, or `None` for "the layer's
    /// own reference resolution".
    pub(crate) fn preview_px(&self, layer: &UiLayer) -> [f32; 2] {
        match RES_PRESETS.get(self.res) {
            Some(("Reference", _)) | None => {
                [layer.reference_width.max(16.0), layer.design_height.max(16.0)]
            }
            Some(("Custom", _)) => [self.custom_res[0].max(16.0), self.custom_res[1].max(16.0)],
            Some((_, px)) => *px,
        }
    }

    /// The snap step in design units — the project's smallest spacing token
    /// when the user hasn't overridden it, so the easy drag lands on a value
    /// from *their* scale rather than on someone else's idea of 8.
    pub(crate) fn grid_step(&self, tokens: &floptle_ui::Tokens) -> f32 {
        if self.snap_grid > 0.0 {
            return self.snap_grid;
        }
        let smallest =
            tokens.spacing.values().copied().filter(|v| *v > 0.5).fold(f32::INFINITY, f32::min);
        smallest.clamp(1.0, 64.0)
    }
}

// ---------------------------------------------------------------------------
// Guide persistence
// ---------------------------------------------------------------------------

/// Where a scene's guides live. Authoring data, not game data: it sits beside
/// the engine's other per-project editor state rather than in the scene file,
/// so a guide can never change what ships.
fn guides_path(project_root: &std::path::Path, scene: &str) -> std::path::PathBuf {
    let safe: String = scene
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    project_root.join(".floptle").join("guides").join(format!("{safe}.ron"))
}

/// Guides are keyed by the layer node's NAME, not its entity index: entity
/// indices are a runtime detail that changes when a scene is edited and
/// reloaded, and guides that silently jump to another layer would be worse than
/// guides that don't persist at all.
pub(crate) fn load_guides(
    project_root: &std::path::Path,
    scene: &str,
    world: &floptle_core::World,
) -> BTreeMap<u32, Guides> {
    let path = guides_path(project_root, scene);
    let Ok(text) = std::fs::read_to_string(path) else { return BTreeMap::new() };
    let Ok(by_name) = ron::from_str::<BTreeMap<String, Guides>>(&text) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (e, _) in world.query::<UiLayer>() {
        let name = world.get::<floptle_core::Name>(e).map(|n| n.0.clone()).unwrap_or_default();
        if let Some(g) = by_name.get(&name) {
            out.insert(e.index(), g.clone());
        }
    }
    out
}

pub(crate) fn save_guides(
    project_root: &std::path::Path,
    scene: &str,
    world: &floptle_core::World,
    guides: &BTreeMap<u32, Guides>,
) {
    let mut by_name: BTreeMap<String, Guides> = BTreeMap::new();
    for (idx, g) in guides {
        if g.x.is_empty() && g.y.is_empty() {
            continue;
        }
        let Some(e) = world.query::<UiLayer>().map(|(e, _)| e).find(|e| e.index() == *idx) else {
            continue;
        };
        let name = world.get::<floptle_core::Name>(e).map(|n| n.0.clone()).unwrap_or_default();
        by_name.insert(name, g.clone());
    }
    let path = guides_path(project_root, scene);
    if by_name.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = ron::ser::to_string_pretty(&by_name, ron::ser::PrettyConfig::default()) {
        let _ = std::fs::write(&path, text);
    }
}

// ---------------------------------------------------------------------------
// Layer tree walking (shared by the outline panel and the canvas)
// ---------------------------------------------------------------------------

/// One row of the layer's element tree, flattened in draw order.
pub(crate) struct Row {
    pub entity: Entity,
    pub id: u32,
    pub name: String,
    pub depth: usize,
    pub parent: u32,
    /// This element arranges its children (so they can't be freely dragged —
    /// dragging them re-orders instead).
    pub is_stack: bool,
    pub visible: bool,
    pub order: i32,
}

/// Flatten a layer's element subtree in the SAME order the renderer walks it —
/// `order` first, scene order breaking ties. The outline panel and the canvas
/// must agree with the draw list or "in front" means two different things in
/// two places.
pub(crate) fn layer_rows(world: &floptle_core::World, layer: Entity) -> Vec<Row> {
    let order: Vec<Entity> = world.query::<Transform>().map(|(e, _)| e).collect();
    let mut kids: HashMap<u32, Vec<Entity>> = HashMap::new();
    for e in &order {
        if let Some(p) = world.get::<Parent>(*e) {
            kids.entry(p.0.index()).or_default().push(*e);
        }
    }
    fn walk(
        world: &floptle_core::World,
        kids: &HashMap<u32, Vec<Entity>>,
        parent: u32,
        depth: usize,
        out: &mut Vec<Row>,
    ) {
        let Some(cs) = kids.get(&parent) else { return };
        let mut cs: Vec<Entity> =
            cs.iter().copied().filter(|c| world.get::<ElementSpec>(*c).is_some()).collect();
        cs.sort_by_key(|c| world.get::<ElementSpec>(*c).map(|s| s.order).unwrap_or(0));
        for c in cs {
            let spec = world.get::<ElementSpec>(c).unwrap();
            out.push(Row {
                entity: c,
                id: c.index(),
                name: world
                    .get::<floptle_core::Name>(c)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| format!("#{}", c.index())),
                depth,
                parent,
                is_stack: spec.stack.is_some(),
                visible: spec.visible,
                order: spec.order,
            });
            walk(world, kids, c.index(), depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(world, &kids, layer.index(), 0, &mut out);
    out
}

/// Renumber a sibling run so `moved` ends up at index `at`, returning
/// `(element, new order)` for every sibling.
///
/// The whole run is rewritten (0, 1, 2, …) rather than one value nudged: a run
/// left full of ties would make the *next* drag depend on scene order again,
/// which is the invisible state `ElementSpec::order` exists to replace.
pub(crate) fn reorder_run(sibs: &[u32], moved: u32, at: usize) -> Vec<(u32, i32)> {
    let mut sibs = sibs.to_vec();
    let Some(from) = sibs.iter().position(|id| *id == moved) else { return Vec::new() };
    sibs.remove(from);
    // `at` indexes the ORIGINAL run; once `moved` is pulled out, every position
    // after it shifts down by one.
    let at = if from < at { at.saturating_sub(1) } else { at };
    sibs.insert(at.min(sibs.len()), moved);
    sibs.iter().enumerate().map(|(i, id)| (*id, i as i32)).collect()
}

/// Renumber so `moved` (which may be several elements) sits at the front or the
/// back of its sibling run.
pub(crate) fn depth_run(sibs: &[u32], moved: &[u32], front: bool) -> Vec<(u32, i32)> {
    let mut rest: Vec<u32> = sibs.iter().copied().filter(|id| !moved.contains(id)).collect();
    let mut picked: Vec<u32> = sibs.iter().copied().filter(|id| moved.contains(id)).collect();
    if front {
        rest.extend(picked);
        picked = rest;
    } else {
        picked.extend(rest);
    }
    picked.iter().enumerate().map(|(i, id)| (*id, i as i32)).collect()
}

// ---------------------------------------------------------------------------
// Align / distribute
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    CenterX,
    Right,
    Top,
    CenterY,
    Bottom,
}

impl Align {
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Align::Left => "⇤",
            Align::CenterX => "⇹",
            Align::Right => "⇥",
            Align::Top => "⇧",
            Align::CenterY => "⇳",
            Align::Bottom => "⇩",
        }
    }
    pub(crate) fn tip(self) -> &'static str {
        match self {
            Align::Left => "align left edges",
            Align::CenterX => "align horizontal centres",
            Align::Right => "align right edges",
            Align::Top => "align top edges",
            Align::CenterY => "align vertical centres",
            Align::Bottom => "align bottom edges",
        }
    }
    fn axis(self) -> usize {
        match self {
            Align::Left | Align::CenterX | Align::Right => 0,
            _ => 1,
        }
    }
}

/// Design-unit moves that align `sel` — to the selection's own bounds when
/// several are picked, to the containing rect when only one is.
///
/// One element aligning to its parent is the case that matters: it's the whole
/// of "centre this panel on the screen", which is otherwise arithmetic in Lua.
pub(crate) fn align_moves(
    sel: &[u32],
    rect_of: &HashMap<u32, [f32; 4]>,
    container_of: &dyn Fn(u32) -> [f32; 4],
    how: Align,
) -> Vec<(u32, [f32; 2])> {
    let a = how.axis();
    let rects: Vec<(u32, [f32; 4])> =
        sel.iter().filter_map(|id| rect_of.get(id).map(|r| (*id, *r))).collect();
    if rects.is_empty() {
        return Vec::new();
    }
    // The line to align to.
    let bounds = if rects.len() >= 2 {
        let lo = rects.iter().map(|(_, r)| r[a]).fold(f32::INFINITY, f32::min);
        let hi = rects.iter().map(|(_, r)| r[a] + r[a + 2]).fold(f32::NEG_INFINITY, f32::max);
        [lo, hi - lo]
    } else {
        let c = container_of(rects[0].0);
        [c[a], c[a + 2]]
    };
    let mut out = Vec::new();
    for (id, r) in rects {
        let target = match how {
            Align::Left | Align::Top => bounds[0],
            Align::CenterX | Align::CenterY => bounds[0] + (bounds[1] - r[a + 2]) * 0.5,
            Align::Right | Align::Bottom => bounds[0] + bounds[1] - r[a + 2],
        };
        let d = target - r[a];
        if d != 0.0 {
            out.push((id, if a == 0 { [d, 0.0] } else { [0.0, d] }));
        }
    }
    out
}

/// Design-unit moves that put equal gaps between three or more elements along
/// `axis`, holding the two extremes still.
pub(crate) fn distribute_moves(
    sel: &[u32],
    rect_of: &HashMap<u32, [f32; 4]>,
    axis: usize,
) -> Vec<(u32, [f32; 2])> {
    let mut rects: Vec<(u32, [f32; 4])> =
        sel.iter().filter_map(|id| rect_of.get(id).map(|r| (*id, *r))).collect();
    if rects.len() < 3 {
        return Vec::new();
    }
    rects.sort_by(|a, b| a.1[axis].total_cmp(&b.1[axis]));
    let first = rects[0].1;
    let last = rects[rects.len() - 1].1;
    let span = (last[axis] + last[axis + 2]) - first[axis];
    let used: f32 = rects.iter().map(|(_, r)| r[axis + 2]).sum();
    let gap = (span - used) / (rects.len() - 1) as f32;
    let mut cursor = first[axis];
    let mut out = Vec::new();
    for (i, (id, r)) in rects.iter().enumerate() {
        if i > 0 && i + 1 < rects.len() {
            let d = cursor - r[axis];
            if d != 0.0 {
                out.push((*id, if axis == 0 { [d, 0.0] } else { [0.0, d] }));
            }
        }
        cursor += r[axis + 2] + gap;
    }
    out
}

// ---------------------------------------------------------------------------
// Snapping
// ---------------------------------------------------------------------------

/// A line the drag snapped to, in design units — drawn as a smart guide.
#[derive(Clone, Copy)]
pub(crate) struct SnapLine {
    pub vertical: bool,
    pub at: f32,
}

pub(crate) struct SnapCfg<'a> {
    pub grid: f32,
    pub guides: Option<&'a Guides>,
    /// Candidate edges/centres from siblings + the container, per axis.
    pub lines: [Vec<f32>; 2],
    /// Snap radius in design units.
    pub radius: f32,
}

/// Snap a proposed rect position, returning the adjusted delta and the lines
/// that caught it.
///
/// The element's leading edge, centre and trailing edge are all candidates, so
/// a panel snaps by whichever of its own edges is closest to something — the
/// behaviour every design tool has and the reason dragging can produce a tidy
/// layout at all.
pub(crate) fn snap_delta(rect: [f32; 4], want: [f32; 2], cfg: &SnapCfg) -> ([f32; 2], Vec<SnapLine>) {
    let mut out = want;
    let mut hits = Vec::new();
    for a in 0..2 {
        let pos = rect[a] + want[a];
        let size = rect[a + 2];
        let mine = [pos, pos + size * 0.5, pos + size];
        let mut best: Option<(f32, f32)> = None; // (distance, correction)
        let consider = |line: f32, best: &mut Option<(f32, f32)>| {
            for m in mine {
                let d = line - m;
                if d.abs() <= cfg.radius && best.is_none_or(|(bd, _)| d.abs() < bd) {
                    *best = Some((d.abs(), d));
                }
            }
        };
        if let Some(g) = cfg.guides {
            for line in if a == 0 { &g.x } else { &g.y } {
                consider(*line, &mut best);
            }
        }
        for line in &cfg.lines[a] {
            consider(*line, &mut best);
        }
        // The grid is the fallback: an explicit guide or a sibling edge is a
        // deliberate target and should win over "a multiple of 8".
        if best.is_none() && cfg.grid > 0.0 {
            let snapped = (pos / cfg.grid).round() * cfg.grid;
            let d = snapped - pos;
            if d.abs() <= cfg.radius {
                best = Some((d.abs(), d));
            }
        }
        if let Some((_, d)) = best {
            out[a] += d;
            let at = rect[a] + out[a];
            // Report the line we actually landed on (whichever edge caught).
            let landed = [at, at + size * 0.5, at + size];
            let mut show = at;
            let mut bestd = f32::INFINITY;
            let candidates: Vec<f32> = cfg
                .guides
                .map(|g| if a == 0 { g.x.clone() } else { g.y.clone() })
                .unwrap_or_default()
                .into_iter()
                .chain(cfg.lines[a].iter().copied())
                .collect();
            for c in candidates {
                for l in landed {
                    if (c - l).abs() < bestd {
                        bestd = (c - l).abs();
                        show = c;
                    }
                }
            }
            if bestd < 0.01 {
                hits.push(SnapLine { vertical: a == 0, at: show });
            }
        }
    }
    (out, hits)
}

// ---------------------------------------------------------------------------
// "Make this a style"
// ---------------------------------------------------------------------------

/// Lift an element's current look into a [`floptle_ui::StyleBlock`] — the
/// properties a style can carry, taken verbatim from what's on the element.
pub(crate) fn block_from(spec: &ElementSpec) -> floptle_ui::StyleBlock {
    use floptle_ui::{ColorRef, NumRef};
    let mut b = floptle_ui::StyleBlock::default();
    if let Some(sh) = &spec.shape {
        b.fill = Some(ColorRef::Lit(sh.fill));
        b.border_color = Some(ColorRef::Lit(sh.border_color));
        b.gradient = sh.gradient.map(|g| floptle_ui::StyleGradient {
            kind: g.kind,
            to: Some(ColorRef::Lit(g.to)),
            angle: g.angle,
            mid: g.mid,
            radius: g.radius,
        });
        b.radius = Some(floptle_ui::CornerRef::Lit(sh.radius));
        b.border = Some(sh.border);
        // Literal colours: this lifts what is ON the element, and the element
        // never knew a token name. Swapping them for tokens afterwards is the
        // natural second step, and one the author has to choose.
        b.shadow = sh.shadow.map(|s| floptle_ui::StyleShadow {
            color: ColorRef::Lit(s.color),
            offset: s.offset,
            blur: s.blur,
            spread: s.spread,
            inset: s.inset,
        });
        b.glow = sh.glow.map(|g| floptle_ui::StyleGlow {
            color: ColorRef::Lit(g.color),
            radius: g.radius,
            spread: g.spread,
        });
        b.grain = sh.grain;
        if sh.blend != floptle_ui::Blend::Normal {
            b.blend = Some(sh.blend);
        }
    }
    if let Some(t) = &spec.text {
        b.text_color = Some(ColorRef::Lit(t.color));
        b.text_size = Some(NumRef::Lit(t.size));
        if t.tracking != 0.0 {
            b.tracking = Some(t.tracking);
        }
        if t.line_height != 1.0 {
            b.line_height = Some(t.line_height);
        }
        if t.case != floptle_ui::Case::AsIs {
            b.case = Some(t.case);
        }
        if !t.font.is_empty() {
            b.font = Some(t.font.clone());
        }
        b.text_stroke = t.stroke.map(|st| floptle_ui::StyleStroke {
            color: ColorRef::Lit(st.color),
            width: st.width,
        });
        b.text_shadow = t.shadow.map(|sh| floptle_ui::StyleTextShadow {
            color: ColorRef::Lit(sh.color),
            offset: sh.offset,
        });
    }
    if let Some(st) = &spec.stack {
        b.pad = Some(NumRef::Lit(st.pad));
        b.gap = Some(NumRef::Lit(st.gap));
    }
    if spec.opacity != 1.0 {
        b.opacity = Some(spec.opacity);
    }
    if spec.tint != [1.0; 4] {
        b.tint = Some(ColorRef::Lit(spec.tint));
    }
    b
}

/// Append `style` to a `.uistyle.ron` under `name`, preserving everything
/// already in the file.
///
/// Textual, not a parse-and-reserialise: a style sheet is hand-written, and
/// rewriting it would silently eat the author's comments and grouping. If the
/// file doesn't exist (or has no closing brace to insert before) a fresh sheet
/// is written instead.
pub(crate) fn append_style(
    path: &std::path::Path,
    name: &str,
    style: &floptle_ui::Style,
) -> Result<(), String> {
    let cfg = ron::ser::PrettyConfig::default().struct_names(false).depth_limit(6);
    let body = ron::ser::to_string_pretty(style, cfg).map_err(|e| e.to_string())?;
    // Indent the block one level so it sits with its neighbours.
    let body: String = body.replace('\n', "\n    ");
    let entry = format!("    {:?}: {body},\n", name);
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let text = match existing.rfind('}') {
        Some(close) if !existing.trim().is_empty() => {
            let mut s = existing[..close].trim_end().to_string();
            // Keep the file valid whether or not the last entry had a comma.
            if !s.trim_end().ends_with(',') && !s.trim_end().ends_with('{') {
                s.push(',');
            }
            s.push('\n');
            s.push_str(&entry);
            s.push_str("}\n");
            s
        }
        _ => format!("// UI styles — see docs/ui-styles.md\n{{\n{entry}}}\n"),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// A short human label for an element's placement, shown on the canvas readout
/// so it's obvious *why* a drag moved a margin instead of a position.
pub(crate) fn place_label(place: &Place) -> &'static str {
    match place {
        Place::Free { .. } => "free",
        Place::Pin { .. } => "pin",
        Place::Stretch { .. } => "stretch",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(v: &[(u32, [f32; 4])]) -> HashMap<u32, [f32; 4]> {
        v.iter().copied().collect()
    }

    #[test]
    fn reordering_renumbers_the_whole_run() {
        let sibs = [10, 20, 30, 40];
        // Move the first element to the end.
        let out = reorder_run(&sibs, 10, 4);
        assert_eq!(out, vec![(20, 0), (30, 1), (40, 2), (10, 3)]);
        // Move the last element to the front.
        let out = reorder_run(&sibs, 40, 0);
        assert_eq!(out, vec![(40, 0), (10, 1), (20, 2), (30, 3)]);
        // Dropping an element where it already is changes nothing but the
        // numbering — no silent off-by-one shuffle.
        let out = reorder_run(&sibs, 20, 1);
        assert_eq!(out, vec![(10, 0), (20, 1), (30, 2), (40, 3)]);
        // An element that isn't in the run can't renumber it.
        assert!(reorder_run(&sibs, 99, 0).is_empty());
    }

    #[test]
    fn depth_moves_a_multi_selection_and_keeps_its_internal_order() {
        let sibs = [10, 20, 30, 40];
        let out = depth_run(&sibs, &[20, 40], true);
        assert_eq!(out, vec![(10, 0), (30, 1), (20, 2), (40, 3)]);
        let out = depth_run(&sibs, &[20, 40], false);
        assert_eq!(out, vec![(20, 0), (40, 1), (10, 2), (30, 3)]);
    }

    #[test]
    fn align_uses_selection_bounds_for_many_and_the_container_for_one() {
        let r = rects(&[(1, [10.0, 0.0, 40.0, 10.0]), (2, [100.0, 0.0, 20.0, 10.0])]);
        let container = |_: u32| [0.0, 0.0, 1280.0, 720.0];
        // Two selected → align to their own bounds (left = 10).
        let m = align_moves(&[1, 2], &r, &container, Align::Left);
        assert_eq!(m, vec![(2, [-90.0, 0.0])]);
        // One selected → centre it in the container.
        let m = align_moves(&[2], &r, &container, Align::CenterX);
        assert_eq!(m.len(), 1);
        assert!((m[0].1[0] - 530.0).abs() < 0.01, "centred in 1280: {:?}", m[0]);
    }

    #[test]
    fn distribute_equalises_gaps_and_holds_the_extremes() {
        let r = rects(&[
            (1, [0.0, 0.0, 100.0, 10.0]),
            (2, [110.0, 0.0, 100.0, 10.0]),
            (3, [600.0, 0.0, 100.0, 10.0]),
        ]);
        let m = distribute_moves(&[1, 2, 3], &r, 0);
        // Span 0..700, 300 used, 2 gaps of 200 → the middle starts at 300.
        assert_eq!(m, vec![(2, [190.0, 0.0])]);
        // Fewer than three has no meaning.
        assert!(distribute_moves(&[1, 2], &r, 0).is_empty());
    }

    #[test]
    fn snapping_prefers_a_guide_over_the_grid() {
        let guides = Guides { x: vec![103.0], y: vec![] };
        let cfg = SnapCfg {
            grid: 8.0,
            guides: Some(&guides),
            lines: [vec![], vec![]],
            radius: 6.0,
        };
        // Dragging to x=100: the grid would say 104, the guide says 103.
        let (d, hits) = snap_delta([0.0, 0.0, 50.0, 20.0], [100.0, 0.0], &cfg);
        assert!((d[0] - 103.0).abs() < 0.01, "guide wins: {d:?}");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].vertical);
    }

    #[test]
    fn snapping_can_catch_by_the_trailing_edge() {
        let cfg = SnapCfg {
            grid: 0.0,
            guides: None,
            lines: [vec![200.0], vec![]],
            radius: 6.0,
        };
        // A 50-wide box dragged so its RIGHT edge lands near 200.
        let (d, _) = snap_delta([0.0, 0.0, 50.0, 20.0], [147.0, 0.0], &cfg);
        assert!((d[0] - 150.0).abs() < 0.01, "right edge snapped: {d:?}");
    }

    #[test]
    fn snapping_off_the_grid_leaves_the_drag_alone() {
        let cfg = SnapCfg { grid: 0.0, guides: None, lines: [vec![], vec![]], radius: 6.0 };
        let (d, hits) = snap_delta([0.0, 0.0, 50.0, 20.0], [37.3, 11.7], &cfg);
        assert_eq!(d, [37.3, 11.7]);
        assert!(hits.is_empty());
    }

    #[test]
    fn appending_a_style_keeps_what_was_already_there() {
        let dir = std::env::temp_dir().join("floptle-ui-style-append-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.uistyle.ron");
        std::fs::write(
            &path,
            "// my comment\n{\n    \"panel\": (base: (fill: \"panel\")),\n}\n",
        )
        .unwrap();
        let style = floptle_ui::Style::default();
        append_style(&path, "button/new", &style).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("// my comment"), "comment survived: {text}");
        assert!(text.contains("\"panel\""), "old entry survived: {text}");
        assert!(text.contains("\"button/new\""), "new entry added: {text}");
        // And the result is still a valid sheet.
        let sheet = floptle_ui::StyleSheet::parse(&text).expect("re-parses");
        assert!(sheet.styles.contains_key("panel"));
        assert!(sheet.styles.contains_key("button/new"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grid_step_comes_from_the_projects_own_spacing_scale() {
        let mut tokens = floptle_ui::Tokens::default();
        let st = UiDesignState::default();
        // No tokens: fall back to something usable rather than 0 or infinity.
        assert!(st.grid_step(&tokens) > 0.0 && st.grid_step(&tokens).is_finite());
        tokens.spacing.insert("xs".into(), 5.0);
        tokens.spacing.insert("md".into(), 20.0);
        assert_eq!(st.grid_step(&tokens), 5.0);
        // An explicit override wins.
        let st = UiDesignState { snap_grid: 12.0, ..Default::default() };
        assert_eq!(st.grid_step(&tokens), 12.0);
    }
}
