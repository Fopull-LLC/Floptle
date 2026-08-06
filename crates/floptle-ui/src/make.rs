//! `ui.make` — a UI tree described as data, reconciled against the one on
//! screen.
//!
//! The case this exists for: a screen whose SHAPE depends on data. A roster of
//! four fighters or nine, an inventory of whatever the player is carrying, a
//! lobby list that arrives over the wire. The scene file can't hold a tree that
//! doesn't exist yet, so both projects built on the engine solved it the same
//! way — a fixed pool of `Icon1`…`Icon8` nodes, positioned by hand-written
//! centring arithmetic and shown/hidden per frame. That is the pain the whole
//! UI arc opened with, moved out of the Inspector and into Lua.
//!
//! Two properties make this a builder rather than a node factory:
//!
//! 1. **It reconciles.** Calling it again with different data spawns and
//!    destroys only the DIFFERENCE, so a list that gains a row keeps the other
//!    nine — with their scroll position, their in-flight style transitions and
//!    what the player typed into them. A builder that rebuilt the subtree would
//!    make a screen that flickers and forgets, which is exactly the hand-rolled
//!    behaviour it replaces.
//! 2. **The description is authoritative.** A property the table stops
//!    mentioning goes back to the element's default, because otherwise removing
//!    a line from your table leaves its effect on screen forever. The exception
//!    is state the PLAYER owns rather than the description — scroll offset, a
//!    field's typed value, a toggle's selection, a draggable slider's value —
//!    which is carried across (see [`MadeNode::rebuild`]).
//!
//! This module is the headless half: the description type, the property
//! vocabulary, and the diff. Reading a Lua table into a [`MadeNode`] is
//! `floptle-script`'s job, and doing what the [`Op`]s say is the editor's.

use crate::{
    Align, Anchor, Case, Dir, ElementSpec, FieldSpec, ImageSpec, Justify, Overflow, Place,
    RepeatSpec, ScrollBar, ScrollSpec, ShapeSpec, Size, SliderPart, SliderSpec, StackCfg,
    TextSpec,
};
use crate::paint::{Corners, ImageFit, Sides};

/// What sort of element a described node is: which sub-specs it starts with,
/// and nothing else. Every kind is reachable from `Box` by setting properties
/// — `col` is a box with a stack, `button` is a box that takes clicks — so the
/// kind is shorthand, never a separate class of thing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// A plain element: a rounded rect, transparent until something paints it.
    #[default]
    Box,
    /// A box whose children flow left to right.
    Row,
    /// A box whose children flow top to bottom.
    Col,
    /// A text run. No shape — a label that painted a white slab behind itself
    /// would be an imposed look.
    Text,
    /// A textured rect.
    Image,
    /// A box that takes clicks, and that a direction press can reach.
    Button,
    /// An editable text field (implicitly focusable).
    Field,
    /// A value track: its `part` children fill and ride.
    Slider,
    /// A clipped, scrollable view of its children.
    Scroll,
}

impl Kind {
    /// The kind named by a table's first entry. Unknown names are `None` so
    /// the caller can say which one it didn't recognise.
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "box" => Kind::Box,
            "row" => Kind::Row,
            "col" | "column" => Kind::Col,
            "text" | "label" => Kind::Text,
            "image" => Kind::Image,
            "button" => Kind::Button,
            "field" => Kind::Field,
            "slider" => Kind::Slider,
            "scroll" => Kind::Scroll,
            _ => return None,
        })
    }

    /// Every kind's name, for the error message that lists them.
    pub const NAMES: &'static [&'static str] = &[
        "box", "row", "col", "text", "image", "button", "field", "slider", "scroll",
    ];

    /// The spec a node of this kind starts from, before its properties.
    pub fn base(self) -> ElementSpec {
        // Transparent, not white. A made box with no fill must be invisible:
        // `ShapeSpec::default()` is opaque white, and a builder that painted
        // white slabs until told otherwise would be choosing a look.
        let clear = || ShapeSpec { fill: [0.0; 4], ..Default::default() };
        let stack = |dir| StackCfg { dir, ..Default::default() };
        let mut spec = ElementSpec::default();
        match self {
            Kind::Box => spec.shape = Some(clear()),
            Kind::Row => {
                spec.shape = Some(clear());
                spec.stack = Some(stack(Dir::Row));
            }
            Kind::Col => {
                spec.shape = Some(clear());
                spec.stack = Some(stack(Dir::Column));
            }
            Kind::Text => spec.text = Some(TextSpec::default()),
            Kind::Image => spec.image = Some(ImageSpec::default()),
            Kind::Button => {
                spec.shape = Some(clear());
                spec.button = true;
                // Reachable by pad and keyboard unless you say otherwise. This
                // is a behaviour default, not a look: what focus LOOKS like is
                // still entirely the style's business.
                spec.focusable = true;
            }
            Kind::Field => {
                spec.shape = Some(clear());
                spec.text = Some(TextSpec::default());
                spec.field = Some(FieldSpec::default());
            }
            Kind::Slider => {
                spec.shape = Some(clear());
                spec.slider = Some(SliderSpec::default());
            }
            Kind::Scroll => {
                spec.shape = Some(clear());
                spec.scroll = Some(ScrollSpec::default());
            }
        }
        spec
    }
}

/// A property value as the description carries it. Deliberately loose: Lua
/// hands over whatever it hands over, and every reading below coerces rather
/// than refusing, so a `1` where a `true` was meant does the obvious thing.
#[derive(Clone, Debug, PartialEq)]
pub enum PropVal {
    Num(f32),
    Bool(bool),
    Str(String),
    Color([f32; 4]),
    /// A number list — `radius = {8, 8, 0, 0}`, `scale = {1.1, 1.1}`.
    List(Vec<f32>),
}

impl PropVal {
    fn num(&self) -> f32 {
        match self {
            PropVal::Num(v) => *v,
            PropVal::Bool(b) => f32::from(u8::from(*b)),
            PropVal::Str(s) => s.parse().unwrap_or(0.0),
            PropVal::Color(c) => c[0],
            PropVal::List(v) => v.first().copied().unwrap_or(0.0),
        }
    }

    fn bool(&self) -> bool {
        match self {
            PropVal::Bool(b) => *b,
            PropVal::Num(v) => *v != 0.0,
            PropVal::Str(s) => !s.is_empty() && s != "false",
            PropVal::Color(c) => c[3] != 0.0,
            PropVal::List(v) => !v.is_empty(),
        }
    }

    fn text(&self) -> String {
        match self {
            PropVal::Str(s) => s.clone(),
            PropVal::Num(v) if v.fract() == 0.0 => format!("{}", *v as i64),
            PropVal::Num(v) => format!("{v}"),
            PropVal::Bool(b) => b.to_string(),
            PropVal::Color(_) | PropVal::List(_) => String::new(),
        }
    }

