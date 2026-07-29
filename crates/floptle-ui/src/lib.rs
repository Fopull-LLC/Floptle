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

pub mod field;
pub mod nav;
pub mod paint;
pub mod style;
pub mod text;

pub use paint::{
    Blend, Corners, GlowSpec, Gradient, GradientKind, GrainSpec, ImageFit, ShadowSpec, Sides,
};
pub use style::{
    apply_styles, ColorRef, Ease, NumRef, StateInput, Style, StyleBlock, StyleRuntime, StyleSheet,
    Tokens, Transition, UiState,
};
pub use nav::Dir4;
pub use text::{Case, Overflow, TextShadow, TextStroke};

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
    /// Two-point anchor rect — the responsive placement. `min`/`max` are
    /// fractions (0..1) of the parent rect; the element anchors to the box
    /// between them, then insets by `margin` = `[left, top, right, bottom]`
    /// design units.
    ///
    /// On an axis where `max > min` the element STRETCHES to fill that span
    /// (its own `size` on that axis is ignored) — e.g. `min:(0,0) max:(1,1)
    /// margin:(16,16,16,16)` is "16 units in from all four edges, at any
    /// window size". On an axis where `min == max` it's a point anchor: the
    /// element keeps its `size` and its top-left sits on that line + the
    /// leading margin. This is how a panel grows with the screen instead of
    /// staying a fixed box.
    Stretch { min: [f32; 2], max: [f32; 2], margin: [f32; 4] },
}

impl Default for Place {
    fn default() -> Self {
        Place::Free { pos: [0.0, 0.0] }
    }
}

impl Place {
    /// A full-parent stretch inset by `m` design units on every side — the
    /// common "fill my parent with a margin" placement.
    pub fn fill(m: f32) -> Self {
        Place::Stretch { min: [0.0, 0.0], max: [1.0, 1.0], margin: [m, m, m, m] }
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
///
/// Everything past `fill`/`radius`/`border` is optional and defaults to off, so
/// a shape authored against the first cut of the UI system looks identical.
/// What the extras buy, in the order they matter: `gradient` (a surface stops
/// reading as a slab), `radius` per corner (headers, tabs, cards), `border` per
/// side (rules and accent bars stop costing an extra node), `grain` (the
/// cheapest cure for plastic-looking UI), and `glow`/inset `shadow` (light and
/// recession).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeSpec {
    /// The flat fill — and, when `gradient` is set, its near colour.
    pub fill: [f32; 4],
    /// Optional two-stop gradient running from `fill` to [`Gradient::to`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gradient: Option<Gradient>,
    /// Corner radii `[TL, TR, BR, BL]`. Accepts a bare number for all four.
    #[serde(default)]
    pub radius: Corners,
    /// Border thickness `[L, T, R, B]` in design units. Accepts a bare number.
    #[serde(default)]
    pub border: Sides,
    pub border_color: [f32; 4],
    /// Optional soft shadow — behind the shape, or inside it (`inset`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<ShadowSpec>,
    /// Optional outer bloom, drawn behind the shadow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glow: Option<GlowSpec>,
    /// Optional per-pixel noise over the fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grain: Option<GrainSpec>,
    /// How this shape composites against what is already drawn.
    #[serde(default, skip_serializing_if = "is_normal_blend")]
    pub blend: Blend,
}

fn is_normal_blend(b: &Blend) -> bool {
    *b == Blend::Normal
}

impl Default for ShapeSpec {
    fn default() -> Self {
        ShapeSpec {
            fill: [1.0, 1.0, 1.0, 1.0],
            gradient: None,
            radius: Corners::all(0.0),
            border: Sides::all(0.0),
            border_color: [0.0, 0.0, 0.0, 1.0],
            shadow: None,
            glow: None,
            grain: None,
            blend: Blend::Normal,
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
    /// Outline around the glyphs — legibility over an arbitrary background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<TextStroke>,
    /// A dropped copy behind the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<TextShadow>,
    /// Extra space between glyphs, in design units. Negative tightens.
    /// Wide tracking on small caps is most of what makes a title look set
    /// rather than typed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tracking: f32,
    /// Line spacing as a multiple of the font's natural line height
    /// (0 = the font's own metrics, which is what the first cut always used).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub line_height: f32,
    /// Wrap to the element's width instead of running past it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wrap: bool,
    /// Cap the rendered line count (0 = unlimited). Pairs with `Ellipsis`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub max_lines: u32,
    /// What happens to text that doesn't fit.
    #[serde(default, skip_serializing_if = "is_show")]
    pub overflow: Overflow,
    /// Case transform applied at draw time; the authored string is untouched.
    #[serde(default, skip_serializing_if = "is_as_is")]
    pub case: Case,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_show(v: &Overflow) -> bool {
    *v == Overflow::Show
}
fn is_as_is(v: &Case) -> bool {
    *v == Case::AsIs
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
            stroke: None,
            shadow: None,
            tracking: 0.0,
            line_height: 0.0,
            wrap: false,
            max_lines: 0,
            overflow: Overflow::Show,
            case: Case::AsIs,
        }
    }
}

impl TextSpec {
    /// The string as it should actually be drawn (case applied).
    pub fn display(&self) -> std::borrow::Cow<'_, str> {
        self.case.apply(&self.text)
    }
}

/// Any texture from the project's assets — same paths the Material slot uses.
/// A texture can be a **spritesheet**: `cols`×`rows` cells, of which the element
/// shows cell index `cell` (row-major). Default 1×1 = the whole image. Animate
/// `cell` with a stepped property track for frame-by-frame sprite animation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageSpec {
    pub texture: String,
    pub tint: [f32; 4],
    /// Spritesheet columns (≥1). 1 with `rows` = whole image.
    #[serde(default = "one_u32")]
    pub cols: u32,
    /// Spritesheet rows (≥1).
    #[serde(default = "one_u32")]
    pub rows: u32,
    /// Which cell to show (row-major, clamped into range).
    #[serde(default)]
    pub cell: u32,
    /// **9-slice** insets `[L, T, R, B]` as a FRACTION of the sampled region
    /// (0 = off). The four corners keep their size, the four edges stretch
    /// along one axis, and the middle stretches both — so one small texture
    /// dresses a panel at any size.
    ///
    /// This is what makes authored panel art usable at all. Without it a
    /// project's own border/frame textures smear when the panel resizes, which
    /// is why both of Ty's projects draw panels with engine rects instead of
    /// their own art.
    #[serde(default, skip_serializing_if = "is_zero4")]
    pub slice: [f32; 4],
    /// Repeat the image across the rect (1 = once). Patterned fills.
    #[serde(default = "one2", skip_serializing_if = "is_one2")]
    pub tiling: [f32; 2],
    /// UV offset in tiles — animate it for a scrolling background.
    #[serde(default, skip_serializing_if = "is_zero2")]
    pub offset: [f32; 2],
    /// How the image fills the rect when its aspect differs.
    #[serde(default, skip_serializing_if = "is_stretch")]
    pub fit: ImageFit,
}

fn one_u32() -> u32 {
    1
}
fn one2() -> [f32; 2] {
    [1.0, 1.0]
}
fn is_one2(v: &[f32; 2]) -> bool {
    v[0] == 1.0 && v[1] == 1.0
}
fn is_zero4(v: &[f32; 4]) -> bool {
    v.iter().all(|x| *x == 0.0)
}
fn is_stretch(f: &ImageFit) -> bool {
    *f == ImageFit::Stretch
}

impl Default for ImageSpec {
    fn default() -> Self {
        ImageSpec {
            texture: String::new(),
            tint: [1.0; 4],
            cols: 1,
            rows: 1,
            cell: 0,
            slice: [0.0; 4],
            tiling: [1.0, 1.0],
            offset: [0.0, 0.0],
            fit: ImageFit::Stretch,
        }
    }
}

impl ImageSpec {
    /// The UV sub-rect `[min_u, min_v, max_u, max_v]` for the current `cell` in
    /// the `cols`×`rows` grid — the whole texture `[0,0,1,1]` when 1×1.
    pub fn cell_uv(&self) -> [f32; 4] {
        let cols = self.cols.max(1);
        let rows = self.rows.max(1);
        let n = cols * rows;
        if n <= 1 {
            return [0.0, 0.0, 1.0, 1.0];
        }
        let cell = self.cell.min(n - 1);
        let (cx, cy) = (cell % cols, cell / cols);
        let (du, dv) = (1.0 / cols as f32, 1.0 / rows as f32);
        [cx as f32 * du, cy as f32 * dv, (cx + 1) as f32 * du, (cy + 1) as f32 * dv]
    }

