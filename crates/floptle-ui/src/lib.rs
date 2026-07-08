//! # floptle-ui — the game-facing UI system (docs/ui-system-proposal.md)
//!
//! NOT the editor UI (that's egui). This crate is the renderer-agnostic core:
//! the element vocabulary (shapes, images, text — no premade widgets, no
//! imposed look), the layout solver (Free placement by default, Pin presets,
//! opt-in Stack flow), and the draw-list builder the GPU pass consumes.
//!
//! Design split, on purpose:
//! - **Layout runs on the CPU** — it's a few hundred adds per dirty layer, and
//!   its outputs (solved rects) must be readable by picking and scripts.
//! - **Everything visual is GPU-instanced** — this crate emits a [`DrawList`]
//!   (rounded-rect quads + text runs) that `floptle-render`'s UI pass draws in
//!   one instanced call per texture run. UI cost scales with *changes*, not
//!   element count.
//!
//! Coordinates: a layer works in *design units* — the layer scales uniformly so
//! [`UiLayer::design_height`] units always span the window's height. The solver
//! outputs rects in design units; the renderer applies the scale.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// The element vocabulary
// ---------------------------------------------------------------------------

/// One axis of an element's size.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[derive(Default)]
pub enum Size {
    /// Exactly this many design units.
    Fixed(f32),
    /// This fraction of the parent's inner size (0.5 = half).
    Pct(f32),
    /// Wrap the content: a stack's children, or the text's measured size.
    /// No content = 0 (give a bare panel a real size).
    #[default]
    Fit,
    /// Inside a Stack only: share the leftover main-axis space by weight.
    /// (Elsewhere it behaves like `Fit`.)
    Grow(f32),
}


/// The 9-point pin grid — element and parent share the anchor point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anchor {
    #[default]
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Anchor {
    /// (x, y) factors in 0..=1 — where on a rect this anchor sits.
    pub fn factors(self) -> [f32; 2] {
        match self {
            Anchor::TopLeft => [0.0, 0.0],
            Anchor::Top => [0.5, 0.0],
            Anchor::TopRight => [1.0, 0.0],
            Anchor::Left => [0.0, 0.5],
            Anchor::Center => [0.5, 0.5],
            Anchor::Right => [1.0, 0.5],
            Anchor::BottomLeft => [0.0, 1.0],
            Anchor::Bottom => [0.5, 1.0],
            Anchor::BottomRight => [1.0, 1.0],
        }
    }
}

/// How an element is placed in its parent — ignored when the parent is a
/// Stack (the stack places its children itself).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Place {
    /// Where you put it: offset from the parent's top-left, in design units.
    /// THE DEFAULT — the designer stays in charge.
    Free { pos: [f32; 2] },
    /// Stick to a parent edge/corner: the same 9-point of the element sits at
    /// the parent's point, plus an offset. HUD corners that follow the window.
    Pin { anchor: Anchor, offset: [f32; 2] },
}

impl Default for Place {
    fn default() -> Self {
        Place::Free { pos: [0.0, 0.0] }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Row,
    #[default]
    Column,
}

/// Cross-axis alignment of a stack's children (and horizontal text align).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    /// Children stretch to fill the cross axis (stack children only).
    Stretch,
}

/// Main-axis distribution of a stack's children.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

/// Opt-in flow: put this on a container and its children arrange themselves.
/// A convenience for lists/grids/button columns — never forced (Free placement
/// is the default everywhere else).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StackCfg {
    pub dir: Dir,
    /// Design units between children.
    pub gap: f32,
    /// Inner padding on all four sides.
    pub pad: f32,
    pub align: Align,
    pub justify: Justify,
}

impl Default for StackCfg {
    fn default() -> Self {
        StackCfg {
            dir: Dir::Column,
            gap: 8.0,
            pad: 8.0,
            align: Align::Start,
            justify: Justify::Start,
        }
    }
}

/// The visual primitive: a rounded rectangle. Radius 0 = sharp panel, radius
/// ≥ half the short side = pill/circle. Transparency via the fill alpha.
/// The engine ships no UI art — shapes + your textures + text ARE the kit.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeSpec {
    pub fill: [f32; 4],
    pub radius: f32,
    /// Border thickness in design units (0 = none).
    pub border: f32,
    pub border_color: [f32; 4],
}