    fn color(&self) -> [f32; 4] {
        match self {
            PropVal::Color(c) => *c,
            // A bare number is a grey, matching `color(gray)` in Lua.
            PropVal::Num(v) => [*v, *v, *v, 1.0],
            PropVal::Str(s) => hex(s).unwrap_or([0.0, 0.0, 0.0, 1.0]),
            PropVal::List(v) => [
                v.first().copied().unwrap_or(0.0),
                v.get(1).copied().unwrap_or(0.0),
                v.get(2).copied().unwrap_or(0.0),
                v.get(3).copied().unwrap_or(1.0),
            ],
            PropVal::Bool(_) => [1.0; 4],
        }
    }

    /// Four numbers from a scalar (all four) or a list (padded with the last).
    fn quad(&self) -> [f32; 4] {
        match self {
            PropVal::List(v) if !v.is_empty() => {
                let last = *v.last().unwrap();
                [
                    v[0],
                    v.get(1).copied().unwrap_or(last),
                    v.get(2).copied().unwrap_or(last),
                    v.get(3).copied().unwrap_or(last),
                ]
            }
            other => [other.num(); 4],
        }
    }

    /// Two numbers from a scalar (both) or a list.
    fn pair(&self) -> [f32; 2] {
        match self {
            PropVal::List(v) if !v.is_empty() => [v[0], v.get(1).copied().unwrap_or(v[0])],
            other => [other.num(); 2],
        }
    }

    /// A size axis: a number is design units, `"50%"` is a fraction of the
    /// parent, `"grow"` / `"grow 2"` shares the leftover, `"fit"` wraps the
    /// content. The string forms exist because `Size` is an enum and a table
    /// of numbers can't say which variant it meant.
    fn size(&self) -> Size {
        let PropVal::Str(s) = self else { return Size::Fixed(self.num()) };
        let s = s.trim();
        if let Some(p) = s.strip_suffix('%') {
            return Size::Pct(p.trim().parse::<f32>().unwrap_or(0.0) / 100.0);
        }
        if let Some(rest) = s.strip_prefix("grow") {
            return Size::Grow(rest.trim().parse::<f32>().unwrap_or(1.0));
        }
        match s {
            "fit" => Size::Fit,
            _ => Size::Fixed(s.parse().unwrap_or(0.0)),
        }
    }
}

/// `#rrggbb` / `#rrggbbaa`, the same forms `color.hex` takes.
fn hex(s: &str) -> Option<[f32; 4]> {
    let h = s.trim().trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok().map(|v| v as f32 / 255.0);
    match h.len() {
        6 | 8 => Some([
            byte(0)?,
            byte(2)?,
            byte(4)?,
            if h.len() == 8 { byte(6)? } else { 1.0 },
        ]),
        _ => None,
    }
}

/// One described element: what it is, what it looks like, and what is inside
/// it. The behaviour hooks (`onClicked` and friends) are NOT here — they are
/// Lua functions, and this crate has never heard of Lua. The parser keeps them
/// beside the tree, addressed by path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MadeNode {
    pub kind: Kind,
    /// Reconciliation identity. Two calls agree about "the same element" by
    /// key when there is one, and by position when there isn't — so a list
    /// keyed by item id survives a re-sort, and an unkeyed one doesn't need to
    /// think about it.
    pub key: String,
    /// The node's name in the hierarchy. Empty = derived from kind + position.
    pub name: String,
    pub props: Vec<(String, PropVal)>,
    pub children: Vec<MadeNode>,
}

impl MadeNode {
    /// Whether the description sets this property.
    pub fn mentions(&self, prop: &str) -> bool {
        self.props.iter().any(|(k, _)| k == prop)
    }

    /// The spec this description asks for, from scratch.
    pub fn build(&self) -> ElementSpec {
        let mut spec = self.kind.base();
        // Two passes so the table's key order can't matter. The first sets the
        // placement MODE and the second fills in numbers within it; written as
        // one pass, `{ margin = 8, inset = 0 }` and `{ inset = 0, margin = 8 }`
        // would quietly mean different things.
        for (name, v) in self.props.iter().filter(|(n, _)| is_place_mode(n)) {
            apply_prop(&mut spec, name, v);
        }
        for (name, v) in self.props.iter().filter(|(n, _)| !is_place_mode(n)) {
            apply_prop(&mut spec, name, v);
        }
        spec
    }

    /// The spec for an element that already exists — [`build`](Self::build),
    /// then the state the player owns carried across.
    ///
    /// Everything the description doesn't mention resets, on purpose. These
    /// four don't, because none of them is something the description SAID: a
    /// scroll position, what was typed into a field, which chip is selected,
    /// and where a draggable slider was left are the player's answers, and a
    /// re-render that threw them away would make the screen fight its user.
    pub fn rebuild(&self, old: &ElementSpec) -> ElementSpec {
        let mut new = self.build();
        if let (Some(o), Some(n)) = (old.scroll, new.scroll.as_mut()) {
            if !self.mentions("scrollY") {
                n.offset = o.offset;
            }
            if !self.mentions("scrollX") {
                n.offset_x = o.offset_x;
            }
        }
        if new.field.is_some()
            && !self.mentions("text")
            && let (Some(o), Some(n)) = (&old.text, new.text.as_mut())
        {
            n.text.clone_from(&o.text);
        }
        if (new.toggle || !new.group.is_empty()) && !self.mentions("selected") {
            new.selected = old.selected;
        }
        if let (Some(o), Some(n)) = (old.slider, new.slider.as_mut())
            && n.interact
            && !self.mentions("value")
        {
            n.value = o.value;
        }
        new
    }
}

/// Properties that decide the PLACEMENT MODE, and so must land before the
/// numbers that live inside it.
fn is_place_mode(name: &str) -> bool {
    matches!(name, "pin" | "inset" | "stretch")
}

/// What [`apply_prop`] made of one `(name, value)` pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Set.
    Set,
    /// No property by that name. Nothing changed.
    NoSuchProp,
    /// The name is ours; the value is not one it takes. Nothing changed — see
    /// [`prop_values`] for what it does take.
    BadValue,
}