    /// [`cell_uv`](Self::cell_uv) with `tiling`/`offset` applied — the UV rect
    /// the renderer actually samples. Repeats above 1 rely on a repeating
    /// sampler; `offset` is in tiles, so animating it scrolls the fill.
    ///
    /// Tiling a spritesheet CELL would sample its neighbours, so tiling is
    /// ignored on a sheet — the cell rect wins.
    pub fn tiled_uv(&self) -> [f32; 4] {
        let base = self.cell_uv();
        let is_sheet = self.cols.max(1) * self.rows.max(1) > 1;
        if is_sheet || (self.tiling == [1.0, 1.0] && self.offset == [0.0, 0.0]) {
            return base;
        }
        let (tu, tv) = (self.tiling[0], self.tiling[1]);
        [
            base[0] + self.offset[0],
            base[1] + self.offset[1],
            base[0] + self.offset[0] + (base[2] - base[0]) * tu,
            base[1] + self.offset[1] + (base[3] - base[1]) * tv,
        ]
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

/// A vertical SCROLL VIEW: children keep their authored layout but shift up by
/// `offset` and clip to this element's rounded rect (an implicit mask over its
/// own subtree — draw AND hit-testing). The wheel drives `offset` while the
/// pointer is anywhere inside the view, clamped so the content can never
/// scroll fully out; scripts read/write it as `UiElement.scrollY`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollSpec {
    /// Current vertical scroll position in design units (0 = top of the content).
    #[serde(default)]
    pub offset: f32,
    /// Current horizontal scroll position (0 = left edge of the content).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset_x: f32,
    /// Design units per wheel notch.
    #[serde(default = "default_scroll_speed")]
    pub speed: f32,
    /// Dragging the view's background pans the content — the touch/kinetic
    /// idiom, and the only way to scroll a list with a thumbstick-less pointer
    /// device. Off by default: on a view full of buttons, a drag that scrolled
    /// would fight every press.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drag: bool,
}

fn default_scroll_speed() -> f32 {
    48.0
}

impl Default for ScrollSpec {
    fn default() -> Self {
        ScrollSpec {
            offset: 0.0,
            offset_x: 0.0,
            speed: default_scroll_speed(),
            drag: false,
        }
    }
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
    /// Lower clamp on the resolved size, per axis (design units). 0 = no floor.
    /// Keeps `Pct`/`Fit`/`Stretch` elements from collapsing on small windows.
    #[serde(default, skip_serializing_if = "is_zero2")]
    pub min_size: [f32; 2],
    /// Upper clamp on the resolved size, per axis (design units). 0 = no cap.
    /// Keeps an element from ballooning on huge/ultrawide displays.
    #[serde(default, skip_serializing_if = "is_zero2")]
    pub max_size: [f32; 2],
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
    /// Vertical scroll view: children shift by the offset and clip to this rect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<ScrollSpec>,
    /// Custom `.flsl` face (a `stage ui` shader, project-relative path): the
    /// element's rect is drawn by that shader — procedural instruments
    /// (navballs, gauges, radar). Draws between shape and image.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shader: String,
    /// Per-element uniform overrides for `shader` (name → vec4 lanes) —
    /// scripts drive these via `node:setShaderParam(...)`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub shader_params: std::collections::BTreeMap<String, [f32; 4]>,
    /// Clickable: the pointer can hover/press/click this element, firing the
    /// script hooks (`hoverStart`/`hoverEnd`/`pressed`/`released`/`clicked`)
    /// on this node's scripts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub button: bool,
    /// Name of a style in the project's style sheet (empty = none). At most
    /// ONE — no lists, no classes, no selectors (see `style.rs`). Whatever the
    /// style doesn't mention stays exactly as authored here.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub style: String,
    /// Greys out and stops responding. A state, not a look: what `disabled`
    /// looks like is the style's business.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// "This is the current one" — the state a menu cursor, a chosen tab, or a
    /// locked-in fighter portrait needs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Multiplies every colour this element draws — **and every descendant's**.
    /// Cascading is what lets a whole menu fade as one thing; before it,
    /// fading a panel meant parking a black rectangle over the screen
    /// (Fofighter's `Front Fade`).
    #[serde(default = "default_one")]
    pub opacity: f32,
    /// Multiplies every colour this element and its descendants draw. Group
    /// flashes (damage red, a disabled wash) without touching each child.
    #[serde(default = "white", skip_serializing_if = "is_white")]
    pub tint: [f32; 4],
    /// Rotation in degrees about `pivot`. Layout is unaffected — the element
    /// occupies the same rect and only its drawing turns, so a tilted label
    /// can't shove its siblings around.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rotation: f32,
    /// Visual scale about `pivot`, per axis. Also layout-neutral: this is the
    /// press-dip and hover-pop channel, and a button that resized its parent
    /// on hover would be a bug, not juice.
    #[serde(default = "one2", skip_serializing_if = "is_one2")]
    pub scale: [f32; 2],
    /// Origin for `rotation`/`scale`, as a fraction of the element's own rect
    /// (0,0 = top-left, 0.5,0.5 = centre).
    #[serde(default = "half2", skip_serializing_if = "is_half2")]
    pub pivot: [f32; 2],
    /// Clicking flips [`Self::selected`] — a checkbox, a mute button, a
    /// filter chip. What "on" looks like is the style's `selected` block.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub toggle: bool,
    /// Radio behaviour: clicking selects this element and deselects every
    /// other element sharing the group name. Tabs, difficulty pickers, weapon
    /// slots, a character-select grid.
    ///
    /// Groups are resolved within a LAYER, so two screens can reuse a name
    /// without interfering — and a group of one is just a toggle that can't be
    /// turned off, which is occasionally exactly what you want.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    /// Drive a named scroll view's offset: this element becomes a scrollbar
    /// track, and its `part: Handle` child becomes the thumb.
    ///
    /// A scrollbar is two of YOUR elements, styled however you like, reusing
    /// the slider machinery that was already there. The engine draws no
    /// scrollbar of its own, because a scrollbar is one of the most
    /// style-defining things on a screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollbar: Option<ScrollBar>,
    /// Reachable by keyboard and gamepad: a direction press can move focus
    /// here, and a submit press fires this element's `clicked` hook.
    ///
    /// Opt-in, because "everything with a button flag is focusable" is wrong
    /// often enough to matter — a clickable background, a drag handle, a row
    /// that only responds to a long press. What a focused element LOOKS like is
    /// the style's `focus` block; the engine draws no ring of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focusable: bool,
    /// Where a direction press goes from here, by element name, when the
    /// geometry gets it wrong. Empty = work it out from the solved rects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nav: Option<Nav>,
    /// Sibling sort key — lower draws first (further back), and inside a
    /// [`StackCfg`] lower comes first in the flow. Ties keep scene order, so a
    /// layer that never touches this behaves exactly as before.
    ///
    /// This exists because "which of these two panels is on top" was previously
    /// a property of *entity creation order*: invisible, unauthorable, and
    /// impossible to change without deleting and re-adding a node. One integer
    /// makes depth an ordinary editable property — that's what the UI tab's
    /// outline drag and "bring forward" write to.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub order: i32,
    /// Editable text: the player can type into this element, and the value it
    /// edits is this element's own [`TextSpec::text`].
    ///
    /// Storing the value in the ordinary text means every bit of typography
    /// already works — font, alignment, tracking, stroke, the style's
    /// `text_color` — and a script reads and writes it the same way it reads
    /// and writes any other label. A field is implicitly focusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<FieldSpec>,
    /// Can be picked up and carried to a [`Self::drop_target`].
    ///
    /// The engine does not move the element and does not draw a ghost: it
    /// reports `dragStart` / `dragMove` / `dropped` and lets the game decide
    /// what dragging looks like, because a card that tilts, an item that snaps
    /// to a grid and a wire that stretches from its socket are all "drag" and
    /// none of them is a translated copy of the source.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draggable: bool,
    /// Can receive a dragged element — fires `dragEnter` / `dragOver` /
    /// `dragLeave` / `dropped`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drop_target: bool,
    /// Hovering this element for [`UiLayer::tooltip_delay`] seconds shows this
    /// string in the layer's [`Self::tooltip_box`]. Empty = no tooltip.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tooltip: String,
    /// One copy of a prefab per row: the engine keeps this element's children
    /// matching [`RepeatSpec::count`], spawning and destroying only the
    /// difference.
    ///
    /// This is the answer to eight hand-placed `Icon1`…`Icon8` elements and to
    /// the four near-identical `*_row.lua` scripts that build lists a node at
    /// a time. The row is an ordinary prefab you author and can open; the
    /// engine only counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeater: Option<RepeatSpec>,
    /// This element IS the layer's tooltip: one of yours, an ordinary panel
    /// with a label inside, styled however you like.
    ///
    /// The engine never draws a tooltip of its own. It hides this element when
    /// nothing is hovered, writes the hovered element's [`Self::tooltip`] into
    /// its first text (its own, or its first labelled descendant's), and moves
    /// it to follow the pointer, keeping it inside the canvas. A tooltip that
    /// should sit somewhere fixed instead is one `Pin` away.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tooltip_box: bool,
}

/// A repeated row (see [`ElementSpec::repeater`]).
///
/// Deliberately two fields. Everything else about a list — what each row says,
/// what it does when clicked, how it is sorted, whether it animates in — is
/// the row prefab's and the game's. The engine's whole job is "there should be
/// `count` of these", which is the part that was being rewritten per screen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepeatSpec {
    /// The prefab instantiated once per row, by name or project-relative path
    /// — the same string [`spawn`](https://example.invalid) takes in Lua.
    pub template: String,
    /// How many rows there should be. A script sets it, or `ui.bind`s it to
    /// the length of a table; the engine spawns or destroys the difference and
    /// leaves every surviving row alone.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub count: u32,
}

/// An editable text element (see [`ElementSpec::field`]).
///
/// The value is the element's [`TextSpec::text`]; everything here is about how
/// it is *entered*. Deliberately small: a field is one of the places where a
/// game most wants its own look, and the three colours below all default to
/// something derived from the text colour you already chose rather than to a
/// colour the engine picked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    /// Shown while the value is empty. Not a value — it never submits and a
    /// script never reads it back as content.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub placeholder: String,
    /// Cap on the number of CHARACTERS (0 = no cap). Characters, not bytes, so
    /// a name field behaves the same for every alphabet.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub max_len: u32,
    /// Draw every character as `mask_char`. Copy and cut are refused while
    /// this is on — a password field that fills the clipboard is a bug.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mask: bool,
    /// What a masked character draws as.
    #[serde(default = "default_mask_char", skip_serializing_if = "is_default_mask_char")]
    pub mask_char: char,
    /// Accept only digits, one leading `-` and one `.`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub numeric: bool,
    /// Force every entered character to upper case as it is typed — lobby
    /// codes, initials, licence keys. The stored value is what you see.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upper: bool,
    /// Caret colour. **Alpha 0 = follow the text colour**, which is almost
    /// always right and is a derived default rather than an imposed one.
    #[serde(default, skip_serializing_if = "is_clear")]
    pub caret_color: [f32; 4],
    /// Selection highlight. Alpha 0 = the text colour at 30%.
    #[serde(default, skip_serializing_if = "is_clear")]
    pub selection_color: [f32; 4],
    /// Placeholder colour. Alpha 0 = the text colour at 45%.
    #[serde(default, skip_serializing_if = "is_clear")]
    pub placeholder_color: [f32; 4],
    /// Caret bar width in design units.
    #[serde(default = "default_caret_width", skip_serializing_if = "is_default_caret_width")]
    pub caret_width: f32,
}

impl Default for FieldSpec {
    fn default() -> Self {
        FieldSpec {
            placeholder: String::new(),
            max_len: 0,
            mask: false,
            mask_char: default_mask_char(),
            numeric: false,
            upper: false,
            caret_color: [0.0; 4],
            selection_color: [0.0; 4],
            placeholder_color: [0.0; 4],
            caret_width: default_caret_width(),
        }
    }
}

fn default_mask_char() -> char {
    '•'
}
fn is_default_mask_char(c: &char) -> bool {
    *c == '•'
}
fn default_caret_width() -> f32 {
    2.0
}
fn is_default_caret_width(v: &f32) -> bool {
    *v == 2.0
}
fn is_clear(v: &[f32; 4]) -> bool {
    v[3] == 0.0
}

fn white() -> [f32; 4] {
    [1.0; 4]
}
fn is_white(v: &[f32; 4]) -> bool {
    v.iter().all(|x| *x == 1.0)
}
fn is_zero(v: &f32) -> bool {
    *v == 0.0
}
fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}
fn half2() -> [f32; 2] {
    [0.5, 0.5]
}
fn is_half2(v: &[f32; 2]) -> bool {
    v[0] == 0.5 && v[1] == 0.5
}

fn default_true() -> bool {
    true
}
fn default_one() -> f32 {
    1.0
}
fn is_zero2(v: &[f32; 2]) -> bool {
    v[0] == 0.0 && v[1] == 0.0
}

impl Default for ElementSpec {
    fn default() -> Self {
        ElementSpec {
            place: Place::default(),
            size: [Size::Fit, Size::Fit],
            min_size: [0.0, 0.0],
            max_size: [0.0, 0.0],
            stack: None,
            shape: None,
            text: None,
            image: None,
            slider: None,
            part: None,
            mask: None,
            scroll: None,
            shader: String::new(),
            shader_params: std::collections::BTreeMap::new(),
            button: false,
            style: String::new(),
            disabled: false,
            selected: false,
            visible: true,
            opacity: 1.0,
            tint: [1.0; 4],
            rotation: 0.0,
            scale: [1.0, 1.0],
            pivot: [0.5, 0.5],
            toggle: false,
            group: String::new(),
            scrollbar: None,
            focusable: false,
            nav: None,
            order: 0,
            field: None,
            draggable: false,
            drop_target: false,
            tooltip: String::new(),
            repeater: None,
            tooltip_box: false,
        }
    }
}