impl Default for ShapeSpec {
    fn default() -> Self {
        ShapeSpec {
            fill: [1.0, 1.0, 1.0, 1.0],
            radius: 0.0,
            border: 0.0,
            border_color: [0.0, 0.0, 0.0, 1.0],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextSpec {
    pub text: String,
    /// Glyph size in design units (ignored when `fit` is on).
    pub size: f32,
    pub color: [f32; 4],
    /// Horizontal alignment inside the element's rect.
    pub align: Align,
    /// Vertical alignment inside the element's rect (Start = top, End = bottom).
    #[serde(default = "default_center")]
    pub valign: Align,
    /// Dynamic sizing: scale the glyphs so the run fills the element's rect
    /// (largest size that fits both axes). `size` becomes irrelevant.
    #[serde(default)]
    pub fit: bool,
    /// A .ttf/.otf from the project's assets (same relative paths as textures);
    /// empty = the engine's neutral fallback font.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub font: String,
}

fn default_center() -> Align {
    Align::Center
}

impl Default for TextSpec {
    fn default() -> Self {
        TextSpec {
            text: String::new(),
            size: 24.0,
            color: [1.0, 1.0, 1.0, 1.0],
            align: Align::Start,
            valign: Align::Center,
            fit: false,
            font: String::new(),
        }
    }
}

/// Any texture from the project's assets — same paths the Material slot uses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageSpec {
    pub texture: String,
    pub tint: [f32; 4],
}

impl Default for ImageSpec {
    fn default() -> Self {
        ImageSpec { texture: String::new(), tint: [1.0; 4] }
    }
}

/// A value-driven bar/slider (health bars, progress, volume…). The slider node
/// is the TRACK; its child elements marked [`SliderPart::Fill`] scale along
/// `dir` with the value, and [`SliderPart::Handle`] children ride the value's
/// position. The parts are ORDINARY elements — retexture, recolor, move, and
/// resize them freely; the slider only drives the value axis and respects your
/// offsets on it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SliderSpec {
    pub min: f32,
    pub max: f32,
    pub value: f32,
    /// Which axis the value runs along: Row = horizontal, Column = vertical.
    #[serde(default = "default_row")]
    pub dir: Dir,
    /// Handle rides from the far end (right/bottom) back toward the start —
    /// for meters that drain toward the origin. (A fill's direction is set by
    /// how you anchor it: pin it Right/Bottom and it empties that way.)
    #[serde(default)]
    pub flip: bool,
    /// Player-draggable: clicking/dragging the track sets the value from the
    /// pointer (settings sliders). Off for display-only meters (health bars).
    #[serde(default)]
    pub interact: bool,
}

fn default_row() -> Dir {
    Dir::Row
}

impl Default for SliderSpec {
    fn default() -> Self {
        SliderSpec { min: 0.0, max: 100.0, value: 65.0, dir: Dir::Row, flip: false, interact: false }
    }
}

impl SliderSpec {
    /// The value as a 0..=1 fraction of the range (0 when the range is empty).
    pub fn t(&self) -> f32 {
        let span = self.max - self.min;
        if span.abs() < f32::EPSILON {
            0.0
        } else {
            ((self.value - self.min) / span).clamp(0.0, 1.0)
        }
    }
}

/// What a child element does under a slider parent (nothing elsewhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SliderPart {
    /// Scales along the slider's axis with the value. Its authored size is the
    /// FULL-value size; anchoring picks the direction it empties toward.
    Fill,
    /// Its center rides the value's position along the slider's axis (the
    /// authored position on that axis becomes an extra offset). The cross axis
    /// stays fully yours.
    Handle,
}

/// Clip other elements to this element's rounded rect. Targets are node names
/// (any elements in the same layer); each target's WHOLE subtree clips. If two
/// masks claim the same element, the mask earliest in scene order wins.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MaskSpec {
    pub targets: Vec<String>,
}

/// A UI element — the ONE node kind. What it looks like is whichever visual
/// specs are present (shape, then image, then text — that's the draw order);
/// how it sits is `place` + `size`; whether it arranges children is `stack`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ElementSpec {
    #[serde(default)]
    pub place: Place,
    #[serde(default)]
    pub size: [Size; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<StackCfg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
    /// Value-driven bar: this element is a track whose Fill/Handle children
    /// follow `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slider: Option<SliderSpec>,
    /// Role under a slider parent (Fill scales, Handle rides the value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<SliderPart>,
    /// Clip the named target elements (+ subtrees) to this element's rect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<MaskSpec>,
    /// Clickable: the pointer can hover/press/click this element, firing the
    /// script hooks (`hoverStart`/`hoverEnd`/`pressed`/`released`/`clicked`)
    /// on this node's scripts. No imposed look — style the states in Lua.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub button: bool,
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Multiplies every color this element draws (self only, v1).
    #[serde(default = "default_one")]
    pub opacity: f32,
}

fn default_true() -> bool {
    true
}
fn default_one() -> f32 {
    1.0
}