/// Set one property. Changes nothing and says which kind of mistake it was when
/// the name isn't one of ours, or the value isn't one that name takes — which is
/// how a typo becomes an error message instead of a screen that silently
/// ignores a line, or answers it with a default.
///
/// The vocabulary is the one a script already writes through
/// `node:getcomponent("UiElement")`, plus the structural properties a live
/// field write can't express (a stack, a placement mode, a sub-spec that
/// doesn't exist yet). Deliberately not a second naming scheme: a test in
/// `floptle-script` asserts every mirrored field name is accepted here.
pub fn apply_prop(spec: &mut ElementSpec, name: &str, v: &PropVal) -> Applied {
    // An enumerated property is checked BEFORE anything is written, so a
    // refused value leaves the spec exactly as it found it.
    if prop_values(name).is_some() && !enum_ok(name, &v.text()) {
        return Applied::BadValue;
    }
    // Sub-specs appear on demand, so `{ "box", text = "hi" }` is a label and
    // `{ "text", texture = ... }` is a label with a picture behind it. Nothing
    // has to be declared before it is used.
    macro_rules! shape {
        () => {
            spec.shape.get_or_insert_with(|| ShapeSpec { fill: [0.0; 4], ..Default::default() })
        };
    }
    macro_rules! text {
        () => {
            spec.text.get_or_insert_with(TextSpec::default)
        };
    }
    macro_rules! image {
        () => {
            spec.image.get_or_insert_with(ImageSpec::default)
        };
    }
    macro_rules! stack {
        () => {
            spec.stack.get_or_insert_with(StackCfg::default)
        };
    }
    macro_rules! field {
        () => {
            spec.field.get_or_insert_with(FieldSpec::default)
        };
    }
    macro_rules! slider {
        () => {
            spec.slider.get_or_insert_with(SliderSpec::default)
        };
    }
    macro_rules! scroll {
        () => {
            spec.scroll.get_or_insert_with(ScrollSpec::default)
        };
    }
    match name {
        // ---- placement -----------------------------------------------------
        "pin" => {
            let anchor = anchor(&v.text()).unwrap_or_default();
            let offset = place_offset(&spec.place);
            spec.place = Place::Pin { anchor, offset };
        }
        "inset" => spec.place = Place::fill(v.num()),
        "stretch" => {
            let q = v.quad();
            let margin = match spec.place {
                Place::Stretch { margin, .. } => margin,
                _ => [0.0; 4],
            };
            spec.place = Place::Stretch { min: [q[0], q[1]], max: [q[2], q[3]], margin };
        }
        "margin" => {
            if let Place::Stretch { margin, .. } = &mut spec.place {
                *margin = v.quad();
            }
        }
        "x" | "y" | "posX" | "posY" => {
            let i = usize::from(matches!(name, "y" | "posY"));
            match &mut spec.place {
                Place::Free { pos } => pos[i] = v.num(),
                Place::Pin { offset, .. } => offset[i] = v.num(),
                Place::Stretch { margin, .. } => margin[i] = v.num(),
            }
        }
        "pos" => {
            let p = v.pair();
            match &mut spec.place {
                Place::Free { pos } => *pos = p,
                Place::Pin { offset, .. } => *offset = p,
                Place::Stretch { margin, .. } => {
                    margin[0] = p[0];
                    margin[1] = p[1];
                }
            }
        }
        "order" => spec.order = v.num().round() as i32,

        // ---- size ----------------------------------------------------------
        "w" | "width" => spec.size[0] = v.size(),
        "h" | "height" => spec.size[1] = v.size(),
        "size" => {
            let p = v.pair();
            spec.size = [Size::Fixed(p[0]), Size::Fixed(p[1])];
        }
        "minW" => spec.min_size[0] = v.num(),
        "minH" => spec.min_size[1] = v.num(),
        "maxW" => spec.max_size[0] = v.num(),
        "maxH" => spec.max_size[1] = v.num(),

        // ---- stack ---------------------------------------------------------
        // Any one of these makes the element a container, so `{ "box", gap = 8 }`
        // needs no second word for "and also flow your children".
        "dir" => stack!().dir = dir_of(&v.text()).unwrap_or_default(),
        "gap" => stack!().gap = v.num(),
        "pad" => stack!().pad = v.num(),
        "align" => stack!().align = align(&v.text()).unwrap_or_default(),
        "justify" => stack!().justify = justify_of(&v.text()).unwrap_or_default(),

        // ---- shape ---------------------------------------------------------
        "fill" => shape!().fill = v.color(),
        "fillR" | "fillG" | "fillB" | "fillA" => shape!().fill[rgba(name)] = v.num(),
        "radius" => shape!().radius = Corners(v.quad()),
        "radiusTL" | "radiusTR" | "radiusBR" | "radiusBL" => {
            shape!().radius.0[quad_i(name, ["TL", "TR", "BR", "BL"])] = v.num();
        }
        "border" => shape!().border = Sides(v.quad()),
        "borderL" | "borderT" | "borderR" | "borderB" => {
            shape!().border.0[quad_i(name, ["L", "T", "R", "B"])] = v.num();
        }
        "borderColor" => shape!().border_color = v.color(),
        // A 9-sliced sprite AS the border — the pixel-art alternative to `border`.
        // `frameSlice` is the inset quad; without it the sprite stretches like a
        // photograph, so naming a frame and no slice is almost always a mistake.
        "frame" => shape!().frame.get_or_insert_with(Default::default).texture = v.text(),
        "frameUV" => shape!().frame.get_or_insert_with(Default::default).uv = v.quad(),
        "frameSlice" => shape!().frame.get_or_insert_with(Default::default).slice = v.quad(),

        // ---- text ----------------------------------------------------------
        "text" => text!().text = v.text(),
        "textSize" => text!().size = v.num(),
        "textColor" => text!().color = v.color(),
        "textR" | "textG" | "textB" | "textA" => text!().color[rgba(name)] = v.num(),
        "textAlign" => text!().align = align(&v.text()).unwrap_or_default(),
        "textValign" => text!().valign = align(&v.text()).unwrap_or_default(),
        "tracking" => text!().tracking = v.num(),
        "lineHeight" => text!().line_height = v.num(),
        "font" => text!().font = v.text(),
        "wrap" => text!().wrap = v.bool(),
        "maxLines" => text!().max_lines = v.num().max(0.0) as u32,
        "textFit" => text!().fit = v.bool(),
        "case" => text!().case = case_of(&v.text()).unwrap_or_default(),
        "overflow" => text!().overflow = overflow_of(&v.text()).unwrap_or_default(),

        // ---- image ---------------------------------------------------------
        "texture" | "image" => image!().texture = v.text(),
        "tint" => image!().tint = v.color(),
        "tintR" | "tintG" | "tintB" | "tintA" => image!().tint[rgba(name)] = v.num(),
        "cols" => image!().cols = (v.num().max(1.0)) as u32,
        "rows" => image!().rows = (v.num().max(1.0)) as u32,
        "cell" => image!().cell = v.num().max(0.0) as u32,
        "slice" => image!().slice = v.quad(),
        "tiling" => image!().tiling = v.pair(),
        "imageFit" => image!().fit = image_fit_of(&v.text()).unwrap_or_default(),

        // ---- interaction ---------------------------------------------------
        "button" => spec.button = v.bool(),
        "toggle" => spec.toggle = v.bool(),
        "group" => spec.group = v.text(),
        "selected" => spec.selected = v.bool(),
        "disabled" => spec.disabled = v.bool(),
        "focusable" => spec.focusable = v.bool(),
        "draggable" => spec.draggable = v.bool(),
        "dropTarget" => spec.drop_target = v.bool(),
        "tooltip" => spec.tooltip = v.text(),
        "tooltipBox" => spec.tooltip_box = v.bool(),
        "navUp" | "navDown" | "navLeft" | "navRight" => {
            let n = spec.nav.get_or_insert_with(Default::default);
            match name {
                "navUp" => n.up = v.text(),
                "navDown" => n.down = v.text(),
                "navLeft" => n.left = v.text(),
                _ => n.right = v.text(),
            }
        }
        "part" => spec.part = part_of(&v.text()).unwrap_or_default(),

        // ---- look ------------------------------------------------------------
        "style" => spec.style = v.text(),
        "visible" => spec.visible = v.bool(),
        "opacity" => spec.opacity = v.num().clamp(0.0, 1.0),
        "groupTint" => spec.tint = v.color(),
        "groupR" | "groupG" | "groupB" | "groupA" => spec.tint[rgba(name)] = v.num(),
        "rotation" => spec.rotation = v.num(),
        "scale" => spec.scale = v.pair(),
        "scaleX" => spec.scale[0] = v.num(),
        "scaleY" => spec.scale[1] = v.num(),
        "pivot" => spec.pivot = v.pair(),
        "shader" => spec.shader = v.text(),

        // ---- field -----------------------------------------------------------
        "field" => {
            if v.bool() {
                field!();
            } else {
                spec.field = None;
            }
        }
        "placeholder" => field!().placeholder = v.text(),
        "maxLen" => field!().max_len = v.num().max(0.0) as u32,
        "numeric" => field!().numeric = v.bool(),
        "upper" => field!().upper = v.bool(),
        "mask" => field!().mask = v.bool(),

        // ---- slider ----------------------------------------------------------
        "min" => slider!().min = v.num(),
        "max" => slider!().max = v.num(),
        "value" => slider!().value = v.num(),
        "interact" => slider!().interact = v.bool(),
        "flip" => slider!().flip = v.bool(),
        "sliderDir" => slider!().dir = dir_of(&v.text()).unwrap_or_default(),

        // ---- scroll ----------------------------------------------------------
        "scrollY" => scroll!().offset = v.num().max(0.0),
        "scrollX" => scroll!().offset_x = v.num().max(0.0),
        "scrollSpeed" => scroll!().speed = v.num(),
        "scrollDrag" => scroll!().drag = v.bool(),
        "scrollbarFor" => {
            let axis = spec.scrollbar.as_ref().map(|s| s.axis).unwrap_or_default();
            spec.scrollbar = Some(ScrollBar { target: v.text(), axis });
        }
        "scrollbarAxis" => {
            let bar = spec.scrollbar.get_or_insert_with(Default::default);
            bar.axis = dir_of(&v.text()).unwrap_or_default();
        }

        // ---- repeater ----------------------------------------------------------
        // A made container can still repeat a prefab: `ui.make` describes the
        // shape of the screen, the repeater fills a list with rows that carry
        // their own scripts and art.
        "template" => {
            let count = spec.repeater.as_ref().map(|r| r.count).unwrap_or(0);
            spec.repeater = Some(RepeatSpec { template: v.text(), count });
        }
        "count" => spec.repeater.get_or_insert_with(Default::default).count = v.num().max(0.0) as u32,

        _ => return Applied::NoSuchProp,
    }
    Applied::Set
}