/// A scrollbar's link to the view it drives.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScrollBar {
    /// The scroll view's element NAME, within this layer.
    pub target: String,
    /// Which axis this bar drives. `Column` = the vertical bar.
    #[serde(default)]
    pub axis: Dir,
}

/// Per-element navigation overrides: the element NAME to focus when this
/// direction is pressed from here. An empty string means "work it out from the
/// geometry", so you override only the edges that need it.
///
/// The cases geometry can't know: a grid that should wrap at the end of a row,
/// a "back" button that should be reachable from anywhere on the screen, two
/// columns that must not be treated as one field of buttons.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Nav {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub up: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub down: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub left: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub right: String,
}

impl Nav {
    /// The override for a direction, if any.
    pub fn get(&self, dir: Dir4) -> Option<&str> {
        let s = match dir {
            Dir4::Up => &self.up,
            Dir4::Down => &self.down,
            Dir4::Left => &self.left,
            Dir4::Right => &self.right,
        };
        (!s.is_empty()).then_some(s.as_str())
    }
}

/// Where a [`UiLayer`] lives when the game runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiSpace {
    /// A flat overlay that fills the window, on top of the 3D scene (HUD,
    /// menus). Resolution-independent via `design_height`.
    #[default]
    Screen,
    /// A flat quad living *inside* the 3D world at the layer node's transform
    /// (diegetic panels, in-world signage). Scaled by `canvas_scale`
    /// (world units per design unit); move/rotate the node to place it.
    World,
}

/// How a layer's design units map to physical pixels as the window resizes —
/// the canvas scaler (cf. Unity's CanvasScaler). Every mode resolves to ONE
/// uniform `scale` (physical px per design unit); the design viewport handed to
/// the solver is then `window_px / scale`, so the whole layout pipeline stays
/// unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiScaleMode {
    /// `design_height` design units always span the window HEIGHT; width floats
    /// with the aspect. The classic default — every existing layer behaves
    /// exactly as before.
    #[default]
    MatchHeight,
    /// `reference_width` design units always span the window WIDTH; height
    /// floats. Good for wide HUDs that must keep their horizontal layout.
    MatchWidth,
    /// Blend between match-width and match-height by `match_wh` (0 = width,
    /// 1 = height) using the log-2 average — the fully responsive choice that
    /// splits the difference across aspect ratios.
    Blend,
    /// Fit the whole reference resolution INSIDE the window (letterbox): the UI
    /// never crops, but leaves empty margins on off-aspect monitors.
    Expand,
    /// Fill the window with the reference resolution (may crop): no empty
    /// margins, but edges can fall outside a very different aspect.
    Shrink,
    /// 1 design unit = 1 physical pixel, always. The UI never rescales with the
    /// window (pixel-perfect art); references are ignored.
    ConstantPixels,
}

/// A UI layer root. Lives on a scene node; its element children form the tree.
/// The layer scales uniformly (see [`UiScaleMode`]) so it stays consistent
/// across window sizes and monitor aspects.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiLayer {
    /// The reference HEIGHT in design units — the vertical half of the canvas's
    /// reference resolution. In `MatchHeight` (and as the height reference for
    /// `Blend`/`Expand`/`Shrink`) this many units span the window height.
    pub design_height: f32,
    /// The reference WIDTH in design units — the horizontal half of the
    /// reference resolution. Used by `MatchWidth`/`Blend`/`Expand`/`Shrink`
    /// (ignored by `MatchHeight`, where width just follows the aspect).
    #[serde(default = "default_reference_width")]
    pub reference_width: f32,
    /// How design units map to pixels as the window resizes (the canvas scaler).
    #[serde(default)]
    pub scale_mode: UiScaleMode,
    /// `Blend` only: 0 = match width, 1 = match height, 0.5 = balance both.
    #[serde(default = "default_match_wh")]
    pub match_wh: f32,
    /// Layers draw lowest-z first.
    pub z: i32,
    /// Master switch: an off layer draws nothing (in-game and in-editor).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Screen overlay vs a quad in the 3D world. Screen-space by default so
    /// existing layers are unchanged.
    #[serde(default)]
    pub space: UiSpace,
    /// World units per design unit for a [`UiSpace::World`] canvas (and the
    /// Scene-view authoring hologram of a screen-space layer). 0.01 → a
    /// 720-design layer stands 7.2 world units tall.
    #[serde(default = "default_canvas_scale")]
    pub canvas_scale: f32,
    /// Seconds a direction must be held before it starts repeating.
    #[serde(default = "default_nav_delay", skip_serializing_if = "is_default_nav_delay")]
    pub nav_delay: f32,
    /// Seconds between repeats once it starts.
    #[serde(default = "default_nav_repeat", skip_serializing_if = "is_default_nav_repeat")]
    pub nav_repeat: f32,
    /// Running off the end of the screen comes back on the other side.
    ///
    /// Off by default: wrapping is right for a short vertical menu and wrong
    /// for a long inventory, and guessing wrong is worse than asking.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nav_wrap: bool,
    /// Seconds of hovering before the tooltip appears.
    #[serde(default = "default_tooltip_delay", skip_serializing_if = "is_default_tooltip_delay")]
    pub tooltip_delay: f32,
}

fn default_canvas_scale() -> f32 {
    0.01
}
// The house menu feel: one move on press, a beat to decide you meant to hold,
// then a steady roll. Both are per-layer, because a fast action menu and a
// long settings list genuinely want different numbers.
fn default_nav_delay() -> f32 {
    0.35
}
fn default_nav_repeat() -> f32 {
    0.12
}
fn is_default_nav_delay(v: &f32) -> bool {
    *v == default_nav_delay()
}
fn is_default_nav_repeat(v: &f32) -> bool {
    *v == default_nav_repeat()
}
fn default_tooltip_delay() -> f32 {
    0.5
}
fn is_default_tooltip_delay(v: &f32) -> bool {
    *v == default_tooltip_delay()
}
fn default_reference_width() -> f32 {
    1280.0
}
fn default_match_wh() -> f32 {
    0.5
}

impl UiLayer {
    /// A world-space layer renders as a quad in the scene at runtime, not a
    /// screen overlay.
    pub fn is_world(&self) -> bool {
        matches!(self.space, UiSpace::World)
    }

    /// Physical pixels per design unit for a window of `viewport_px` — the one
    /// number every screen-space consumer needs. Divide the window size by it to
    /// get the design-space viewport handed to [`solve`]. Always finite and > 0.
    pub fn scale_for(&self, viewport_px: [f32; 2]) -> f32 {
        let w = viewport_px[0].max(1.0);
        let h = viewport_px[1].max(1.0);
        let ref_w = self.reference_width.max(1.0);
        let ref_h = self.design_height.max(1.0);
        let by_w = w / ref_w;
        let by_h = h / ref_h;
        let s = match self.scale_mode {
            UiScaleMode::MatchHeight => by_h,
            UiScaleMode::MatchWidth => by_w,
            UiScaleMode::Blend => {
                let m = self.match_wh.clamp(0.0, 1.0);
                // Log-2 average, so halving one dimension halves the scale
                // symmetrically regardless of which axis dominates.
                (by_w.ln() * (1.0 - m) + by_h.ln() * m).exp()
            }
            UiScaleMode::Expand => by_w.min(by_h),
            UiScaleMode::Shrink => by_w.max(by_h),
            UiScaleMode::ConstantPixels => 1.0,
        };
        if s.is_finite() && s > 0.0 {
            s
        } else {
            0.01
        }
    }
}