impl Default for ElementSpec {
    fn default() -> Self {
        ElementSpec {
            place: Place::default(),
            size: [Size::Fit, Size::Fit],
            stack: None,
            shape: None,
            text: None,
            image: None,
            slider: None,
            part: None,
            mask: None,
            button: false,
            visible: true,
            opacity: 1.0,
        }
    }
}

/// A UI layer root (screen-space canvas). Lives on a scene node; its element
/// children form the tree. The layer scales uniformly so `design_height`
/// design units always span the window height (resolution independence).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiLayer {
    /// Resolution independence: this many design units ALWAYS span the window
    /// height, at any resolution — bigger number = smaller-looking UI. The
    /// width follows the window's aspect.
    pub design_height: f32,
    /// Layers draw lowest-z first.
    pub z: i32,
    /// Master switch: an off layer draws nothing (in-game and in-editor).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Scene-view authoring only: world units per design unit for the canvas
    /// hologram (gameplay always fills the screen regardless). 0.01 → a
    /// 720-design layer stands 7.2 world units tall.
    #[serde(default = "default_canvas_scale")]
    pub canvas_scale: f32,
}

fn default_canvas_scale() -> f32 {
    0.01
}

impl Default for UiLayer {
    fn default() -> Self {
        UiLayer { design_height: 720.0, z: 0, enabled: true, canvas_scale: 0.01 }
    }
}

// ---------------------------------------------------------------------------
// The tree + solver
// ---------------------------------------------------------------------------

/// The solver's input tree: element ids are scene-entity indices, so solved
/// rects map straight back to nodes (picking, scripts).
#[derive(Clone, Debug)]
pub struct Node {
    pub id: u32,
    pub spec: ElementSpec,
    pub children: Vec<Node>,
}

/// A solved element: its rect in layer design units, `[x, y, w, h]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub id: u32,
    pub rect: [f32; 4],
}

/// Text measurement, provided by whoever owns the font (the renderer; tests
/// stub it): returns [width, height] of the run at [`TextSpec::size`].
pub type MeasureText<'a> = &'a dyn Fn(&TextSpec) -> [f32; 2];

/// Solve a layer: place every visible element of `roots` inside a viewport of
/// `viewport` design units. Output order is parent-before-children (painter's
/// order — the draw list reuses it directly).
pub fn solve(roots: &[Node], viewport: [f32; 2], measure: MeasureText) -> Vec<Placed> {
    let mut out = Vec::new();
    for n in roots {
        if !n.spec.visible {
            continue;
        }
        let size = measure_node(n, viewport, measure);
        let pos = place_in(&n.spec, size, [0.0, 0.0], viewport);
        layout_node(n, [pos[0], pos[1], size[0], size[1]], measure, &mut out);
    }
    out
}

/// An element's own size (before Grow expansion), given the parent's inner
/// size. `Fit` recurses into content.
fn measure_node(n: &Node, avail: [f32; 2], measure: MeasureText) -> [f32; 2] {
    let needs_fit = n
        .spec
        .size
        .iter()
        .any(|s| matches!(s, Size::Fit | Size::Grow(_)));
    let fit = if needs_fit { fit_size(n, avail, measure) } else { [0.0, 0.0] };
    let mut size = [0.0f32; 2];
    for a in 0..2 {
        size[a] = match n.spec.size[a] {
            Size::Fixed(v) => v.max(0.0),
            Size::Pct(p) => (avail[a] * p).max(0.0),
            Size::Fit | Size::Grow(_) => fit[a],
        };
    }
    size
}

/// Content size for `Fit`: text measurement, or the stacked/overlaid children.
fn fit_size(n: &Node, avail: [f32; 2], measure: MeasureText) -> [f32; 2] {
    if let Some(t) = &n.spec.text {
        return measure(t);
    }
    let visible: Vec<&Node> = n.children.iter().filter(|c| c.spec.visible).collect();
    if visible.is_empty() {
        return [0.0, 0.0];
    }
    if let Some(s) = n.spec.stack {
        let (main, cross) = axes(s.dir);
        let mut total_main = s.pad * 2.0 + s.gap * (visible.len().saturating_sub(1)) as f32;
        let mut max_cross = 0.0f32;
        let inner = [(avail[0] - s.pad * 2.0).max(0.0), (avail[1] - s.pad * 2.0).max(0.0)];
        for c in &visible {
            let cs = measure_node(c, inner, measure);
            total_main += cs[main];
            max_cross = max_cross.max(cs[cross]);
        }
        let mut out = [0.0; 2];
        out[main] = total_main;
        out[cross] = max_cross + s.pad * 2.0;
        out
    } else {
        // Free children: fit their placements' bounding box.
        let mut max = [0.0f32; 2];
        for c in &visible {
            let cs = measure_node(c, avail, measure);
            if let Place::Free { pos } = c.spec.place {
                max[0] = max[0].max(pos[0] + cs[0]);
                max[1] = max[1].max(pos[1] + cs[1]);
            } else {
                max[0] = max[0].max(cs[0]);
                max[1] = max[1].max(cs[1]);
            }
        }
        max
    }
}