/// Whether `apply_prop` would accept this name — the check the parser makes so
/// a mistyped property is reported instead of dropped.
pub fn known_prop(name: &str) -> bool {
    // A number is not a valid value for any of the enumerated properties, so
    // this asks only about the NAME — which is the question.
    apply_prop(&mut ElementSpec::default(), name, &PropVal::Num(0.0)) != Applied::NoSuchProp
}

/// Whether this property accepts this value. Always true for a property that
/// isn't enumerated — see [`prop_values`].
pub fn known_value(name: &str, v: &PropVal) -> bool {
    apply_prop(&mut ElementSpec::default(), name, v) != Applied::BadValue
}

/// The closest few property names to a typo, for the error message.
pub fn suggest(name: &str) -> Vec<&'static str> {
    let lower = name.to_lowercase();
    let rank = |p: &&'static str| -> Option<(u8, usize)> {
        let pl = p.to_lowercase();
        // Nearest first: the name you meant usually shares a prefix with the
        // one you typed, and among those the shortest is the likeliest.
        let tier = if pl == lower {
            0
        } else if lower.starts_with(&pl) || pl.starts_with(&lower) {
            1
        } else if pl.contains(&lower) || lower.contains(&pl) {
            2
            // A near-miss spelling, whole or per camelCase word. `colour` for
            // `textColor` is the one this is really for, and neither prefixes
            // nor a whole-string distance finds that one.
        } else if edits(&pl, &lower) <= 2
            || words(p).iter().any(|w| edits(w, &lower) <= 1)
        {
            3
        } else {
            return None;
        };
        Some((tier, pl.len().abs_diff(lower.len())))
    };
    let mut hits: Vec<&'static str> =
        ALL_PROPS.iter().filter(|p| rank(p).is_some()).copied().collect();
    hits.sort_by_key(|p| rank(p).unwrap_or((3, 0)));
    hits.truncate(4);
    hits
}

/// A camelCase name's words, lowercased: `textColor` → `["text", "color"]`.
fn words(name: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    for c in name.chars() {
        if c.is_uppercase() && !out.last().is_some_and(String::is_empty) {
            out.push(String::new());
        }
        out.last_mut().expect("never empty").push(c.to_ascii_lowercase());
    }
    out.retain(|w| !w.is_empty());
    out
}