impl Default for UiLayer {
    fn default() -> Self {
        UiLayer {
            design_height: 720.0,
            reference_width: default_reference_width(),
            scale_mode: UiScaleMode::default(),
            match_wh: default_match_wh(),
            z: 0,
            enabled: true,
            space: UiSpace::Screen,
            canvas_scale: 0.01,
            nav_delay: default_nav_delay(),
            nav_repeat: default_nav_repeat(),
            nav_wrap: false,
            tooltip_delay: default_tooltip_delay(),
        }
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

impl Node {
    /// Build a node with its children in *draw order* — sorted by
    /// [`ElementSpec::order`], scene order breaking ties.
    ///
    /// Every tree-building path goes through here so depth means the same thing
    /// to the solver, the draw list, hit-testing and the editor's outline. A
    /// stable sort is load-bearing: with the default `order: 0` everywhere, this
    /// is exactly the old scene order.
    pub fn with_children(id: u32, spec: ElementSpec, mut children: Vec<Node>) -> Self {
        children.sort_by_key(|c| c.spec.order);
        Node { id, spec, children }
    }
}

/// Put a layer's root elements in draw order (see [`Node::with_children`] — the
/// roots have no parent node to sort them).
pub fn sort_roots(roots: &mut [Node]) {
    roots.sort_by_key(|n| n.spec.order);
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
/// size. `Fit` recurses into content; a `Stretch` axis fills the anchor span.
fn measure_node(n: &Node, avail: [f32; 2], measure: MeasureText) -> [f32; 2] {
    // On a stretching axis the size comes from the parent span, not `size`.
    let stretch = match n.spec.place {
        Place::Stretch { min, max, .. } => [max[0] - min[0] > 0.0, max[1] - min[1] > 0.0],
        _ => [false, false],
    };
    let needs_fit = n.spec.size.iter().zip(stretch).any(|(s, st)| {
        !st && matches!(s, Size::Fit | Size::Grow(_))
    });
    let fit = if needs_fit { fit_size(n, avail, measure) } else { [0.0, 0.0] };
    let mut size = [0.0f32; 2];
    for a in 0..2 {
        size[a] = if stretch[a] {
            // Fill the anchored span (fraction of the parent) minus the margins.
            let Place::Stretch { min, max, margin } = n.spec.place else { unreachable!() };
            let span = (max[a] - min[a]).clamp(0.0, 1.0);
            let (lead, trail) = if a == 0 { (margin[0], margin[2]) } else { (margin[1], margin[3]) };
            (avail[a] * span - lead - trail).max(0.0)
        } else {
            match n.spec.size[a] {
                Size::Fixed(v) => v.max(0.0),
                Size::Pct(p) => (avail[a] * p).max(0.0),
                Size::Fit | Size::Grow(_) => fit[a],
            }
        };
        // Clamp to the authored min/max (0 = unbounded on that end).
        let (lo, hi) = (n.spec.min_size[a], n.spec.max_size[a]);
        if lo > 0.0 {
            size[a] = size[a].max(lo);
        }
        if hi > 0.0 {
            size[a] = size[a].min(hi);
        }
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
        // The element's top-left sits on the min-anchor line + the leading
        // margin. A stretched axis already sized itself to reach the max line
        // minus the trailing margin (see `measure_node`); a point axis (min ==
        // max) keeps its own size and hangs off the anchor line.
        Place::Stretch { min, margin, .. } => [
            parent_pos[0] + parent_size[0] * min[0] + margin[0],
            parent_pos[1] + parent_size[1] * min[1] + margin[1],
        ],
    }
}

/// Recursively place `n`'s children inside its solved rect.
fn layout_node(n: &Node, rect: [f32; 4], measure: MeasureText, out: &mut Vec<Placed>) {
    out.push(Placed { id: n.id, rect });
    let visible: Vec<&Node> = n.children.iter().filter(|c| c.spec.visible).collect();
    if visible.is_empty() {
        return;
    }
    // A scroll view's children lay out exactly as authored, then the whole
    // content shifts up by the scroll offset (clipping happens in draw_list /
    // hit-testing via the implicit self-mask).
    let scroll_y = n.spec.scroll.map(|s| s.offset.max(0.0)).unwrap_or(0.0);
    let scroll_x = n.spec.scroll.map(|s| s.offset_x.max(0.0)).unwrap_or(0.0);
    let (px, py, pw, ph) = (rect[0] - scroll_x, rect[1] - scroll_y, rect[2], rect[3]);
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
                    Place::Stretch { margin, .. } => margin[axis],
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

/// A visual (NOT layout) transform applied to a quad about a pivot inside its
/// own rect. Layout already happened; this only turns and scales the drawing,
/// so juice can never reflow a screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Xform {
    /// Radians, clockwise on screen (y is down).
    pub rotation: f32,
    pub scale: [f32; 2],
    /// Fraction of the quad's own rect.
    pub pivot: [f32; 2],
}

impl Default for Xform {
    fn default() -> Self {
        Xform { rotation: 0.0, scale: [1.0, 1.0], pivot: [0.5, 0.5] }
    }
}

impl Xform {
    /// True when this transform would change nothing (the overwhelmingly
    /// common case — worth an early-out in the packer).
    pub fn is_identity(&self) -> bool {
        self.rotation == 0.0 && self.scale == [1.0, 1.0]
    }
}

/// What kind of face a [`Quad`] is — picks the fragment path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuadKind {
    /// Rounded rect: gradient/flat fill, per-side border, optional texture.
    #[default]
    Shape,
    /// A feathered rounded rect drawn BEHIND the shape (drop shadow, glow).
    Shadow,
    /// A feathered rounded rect drawn INSIDE the shape (recessed well).
    InsetShadow,
}

/// One rounded-rect quad, in design units (the renderer scales).
#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    pub rect: [f32; 4],
    /// Flat fill, or the near stop when `gradient` is set.
    pub color: [f32; 4],
    /// Two-stop gradient over the fill (`None` = flat).
    pub gradient: Option<Gradient>,
    /// Corner radii `[TL, TR, BR, BL]`.
    pub radius: [f32; 4],
    /// Border widths `[L, T, R, B]`.
    pub border: [f32; 4],
    pub border_color: [f32; 4],
    /// Texture asset path (empty = solid fill).
    pub texture: String,
    /// UV sub-rect `[min_u, min_v, max_u, max_v]` to sample — the whole texture
    /// `[0,0,1,1]` normally, a single cell for a spritesheet image, and the
    /// region a 9-slice cuts its nine patches out of.
    pub uv: [f32; 4],
    /// 9-slice insets `[L, T, R, B]` as a fraction of the sampled region
    /// (all zero = off). Expanded into nine patches by the renderer, which is
    /// the only place that knows the texture's pixel size — and therefore how
    /// big an unstretched corner should be drawn.
    pub slice: [f32; 4],
    /// Aspect handling when the image doesn't match the rect.
    pub fit: ImageFit,
    /// Set when a mask claims this element.
    pub clip: Option<Clip>,
    /// Custom-shader face: `(flsl path, owner element id)` — the renderer
    /// resolves this to a pipeline + per-element param binding.
    pub shader: Option<(String, u32)>,
    /// Which fragment path draws this quad.
    pub kind: QuadKind,
    /// Soft-edge width in design units for the two shadow kinds.
    pub feather: f32,
    /// `InsetShadow` only: how far the inner shape is displaced.
    pub shadow_offset: [f32; 2],
    /// Per-pixel noise over the fill (`None` = clean).
    pub grain: Option<GrainSpec>,
    /// Compositing mode — a batch key, so runs of one mode stay one draw call.
    pub blend: Blend,
    /// Visual rotate/scale about a pivot.
    pub xform: Xform,
}

impl Default for Quad {
    fn default() -> Self {
        Quad {
            rect: [0.0; 4],
            color: [1.0; 4],
            gradient: None,
            radius: [0.0; 4],
            border: [0.0; 4],
            border_color: [0.0; 4],
            texture: String::new(),
            uv: [0.0, 0.0, 1.0, 1.0],
            slice: [0.0; 4],
            fit: ImageFit::Stretch,
            clip: None,
            shader: None,
            kind: QuadKind::Shape,
            feather: 0.0,
            shadow_offset: [0.0; 2],
            grain: None,
            blend: Blend::Normal,
            xform: Xform::default(),
        }
    }
}

/// One text run (the renderer owns the font and lays out glyphs).
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    /// The element's rect — alignment happens inside it.
    pub rect: [f32; 4],
    /// The string as it should be drawn: the case transform is already applied,
    /// so the renderer never has to re-derive it (and the authored string in
    /// the scene stays what the designer typed).
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
    /// Outline around the glyphs (extra offset copies at pack time).
    pub stroke: Option<TextStroke>,
    /// A dropped copy behind the run.
    pub shadow: Option<TextShadow>,
    /// Extra advance between glyphs, design units.
    pub tracking: f32,
    /// Line spacing multiplier (0 = the font's own metrics).
    pub line_height: f32,
    /// Wrap to `rect`'s width.
    pub wrap: bool,
    /// Cap on rendered lines (0 = unlimited).
    pub max_lines: u32,
    pub overflow: Overflow,
    /// Visual rotate/scale, matching the owning element's.
    pub xform: Xform,
    /// Set on the one run currently being edited: where the caret sits and
    /// what is selected. The renderer owns glyph layout, so it is the only
    /// place that can turn a character index into an x position — which is why
    /// this rides the run rather than arriving as pre-computed quads.
    pub caret: Option<Caret>,
}

/// A text caret and selection, in CHARACTER indices into [`TextRun::text`]
/// (the string as drawn — masked, case-transformed, whatever it ended up).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Caret {
    pub index: usize,
    /// The other end of the selection; `== index` when nothing is selected.
    pub anchor: usize,
    pub color: [f32; 4],
    pub selection: [f32; 4],
    /// Bar width in design units.
    pub width: f32,
    /// Blink phase: false hides the bar (the selection still draws).
    pub on: bool,
}

impl Default for TextRun {
    fn default() -> Self {
        TextRun {
            rect: [0.0; 4],
            text: String::new(),
            size: 24.0,
            color: [1.0; 4],
            align: Align::Start,
            valign: Align::Center,
            fit: false,
            font: String::new(),
            clip: None,
            stroke: None,
            shadow: None,
            tracking: 0.0,
            line_height: 0.0,
            wrap: false,
            max_lines: 0,
            overflow: Overflow::Show,
            xform: Xform::default(),
            caret: None,
        }
    }
}

/// Which element is being typed into, and where its caret is.
///
/// Runtime state, held by the editor/player and handed to [`draw_list_with`]
/// once per frame. It is deliberately NOT part of [`ElementSpec`]: a caret
/// position in a saved scene would be nonsense, and keeping it out means it is
/// structurally impossible for one to get there.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EditState {
    /// The element being edited.
    pub id: u32,
    /// Caret position, in characters into the element's VALUE (the authored
    /// string — masking and case happen afterwards).
    pub caret: usize,
    /// The other end of the selection.
    pub anchor: usize,
    /// Blink phase.
    pub on: bool,
}

/// Everything a layer draws this frame, painter's order.
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub quads: Vec<Quad>,
    pub texts: Vec<TextRun>,
}

/// The implicit clips scroll views impose: every scroll element clips its own
/// placed subtree to its rect (first/outermost claim wins, same rule as
/// masks). Shared by [`draw_list`] and pointer hit-testing — an element
/// scrolled out of view must neither draw nor click.
pub fn scroll_clips(
    roots: &[Node],
    placed: &[Placed],
) -> std::collections::HashMap<u32, Clip> {
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
    let mut out: std::collections::HashMap<u32, Clip> = std::collections::HashMap::new();
    // Painter's order = parents before children, so an outer scroll claims a
    // nested scroll's content before the inner one can.
    for p in placed {
        let Some(n) = nodes.get(&p.id) else { continue };
        if n.spec.scroll.is_none() {
            continue;
        }
        let radius = n.spec.shape.map(|s| s.radius.max()).unwrap_or(0.0);
        let clip = Clip { rect: p.rect, radius };
        let mut stack: Vec<u32> = n.children.iter().map(|c| c.id).collect();
        while let Some(id) = stack.pop() {
            out.entry(id).or_insert(clip);
            if let Some(c) = nodes.get(&id) {
                stack.extend(c.children.iter().map(|k| k.id));
            }
        }
    }
    out
}

/// Size and position every scrollbar's thumb from the view it tracks.
///
/// A post-pass rather than solver work, because a bar and the view it drives
/// live in different subtrees and the solver is one parent-to-child walk. Each
/// pair is `(bar element, scroll view)`, resolved by name by the caller (names
/// are the engine's, not this crate's).
///
/// The thumb is the bar's `part: Handle` child. Its length becomes the visible
/// fraction of the content — which is the thing that makes a scrollbar readable
/// as "how much of this list am I seeing" rather than just a position — and its
/// cross-axis geometry is left exactly as authored, so a 4-unit hairline and a
/// chunky 20-unit slab are both yours to make.
pub fn place_scrollbars(roots: &[Node], placed: &mut [Placed], pairs: &[(u32, u32)]) {
    fn find(ns: &[Node], id: u32) -> Option<&Node> {
        for n in ns {
            if n.id == id {
                return Some(n);
            }
            if let Some(f) = find(&n.children, id) {
                return Some(f);
            }
        }
        None
    }
    for (bar_id, view_id) in pairs {
        let Some(bar) = find(roots, *bar_id) else { continue };
        let Some(sb) = bar.spec.scrollbar.as_ref() else { continue };
        let Some(thumb) = bar
            .children
            .iter()
            .find(|c| c.spec.part == Some(SliderPart::Handle) && c.spec.visible)
        else {
            continue;
        };
        let Some(view) = placed.iter().find(|p| p.id == *view_id).map(|p| p.rect) else { continue };
        let Some(track) = placed.iter().find(|p| p.id == *bar_id).map(|p| p.rect) else { continue };
        let Some(scroll) = find(roots, *view_id).and_then(|n| n.spec.scroll) else { continue };
        let max = scroll_max(roots, placed, *view_id);
        let a = match sb.axis {
            Dir::Row => 0,
            Dir::Column => 1,
        };
        let offset = if a == 0 { scroll.offset_x } else { scroll.offset };
        let content = view[a + 2] + max[a];
        // Nothing to scroll: a full-length thumb, which reads correctly as
        // "this is all of it" instead of vanishing or pinning to the top.
        let frac = if content > 0.0 { (view[a + 2] / content).clamp(0.05, 1.0) } else { 1.0 };
        let len = (track[a + 2] * frac).max(4.0);
        let t = if max[a] > 0.0 { (offset / max[a]).clamp(0.0, 1.0) } else { 0.0 };
        let pos = track[a] + (track[a + 2] - len) * t;
        if let Some(p) = placed.iter_mut().find(|p| p.id == thumb.id) {
            p.rect[a] = pos;
            p.rect[a + 2] = len;
        }
    }
}