fn axes(dir: Dir) -> (usize, usize) {
    match dir {
        Dir::Row => (0, 1),
        Dir::Column => (1, 0),
    }
}

/// Where a Free/Pin element's top-left lands inside a parent rect.
fn place_in(
    spec: &ElementSpec,
    size: [f32; 2],
    parent_pos: [f32; 2],
    parent_size: [f32; 2],
) -> [f32; 2] {
    match spec.place {
        Place::Free { pos } => [parent_pos[0] + pos[0], parent_pos[1] + pos[1]],
        Place::Pin { anchor, offset } => {
            let f = anchor.factors();
            [
                parent_pos[0] + parent_size[0] * f[0] - size[0] * f[0] + offset[0],
                parent_pos[1] + parent_size[1] * f[1] - size[1] * f[1] + offset[1],
            ]
        }
    }
}

/// Recursively place `n`'s children inside its solved rect.
fn layout_node(n: &Node, rect: [f32; 4], measure: MeasureText, out: &mut Vec<Placed>) {
    out.push(Placed { id: n.id, rect });
    let visible: Vec<&Node> = n.children.iter().filter(|c| c.spec.visible).collect();
    if visible.is_empty() {
        return;
    }
    let (px, py, pw, ph) = (rect[0], rect[1], rect[2], rect[3]);
    if let Some(s) = n.spec.stack {
        let (main, cross) = axes(s.dir);
        let inner_pos = [px + s.pad, py + s.pad];
        let inner_size = [(pw - s.pad * 2.0).max(0.0), (ph - s.pad * 2.0).max(0.0)];
        let inner = [inner_size[main], inner_size[cross]];
        // Measure everyone, find grow weights + used main space.
        let mut sizes: Vec<[f32; 2]> = Vec::with_capacity(visible.len());
        let mut grow_total = 0.0f32;
        let mut used = s.gap * (visible.len().saturating_sub(1)) as f32;
        for c in &visible {
            let mut cs = measure_node(c, inner_size, measure);
            if let Size::Grow(w) = c.spec.size[main] {
                grow_total += w.max(0.0);
                cs[main] = 0.0;
            }
            if matches!(c.spec.size[cross], Size::Grow(_)) || s.align == Align::Stretch {
                cs[cross] = inner[1];
            }
            used += cs[main];
            sizes.push(cs);
        }
        // Grow shares the leftover; justify distributes what remains after.
        let leftover = (inner[0] - used).max(0.0);
        if grow_total > 0.0 {
            for (c, cs) in visible.iter().zip(sizes.iter_mut()) {
                if let Size::Grow(w) = c.spec.size[main] {
                    cs[main] = leftover * (w.max(0.0) / grow_total);
                }
            }
        }
        let free = if grow_total > 0.0 { 0.0 } else { leftover };
        let (mut cursor, extra_gap) = match s.justify {
            Justify::Start => (0.0, 0.0),
            Justify::Center => (free * 0.5, 0.0),
            Justify::End => (free, 0.0),
            Justify::SpaceBetween => {
                (0.0, if visible.len() > 1 { free / (visible.len() - 1) as f32 } else { 0.0 })
            }
        };
        for (c, cs) in visible.iter().zip(sizes.iter()) {
            let cross_off = match s.align {
                Align::Start | Align::Stretch => 0.0,
                Align::Center => (inner[1] - cs[cross]) * 0.5,
                Align::End => inner[1] - cs[cross],
            };
            let mut pos = [0.0f32; 2];
            pos[main] = inner_pos[main] + cursor;
            pos[cross] = inner_pos[cross] + cross_off;
            layout_node(c, [pos[0], pos[1], cs[0], cs[1]], measure, out);
            cursor += cs[main] + s.gap + extra_gap;
        }
    } else {
        for c in visible {
            let mut cs = measure_node(c, [pw, ph], measure);
            // A slider parent drives its Fill/Handle parts along its axis —
            // everything else about the part (cross axis, anchoring, offsets)
            // stays exactly as the designer authored it.
            let drive = n.spec.slider.zip(c.spec.part);
            if let Some((s, SliderPart::Fill)) = drive {
                let (axis, _) = axes(s.dir);
                cs[axis] *= s.t();
            }
            let mut pos = place_in(&c.spec, cs, [px, py], [pw, ph]);
            if let Some((s, SliderPart::Handle)) = drive {
                let (axis, _) = axes(s.dir);
                let t = if s.flip { 1.0 - s.t() } else { s.t() };
                let authored = match c.spec.place {
                    Place::Free { pos } => pos[axis],
                    Place::Pin { offset, .. } => offset[axis],
                };
                let parent = [px, py];
                let extent = [pw, ph];
                pos[axis] = parent[axis] + extent[axis] * t - cs[axis] * 0.5 + authored;
            }
            layout_node(c, [pos[0], pos[1], cs[0], cs[1]], measure, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Draw list
// ---------------------------------------------------------------------------

/// A mask's clip region: pixels outside this rounded rect are discarded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clip {
    /// x, y, w, h in design units.
    pub rect: [f32; 4],
    pub radius: f32,
}

/// One rounded-rect quad, in design units (the renderer scales).
#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    pub rect: [f32; 4],
    pub color: [f32; 4],
    pub radius: f32,
    pub border: f32,
    pub border_color: [f32; 4],
    /// Texture asset path (empty = solid fill).
    pub texture: String,
    /// Set when a mask claims this element.
    pub clip: Option<Clip>,
}

/// One text run (the renderer owns the font and lays out glyphs).
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    /// The element's rect — alignment happens inside it.
    pub rect: [f32; 4],
    pub text: String,
    pub size: f32,
    pub color: [f32; 4],
    pub align: Align,
    /// Vertical alignment (Start = top, Center, End = bottom).
    pub valign: Align,
    /// Scale glyphs to fill the rect instead of using `size`.
    pub fit: bool,
    /// Project font asset path (empty = fallback font).
    pub font: String,
    /// Set when a mask claims this element.
    pub clip: Option<Clip>,
}

/// Everything a layer draws this frame, painter's order.
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub quads: Vec<Quad>,
    pub texts: Vec<TextRun>,
}