/// Levenshtein distance, capped in practice by the short names it compares.
fn edits(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Every accepted property name. Kept beside [`apply_prop`] and guarded by a
/// test that walks it — the list exists for error messages and IDE completion,
/// and a list that drifts from the match is worse than no list.
pub const ALL_PROPS: &[&str] = &[
    "align", "border", "borderB", "borderColor", "borderL", "borderR", "borderT", "button",
    "case", "cell", "cols", "count", "dir", "disabled", "draggable", "dropTarget", "field",
    "fill", "fillA", "fillB", "fillG", "fillR", "flip", "focusable", "font", "frame", "frameSlice",
    "frameUV", "gap", "group",
    "groupA", "groupB", "groupG", "groupR", "groupTint", "h", "height", "image", "imageFit",
    "inset", "interact", "justify", "lineHeight", "margin", "mask", "max", "maxH", "maxLen",
    "maxLines", "maxW", "min", "minH", "minW", "numeric", "opacity", "order", "overflow", "pad",
    "part", "pin", "pivot", "placeholder", "pos", "posX", "posY", "radius", "radiusBL",
    "radiusBR", "radiusTL", "radiusTR", "rotation", "rows", "scale", "scaleX", "scaleY",
    "scrollDrag", "scrollSpeed", "scrollX", "scrollY", "scrollbarAxis", "scrollbarFor",
    "selected", "shader", "size", "slice", "sliderDir", "stretch", "style", "template", "text",
    "textA", "textAlign", "textB", "textColor", "textFit", "textG", "textR", "textSize",
    "textValign", "texture", "tiling", "tint", "tintA", "tintB", "tintG", "tintR", "toggle",
    "tooltip", "tooltipBox", "tracking", "upper", "value", "visible", "w", "width", "wrap", "x",
    "y", "navUp", "navDown", "navLeft", "navRight",
];

/// The channel a `*R`/`*G`/`*B`/`*A` name addresses. Only ever asked about
/// colour names — the `borderB` that means "bottom" goes through [`quad_i`].
fn rgba(name: &str) -> usize {
    match name.chars().last() {
        Some('R') => 0,
        Some('G') => 1,
        Some('B') => 2,
        _ => 3,
    }
}

fn quad_i(name: &str, keys: [&str; 4]) -> usize {
    keys.iter().position(|k| name.ends_with(k)).unwrap_or(0)
}

fn align(s: &str) -> Option<Align> {
    Some(match s {
        "start" | "left" | "top" => Align::Start,
        "center" | "centre" => Align::Center,
        "end" | "right" | "bottom" => Align::End,
        "stretch" => Align::Stretch,
        _ => return None,
    })
}

fn anchor(s: &str) -> Option<Anchor> {
    Some(match s {
        "topLeft" => Anchor::TopLeft,
        // `topCenter` and `bottomCenter` are what people write, because the
        // other seven anchors are `topLeft`, `bottomRight` and friends — the
        // two that take a bare direction are the two you have to look up. They
        // were the four HUD elements that all landed in one corner
        // (`floptle/0072`).
        "top" | "topCenter" | "topCentre" => Anchor::Top,
        "topRight" => Anchor::TopRight,
        "left" | "leftCenter" | "leftCentre" => Anchor::Left,
        "center" | "centre" => Anchor::Center,
        "right" | "rightCenter" | "rightCentre" => Anchor::Right,
        "bottomLeft" => Anchor::BottomLeft,
        "bottom" | "bottomCenter" | "bottomCentre" => Anchor::Bottom,
        "bottomRight" => Anchor::BottomRight,
        _ => return None,
    })
}

/// The values an enumerated property takes, or `None` for a property that takes
/// free text, a number or a boolean.
///
/// Used to REFUSE anything else, and to say what was expected. `ui.make` has
/// always raised on a property NAME it doesn't know — "a declarative screen
/// that silently ignores a line is worse than one that stops" — and a value it
/// doesn't know is the same bug wearing different clothes. `pin = "topCenter"`
/// answered `TopLeft`, silently, forever, and four HUD elements stacked into one
/// corner over the panel that legitimately lived there. It read as a layout bug
/// and pointed nowhere near the spelling that caused it (`floptle/0072`).
///
/// Spelling variants are accepted but not listed: `centre` for `center` is not
/// a different answer, and an error message that lists both teaches neither.
pub fn prop_values(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "pin" => &[
            "topLeft", "top", "topRight", "left", "center", "right", "bottomLeft", "bottom",
            "bottomRight", "topCenter", "bottomCenter", "leftCenter", "rightCenter",
        ],
        "align" | "textAlign" | "textValign" => {
            &["start", "left", "top", "center", "end", "right", "bottom", "stretch"]
        }
        "justify" => &["start", "center", "end", "between", "spaceBetween"],
        "dir" | "sliderDir" | "scrollbarAxis" => &["row", "column"],
        "case" => &["asIs", "upper", "lower", "title"],
        "overflow" => &["show", "clip", "ellipsis"],
        "imageFit" => &["stretch", "contain", "cover"],
        "part" => &["fill", "handle", "none"],
        _ => return None,
    })
}

/// A direction word: `row` or `column`, and nothing else.
fn dir_of(s: &str) -> Option<Dir> {
    match s {
        "row" => Some(Dir::Row),
        "column" | "col" => Some(Dir::Column),
        _ => None,
    }
}

fn justify_of(s: &str) -> Option<Justify> {
    Some(match s {
        "start" => Justify::Start,
        "center" | "centre" => Justify::Center,
        "end" => Justify::End,
        "between" | "spaceBetween" => Justify::SpaceBetween,
        _ => return None,
    })
}

fn case_of(s: &str) -> Option<Case> {
    Some(match s {
        "asIs" | "none" => Case::AsIs,
        "upper" => Case::Upper,
        "lower" => Case::Lower,
        "title" => Case::Title,
        _ => return None,
    })
}

fn overflow_of(s: &str) -> Option<Overflow> {
    Some(match s {
        "show" => Overflow::Show,
        "clip" => Overflow::Clip,
        "ellipsis" => Overflow::Ellipsis,
        _ => return None,
    })
}

fn image_fit_of(s: &str) -> Option<ImageFit> {
    Some(match s {
        "stretch" => ImageFit::Stretch,
        "contain" => ImageFit::Contain,
        "cover" => ImageFit::Cover,
        _ => return None,
    })
}

/// A slider part, where "none" is itself an answer — hence the nested option.
fn part_of(s: &str) -> Option<Option<SliderPart>> {
    Some(match s {
        "fill" => Some(SliderPart::Fill),
        "handle" => Some(SliderPart::Handle),
        "none" => None,
        _ => return None,
    })
}

/// Whether an enumerated property accepts this value.
///
/// Delegates to the same parsers the `match` arms use, so the check and the
/// behaviour cannot disagree — a second list to keep in step is how `collide`
/// survived two releases.
fn enum_ok(name: &str, s: &str) -> bool {
    match name {
        "pin" => anchor(s).is_some(),
        "align" | "textAlign" | "textValign" => align(s).is_some(),
        "justify" => justify_of(s).is_some(),
        "dir" | "sliderDir" | "scrollbarAxis" => dir_of(s).is_some(),
        "case" => case_of(s).is_some(),
        "overflow" => overflow_of(s).is_some(),
        "imageFit" => image_fit_of(s).is_some(),
        "part" => part_of(s).is_some(),
        _ => true,
    }
}