/// How far a scroll view can scroll on each axis: `max(0, content − view)`,
/// where content is the placed subtree's far edge measured in content space
/// (offset-independent). The input driver clamps [`ScrollSpec::offset`] and
/// `offset_x` to this every frame, so content can never be scrolled fully away
/// — and a view whose content fits doesn't scroll at all.
///
/// Returns `[x, y]`. An axis that returns 0 has nothing to scroll, which is
/// also how the wheel decides which axis it drives.
pub fn scroll_max(roots: &[Node], placed: &[Placed], scroll_id: u32) -> [f32; 2] {
    fn find(roots: &[Node], id: u32) -> Option<&Node> {
        for n in roots {
            if n.id == id {
                return Some(n);
            }
            if let Some(f) = find(&n.children, id) {
                return Some(f);
            }
        }
        None
    }
    let Some(n) = find(roots, scroll_id) else { return [0.0, 0.0] };
    let off = n
        .spec
        .scroll
        .map(|s| [s.offset_x.max(0.0), s.offset.max(0.0)])
        .unwrap_or([0.0, 0.0]);
    let rects: std::collections::HashMap<u32, [f32; 4]> =
        placed.iter().map(|p| (p.id, p.rect)).collect();
    let Some(&view) = rects.get(&scroll_id) else { return [0.0, 0.0] };
    // Content extents, measured from the SHIFTED rects and then un-shifted, so
    // the answer doesn't depend on where the view happens to be scrolled to.
    let mut far = [view[0] - off[0], view[1] - off[1]];
    let mut stack: Vec<&Node> = n.children.iter().collect();
    while let Some(c) = stack.pop() {
        if let Some(r) = rects.get(&c.id) {
            far[0] = far[0].max(r[0] + r[2]);
            far[1] = far[1].max(r[1] + r[3]);
        }
        stack.extend(c.children.iter());
    }
    [
        ((far[0] + off[0]) - view[0] - view[2]).max(0.0),
        ((far[1] + off[1]) - view[1] - view[3]).max(0.0),
    ]
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
    draw_list_with(roots, placed, masks, None)
}