/// Build the draw list for solved elements. `roots`/`placed` must come from
/// the same [`solve`] call (painter's order is reused).
///
/// `masks` is `(mask element id, target element id)` pairs: the target and its
/// whole subtree clip to the mask's solved rect (+ the mask's shape radius).
/// When several masks claim the same element, the FIRST pair wins — build the
/// list in scene order and the rule is "earliest mask in the scene wins". A
/// mask that wasn't placed this frame (hidden) clips nothing.
pub fn draw_list(roots: &[Node], placed: &[Placed], masks: &[(u32, u32)]) -> DrawList {
    fn collect<'a>(n: &'a Node, m: &mut std::collections::HashMap<u32, &'a Node>) {
        m.insert(n.id, n);
        for c in &n.children {
            collect(c, m);
        }
    }
    let mut nodes = std::collections::HashMap::new();
    for r in roots {
        collect(r, &mut nodes);
    }
    let rects: std::collections::HashMap<u32, [f32; 4]> =
        placed.iter().map(|p| (p.id, p.rect)).collect();
    // Resolve masks: element id → the clip it draws under (first claim wins).
    let mut clip_of: std::collections::HashMap<u32, Clip> = std::collections::HashMap::new();
    for (mask_id, target_id) in masks {
        let Some(&rect) = rects.get(mask_id) else { continue };
        let radius = nodes
            .get(mask_id)
            .and_then(|n| n.spec.shape)
            .map(|s| s.radius)
            .unwrap_or(0.0);
        let clip = Clip { rect, radius };
        let mut stack = vec![*target_id];
        while let Some(id) = stack.pop() {
            clip_of.entry(id).or_insert(clip);
            if let Some(n) = nodes.get(&id) {
                stack.extend(n.children.iter().map(|c| c.id));
            }
        }
    }
    let mut dl = DrawList::default();
    for p in placed {
        let Some(node) = nodes.get(&p.id) else { continue };
        let spec = &node.spec;
        let clip = clip_of.get(&p.id).copied();
        let a = spec.opacity.clamp(0.0, 1.0);
        if let Some(s) = spec.shape {
            let mut fill = s.fill;
            fill[3] *= a;
            let mut bc = s.border_color;
            bc[3] *= a;
            dl.quads.push(Quad {
                rect: p.rect,
                color: fill,
                radius: s.radius,
                border: s.border,
                border_color: bc,
                texture: String::new(),
                clip,
            });
        }
        if let Some(img) = &spec.image
            && !img.texture.is_empty()
        {
            let mut tint = img.tint;
            tint[3] *= a;
            dl.quads.push(Quad {
                rect: p.rect,
                color: tint,
                radius: spec.shape.map(|s| s.radius).unwrap_or(0.0),
                border: 0.0,
                border_color: [0.0; 4],
                texture: img.texture.clone(),
                clip,
            });
        }
        if let Some(t) = &spec.text
            && !t.text.is_empty()
        {
            let mut color = t.color;
            color[3] *= a;
            dl.texts.push(TextRun {
                rect: p.rect,
                text: t.text.clone(),
                size: t.size,
                color,
                align: t.align,
                valign: t.valign,
                fit: t.fit,
                font: t.font.clone(),
                clip,
            });
        }
    }
    dl
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic test metrics: 0.6·size per char wide, size tall.
    fn m(t: &TextSpec) -> [f32; 2] {
        [t.text.chars().count() as f32 * t.size * 0.6, t.size]
    }

    fn el(spec: ElementSpec, children: Vec<Node>) -> Node {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        Node { id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), spec, children }
    }

    fn rect_of(placed: &[Placed], id: u32) -> [f32; 4] {
        placed.iter().find(|p| p.id == id).unwrap().rect
    }

    #[test]
    fn free_placement_is_exactly_where_you_put_it() {
        let n = el(
            ElementSpec {
                place: Place::Free { pos: [40.0, 60.0] },
                size: [Size::Fixed(200.0), Size::Fixed(100.0)],
                shape: Some(ShapeSpec::default()),
                ..Default::default()
            },
            vec![],
        );
        let placed = solve(std::slice::from_ref(&n), [1280.0, 720.0], &m);
        assert_eq!(rect_of(&placed, n.id), [40.0, 60.0, 200.0, 100.0]);
    }

    #[test]
    fn pin_bottom_right_hugs_the_corner_at_any_viewport() {
        let n = el(
            ElementSpec {
                place: Place::Pin { anchor: Anchor::BottomRight, offset: [-10.0, -10.0] },
                size: [Size::Fixed(100.0), Size::Fixed(50.0)],
                ..Default::default()
            },
            vec![],
        );
        for vp in [[1280.0f32, 720.0f32], [2560.0, 1440.0]] {
            let placed = solve(std::slice::from_ref(&n), vp, &m);
            let r = rect_of(&placed, n.id);
            assert_eq!([r[0] + r[2], r[1] + r[3]], [vp[0] - 10.0, vp[1] - 10.0]);
        }
    }

    #[test]
    fn pct_sizes_follow_the_parent() {
        let child = el(
            ElementSpec { size: [Size::Pct(0.5), Size::Pct(0.25)], ..Default::default() },
            vec![],
        );
        let cid = child.id;
        let parent = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(400.0), Size::Fixed(400.0)],
                ..Default::default()
            },
            vec![child],
        );
        let placed = solve(&[parent], [1280.0, 720.0], &m);
        let r = rect_of(&placed, cid);
        assert_eq!([r[2], r[3]], [200.0, 100.0]);
    }

    #[test]
    fn column_stack_flows_with_gap_pad_and_center() {
        let a = el(
            ElementSpec { size: [Size::Fixed(100.0), Size::Fixed(30.0)], ..Default::default() },
            vec![],
        );
        let b = el(
            ElementSpec { size: [Size::Fixed(60.0), Size::Fixed(30.0)], ..Default::default() },
            vec![],
        );
        let (ida, idb) = (a.id, b.id);
        let stack = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(200.0), Size::Fit],
                stack: Some(StackCfg {
                    dir: Dir::Column,
                    gap: 10.0,
                    pad: 5.0,
                    align: Align::Center,
                    justify: Justify::Start,
                }),
                ..Default::default()
            },
            vec![a, b],
        );
        let sid = stack.id;
        let placed = solve(&[stack], [1280.0, 720.0], &m);
        // Fit height: 5 + 30 + 10 + 30 + 5 = 80.
        assert_eq!(rect_of(&placed, sid)[3], 80.0);
        let ra = rect_of(&placed, ida);
        let rb = rect_of(&placed, idb);
        assert_eq!([ra[1], rb[1]], [5.0, 45.0], "flow: pad, then gap");
        // Center align on a 190-wide inner: (190-100)/2+5 and (190-60)/2+5.
        assert_eq!([ra[0], rb[0]], [50.0, 70.0]);
    }

    #[test]
    fn grow_shares_leftover_space_by_weight() {
        let fixed = el(
            ElementSpec { size: [Size::Fixed(100.0), Size::Fixed(20.0)], ..Default::default() },
            vec![],
        );
        let g1 = el(
            ElementSpec { size: [Size::Grow(1.0), Size::Fixed(20.0)], ..Default::default() },
            vec![],
        );
        let g2 = el(
            ElementSpec { size: [Size::Grow(3.0), Size::Fixed(20.0)], ..Default::default() },
            vec![],
        );
        let (i1, i2) = (g1.id, g2.id);
        let row = el(
            ElementSpec {
                size: [Size::Fixed(500.0), Size::Fixed(40.0)],
                stack: Some(StackCfg {
                    dir: Dir::Row,
                    gap: 0.0,
                    pad: 0.0,
                    align: Align::Start,
                    justify: Justify::Start,
                }),
                ..Default::default()
            },
            vec![fixed, g1, g2],
        );
        let placed = solve(&[row], [1280.0, 720.0], &m);
        // 400 leftover split 1:3.
        assert_eq!(rect_of(&placed, i1)[2], 100.0);
        assert_eq!(rect_of(&placed, i2)[2], 300.0);
    }

    #[test]
    fn space_between_pushes_children_apart() {
        let a = el(
            ElementSpec { size: [Size::Fixed(50.0), Size::Fixed(20.0)], ..Default::default() },
            vec![],
        );
        let b = el(
            ElementSpec { size: [Size::Fixed(50.0), Size::Fixed(20.0)], ..Default::default() },
            vec![],
        );
        let (ida, idb) = (a.id, b.id);
        let row = el(
            ElementSpec {
                size: [Size::Fixed(300.0), Size::Fixed(20.0)],
                stack: Some(StackCfg {
                    dir: Dir::Row,
                    gap: 0.0,
                    pad: 0.0,
                    align: Align::Start,
                    justify: Justify::SpaceBetween,
                }),
                ..Default::default()
            },
            vec![a, b],
        );
        let placed = solve(&[row], [1280.0, 720.0], &m);
        assert_eq!(rect_of(&placed, ida)[0], 0.0);
        assert_eq!(rect_of(&placed, idb)[0], 250.0, "second child hugs the far edge");
    }

    #[test]
    fn text_fit_uses_the_measure_callback() {
        let label = el(
            ElementSpec {
                text: Some(TextSpec { text: "HELLO".into(), size: 20.0, ..Default::default() }),
                ..Default::default()
            },
            vec![],
        );
        let id = label.id;
        let placed = solve(&[label], [1280.0, 720.0], &m);
        let r = rect_of(&placed, id);
        // 5 chars · 0.6 · 20 (allow float noise).
        assert!((r[2] - 60.0).abs() < 1e-3 && (r[3] - 20.0).abs() < 1e-3, "got {r:?}");
    }

    #[test]
    fn invisible_elements_and_their_subtrees_vanish() {
        let child = el(
            ElementSpec { size: [Size::Fixed(10.0), Size::Fixed(10.0)], ..Default::default() },
            vec![],
        );
        let cid = child.id;
        let hidden = el(
            ElementSpec {
                visible: false,
                size: [Size::Fixed(100.0), Size::Fixed(100.0)],
                ..Default::default()
            },
            vec![child],
        );
        let hid = hidden.id;
        let placed = solve(&[hidden], [1280.0, 720.0], &m);
        assert!(placed.iter().all(|p| p.id != hid && p.id != cid));
    }

    #[test]
    fn draw_list_paints_shape_then_image_then_text_with_opacity() {
        let n = el(
            ElementSpec {
                size: [Size::Fixed(100.0), Size::Fixed(40.0)],
                shape: Some(ShapeSpec { fill: [1.0, 0.0, 0.0, 0.8], ..Default::default() }),
                image: Some(ImageSpec { texture: "textures/Grass.png".into(), tint: [1.0; 4] }),
                text: Some(TextSpec { text: "hi".into(), ..Default::default() }),
                opacity: 0.5,
                ..Default::default()
            },
            vec![],
        );
        let placed = solve(std::slice::from_ref(&n), [1280.0, 720.0], &m);
        let dl = draw_list(&[n], &placed, &[]);
        assert_eq!(dl.quads.len(), 2, "shape + image");
        assert!((dl.quads[0].color[3] - 0.4).abs() < 1e-6, "opacity multiplies fill alpha");
        assert_eq!(dl.quads[1].texture, "textures/Grass.png");
        assert_eq!(dl.texts.len(), 1);
        assert!((dl.texts[0].color[3] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn slider_fill_scales_and_pin_right_empties_leftward() {
        let fill = el(
            ElementSpec {
                part: Some(SliderPart::Fill),
                place: Place::Pin { anchor: Anchor::Right, offset: [0.0, 0.0] },
                size: [Size::Pct(1.0), Size::Pct(1.0)],
                ..Default::default()
            },
            vec![],
        );
        let fid = fill.id;
        let track = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(400.0), Size::Fixed(40.0)],
                slider: Some(SliderSpec { min: 0.0, max: 100.0, value: 25.0, ..Default::default() }),
                ..Default::default()
            },
            vec![fill],
        );
        let placed = solve(&[track], [1280.0, 720.0], &m);
        let r = rect_of(&placed, fid);
        assert_eq!(r[2], 100.0, "quarter value = quarter width");
        assert_eq!(r[0] + r[2], 400.0, "pinned Right: the fill empties leftward");
        assert_eq!(r[3], 40.0, "cross axis untouched");
    }

    #[test]
    fn slider_handle_rides_the_value_and_flip_reverses_it() {
        for (flip, expected_center) in [(false, 300.0), (true, 100.0)] {
            let handle = el(
                ElementSpec {
                    part: Some(SliderPart::Handle),
                    place: Place::Pin { anchor: Anchor::Left, offset: [0.0, 0.0] },
                    size: [Size::Fixed(20.0), Size::Fixed(20.0)],
                    ..Default::default()
                },
                vec![],
            );
            let hid = handle.id;
            let track = el(
                ElementSpec {
                    place: Place::Free { pos: [0.0, 0.0] },
                    size: [Size::Fixed(400.0), Size::Fixed(40.0)],
                    slider: Some(SliderSpec {
                        min: 0.0,
                        max: 1.0,
                        value: 0.75,
                        dir: Dir::Row,
                        flip,
                        interact: false,
                    }),
                    ..Default::default()
                },
                vec![handle],
            );
            let placed = solve(&[track], [1280.0, 720.0], &m);
            let r = rect_of(&placed, hid);
            assert_eq!(r[0] + r[2] * 0.5, expected_center, "flip={flip}");
            assert_eq!(r[1], 10.0, "cross axis: Pin Left centers vertically");
        }
    }

    #[test]
    fn empty_slider_range_reads_as_zero() {
        let s = SliderSpec { min: 5.0, max: 5.0, value: 5.0, ..Default::default() };
        assert_eq!(s.t(), 0.0);
    }

    #[test]
    fn mask_clips_target_subtree_and_first_mask_wins() {
        let inner = el(
            ElementSpec { size: [Size::Fixed(10.0), Size::Fixed(10.0)],
                shape: Some(ShapeSpec::default()), ..Default::default() },
            vec![],
        );
        let iid = inner.id;
        let target = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(50.0), Size::Fixed(50.0)],
                shape: Some(ShapeSpec::default()),
                ..Default::default()
            },
            vec![inner],
        );
        let tid = target.id;
        let mask_a = el(
            ElementSpec {
                place: Place::Free { pos: [100.0, 0.0] },
                size: [Size::Fixed(80.0), Size::Fixed(80.0)],
                shape: Some(ShapeSpec { radius: 12.0, ..Default::default() }),
                mask: Some(MaskSpec { targets: vec!["t".into()] }),
                ..Default::default()
            },
            vec![],
        );
        let aid = mask_a.id;
        let mask_b = el(
            ElementSpec {
                place: Place::Free { pos: [300.0, 0.0] },
                size: [Size::Fixed(9.0), Size::Fixed(9.0)],
                mask: Some(MaskSpec { targets: vec!["t".into()] }),
                ..Default::default()
            },
            vec![],
        );
        let bid = mask_b.id;
        let roots = vec![target, mask_a, mask_b];
        let placed = solve(&roots, [1280.0, 720.0], &m);
        // Both masks claim the target; A comes first in the pair list.
        let dl = draw_list(&roots, &placed, &[(aid, tid), (bid, tid)]);
        let clip = Clip { rect: [100.0, 0.0, 80.0, 80.0], radius: 12.0 };
        let target_quad = dl.quads.iter().find(|q| q.rect == [0.0, 0.0, 50.0, 50.0]).unwrap();
        assert_eq!(target_quad.clip, Some(clip), "first mask wins");
        let inner_quad = dl.quads.iter().find(|q| q.rect[2] == 10.0).unwrap();
        assert_eq!(inner_quad.clip, Some(clip), "the whole subtree clips");
        let _ = (iid, tid);
        // The masks themselves aren't clipped.
        assert!(dl
            .quads
            .iter()
            .filter(|q| q.rect[0] >= 100.0)
            .all(|q| q.clip.is_none()));
    }

    #[test]
    fn text_valign_and_fit_reach_the_draw_list() {
        let n = el(
            ElementSpec {
                size: [Size::Fixed(200.0), Size::Fixed(80.0)],
                text: Some(TextSpec {
                    text: "hp".into(),
                    valign: Align::End,
                    fit: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            vec![],
        );
        let placed = solve(std::slice::from_ref(&n), [1280.0, 720.0], &m);
        let dl = draw_list(&[n], &placed, &[]);
        assert_eq!(dl.texts[0].valign, Align::End);
        assert!(dl.texts[0].fit);
    }

    #[test]
    fn defaults_are_designer_friendly() {
        let spec = ElementSpec::default();
        assert!(spec.visible);
        assert_eq!(spec.opacity, 1.0);
        assert!(matches!(spec.place, Place::Free { .. }), "free placement is the default");
        assert!(spec.stack.is_none(), "flow is opt-in");
    }
}