fn place_offset(p: &Place) -> [f32; 2] {
    match p {
        Place::Free { pos } => *pos,
        Place::Pin { offset, .. } => *offset,
        Place::Stretch { margin, .. } => [margin[0], margin[1]],
    }
}

// ---------------------------------------------------------------------------
// The diff
// ---------------------------------------------------------------------------

/// What the caller must do to one child slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// The existing element at `old` is the described element at `new`: patch
    /// it in place, keeping the entity — and with it every scrap of runtime
    /// state hanging off that entity, from style transitions to focus.
    Keep { old: usize, new: usize },
    /// Nothing matched the described element at `new`: create it.
    Create { new: usize },
    /// The existing element at `old` is no longer described: destroy it.
    Remove { old: usize },
}

/// What the reconciler already knows about one existing made child.
#[derive(Clone, Debug, PartialEq)]
pub struct Existing {
    pub key: String,
    pub kind: Kind,
}

/// Match the described children against the ones already there.
///
/// Keyed elements pair up by key wherever they moved to; the rest pair up in
/// order. A kind change never matches — a `text` becoming an `image` is a
/// different element, and patching one into the other would leave the loser's
/// sub-specs behind.
///
/// The output is ordered: every `Keep`/`Create` in described order, then the
/// `Remove`s. Callers that destroy first and create second stay correct either
/// way, but reading the plan should mirror reading the description.
pub fn plan(existing: &[Existing], wanted: &[MadeNode]) -> Vec<Op> {
    let mut taken = vec![false; existing.len()];
    let mut ops: Vec<Op> = Vec::with_capacity(wanted.len() + existing.len());
    // Keys first, across the whole list: a keyed row that moved from position
    // 2 to position 0 must find its own element, not position 0's.
    let mut matched: Vec<Option<usize>> = vec![None; wanted.len()];
    for (i, w) in wanted.iter().enumerate() {
        if w.key.is_empty() {
            continue;
        }
        if let Some(j) = existing
            .iter()
            .enumerate()
            .position(|(j, e)| !taken[j] && e.key == w.key && e.kind == w.kind)
        {
            taken[j] = true;
            matched[i] = Some(j);
        }
    }
    // Then the unkeyed ones, in order, against whatever is left over. An
    // existing element that CARRIES a key is not available to an unkeyed slot:
    // its identity was stated, and quietly reusing it would move a keyed row's
    // state into an anonymous one.
    let mut cursor = 0usize;
    for (i, w) in wanted.iter().enumerate() {
        if !w.key.is_empty() {
            continue;
        }
        while cursor < existing.len()
            && (taken[cursor] || !existing[cursor].key.is_empty() || existing[cursor].kind != w.kind)
        {
            // Only skip past a slot that can never match this one; a kind
            // mismatch means this described element is new and the existing
            // one will fall out as a Remove.
            if !taken[cursor] && existing[cursor].key.is_empty() && existing[cursor].kind != w.kind {
                break;
            }
            cursor += 1;
        }
        if cursor < existing.len() && !taken[cursor] && existing[cursor].kind == w.kind {
            taken[cursor] = true;
            matched[i] = Some(cursor);
            cursor += 1;
        }
    }
    for (i, m) in matched.iter().enumerate() {
        ops.push(match m {
            Some(j) => Op::Keep { old: *j, new: i },
            None => Op::Create { new: i },
        });
    }
    for (j, t) in taken.iter().enumerate() {
        if !t {
            ops.push(Op::Remove { old: j });
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: Kind, key: &str) -> MadeNode {
        MadeNode { kind, key: key.to_string(), ..Default::default() }
    }

    fn prop(n: &str, v: PropVal) -> MadeNode {
        MadeNode { kind: Kind::Box, props: vec![(n.to_string(), v)], ..Default::default() }
    }

    /// The one thing a builder must never do: paint something you didn't ask
    /// for. `ShapeSpec::default()` is opaque white, so a made box has to
    /// override it or every screen starts as a pile of white slabs.
    #[test]
    fn a_made_box_is_invisible_until_something_paints_it() {
        assert_eq!(Kind::Box.base().shape.unwrap().fill, [0.0; 4]);
        assert!(Kind::Text.base().shape.is_none(), "a label paints no background");
    }

    #[test]
    fn a_button_takes_clicks_and_can_be_reached_by_a_pad() {
        let b = Kind::Button.base();
        assert!(b.button && b.focusable);
    }

    #[test]
    fn sizes_read_their_mode_off_the_string() {
        let cases: [(PropVal, Size); 5] = [
            (PropVal::Num(120.0), Size::Fixed(120.0)),
            (PropVal::Str("50%".into()), Size::Pct(0.5)),
            (PropVal::Str("grow".into()), Size::Grow(1.0)),
            (PropVal::Str("grow 2".into()), Size::Grow(2.0)),
            (PropVal::Str("fit".into()), Size::Fit),
        ];
        for (v, want) in cases {
            assert_eq!(prop("w", v).build().size[0], want);
        }
    }

    /// Table order is not something a Lua author controls (nor should have to
    /// think about), so the two passes have to make it irrelevant.
    #[test]
    fn the_order_of_the_table_does_not_change_the_result() {
        let a = MadeNode {
            props: vec![
                ("inset".into(), PropVal::Num(0.0)),
                ("margin".into(), PropVal::Num(12.0)),
            ],
            ..Default::default()
        };
        let b = MadeNode { props: a.props.iter().rev().cloned().collect(), ..a.clone() };
        assert_eq!(a.build(), b.build());
        match a.build().place {
            Place::Stretch { margin, .. } => assert_eq!(margin, [12.0; 4]),
            p => panic!("expected a stretch, got {p:?}"),
        }
    }

    /// Every value the error message promises actually works. A list that
    /// drifted from the `match` would send someone to a spelling that also
    /// does nothing — worse than no list, which is why `enum_ok` asks the same
    /// parsers the arms do.
    #[test]
    fn every_value_the_message_offers_is_one_the_property_takes() {
        for name in ALL_PROPS {
            let Some(values) = prop_values(name) else { continue };
            assert!(!values.is_empty(), "{name} lists no values");
            for v in values {
                assert_eq!(
                    apply_prop(&mut ElementSpec::default(), name, &PropVal::Str((*v).into())),
                    Applied::Set,
                    "{name} offers `{v}` and then refuses it"
                );
            }
            // …and it is a real check, not one that accepts everything.
            assert_eq!(
                apply_prop(&mut ElementSpec::default(), name, &PropVal::Str("wumpus".into())),
                Applied::BadValue,
                "{name} accepted a value that is not a value"
            );
        }
    }

    /// The bug itself: an unrecognised `pin` answered `TopLeft`, silently and
    /// forever. Four HUD elements stacked into one corner on top of the panel
    /// that legitimately lived there, and the report was "the HUD is clipping
    /// over things" — a perfect description that points nowhere near a spelling
    /// mistake (`floptle/0072`).
    #[test]
    fn an_unknown_pin_is_refused_rather_than_answered_with_the_top_left_corner() {
        let mut spec = ElementSpec {
            place: Place::Pin { anchor: Anchor::Center, offset: [4.0, 5.0] },
            ..Default::default()
        };
        let before = spec.clone();
        assert_eq!(
            apply_prop(&mut spec, "pin", &PropVal::Str("middle".into())),
            Applied::BadValue
        );
        assert_eq!(spec, before, "a refused value still changed the element");

        // …and the two spellings people actually write are ANSWERED, not
        // refused. The other seven anchors are `topLeft` and friends; the two
        // that take a bare direction are the two you have to look up.
        for (wrote, meant) in [
            ("topCenter", Anchor::Top),
            ("bottomCenter", Anchor::Bottom),
            ("leftCenter", Anchor::Left),
            ("rightCenter", Anchor::Right),
            ("bottomCentre", Anchor::Bottom),
            ("bottom", Anchor::Bottom),
        ] {
            match prop("pin", PropVal::Str(wrote.into())).build().place {
                Place::Pin { anchor, .. } => assert_eq!(anchor, meant, "pin = {wrote:?}"),
                p => panic!("pin = {wrote:?} gave {p:?}"),
            }
        }
    }

    /// The quieter half of the same bug. `Align::Start` for a bad `align` is
    /// less dramatic than `TopLeft` for a bad `pin`, and it would have been
    /// found the same way — by a player saying something looks wrong.
    #[test]
    fn the_other_enumerated_properties_are_refused_the_same_way() {
        for (name, junk) in [
            ("align", "middle"),
            ("justify", "spread"),
            ("dir", "horizontal"),
            ("case", "caps"),
            ("overflow", "hidden"),
            ("imageFit", "fill"),
            ("textAlign", "centered"),
            ("sliderDir", "vertical"),
        ] {
            assert_eq!(
                apply_prop(&mut ElementSpec::default(), name, &PropVal::Str(junk.into())),
                Applied::BadValue,
                "{name} = {junk:?} was answered instead of refused"
            );
        }
        // A number where a word belongs is refused too — it is the same mistake.
        assert_eq!(
            apply_prop(&mut ElementSpec::default(), "pin", &PropVal::Num(3.0)),
            Applied::BadValue
        );
        // …while the name itself is still known, so the message can say which
        // of the two mistakes it was.
        assert!(known_prop("pin") && known_prop("align"));
    }

    #[test]
    fn a_sub_spec_appears_when_a_property_needs_it() {
        assert!(prop("text", PropVal::Str("hi".into())).build().text.is_some());
        assert!(prop("gap", PropVal::Num(4.0)).build().stack.is_some());
        assert!(prop("texture", PropVal::Str("t.png".into())).build().image.is_some());
        assert!(prop("placeholder", PropVal::Str("name".into())).build().field.is_some());
    }

    #[test]
    fn colors_arrive_as_tables_numbers_or_hex() {
        assert_eq!(prop("fill", PropVal::Color([1.0, 0.0, 0.0, 1.0])).build().shape.unwrap().fill, [
            1.0, 0.0, 0.0, 1.0
        ]);
        let hexed = prop("fill", PropVal::Str("#ff8800".into())).build().shape.unwrap().fill;
        assert!(
            hexed.iter().zip([1.0, 0.533, 0.0, 1.0]).all(|(a, b)| (a - b).abs() < 0.001),
            "got {hexed:?}"
        );
        // A bare number is a grey, exactly as `color(0.2)` is in Lua.
        assert_eq!(prop("fill", PropVal::Num(0.25)).build().shape.unwrap().fill, [
            0.25, 0.25, 0.25, 1.0
        ]);
    }

    #[test]
    fn a_quad_takes_a_scalar_or_a_list() {
        let r = prop("radius", PropVal::Num(8.0)).build().shape.unwrap().radius;
        assert_eq!(r.0, [8.0; 4]);
        let r = prop("radius", PropVal::List(vec![8.0, 8.0, 0.0, 0.0])).build().shape.unwrap().radius;
        assert_eq!(r.0, [8.0, 8.0, 0.0, 0.0]);
    }

    /// `floptle/0124`: `ui.make` CAN name a font — reported as though it could
    /// not, and worth a test rather than a correction, because a property is
    /// only useful if it survives the rebuild a reconcile puts a node through.
    #[test]
    fn a_made_label_can_name_its_own_font() {
        assert!(known_prop("font"));
        let mut node = MadeNode {
            kind: Kind::Text,
            props: vec![
                ("text".to_string(), PropVal::Str("hi".into())),
                ("font".to_string(), PropVal::Str("fonts/Pixel.ttf".into())),
            ],
            ..Default::default()
        };
        node.props.sort_by(|a, b| a.0.cmp(&b.0));
        let built = node.build();
        let t = built.text.as_ref().expect("a text slot");
        assert_eq!(t.font, "fonts/Pixel.ttf");
        assert_eq!(t.text, "hi");
        // …and it survives the rebuild path, which is the one a live tree takes.
        assert_eq!(node.rebuild(&built).text.unwrap().font, "fonts/Pixel.ttf");
        // An empty font is not "no font" — it is *the project's* font now,
        // which is what every label that never says otherwise gets.
        assert_eq!(prop("text", PropVal::Str("hi".into())).build().text.unwrap().font, "");
    }

    #[test]
    fn an_unknown_property_is_refused_rather_than_ignored() {
        assert!(!known_prop("colour"));
        assert!(!known_prop("padding"));
        assert!(known_prop("pad"));
        // And the message can point somewhere useful — nearest first.
        assert_eq!(suggest("padding").first(), Some(&"pad"));
        assert_eq!(suggest("colour").first(), Some(&"textColor"));
        assert!(suggest("qqq").is_empty());
    }

    /// Every name the list advertises must actually be handled, or the error
    /// message and the IDE completion are lying about the API.
    #[test]
    fn the_advertised_property_list_matches_the_code() {
        for p in ALL_PROPS {
            assert!(known_prop(p), "{p} is listed but not handled");
        }
        let mut sorted = ALL_PROPS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ALL_PROPS.len(), "duplicate names in ALL_PROPS");
    }

    // ---- reconciliation ---------------------------------------------------

    #[test]
    fn an_unchanged_list_keeps_every_element() {
        let have = vec![
            Existing { key: String::new(), kind: Kind::Row },
            Existing { key: String::new(), kind: Kind::Row },
        ];
        let want = vec![node(Kind::Row, ""), node(Kind::Row, "")];
        assert_eq!(plan(&have, &want), vec![Op::Keep { old: 0, new: 0 }, Op::Keep {
            old: 1,
            new: 1
        }]);
    }

    /// The whole reason to reconcile rather than rebuild: adding a row must
    /// cost one spawn, not ten spawns and ten despawns.
    #[test]
    fn a_list_that_gains_a_row_only_creates_that_row() {
        let have = vec![Existing { key: String::new(), kind: Kind::Row }; 3];
        let want = vec![node(Kind::Row, ""); 4];
        let ops = plan(&have, &want);
        assert_eq!(ops.iter().filter(|o| matches!(o, Op::Create { .. })).count(), 1);
        assert_eq!(ops.iter().filter(|o| matches!(o, Op::Remove { .. })).count(), 0);
        assert_eq!(ops.iter().filter(|o| matches!(o, Op::Keep { .. })).count(), 3);
    }

    #[test]
    fn a_list_that_loses_a_row_removes_the_last_one() {
        let have = vec![Existing { key: String::new(), kind: Kind::Row }; 3];
        let want = vec![node(Kind::Row, ""); 2];
        assert!(plan(&have, &want).contains(&Op::Remove { old: 2 }));
    }

    /// Keys are the answer to a re-sort. Without them, "Ana, Bo, Cy" becoming
    /// "Cy, Ana, Bo" would leave every row's scroll/typing/selection one slot
    /// out of place while the labels moved.
    #[test]
    fn keyed_rows_follow_their_key_through_a_reorder() {
        let have = vec![
            Existing { key: "ana".into(), kind: Kind::Row },
            Existing { key: "bo".into(), kind: Kind::Row },
            Existing { key: "cy".into(), kind: Kind::Row },
        ];
        let want = vec![node(Kind::Row, "cy"), node(Kind::Row, "ana"), node(Kind::Row, "bo")];
        assert_eq!(plan(&have, &want), vec![
            Op::Keep { old: 2, new: 0 },
            Op::Keep { old: 0, new: 1 },
            Op::Keep { old: 1, new: 2 },
        ]);
    }

    #[test]
    fn a_keyed_row_that_leaves_is_the_only_one_destroyed() {
        let have = vec![
            Existing { key: "ana".into(), kind: Kind::Row },
            Existing { key: "bo".into(), kind: Kind::Row },
        ];
        let want = vec![node(Kind::Row, "ana")];
        assert_eq!(plan(&have, &want), vec![Op::Keep { old: 0, new: 0 }, Op::Remove { old: 1 }]);
    }

    /// A slot that changes kind is a different element: patching a text spec
    /// into an image would leave the text hanging off it.
    #[test]
    fn changing_kind_replaces_rather_than_patches() {
        let have = vec![Existing { key: String::new(), kind: Kind::Text }];
        let want = vec![node(Kind::Image, "")];
        let ops = plan(&have, &want);
        assert!(ops.contains(&Op::Create { new: 0 }));
        assert!(ops.contains(&Op::Remove { old: 0 }));
    }

    #[test]
    fn an_unkeyed_slot_never_steals_a_keyed_element() {
        let have = vec![Existing { key: "ana".into(), kind: Kind::Row }];
        let want = vec![node(Kind::Row, "")];
        let ops = plan(&have, &want);
        assert!(ops.contains(&Op::Create { new: 0 }));
        assert!(ops.contains(&Op::Remove { old: 0 }));
    }

    // ---- what a re-render keeps -------------------------------------------

    /// The description is authoritative: drop a property from your table and
    /// its effect must leave the screen, or removing a line does nothing and
    /// the table stops describing what you see.
    #[test]
    fn a_property_the_description_drops_goes_back_to_default() {
        let old = prop("fill", PropVal::Color([1.0, 0.0, 0.0, 1.0])).build();
        let plain = MadeNode::default();
        assert_eq!(plain.rebuild(&old).shape.unwrap().fill, [0.0; 4]);
    }

    /// …but what the PLAYER did is not something the description said.
    #[test]
    fn a_re_render_keeps_what_the_player_did() {
        // Scrolled halfway down a list.
        let mut old = MadeNode { kind: Kind::Scroll, ..Default::default() }.build();
        old.scroll.as_mut().unwrap().offset = 140.0;
        let again = MadeNode { kind: Kind::Scroll, ..Default::default() };
        assert_eq!(again.rebuild(&old).scroll.unwrap().offset, 140.0);

        // Typed a name into a field.
        let mut old = MadeNode { kind: Kind::Field, ..Default::default() }.build();
        old.text.as_mut().unwrap().text = "ANA".into();
        let again = MadeNode { kind: Kind::Field, ..Default::default() };
        assert_eq!(again.rebuild(&old).text.unwrap().text, "ANA");

        // Ticked a checkbox.
        let mut old = prop("toggle", PropVal::Bool(true)).build();
        old.selected = true;
        assert!(prop("toggle", PropVal::Bool(true)).rebuild(&old).selected);

        // Dragged a volume slider.
        let dragged = MadeNode {
            kind: Kind::Slider,
            props: vec![("interact".into(), PropVal::Bool(true))],
            ..Default::default()
        };
        let mut old = dragged.build();
        old.slider.as_mut().unwrap().value = 33.0;
        assert_eq!(dragged.rebuild(&old).slider.unwrap().value, 33.0);
    }

    /// Carrying player state must not become "ignores the description": when
    /// the table DOES say, the table wins.
    #[test]
    fn the_description_still_wins_when_it_speaks() {
        let mut old = MadeNode { kind: Kind::Field, ..Default::default() }.build();
        old.text.as_mut().unwrap().text = "typed".into();
        let says = MadeNode {
            kind: Kind::Field,
            props: vec![("text".into(), PropVal::Str("reset".into()))],
            ..Default::default()
        };
        assert_eq!(says.rebuild(&old).text.unwrap().text, "reset");
    }

    /// A display-only meter is driven by the game every frame; carrying the
    /// old value would fight `ui.bind`.
    #[test]
    fn a_display_only_meter_does_not_keep_its_old_value() {
        let bar = MadeNode {
            kind: Kind::Slider,
            props: vec![("value".into(), PropVal::Num(80.0))],
            ..Default::default()
        };
        let mut old = bar.build();
        old.slider.as_mut().unwrap().value = 12.0;
        assert_eq!(bar.rebuild(&old).slider.unwrap().value, 80.0);
    }
}