/// [`draw_list`] plus the caret of whichever text field is being edited.
///
/// A separate entry point rather than a fifth parameter on `draw_list`,
/// because every caller that has no text field — the UI tab's canvas, the
/// probes, the tests — should not have to say so.
pub fn draw_list_with(
    roots: &[Node],
    placed: &[Placed],
    masks: &[(u32, u32)],
    edit: Option<EditState>,
) -> DrawList {
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
    // Scroll views clip their own subtree first (innermost intent), then
    // explicit masks claim whatever is left (first claim wins).
    let mut clip_of = scroll_clips(roots, placed);
    for (mask_id, target_id) in masks {
        let Some(&rect) = rects.get(mask_id) else { continue };
        // A clip is one scalar radius in the instance data, so mixed corners
        // round the mask by their largest — over-rounding hides less than
        // under-rounding would show.
        let radius = nodes
            .get(mask_id)
            .and_then(|n| n.spec.shape)
            .map(|s| s.radius.max())
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
    // Opacity and tint CASCADE: an element's effective multiplier is its own
    // times every ancestor's. This is what lets one `opacity` fade a whole
    // menu — before it, `opacity` was self-only and fading a panel meant
    // parking a black rectangle over the screen.
    let inherited = inherited_tints(roots);
    let white = [1.0f32; 4];

    let mut dl = DrawList::default();
    for p in placed {
        let Some(node) = nodes.get(&p.id) else { continue };
        let spec = &node.spec;
        let clip = clip_of.get(&p.id).copied();
        let mul = inherited.get(&p.id).copied().unwrap_or(white);
        // Every colour this element draws passes through the cascade.
        let paint = |c: [f32; 4]| -> [f32; 4] {
            [c[0] * mul[0], c[1] * mul[1], c[2] * mul[2], c[3] * mul[3]]
        };
        let xform = Xform {
            rotation: spec.rotation.to_radians(),
            scale: spec.scale,
            pivot: spec.pivot,
        };
        let radius = spec.shape.map(|s| s.radius.0).unwrap_or([0.0; 4]);
        let blend = spec.shape.map(|s| s.blend).unwrap_or(Blend::Normal);

        if let Some(s) = spec.shape {
            // Glow sits furthest back — it is light spilling from under the
            // element, so a drop shadow drawn after it still reads as contact.
            if let Some(g) = s.glow
                && g.color[3] > 0.0
            {
                dl.quads.push(Quad {
                    rect: grow(p.rect, g.spread),
                    color: paint(g.color),
                    radius: grow_radius(radius, g.spread),
                    clip,
                    kind: QuadKind::Shadow,
                    feather: g.radius.max(0.0),
                    // Glow is light: it adds rather than covers, which is the
                    // difference between "lit" and "a coloured smudge".
                    blend: Blend::Additive,
                    xform,
                    ..Default::default()
                });
            }
            // Outer drop shadow: behind the shape, lifting it off whatever was
            // drawn before. Grows by `spread`, offsets by `offset`, with a
            // `blur`-wide soft edge (the `feather`).
            if let Some(sh) = s.shadow
                && !sh.inset
                && sh.color[3] > 0.0
            {
                let mut r = grow(p.rect, sh.spread);
                r[0] += sh.offset[0];
                r[1] += sh.offset[1];
                dl.quads.push(Quad {
                    rect: r,
                    color: paint(sh.color),
                    radius: grow_radius(radius, sh.spread),
                    clip,
                    kind: QuadKind::Shadow,
                    feather: sh.blur.max(0.0),
                    xform,
                    ..Default::default()
                });
            }
            dl.quads.push(Quad {
                rect: p.rect,
                color: paint(s.fill),
                gradient: s.gradient.map(|mut g| {
                    g.to = paint(g.to);
                    g
                }),
                radius,
                border: s.border.0,
                border_color: paint(s.border_color),
                clip,
                grain: s.grain,
                blend,
                xform,
                ..Default::default()
            });
            // Inset shadow rides ON TOP of the fill (it is a hole in the
            // surface, not something behind it) but under the image and text.
            if let Some(sh) = s.shadow
                && sh.inset
                && sh.color[3] > 0.0
            {
                dl.quads.push(Quad {
                    rect: p.rect,
                    color: paint(sh.color),
                    radius,
                    clip,
                    kind: QuadKind::InsetShadow,
                    feather: sh.blur.max(0.0),
                    shadow_offset: sh.offset,
                    // `spread` pushes the inner edge further in.
                    border: [sh.spread; 4],
                    blend,
                    xform,
                    ..Default::default()
                });
            }
        }
        if !spec.shader.is_empty() {
            // A custom-shader face: white tint (the shader reads it as
            // `instanceColor`, alpha = the element's effective opacity). The
            // corner radii ride along so the transpiled shader can clip its
            // output to the element's rounded rect (no spill past the corners).
            // The element's image (if any) binds at group(1), so the shader can
            // sample it with `baseTexture(uv)` — the shader then OWNS the image
            // (the plain image quad below is suppressed).
            let img_tex = spec
                .image
                .as_ref()
                .map(|i| i.texture.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_default();
            let img_uv = spec.image.as_ref().map(|i| i.cell_uv()).unwrap_or([0.0, 0.0, 1.0, 1.0]);
            dl.quads.push(Quad {
                rect: p.rect,
                color: paint(white),
                radius,
                texture: img_tex,
                uv: img_uv,
                clip,
                shader: Some((spec.shader.clone(), p.id)),
                blend,
                xform,
                ..Default::default()
            });
        }
        // The plain image quad — skipped when a shader owns the element (the
        // shader draws the image itself via `baseTexture`).
        if let Some(img) = &spec.image
            && !img.texture.is_empty()
            && spec.shader.is_empty()
        {
            dl.quads.push(Quad {
                rect: p.rect,
                color: paint(img.tint),
                radius,
                texture: img.texture.clone(),
                uv: img.tiled_uv(),
                slice: img.slice,
                fit: img.fit,
                clip,
                blend,
                xform,
                ..Default::default()
            });
        }
        if let Some(t) = &spec.text {
            // A field's text is what the PLAYER typed, so three things differ
            // from an ordinary label: an empty one shows the placeholder, a
            // masked one shows dots, and the one being edited carries a caret.
            let editing = edit.filter(|e| e.id == p.id);
            let field = spec.field.as_ref();
            let (shown, mut color, caret_chars) = match field {
                Some(f) if t.text.is_empty() && editing.is_none() && !f.placeholder.is_empty() => {
                    // Alpha 0 means "derive from the text colour" — a default
                    // that comes out of the design instead of out of us.
                    let c = if f.placeholder_color[3] > 0.0 {
                        f.placeholder_color
                    } else {
                        [t.color[0], t.color[1], t.color[2], t.color[3] * 0.45]
                    };
                    (f.placeholder.clone(), c, None)
                }
                Some(f) => {
                    let n = t.text.chars().count();
                    let shown = if f.mask {
                        std::iter::repeat_n(f.mask_char, n).collect()
                    } else {
                        t.display().into_owned()
                    };
                    (shown, t.color, Some(n))
                }
                None => (t.display().into_owned(), t.color, None),
            };
            color = paint(color);
            // A field always clips to its own rect: the value is the player's,
            // so it can be longer than the box, and text running out of a box
            // and across the screen is never what anyone wanted.
            let clip = if field.is_some() {
                Some(clip.unwrap_or(Clip {
                    rect: p.rect,
                    radius: spec.shape.map(|s| s.radius.max()).unwrap_or(0.0),
                }))
            } else {
                clip
            };
            let caret = match (editing, field, caret_chars) {
                (Some(e), Some(f), Some(n)) => Some(Caret {
                    index: e.caret.min(n),
                    anchor: e.anchor.min(n),
                    color: paint(if f.caret_color[3] > 0.0 { f.caret_color } else { t.color }),
                    selection: paint(if f.selection_color[3] > 0.0 {
                        f.selection_color
                    } else {
                        [t.color[0], t.color[1], t.color[2], t.color[3] * 0.3]
                    }),
                    width: f.caret_width,
                    on: e.on,
                }),
                _ => None,
            };
            // An empty label still draws nothing; an empty FIELD being edited
            // has to, or the caret has nowhere to appear.
            if !shown.is_empty() || caret.is_some() {
                dl.texts.push(TextRun {
                    rect: p.rect,
                    text: shown,
                    size: t.size,
                    color,
                    align: t.align,
                    valign: t.valign,
                    fit: t.fit,
                    font: t.font.clone(),
                    clip,
                    stroke: t.stroke.map(|mut s| {
                        s.color = paint(s.color);
                        s
                    }),
                    shadow: t.shadow.map(|mut s| {
                        s.color = paint(s.color);
                        s
                    }),
                    tracking: t.tracking,
                    line_height: t.line_height,
                    wrap: t.wrap,
                    max_lines: t.max_lines,
                    overflow: t.overflow,
                    xform,
                    caret,
                });
            }
        }
    }
    dl
}

/// Expand a rect by `d` on every side.
fn grow(r: [f32; 4], d: f32) -> [f32; 4] {
    [r[0] - d, r[1] - d, r[2] + d * 2.0, r[3] + d * 2.0]
}

/// Corner radii for a rect grown by `d` — a spread shadow's corners open up by
/// the same amount, or the silhouette pinches at the corners.
fn grow_radius(r: [f32; 4], d: f32) -> [f32; 4] {
    let d = d.max(0.0);
    [r[0] + d, r[1] + d, r[2] + d, r[3] + d]
}

/// Effective colour multiplier per element: own `tint` (with `opacity` folded
/// into alpha) times every ancestor's.
///
/// Computed over the TREE, not the placed list, because an invisible parent
/// still parents its children's cascade — and hidden subtrees never reach the
/// draw list anyway.
fn inherited_tints(roots: &[Node]) -> std::collections::HashMap<u32, [f32; 4]> {
    fn walk(n: &Node, parent: [f32; 4], out: &mut std::collections::HashMap<u32, [f32; 4]>) {
        let o = n.spec.opacity.clamp(0.0, 1.0);
        let t = n.spec.tint;
        let mine = [
            parent[0] * t[0],
            parent[1] * t[1],
            parent[2] * t[2],
            parent[3] * t[3] * o,
        ];
        out.insert(n.id, mine);
        for c in &n.children {
            walk(c, mine, out);
        }
    }
    let mut out = std::collections::HashMap::new();
    for r in roots {
        walk(r, [1.0; 4], &mut out);
    }
    out
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

    /// The scroll-view contract in one place: children shift up by the offset,
    /// the view clips its subtree (draw AND hit-test share `scroll_clips`),
    /// and `scroll_max` is exactly content-height − view-height (and 0 when
    /// the content fits — a fitting view must never scroll).
    #[test]
    fn scroll_view_shifts_clips_and_clamps() {
        let row = |y: f32| {
            el(
                ElementSpec {
                    place: Place::Free { pos: [0.0, y] },
                    size: [Size::Fixed(100.0), Size::Fixed(40.0)],
                    shape: Some(ShapeSpec::default()),
                    ..Default::default()
                },
                vec![],
            )
        };
        let rows = [row(0.0), row(50.0), row(100.0), row(150.0)];
        let (r0, r3) = (rows[0].id, rows[3].id);
        let view = el(
            ElementSpec {
                place: Place::Free { pos: [10.0, 20.0] },
                size: [Size::Fixed(120.0), Size::Fixed(100.0)],
                scroll: Some(ScrollSpec { offset: 30.0, speed: 48.0, ..Default::default() }),
                ..Default::default()
            },
            rows.into(),
        );
        let roots = [view];
        let placed = solve(&roots, [1280.0, 720.0], &m);
        // Children shift up by the offset from their authored spots.
        assert_eq!(rect_of(&placed, r0)[1], 20.0 - 30.0);
        assert_eq!(rect_of(&placed, r3)[1], 20.0 + 150.0 - 30.0);
        // Every child clips to the view's rect.
        let clips = scroll_clips(&roots, &placed);
        assert_eq!(clips.get(&r0).map(|c| c.rect), Some([10.0, 20.0, 120.0, 100.0]));
        assert_eq!(clips.get(&r3).map(|c| c.rect), Some([10.0, 20.0, 120.0, 100.0]));
        assert!(!clips.contains_key(&roots[0].id), "the view itself is not clipped");
        // Content is 190 tall in a 100-tall view → 90 of travel, at ANY offset.
        // The rows are 120 wide in a 120-wide view, so there is no X travel —
        // which is also how the wheel knows this view scrolls vertically.
        assert_eq!(scroll_max(&roots, &placed, roots[0].id), [0.0, 90.0]);
        // A view whose content fits has no travel.
        let fits = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(120.0), Size::Fixed(300.0)],
                scroll: Some(ScrollSpec::default()),
                ..Default::default()
            },
            vec![row(0.0)],
        );
        let roots = [fits];
        let placed = solve(&roots, [1280.0, 720.0], &m);
        assert_eq!(scroll_max(&roots, &placed, roots[0].id), [0.0, 0.0]);
    }

    #[test]
    fn a_scrollbar_thumb_shows_position_and_how_much_you_can_see() {
        let m: MeasureText = &|_| [0.0, 0.0];
        let row = |y: f32| {
            el(
                ElementSpec {
                    place: Place::Free { pos: [0.0, y] },
                    size: [Size::Fixed(100.0), Size::Fixed(40.0)],
                    ..Default::default()
                },
                vec![],
            )
        };
        // A 100-tall view over 400 of content: a quarter is visible.
        let view = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(100.0), Size::Fixed(100.0)],
                scroll: Some(ScrollSpec { offset: 150.0, ..Default::default() }),
                ..Default::default()
            },
            vec![row(0.0), row(360.0)],
        );
        let thumb = el(
            ElementSpec {
                part: Some(SliderPart::Handle),
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(8.0), Size::Fixed(10.0)],
                ..Default::default()
            },
            vec![],
        );
        let thumb_id = thumb.id;
        let bar = el(
            ElementSpec {
                place: Place::Free { pos: [120.0, 0.0] },
                size: [Size::Fixed(8.0), Size::Fixed(100.0)],
                scrollbar: Some(ScrollBar { target: "View".into(), axis: Dir::Column }),
                ..Default::default()
            },
            vec![thumb],
        );
        let view_id = view.id;
        let bar_id = bar.id;
        let roots = [view, bar];
        let mut placed = solve(&roots, [1280.0, 720.0], &m);
        place_scrollbars(&roots, &mut placed, &[(bar_id, view_id)]);
        let t = rect_of(&placed, thumb_id);
        // Content 400, view 100 → the thumb is a quarter of the 100-unit track.
        assert!((t[3] - 25.0).abs() < 0.01, "thumb length tracks visible fraction: {t:?}");
        // Offset 150 of 300 travel → halfway down the remaining 75 units.
        assert!((t[1] - 37.5).abs() < 0.01, "thumb position tracks the offset: {t:?}");
        // The cross axis is left exactly as authored — a hairline stays a
        // hairline, and the engine never picks a scrollbar width.
        assert_eq!(t[0], 120.0);
        assert_eq!(t[2], 8.0);
    }

    #[test]
    fn a_scrollbar_over_content_that_fits_is_full_length() {
        let m: MeasureText = &|_| [0.0, 0.0];
        let view = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(100.0), Size::Fixed(200.0)],
                scroll: Some(ScrollSpec::default()),
                ..Default::default()
            },
            vec![el(
                ElementSpec {
                    place: Place::Free { pos: [0.0, 0.0] },
                    size: [Size::Fixed(100.0), Size::Fixed(40.0)],
                    ..Default::default()
                },
                vec![],
            )],
        );
        let thumb = el(
            ElementSpec {
                part: Some(SliderPart::Handle),
                size: [Size::Fixed(8.0), Size::Fixed(10.0)],
                ..Default::default()
            },
            vec![],
        );
        let thumb_id = thumb.id;
        let bar = el(
            ElementSpec {
                place: Place::Free { pos: [120.0, 0.0] },
                size: [Size::Fixed(8.0), Size::Fixed(200.0)],
                scrollbar: Some(ScrollBar { target: "View".into(), axis: Dir::Column }),
                ..Default::default()
            },
            vec![thumb],
        );
        let (view_id, bar_id) = (view.id, bar.id);
        let roots = [view, bar];
        let mut placed = solve(&roots, [1280.0, 720.0], &m);
        place_scrollbars(&roots, &mut placed, &[(bar_id, view_id)]);
        let t = rect_of(&placed, thumb_id);
        assert!((t[3] - 200.0).abs() < 0.01, "full track when it all fits: {t:?}");
        assert_eq!(t[1], 0.0);
    }

    #[test]
    fn a_scroll_view_scrolls_sideways_too() {
        let m: MeasureText = &|_| [0.0, 0.0];
        let card = |x: f32| {
            el(
                ElementSpec {
                    place: Place::Free { pos: [x, 0.0] },
                    size: [Size::Fixed(200.0), Size::Fixed(80.0)],
                    ..Default::default()
                },
                vec![],
            )
        };
        let cards = [card(0.0), card(220.0), card(440.0)];
        let first = cards[0].id;
        let view = el(
            ElementSpec {
                place: Place::Free { pos: [10.0, 10.0] },
                size: [Size::Fixed(300.0), Size::Fixed(80.0)],
                scroll: Some(ScrollSpec { offset_x: 120.0, ..Default::default() }),
                ..Default::default()
            },
            cards.into(),
        );
        let roots = [view];
        let placed = solve(&roots, [1280.0, 720.0], &m);
        // Content slid LEFT by the horizontal offset; vertical is untouched.
        assert_eq!(rect_of(&placed, first)[0], 10.0 - 120.0);
        assert_eq!(rect_of(&placed, first)[1], 10.0);
        // 640 of content in a 300 view → 340 of horizontal travel, no vertical.
        assert_eq!(scroll_max(&roots, &placed, roots[0].id), [340.0, 0.0]);
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
                image: Some(ImageSpec { texture: "textures/Grass.png".into(), ..Default::default() }),
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
        // A plain (1×1) image samples the whole texture.
        assert_eq!(dl.quads[1].uv, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn spritesheet_cell_uv_walks_the_grid() {
        // 4×2 sheet: cell 0 = top-left quarter-width/half-height, cell 5 = row 1 col 1.
        let mut img = ImageSpec { texture: "sheet.png".into(), cols: 4, rows: 2, ..Default::default() };
        assert_eq!(img.cell_uv(), [0.0, 0.0, 0.25, 0.5]);
        img.cell = 5; // row 1 (5/4), col 1 (5%4)
        assert_eq!(img.cell_uv(), [0.25, 0.5, 0.5, 1.0]);
        img.cell = 99; // clamps to the last cell (7)
        assert_eq!(img.cell_uv(), [0.75, 0.5, 1.0, 1.0]);
        // 1×1 = whole texture regardless of cell.
        let plain = ImageSpec { texture: "x".into(), cell: 3, ..Default::default() };
        assert_eq!(plain.cell_uv(), [0.0, 0.0, 1.0, 1.0]);
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
                shape: Some(ShapeSpec { radius: 12.0.into(), ..Default::default() }),
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
        assert_eq!(spec.min_size, [0.0, 0.0]);
        assert_eq!(spec.max_size, [0.0, 0.0]);
        assert_eq!(spec.scale, [1.0, 1.0], "the transform starts neutral");
        assert_eq!(spec.tint, [1.0; 4]);
    }

    // -----------------------------------------------------------------------
    // The paint box (docs/ui-system-2-proposal.md §A)
    // -----------------------------------------------------------------------

    /// A shape with a fill, so cascade tests have something coloured to look at.
    fn painted(fill: [f32; 4]) -> Option<ShapeSpec> {
        Some(ShapeSpec { fill, ..Default::default() })
    }

    /// THE headline behaviour change: `opacity` used to be self-only, so a
    /// parent could not fade its children and projects parked a black rect over
    /// the screen instead.
    #[test]
    fn opacity_cascades_to_descendants() {
        let child = el(
            ElementSpec {
                size: [Size::Fixed(10.0), Size::Fixed(10.0)],
                shape: painted([1.0, 1.0, 1.0, 1.0]),
                opacity: 0.5,
                ..Default::default()
            },
            vec![],
        );
        let cid = child.id;
        let parent = el(
            ElementSpec {
                size: [Size::Fixed(100.0), Size::Fixed(100.0)],
                opacity: 0.5,
                ..Default::default()
            },
            vec![child],
        );
        let placed = solve(std::slice::from_ref(&parent), [1280.0, 720.0], &m);
        let dl = draw_list(&[parent], &placed, &[]);
        let q = dl.quads.iter().find(|_| true).unwrap();
        assert_eq!(q.rect[2], 10.0, "the only shape is the child's");
        // 0.5 (parent) × 0.5 (own) — multiplicative, like every other engine.
        assert!((q.color[3] - 0.25).abs() < 1e-6, "got {}", q.color[3]);
        let _ = cid;
    }

    /// `tint` multiplies RGB down the subtree — a group flash without touching
    /// each child's own colour.
    #[test]
    fn tint_cascades_and_multiplies_rgb() {
        let child = el(
            ElementSpec {
                size: [Size::Fixed(10.0), Size::Fixed(10.0)],
                shape: painted([1.0, 1.0, 1.0, 1.0]),
                ..Default::default()
            },
            vec![],
        );
        let parent = el(
            ElementSpec {
                size: [Size::Fixed(100.0), Size::Fixed(100.0)],
                tint: [1.0, 0.0, 0.0, 1.0],
                ..Default::default()
            },
            vec![child],
        );
        let placed = solve(std::slice::from_ref(&parent), [1280.0, 720.0], &m);
        let dl = draw_list(&[parent], &placed, &[]);
        let q = &dl.quads[0];
        assert_eq!([q.color[0], q.color[1], q.color[2]], [1.0, 0.0, 0.0]);
    }

    /// Glow behind, fill next, inset shadow on top — the order that makes an
    /// inset read as a hole in the surface rather than something behind it.
    #[test]
    fn shape_layers_emit_back_to_front() {
        let n = el(
            ElementSpec {
                size: [Size::Fixed(100.0), Size::Fixed(40.0)],
                shape: Some(ShapeSpec {
                    fill: [0.2, 0.2, 0.2, 1.0],
                    glow: Some(GlowSpec::default()),
                    shadow: Some(ShadowSpec { inset: true, ..Default::default() }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vec![],
        );
        let placed = solve(std::slice::from_ref(&n), [1280.0, 720.0], &m);
        let dl = draw_list(&[n], &placed, &[]);
        let kinds: Vec<QuadKind> = dl.quads.iter().map(|q| q.kind).collect();
        assert_eq!(kinds, vec![QuadKind::Shadow, QuadKind::Shape, QuadKind::InsetShadow]);
        assert_eq!(dl.quads[0].blend, Blend::Additive, "glow adds light");
    }

    /// An outer shadow's spread has to open the corner radii too, or a spread
    /// shadow pinches at the corners and reads as a misaligned second rect.
    #[test]
    fn spread_opens_the_shadow_corners() {
        let n = el(
            ElementSpec {
                size: [Size::Fixed(100.0), Size::Fixed(40.0)],
                shape: Some(ShapeSpec {
                    radius: Corners::all(6.0),
                    shadow: Some(ShadowSpec {
                        spread: 4.0,
                        offset: [0.0, 0.0],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vec![],
        );
        let placed = solve(std::slice::from_ref(&n), [1280.0, 720.0], &m);
        let dl = draw_list(&[n], &placed, &[]);
        let sh = &dl.quads[0];
        assert_eq!(sh.kind, QuadKind::Shadow);
        assert_eq!(sh.rect[2], 108.0, "grew by spread on both sides");
        assert_eq!(sh.radius, [10.0; 4]);
    }

    #[test]
    fn per_corner_radius_and_per_side_border_reach_the_draw_list() {
        let n = el(
            ElementSpec {
                size: [Size::Fixed(100.0), Size::Fixed(40.0)],
                shape: Some(ShapeSpec {
                    radius: Corners([12.0, 12.0, 0.0, 0.0]),
                    border: Sides([0.0, 0.0, 0.0, 2.0]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vec![],
        );
        let placed = solve(std::slice::from_ref(&n), [1280.0, 720.0], &m);
        let dl = draw_list(&[n], &placed, &[]);
        assert_eq!(dl.quads[0].radius, [12.0, 12.0, 0.0, 0.0]);
        assert_eq!(dl.quads[0].border, [0.0, 0.0, 0.0, 2.0]);
    }

    /// The case transform is resolved into the draw list, so the renderer never
    /// re-derives it and the authored string stays what the designer typed.
    #[test]
    fn case_is_applied_for_drawing_but_not_stored() {
        let spec = TextSpec { text: "menu".into(), case: Case::Upper, ..Default::default() };
        let n = el(ElementSpec { text: Some(spec.clone()), ..Default::default() }, vec![]);
        let placed = solve(std::slice::from_ref(&n), [1280.0, 720.0], &m);
        let dl = draw_list(&[n], &placed, &[]);
        assert_eq!(dl.texts[0].text, "MENU");
        assert_eq!(spec.text, "menu", "the scene keeps the authored string");
    }

    /// Tiling a spritesheet cell would sample its neighbours — a silent, ugly
    /// failure. The cell wins.
    #[test]
    fn tiling_is_ignored_on_a_spritesheet() {
        let sheet = ImageSpec {
            cols: 4,
            rows: 1,
            cell: 1,
            tiling: [3.0, 3.0],
            ..Default::default()
        };
        assert_eq!(sheet.tiled_uv(), sheet.cell_uv());
        let plain = ImageSpec { tiling: [3.0, 2.0], ..Default::default() };
        assert_eq!(plain.tiled_uv(), [0.0, 0.0, 3.0, 2.0]);
    }

    /// Every scene in Ty's projects was authored against the first cut. They
    /// must load with no edits — this is the exact shape RON from
    /// `Fofighter/scenes/menu.ron`.
    #[test]
    fn first_cut_scenes_still_parse() {
        let old = r#"(
            place: Free(pos: (330.0, 196.0)),
            size: (Fixed(620.0), Fixed(430.0)),
            shape: Some((
                fill: (0.043, 0.051, 0.086, 0.970),
                radius: 14.0,
                border: 2.0,
                border_color: (0.620, 0.520, 0.240, 1.000),
            )),
            visible: true,
            opacity: 1.00,
        )"#;
        let spec: ElementSpec = ron::from_str(old).unwrap();
        let s = spec.shape.unwrap();
        assert_eq!(s.radius.0, [14.0; 4]);
        assert_eq!(s.border.0, [2.0; 4]);
        assert!(s.gradient.is_none() && s.glow.is_none() && s.grain.is_none());
        assert_eq!(spec.scale, [1.0, 1.0], "new transform fields default to neutral");
        assert_eq!(spec.tint, [1.0; 4]);
    }

    /// …and a text element from the same file, now that TextSpec has grown
    /// seven fields.
    #[test]
    fn first_cut_text_still_parses() {
        let old = r#"(
            text: "an arcade fighter built on parries",
            size: 16.0,
            color: (0.560, 0.610, 0.740, 1.000),
            align: Center,
            valign: Center,
            fit: false,
        )"#;
        let t: TextSpec = ron::from_str(old).unwrap();
        assert_eq!(t.size, 16.0);
        assert_eq!(t.tracking, 0.0);
        assert_eq!(t.overflow, Overflow::Show);
        assert_eq!(t.case, Case::AsIs);
        assert!(t.stroke.is_none());
    }

    /// Untouched extras must not appear in saved scenes, or every save churns
    /// the whole file and the diff stops being reviewable.
    ///
    /// Checked against a REAL element shape (shape + text + image all present),
    /// because the first version of this test only covered `ShapeSpec` and
    /// happily let five new `TextSpec` fields into every save.
    #[test]
    fn unused_extras_do_not_serialize() {
        let text = ron::to_string(&ElementSpec {
            shape: Some(ShapeSpec::default()),
            text: Some(TextSpec { text: "hi".into(), ..Default::default() }),
            image: Some(ImageSpec { texture: "t.png".into(), ..Default::default() }),
            ..Default::default()
        })
        .unwrap();
        for absent in [
            // ShapeSpec
            "gradient", "glow", "grain", "blend",
            // ElementSpec transform
            "rotation", "scale", "pivot",
            // TextSpec
            "stroke", "shadow", "tracking", "line_height", "wrap", "max_lines", "overflow",
            "case",
            // ImageSpec. `tint` is pre-existing and always written, so it is
            // checked separately below where no image can supply it; the image
            // fit is matched by its value because TextSpec has a `fit` too.
            "slice", "tiling", "offset", "fit:Stretch",
            // Behaviour (phases C + D). Matched as `,key:` / `(key:` further
            // down, because "order" is a substring of "border" and a naive
            // `contains` would fire on a shape that is behaving perfectly.
        ] {
            assert!(!text.contains(absent), "`{absent}` leaked into {text}");
        }
        // Key-position match: a field name only counts when it's actually a key.
        let has_key = |text: &str, k: &str| {
            text.contains(&format!(",{k}:")) || text.contains(&format!("({k}:"))
        };
        for absent in [
            "order",
            "focusable",
            "nav",
            "toggle",
            "group",
            "scrollbar",
            // Phase D part 3.
            "field",
            "draggable",
            "drop_target",
            "tooltip",
            "tooltip_box",
            // Phase E.
            "repeater",
        ] {
            assert!(!has_key(&text, absent), "`{absent}` leaked into {text}");
        }
        // The element-level group tint, on an element with no image to confuse
        // the search.
        let bare = ron::to_string(&ElementSpec {
            shape: Some(ShapeSpec::default()),
            ..Default::default()
        })
        .unwrap();
        assert!(!bare.contains("tint"), "the group tint leaked into {bare}");

        // A default LAYER writes no new keys either. This is the one that got
        // away: `nav_delay` / `nav_repeat` had `serde(default)` but no
        // `skip_serializing_if`, so every saved scene grew two lines per layer.
        // Caught by round-tripping the real projects, not by this file — which
        // is why it is now also caught by this file.
        let layer = ron::to_string(&UiLayer::default()).unwrap();
        for absent in ["nav_delay", "nav_repeat", "nav_wrap", "tooltip_delay"] {
            assert!(!layer.contains(absent), "`{absent}` leaked into {layer}");
        }
        // …and a non-default one still round-trips.
        let tuned = UiLayer { nav_delay: 0.2, nav_repeat: 0.05, nav_wrap: true, ..Default::default() };
        let back: UiLayer = ron::from_str(&ron::to_string(&tuned).unwrap()).unwrap();
        assert_eq!(back, tuned);

        // Same for a default scroll view: `offset_x` and `drag` are additive.
        let scroller = ron::to_string(&ElementSpec {
            scroll: Some(ScrollSpec::default()),
            ..Default::default()
        })
        .unwrap();
        for absent in ["offset_x", "drag"] {
            assert!(!scroller.contains(absent), "`{absent}` leaked into {scroller}");
        }

        // A default FIELD writes only the fact that it is one — the caret
        // width, the mask character and the three "follow the text colour"
        // sentinels all stay out of the file.
        let f = ron::to_string(&ElementSpec {
            field: Some(FieldSpec::default()),
            ..Default::default()
        })
        .unwrap();
        for absent in [
            "placeholder",
            "max_len",
            "mask",
            "mask_char",
            "numeric",
            "upper",
            "caret_color",
            "selection_color",
            "placeholder_color",
            "caret_width",
        ] {
            assert!(!f.contains(absent), "`{absent}` leaked into {f}");
        }
        // …and a configured one still round-trips through the sentinels.
        let tuned = FieldSpec {
            placeholder: "code".into(),
            max_len: 8,
            mask: true,
            mask_char: '*',
            upper: true,
            caret_color: [1.0, 0.2, 0.4, 1.0],
            ..Default::default()
        };
        let back: FieldSpec = ron::from_str(&ron::to_string(&tuned).unwrap()).unwrap();
        assert_eq!(back, tuned);
    }

    /// Build one field element and draw it, with and without a caret.
    fn field_dl(value: &str, f: FieldSpec, edit: Option<EditState>) -> DrawList {
        let n = Node::with_children(
            1,
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(200.0), Size::Fixed(40.0)],
                text: Some(TextSpec { text: value.into(), ..Default::default() }),
                field: Some(f),
                ..Default::default()
            },
            vec![],
        );
        let m: MeasureText = &|_| [0.0, 0.0];
        let placed = solve(std::slice::from_ref(&n), [400.0, 300.0], m);
        draw_list_with(std::slice::from_ref(&n), &placed, &[], edit)
    }

    #[test]
    fn an_empty_field_shows_its_placeholder_until_you_start_typing() {
        let spec = FieldSpec { placeholder: "Lobby code".into(), ..Default::default() };
        let dl = field_dl("", spec.clone(), None);
        assert_eq!(dl.texts[0].text, "Lobby code");
        assert!(dl.texts[0].caret.is_none());
        // Focused, the hint gets out of the way — otherwise the caret sits in
        // the middle of a word the player did not type.
        let editing = field_dl("", spec, Some(EditState { id: 1, caret: 0, anchor: 0, on: true }));
        assert_eq!(editing.texts[0].text, "");
        assert!(editing.texts[0].caret.is_some(), "an empty field still needs somewhere to blink");
    }

    #[test]
    fn a_masked_field_draws_dots_and_the_caret_counts_them() {
        let spec = FieldSpec { mask: true, ..Default::default() };
        let dl =
            field_dl("hunter2", spec, Some(EditState { id: 1, caret: 7, anchor: 4, on: true }));
        assert_eq!(dl.texts[0].text, "•••••••");
        let c = dl.texts[0].caret.expect("caret");
        assert_eq!((c.index, c.anchor), (7, 4), "indices count the DRAWN characters");
    }

    #[test]
    fn a_field_clips_to_itself_even_with_no_mask_around_it() {
        // The value is the player's, so it can be longer than the box. Text
        // running out of a box and across the screen is never what was meant.
        let dl = field_dl("x", FieldSpec::default(), None);
        let clip = dl.texts[0].clip.expect("a field always clips");
        assert_eq!(clip.rect, [0.0, 0.0, 200.0, 40.0]);
        // An ordinary label is untouched by this.
        let label = Node::with_children(
            2,
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                text: Some(TextSpec { text: "hi".into(), ..Default::default() }),
                ..Default::default()
            },
            vec![],
        );
        let m: MeasureText = &|_| [0.0, 0.0];
        let placed = solve(std::slice::from_ref(&label), [400.0, 300.0], m);
        let dl = draw_list(std::slice::from_ref(&label), &placed, &[]);
        assert!(dl.texts[0].clip.is_none());
    }

    #[test]
    fn the_caret_colours_follow_the_text_unless_you_say_otherwise() {
        // "Alpha 0 = derive" is the rule that keeps the engine out of the
        // business of picking a caret colour.
        let dl = field_dl(
            "abc",
            FieldSpec::default(),
            Some(EditState { id: 1, caret: 1, anchor: 3, on: true }),
        );
        let t = &dl.texts[0];
        let c = t.caret.expect("caret");
        assert_eq!(c.color, t.color, "the caret is the text colour by default");
        assert!(
            (c.selection[3] - t.color[3] * 0.3).abs() < 1e-6,
            "and the selection is that colour, quieter"
        );
        // An explicit colour wins, obviously.
        let spec = FieldSpec { caret_color: [1.0, 0.0, 0.0, 1.0], ..Default::default() };
        let dl = field_dl("abc", spec, Some(EditState { id: 1, caret: 1, anchor: 1, on: true }));
        assert_eq!(dl.texts[0].caret.unwrap().color, [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_caret_past_the_end_is_clamped_rather_than_drawn_off_the_word() {
        // A script can assign `text` while the player is mid-edit.
        let dl = field_dl(
            "ab",
            FieldSpec::default(),
            Some(EditState { id: 1, caret: 99, anchor: 99, on: true }),
        );
        let c = dl.texts[0].caret.unwrap();
        assert_eq!((c.index, c.anchor), (2, 2));
    }

    #[test]
    fn stretch_fills_parent_with_margins_at_any_size() {
        // "16 in from every edge" must track the parent, not stay a fixed box.
        let child = el(
            ElementSpec { place: Place::fill(16.0), ..Default::default() },
            vec![],
        );
        let cid = child.id;
        let parent = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(400.0), Size::Fixed(300.0)],
                ..Default::default()
            },
            vec![child],
        );
        let roots = [parent];
        let placed = solve(&roots, [1000.0, 800.0], &m);
        // Inset 16 on each side of a 400×300 parent → [16,16, 368,268].
        assert_eq!(rect_of(&placed, cid), [16.0, 16.0, 368.0, 268.0]);
    }

    #[test]
    fn stretch_point_axis_keeps_its_own_size() {
        // A bottom bar: stretch across x, fixed height pinned to the bottom edge.
        let bar = el(
            ElementSpec {
                place: Place::Stretch { min: [0.0, 1.0], max: [1.0, 1.0], margin: [8.0, 0.0, 8.0, 8.0] },
                size: [Size::Fit, Size::Fixed(48.0)],
                ..Default::default()
            },
            vec![],
        );
        let bid = bar.id;
        let parent = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(600.0), Size::Fixed(400.0)],
                ..Default::default()
            },
            vec![bar],
        );
        let roots = [parent];
        let placed = solve(&roots, [1280.0, 720.0], &m);
        let r = rect_of(&placed, bid);
        assert_eq!(r[2], 600.0 - 16.0, "stretched x fills width minus L+R margin");
        assert_eq!(r[3], 48.0, "point y keeps the fixed height");
        assert_eq!(r[0], 8.0, "left margin");
        // Anchored to the bottom line (y=1.0 of 400) + leading (top) margin 0.
        assert_eq!(r[1], 400.0);
    }

    #[test]
    fn min_max_size_clamp_resolved_size() {
        let clamped = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Pct(0.5), Size::Pct(0.5)],
                min_size: [200.0, 0.0],
                max_size: [0.0, 100.0],
                ..Default::default()
            },
            vec![],
        );
        let id = clamped.id;
        // Parent 300×400: 50% = 150×200 → clamped to min-w 200 and max-h 100.
        let parent = el(
            ElementSpec {
                place: Place::Free { pos: [0.0, 0.0] },
                size: [Size::Fixed(300.0), Size::Fixed(400.0)],
                ..Default::default()
            },
            vec![clamped],
        );
        let roots = [parent];
        let placed = solve(&roots, [1280.0, 720.0], &m);
        let r = rect_of(&placed, id);
        assert_eq!(r[2], 200.0, "width floored to min");
        assert_eq!(r[3], 100.0, "height capped to max");
    }

    #[test]
    fn canvas_scale_modes() {
        let layer = |mode: UiScaleMode| UiLayer {
            design_height: 720.0,
            reference_width: 1280.0,
            scale_mode: mode,
            match_wh: 0.5,
            ..Default::default()
        };
        // Match-height: scale = h/refH regardless of width (the classic default).
        assert_eq!(layer(UiScaleMode::MatchHeight).scale_for([1920.0, 1440.0]), 2.0);
        assert_eq!(layer(UiScaleMode::MatchHeight).scale_for([100.0, 1440.0]), 2.0);
        // Match-width: scale = w/refW.
        assert_eq!(layer(UiScaleMode::MatchWidth).scale_for([2560.0, 100.0]), 2.0);
        // Expand fits inside (min); Shrink fills (max).
        let vp = [1280.0, 1440.0]; // by_w = 1, by_h = 2
        assert_eq!(layer(UiScaleMode::Expand).scale_for(vp), 1.0);
        assert_eq!(layer(UiScaleMode::Shrink).scale_for(vp), 2.0);
        // Constant px never rescales.
        assert_eq!(layer(UiScaleMode::ConstantPixels).scale_for([9999.0, 9999.0]), 1.0);
        // Blend at 0.5 is the geometric mean of by_w and by_h (1 and 2 → √2).
        let b = layer(UiScaleMode::Blend).scale_for([1280.0, 1440.0]);
        assert!((b - 2.0f32.sqrt()).abs() < 1e-4, "blend geo-mean, got {b}");
        // Degenerate viewport never yields a non-positive scale.
        assert!(layer(UiScaleMode::MatchHeight).scale_for([0.0, 0.0]) > 0.0);
    }

    #[test]
    fn stretch_place_round_trips_and_defaults_omit() {
        // New fields must not appear in a default element's RON (back-compat).
        let text = ron::ser::to_string(&ElementSpec::default()).unwrap();
        assert!(!text.contains("min_size"), "default min_size omitted: {text}");
        assert!(!text.contains("max_size"), "default max_size omitted: {text}");
        // Stretch + clamps survive a round-trip.
        let spec = ElementSpec {
            place: Place::Stretch { min: [0.0, 0.0], max: [1.0, 0.5], margin: [4.0, 8.0, 4.0, 0.0] },
            min_size: [10.0, 20.0],
            max_size: [300.0, 0.0],
            ..Default::default()
        };
        let t = ron::ser::to_string(&spec).unwrap();
        let back: ElementSpec = ron::from_str(&t).unwrap();
        assert_eq!(back, spec);
        // An old layer RON (no scaler fields) loads with the classic defaults.
        let old: UiLayer = ron::from_str("(design_height: 720.0, z: 0)").unwrap();
        assert_eq!(old.scale_mode, UiScaleMode::MatchHeight);
        assert_eq!(old.reference_width, 1280.0);
    }

    #[test]
    fn order_sorts_siblings_and_ties_keep_scene_order() {
        let el = |id: u32, order: i32| {
            Node::with_children(id, ElementSpec { order, ..Default::default() }, vec![])
        };
        // Built in scene order 1,2,3,4 with orders 5,0,0,-5.
        let root = Node::with_children(
            0,
            ElementSpec::default(),
            vec![el(1, 5), el(2, 0), el(3, 0), el(4, -5)],
        );
        let ids: Vec<u32> = root.children.iter().map(|c| c.id).collect();
        // Sorted by order; the two ties (2, 3) keep the order they were built in.
        assert_eq!(ids, vec![4, 2, 3, 1]);

        // An untouched tree is EXACTLY scene order — the back-compat guarantee.
        let plain = Node::with_children(
            0,
            ElementSpec::default(),
            vec![el(7, 0), el(3, 0), el(9, 0), el(1, 0)],
        );
        let ids: Vec<u32> = plain.children.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![7, 3, 9, 1]);

        // And `order: 0` never reaches a saved scene.
        let text = ron::ser::to_string(&ElementSpec::default()).unwrap();
        assert!(!text.contains("order"), "default order omitted: {text}");
    }
}
