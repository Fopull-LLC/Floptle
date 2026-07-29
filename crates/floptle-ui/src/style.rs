//! Styles, tokens, states and transitions — the consistency engine
//! (docs/ui-system-2-proposal.md §B).
//!
//! # Why this exists
//!
//! Without it, every colour in a project is four literal floats typed onto one
//! element. `Fofighter/scenes/menu.ron` has about forty of them; changing the
//! accent means editing all forty. And because there is no way to say "this is
//! a primary button", there is no such thing as a primary button — only forty
//! rectangles that happen to be similar. Hover states get re-derived per
//! script (`solar/scripts/menu_button.lua` computes `idle * 1.5 + 0.08` per
//! channel) and nine of those scripts disagree with each other.
//!
//! # The model, and the four rules that keep it from becoming CSS
//!
//! 1. An element names **at most one** style. No lists, no classes, no
//!    selectors.
//! 2. The element's own properties **always** win over the style. One rule,
//!    no specificity.
//! 3. Inheritance covers exactly font, text colour, and the opacity/tint
//!    cascade. Nothing else.
//! 4. States are a **fixed, closed set**: hover, pressed, disabled, focus,
//!    selected. If you need a sixth, that's a script.
//!
//! Those are constraints, not defaults. Every "but what about…" that needs a
//! fifth rule is answered with Lua, deliberately.
//!
//! # Tokens
//!
//! A project *may* define named colours/spacing/radii/type sizes. The engine
//! ships none. What tokens really buy is not indirection — it's that a project
//! is forced to *have* a spacing scale and a type scale, which is the single
//! biggest structural cure for the flat, uniform, everything-at-one-weight look
//! that reads as machine-made.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    Blend, Case, Corners, GlowSpec, Gradient, GrainSpec, ShadowSpec, Sides, TextShadow, TextStroke,
};

// ---------------------------------------------------------------------------
// Token references
// ---------------------------------------------------------------------------

/// A colour: either written out, or the name of a project token.
///
/// In RON these are `(0.1, 0.2, 0.3, 1.0)` and `"accent"` — the untagged form,
/// because a style sheet is something a designer edits by hand and
/// `Token("accent")` reads like ceremony.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorRef {
    Token(String),
    Lit([f32; 4]),
}

/// A number: either written out, or the name of a project token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NumRef {
    Token(String),
    Lit(f32),
}

/// A project's named values. Everything is optional and a project defines its
/// own; the engine ships nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub colors: BTreeMap<String, [f32; 4]>,
    /// The spacing scale — gaps, padding, offsets.
    #[serde(default)]
    pub spacing: BTreeMap<String, f32>,
    #[serde(default)]
    pub radii: BTreeMap<String, f32>,
    /// The type scale — glyph sizes.
    #[serde(default)]
    pub text: BTreeMap<String, f32>,
    /// Named font asset paths.
    #[serde(default)]
    pub fonts: BTreeMap<String, String>,
}

impl Tokens {
    /// Parse one `.tokens.ron` file.
    pub fn parse(text: &str) -> Result<Tokens, ron::error::SpannedError> {
        parse_ron(text)
    }

    /// Merge another token file into this one (later files win on a clash).
    /// Projects split tokens across files the way they split materials.
    pub fn merge(&mut self, other: Tokens) {
        self.colors.extend(other.colors);
        self.spacing.extend(other.spacing);
        self.radii.extend(other.radii);
        self.text.extend(other.text);
        self.fonts.extend(other.fonts);
    }

    /// Resolve a colour reference. An unknown token name resolves to
    /// **magenta**, not to a default colour: a typo has to be visible on
    /// screen, because a silently-black panel looks like an authoring mistake
    /// and gets debugged for twenty minutes.
    pub fn color(&self, r: &ColorRef) -> [f32; 4] {
        match r {
            ColorRef::Lit(c) => *c,
            ColorRef::Token(name) => {
                self.colors.get(name).copied().unwrap_or([1.0, 0.0, 1.0, 1.0])
            }
        }
    }

    /// Resolve a number against a named scale, falling back to `0.0` for an
    /// unknown token (a missing gap is visible as a squashed layout; a missing
    /// colour is not, hence the different treatment).
    pub fn num(&self, r: &NumRef, scale: Scale) -> f32 {
        match r {
            NumRef::Lit(v) => *v,
            NumRef::Token(name) => {
                let table = match scale {
                    Scale::Spacing => &self.spacing,
                    Scale::Radii => &self.radii,
                    Scale::Text => &self.text,
                };
                table.get(name).copied().unwrap_or(0.0)
            }
        }
    }

    /// Resolve a font token to an asset path (an unknown name = the fallback
    /// font, i.e. an empty path).
    pub fn font(&self, name: &str) -> String {
        self.fonts.get(name).cloned().unwrap_or_else(|| {
            // A name that isn't a token is taken as a literal path, so a style
            // can point straight at a .ttf without declaring a token first.
            if name.contains('.') { name.to_string() } else { String::new() }
        })
    }
}

/// Which named scale a [`NumRef`] token looks up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Spacing,
    Radii,
    Text,
}

// ---------------------------------------------------------------------------
// Easing
// ---------------------------------------------------------------------------

/// Easing curves for transitions and tweens.
///
/// Wider than the scheduler's original four (`linear | smooth | in | out`)
/// because UI motion needs shape the old set can't express — `Back` in
/// particular is what makes a press feel physical, and it cannot be faked by
/// composing the others.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ease {
    Linear,
    #[default]
    OutCubic,
    InCubic,
    InOutCubic,
    OutQuad,
    InQuad,
    /// Overshoots the target then settles — the "physical" one.
    OutBack,
    /// Springs past and oscillates in.
    OutElastic,
}

impl Ease {
    pub const ALL: [Ease; 8] = [
        Ease::Linear,
        Ease::OutCubic,
        Ease::InCubic,
        Ease::InOutCubic,
        Ease::OutQuad,
        Ease::InQuad,
        Ease::OutBack,
        Ease::OutElastic,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Ease::Linear => "linear",
            Ease::OutCubic => "outCubic",
            Ease::InCubic => "inCubic",
            Ease::InOutCubic => "inOutCubic",
            Ease::OutQuad => "outQuad",
            Ease::InQuad => "inQuad",
            Ease::OutBack => "outBack",
            Ease::OutElastic => "outElastic",
        }
    }

    /// Parse the camelCase name a script or a `.ron` uses.
    pub fn parse(s: &str) -> Option<Ease> {
        Ease::ALL.into_iter().find(|e| e.label() == s)
    }

    /// Shape a 0..1 progress. Values may leave 0..1 in the middle for the
    /// overshooting curves — that IS the effect, so nothing clamps here.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::OutCubic => 1.0 - (1.0 - t).powi(3),
            Ease::InCubic => t * t * t,
            Ease::InOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Ease::OutQuad => 1.0 - (1.0 - t) * (1.0 - t),
            Ease::InQuad => t * t,
            Ease::OutBack => {
                const C1: f32 = 1.70158;
                const C3: f32 = C1 + 1.0;
                1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
            }
            Ease::OutElastic => {
                if t == 0.0 || t == 1.0 {
                    return t;
                }
                const C4: f32 = 2.0 * std::f32::consts::PI / 3.0;
                2f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * C4).sin() + 1.0
            }
        }
    }
}

/// How long a state change takes and what shape it follows.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    /// Seconds. 0 = snap (the historical behaviour).
    pub duration: f32,
    pub ease: Ease,
}

impl Default for Transition {
    fn default() -> Self {
        // 90 ms of OutCubic: fast enough to feel like a response rather than an
        // animation, slow enough to read as motion. This is the single default
        // that turns "a button" into "a button that feels good".
        Transition { duration: 0.09, ease: Ease::OutCubic }
    }
}

// ---------------------------------------------------------------------------
// The interaction states
// ---------------------------------------------------------------------------

/// The closed set of element states. Order matters: [`UiState::pick`] returns
/// the FIRST match, so disabled beats pressed beats hover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UiState {
    #[default]
    Base,
    Disabled,
    Pressed,
    Hover,
    Focus,
    Selected,
}

/// Which elements are currently in a runtime interaction state. Hover/press/
/// focus come from the pointer and keyboard; disabled/selected are authored or
/// script-driven flags on the element itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct StateInput {
    pub hovered: Option<u32>,
    pub pressed: Option<u32>,
    pub focused: Option<u32>,
}

impl UiState {
    /// Resolve one element's state. Precedence, highest first: disabled (you
    /// cannot hover something that ignores you), pressed, hover, focus,
    /// selected.
    pub fn pick(id: u32, spec: &crate::ElementSpec, input: &StateInput) -> UiState {
        if spec.disabled {
            return UiState::Disabled;
        }
        if input.pressed == Some(id) {
            return UiState::Pressed;
        }
        if input.hovered == Some(id) {
            return UiState::Hover;
        }
        if input.focused == Some(id) {
            return UiState::Focus;
        }
        if spec.selected {
            return UiState::Selected;
        }
        UiState::Base
    }
}

// ---------------------------------------------------------------------------
// Style blocks
// ---------------------------------------------------------------------------

/// A set of property overrides. Every field is optional: `None` means "leave
/// whatever was there", which is what makes a `hover` block a small delta
/// rather than a full restatement of the element.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StyleBlock {
    // --- shape ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<ColorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gradient: Option<Gradient>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<Corners>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<Sides>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_color: Option<ColorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<ShadowSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glow: Option<GlowSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grain: Option<GrainSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blend: Option<Blend>,
    // --- element ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tint: Option<ColorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<[f32; 2]>,
    // --- text ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_color: Option<ColorRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_size: Option<NumRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case: Option<Case>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_stroke: Option<TextStroke>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_shadow: Option<TextShadow>,
    // --- layout (opt-in; a style CAN own padding/gap so a "card" is one name) ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pad: Option<NumRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<NumRef>,
}

/// A named style: a base look plus overrides per state, and how long it takes
/// to move between them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Style {
    #[serde(default)]
    pub base: StyleBlock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover: Option<StyleBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressed: Option<StyleBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<StyleBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<StyleBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<StyleBlock>,
    #[serde(default)]
    pub transition: Transition,
}

impl Style {
    /// The block for a state, or `None` when the style doesn't define one.
    pub fn block(&self, state: UiState) -> Option<&StyleBlock> {
        match state {
            UiState::Base => Some(&self.base),
            UiState::Hover => self.hover.as_ref(),
            UiState::Pressed => self.pressed.as_ref(),
            UiState::Disabled => self.disabled.as_ref(),
            UiState::Focus => self.focus.as_ref(),
            UiState::Selected => self.selected.as_ref(),
        }
    }
}

/// A project's named styles. Several `.uistyle.ron` files merge into one sheet,
/// the way materials and prefabs already work.
///
/// `transparent`, not `flatten`: a file is a bare RON map (`{ "button": (…) }`)
/// so style names can contain `/` and other characters a RON identifier can't.
/// `flatten` makes the deserializer expect identifiers and rejects the whole
/// file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StyleSheet {
    pub styles: BTreeMap<String, Style>,
}

/// Parse a `.uistyle.ron` / `.tokens.ron` with RON's **implicit-some**
/// extension enabled.
///
/// Every property in a [`StyleBlock`] is an `Option` (that is how "leave this
/// alone" is expressed), and plain RON would make a designer write
/// `fill: Some("accent")` on every single line. These files are the primary
/// hand-edited surface of the whole style system; the ceremony would be paid on
/// every property of every state of every style. `Some(…)` still parses, so
/// nothing is lost.
pub fn parse_ron<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, ron::error::SpannedError> {
    ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .from_str(text)
}

impl StyleSheet {
    /// Parse one `.uistyle.ron` file.
    pub fn parse(text: &str) -> Result<StyleSheet, ron::error::SpannedError> {
        parse_ron(text)
    }

    pub fn get(&self, name: &str) -> Option<&Style> {
        self.styles.get(name)
    }

    /// Merge another sheet in, returning the names that collided so the editor
    /// can warn. Silently shadowing a style would be a debugging nightmare.
    pub fn merge(&mut self, other: StyleSheet) -> Vec<String> {
        let mut clashes = Vec::new();
        for (name, style) in other.styles {
            if self.styles.insert(name.clone(), style).is_some() {
                clashes.push(name);
            }
        }
        clashes
    }
}

// ---------------------------------------------------------------------------
// Resolution + animation
// ---------------------------------------------------------------------------

/// The animatable subset of a style, flattened to plain numbers.
///
/// Only continuous properties live here — those are the ones a transition can
/// interpolate. Discrete properties (case, font, blend, gradient kind) apply
/// instantly on the state change, because half a font is not a thing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Animated {
    pub fill: [f32; 4],
    pub border_color: [f32; 4],
    pub text_color: [f32; 4],
    pub tint: [f32; 4],
    pub radius: [f32; 4],
    pub border: [f32; 4],
    pub opacity: f32,
    pub rotation: f32,
    pub scale: [f32; 2],
    pub text_size: f32,
    pub tracking: f32,
    /// Gradient far stop — carried so a gradient can animate with its fill.
    pub grad_to: [f32; 4],
}

impl Animated {
    fn lerp(a: &Animated, b: &Animated, t: f32) -> Animated {
        fn l(a: f32, b: f32, t: f32) -> f32 {
            a + (b - a) * t
        }
        fn l4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
            [l(a[0], b[0], t), l(a[1], b[1], t), l(a[2], b[2], t), l(a[3], b[3], t)]
        }
        Animated {
            fill: l4(a.fill, b.fill, t),
            border_color: l4(a.border_color, b.border_color, t),
            text_color: l4(a.text_color, b.text_color, t),
            tint: l4(a.tint, b.tint, t),
            radius: l4(a.radius, b.radius, t),
            border: l4(a.border, b.border, t),
            opacity: l(a.opacity, b.opacity, t),
            rotation: l(a.rotation, b.rotation, t),
            scale: [l(a.scale[0], b.scale[0], t), l(a.scale[1], b.scale[1], t)],
            text_size: l(a.text_size, b.text_size, t),
            tracking: l(a.tracking, b.tracking, t),
            grad_to: l4(a.grad_to, b.grad_to, t),
        }
    }
}

/// One element's in-flight transition.
#[derive(Clone, Debug)]
struct Anim {
    state: UiState,
    from: Animated,
    to: Animated,
    /// Seconds elapsed into the current transition.
    t: f32,
    dur: f32,
    ease: Ease,
}

/// Per-element transition state, owned by whoever drives the frame.
///
/// Kept OUTSIDE the scene on purpose: a hover that persisted into the saved
/// `.ron` would be a bug, and the play-snapshot machinery would have to know
/// about it. Nothing here is serialized, ever.
#[derive(Clone, Debug, Default)]
pub struct StyleRuntime {
    live: std::collections::HashMap<u32, Anim>,
}

impl StyleRuntime {
    /// Forget everything (scene change, Stop). Cheap; the map rebuilds itself
    /// on the next frame from whatever is on screen.
    pub fn clear(&mut self) {
        self.live.clear();
    }

    /// Drop entries for elements that no longer exist, so a long session
    /// doesn't accumulate state for despawned UI.
    pub fn retain(&mut self, alive: &dyn Fn(u32) -> bool) {
        self.live.retain(|id, _| alive(*id));
    }

    /// Advance `id` toward `target`, returning what to draw this frame.
    ///
    /// A state change restarts the transition FROM THE CURRENT ANIMATED VALUE,
    /// not from the old state's target — so un-hovering halfway through a hover
    /// eases back from where it actually is instead of snapping to full hover
    /// and then leaving.
    fn step(&mut self, id: u32, state: UiState, target: Animated, tr: Transition, dt: f32) -> Animated {
        let entry = self.live.entry(id).or_insert_with(|| Anim {
            state,
            from: target,
            to: target,
            t: tr.duration,
            dur: tr.duration,
            ease: tr.ease,
        });
        if entry.state != state || entry.to != target {
            let current = entry.current();
            *entry = Anim {
                state,
                from: current,
                to: target,
                t: 0.0,
                dur: tr.duration.max(0.0),
                ease: tr.ease,
            };
        }
        entry.t += dt;
        entry.current()
    }
}

impl Anim {
    fn current(&self) -> Animated {
        if self.dur <= 0.0 {
            return self.to;
        }
        let p = self.ease.apply((self.t / self.dur).clamp(0.0, 1.0));
        Animated::lerp(&self.from, &self.to, p)
    }
}

/// Read the animatable properties an element currently has authored on it —
/// the values a style with no override for a property should leave alone.
fn authored(spec: &crate::ElementSpec) -> Animated {
    let shape = spec.shape;
    Animated {
        fill: shape.map(|s| s.fill).unwrap_or([1.0; 4]),
        border_color: shape.map(|s| s.border_color).unwrap_or([0.0; 4]),
        text_color: spec.text.as_ref().map(|t| t.color).unwrap_or([1.0; 4]),
        tint: spec.tint,
        radius: shape.map(|s| s.radius.0).unwrap_or([0.0; 4]),
        border: shape.map(|s| s.border.0).unwrap_or([0.0; 4]),
        opacity: spec.opacity,
        rotation: spec.rotation,
        scale: spec.scale,
        text_size: spec.text.as_ref().map(|t| t.size).unwrap_or(24.0),
        tracking: spec.text.as_ref().map(|t| t.tracking).unwrap_or(0.0),
        grad_to: shape.and_then(|s| s.gradient).map(|g| g.to).unwrap_or([0.0; 4]),
    }
}

/// Fold a block's overrides onto a set of animatable values.
fn overlay(mut a: Animated, block: &StyleBlock, tk: &Tokens) -> Animated {
    if let Some(c) = &block.fill {
        a.fill = tk.color(c);
    }
    if let Some(c) = &block.border_color {
        a.border_color = tk.color(c);
    }
    if let Some(c) = &block.text_color {
        a.text_color = tk.color(c);
    }
    if let Some(c) = &block.tint {
        a.tint = tk.color(c);
    }
    if let Some(r) = block.radius {
        a.radius = r.0;
    }
    if let Some(b) = block.border {
        a.border = b.0;
    }
    if let Some(v) = block.opacity {
        a.opacity = v;
    }
    if let Some(v) = block.rotation {
        a.rotation = v;
    }
    if let Some(v) = block.scale {
        a.scale = v;
    }
    if let Some(v) = &block.text_size {
        a.text_size = tk.num(v, Scale::Text);
    }
    if let Some(v) = block.tracking {
        a.tracking = v;
    }
    if let Some(g) = block.gradient {
        a.grad_to = g.to;
    }
    a
}

/// Apply the discrete (non-interpolated) parts of a block straight onto a spec.
fn apply_discrete(spec: &mut crate::ElementSpec, block: &StyleBlock, tk: &Tokens) {
    if let Some(g) = block.gradient {
        let s = spec.shape.get_or_insert_with(Default::default);
        s.gradient = Some(g);
    }
    if let Some(sh) = block.shadow {
        spec.shape.get_or_insert_with(Default::default).shadow = Some(sh);
    }
    if let Some(gl) = block.glow {
        spec.shape.get_or_insert_with(Default::default).glow = Some(gl);
    }
    if let Some(gr) = block.grain {
        spec.shape.get_or_insert_with(Default::default).grain = Some(gr);
    }
    if let Some(b) = block.blend {
        spec.shape.get_or_insert_with(Default::default).blend = b;
    }
    if let Some(case) = block.case
        && let Some(t) = &mut spec.text
    {
        t.case = case;
    }
    if let Some(f) = &block.font
        && let Some(t) = &mut spec.text
    {
        t.font = tk.font(f);
    }
    if let Some(st) = block.text_stroke
        && let Some(t) = &mut spec.text
    {
        t.stroke = Some(st);
    }
    if let Some(sh) = block.text_shadow
        && let Some(t) = &mut spec.text
    {
        t.shadow = Some(sh);
    }
    if let Some(p) = &block.pad
        && let Some(st) = &mut spec.stack
    {
        st.pad = tk.num(p, Scale::Spacing);
    }
    if let Some(g) = &block.gap
        && let Some(st) = &mut spec.stack
    {
        st.gap = tk.num(g, Scale::Spacing);
    }
}

/// Write animated values back onto a spec.
fn apply_animated(spec: &mut crate::ElementSpec, a: &Animated, styled_shape: bool) {
    if styled_shape {
        let s = spec.shape.get_or_insert_with(Default::default);
        s.fill = a.fill;
        s.border_color = a.border_color;
        s.radius = Corners(a.radius);
        s.border = Sides(a.border);
        if let Some(g) = &mut s.gradient {
            g.to = a.grad_to;
        }
    }
    spec.opacity = a.opacity;
    spec.tint = a.tint;
    spec.rotation = a.rotation;
    spec.scale = a.scale;
    if let Some(t) = &mut spec.text {
        t.color = a.text_color;
        t.size = a.text_size;
        t.tracking = a.tracking;
    }
}

/// Resolve every styled element in a tree and advance its transitions.
///
/// Call this on the freshly-built [`crate::Node`] tree each frame, BEFORE
/// [`crate::solve`]. It mutates the tree's spec copies, never the scene — which
/// is the whole reason play-time hover states can't end up in a saved `.ron`.
///
/// `dt` is seconds since the last call. Pass 0 to resolve without advancing
/// (an editor drawing a paused frame).
pub fn apply_styles(
    roots: &mut [crate::Node],
    sheet: &StyleSheet,
    tokens: &Tokens,
    input: &StateInput,
    rt: &mut StyleRuntime,
    dt: f32,
) {
    fn walk(
        n: &mut crate::Node,
        sheet: &StyleSheet,
        tokens: &Tokens,
        input: &StateInput,
        rt: &mut StyleRuntime,
        dt: f32,
    ) {
        if !n.spec.style.is_empty()
            && let Some(style) = sheet.get(&n.spec.style)
        {
            let state = UiState::pick(n.id, &n.spec, input);
            // base always applies; the state block layers on top of it.
            let mut target = overlay(authored(&n.spec), &style.base, tokens);
            apply_discrete(&mut n.spec, &style.base, tokens);
            if state != UiState::Base
                && let Some(block) = style.block(state)
            {
                target = overlay(target, block, tokens);
                apply_discrete(&mut n.spec, block, tokens);
            }
            // A style that paints the shape needs one to paint: an element with
            // a fill in its style but no shape component would otherwise stay
            // invisible for no visible reason.
            let styled_shape = n.spec.shape.is_some()
                || style.base.fill.is_some()
                || style.base.border.is_some();
            let now = rt.step(n.id, state, target, style.transition, dt);
            apply_animated(&mut n.spec, &now, styled_shape);
        }
        for c in &mut n.children {
            walk(c, sheet, tokens, input, rt, dt);
        }
    }
    for r in roots {
        walk(r, sheet, tokens, input, rt, dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ElementSpec, Node, ShapeSpec, TextSpec};

    fn node(id: u32, spec: ElementSpec) -> Node {
        Node { id, spec, children: vec![] }
    }

    fn tokens() -> Tokens {
        Tokens {
            colors: BTreeMap::from([
                ("accent".into(), [1.0, 0.85, 0.35, 1.0]),
                ("bg".into(), [0.05, 0.06, 0.09, 1.0]),
            ]),
            spacing: BTreeMap::from([("md".into(), 12.0)]),
            radii: BTreeMap::from([("pill".into(), 999.0)]),
            text: BTreeMap::from([("title".into(), 28.0)]),
            fonts: BTreeMap::from([("ui".into(), "fonts/Inter.ttf".into())]),
        }
    }

    fn button_sheet() -> StyleSheet {
        let mut styles = BTreeMap::new();
        styles.insert(
            "button".to_string(),
            Style {
                base: StyleBlock {
                    fill: Some(ColorRef::Token("bg".into())),
                    text_color: Some(ColorRef::Token("accent".into())),
                    ..Default::default()
                },
                hover: Some(StyleBlock {
                    fill: Some(ColorRef::Token("accent".into())),
                    scale: Some([1.05, 1.05]),
                    ..Default::default()
                }),
                transition: Transition { duration: 0.1, ease: Ease::Linear },
                ..Default::default()
            },
        );
        StyleSheet { styles }
    }

    /// Untagged token refs: a designer writes `"accent"` or a literal colour,
    /// and both parse into the same field.
    #[test]
    fn token_refs_parse_both_ways() {
        #[derive(Deserialize)]
        struct S {
            a: ColorRef,
            b: ColorRef,
            c: NumRef,
        }
        let s: S = ron::from_str(r#"(a: "accent", b: (1.0, 0.0, 0.0, 1.0), c: 12.0)"#).unwrap();
        assert_eq!(s.a, ColorRef::Token("accent".into()));
        assert_eq!(s.b, ColorRef::Lit([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(s.c, NumRef::Lit(12.0));
    }

    /// An unknown colour token has to be VISIBLE. Falling back to a plausible
    /// colour turns a typo into a twenty-minute debugging session.
    #[test]
    fn unknown_color_token_is_loud() {
        let tk = tokens();
        assert_eq!(tk.color(&ColorRef::Token("nope".into())), [1.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn tokens_resolve_by_scale() {
        let tk = tokens();
        assert_eq!(tk.num(&NumRef::Token("md".into()), Scale::Spacing), 12.0);
        assert_eq!(tk.num(&NumRef::Token("pill".into()), Scale::Radii), 999.0);
        assert_eq!(tk.num(&NumRef::Token("title".into()), Scale::Text), 28.0);
        // Same name, wrong scale — a spacing token is not a radius.
        assert_eq!(tk.num(&NumRef::Token("md".into()), Scale::Radii), 0.0);
    }

    /// Rule 4: precedence is fixed. Disabled outranks everything, because an
    /// element that ignores you must not light up under the cursor.
    #[test]
    fn state_precedence_is_fixed() {
        let mut spec = ElementSpec { disabled: true, selected: true, ..Default::default() };
        let input = StateInput { hovered: Some(1), pressed: Some(1), focused: Some(1) };
        assert_eq!(UiState::pick(1, &spec, &input), UiState::Disabled);
        spec.disabled = false;
        assert_eq!(UiState::pick(1, &spec, &input), UiState::Pressed);
        let input = StateInput { hovered: Some(1), ..Default::default() };
        assert_eq!(UiState::pick(1, &spec, &input), UiState::Hover);
        assert_eq!(UiState::pick(1, &spec, &StateInput::default()), UiState::Selected);
    }

    #[test]
    fn base_block_paints_from_tokens() {
        let mut roots = vec![node(
            1,
            ElementSpec {
                style: "button".into(),
                shape: Some(ShapeSpec::default()),
                text: Some(TextSpec { text: "go".into(), ..Default::default() }),
                ..Default::default()
            },
        )];
        let mut rt = StyleRuntime::default();
        apply_styles(&mut roots, &button_sheet(), &tokens(), &StateInput::default(), &mut rt, 1.0);
        assert_eq!(roots[0].spec.shape.unwrap().fill, [0.05, 0.06, 0.09, 1.0]);
        assert_eq!(roots[0].spec.text.as_ref().unwrap().color, [1.0, 0.85, 0.35, 1.0]);
    }

    /// The transition actually takes time: halfway through, the value is
    /// halfway there. This is the whole "zero lines of Lua" promise.
    #[test]
    fn hover_transitions_over_its_duration() {
        let sheet = button_sheet();
        let tk = tokens();
        let mut rt = StyleRuntime::default();
        let make = || {
            vec![node(
                1,
                ElementSpec {
                    style: "button".into(),
                    shape: Some(ShapeSpec::default()),
                    ..Default::default()
                },
            )]
        };
        // Settle on base.
        let mut roots = make();
        apply_styles(&mut roots, &sheet, &tk, &StateInput::default(), &mut rt, 1.0);
        assert_eq!(roots[0].spec.scale, [1.0, 1.0]);

        // Hover for half the 0.1 s duration, linear ease → halfway to 1.05.
        let hover = StateInput { hovered: Some(1), ..Default::default() };
        let mut roots = make();
        apply_styles(&mut roots, &sheet, &tk, &hover, &mut rt, 0.05);
        let s = roots[0].spec.scale[0];
        assert!((s - 1.025).abs() < 1e-4, "expected halfway (1.025), got {s}");

        // Finish it.
        let mut roots = make();
        apply_styles(&mut roots, &sheet, &tk, &hover, &mut rt, 0.05);
        assert!((roots[0].spec.scale[0] - 1.05).abs() < 1e-4);
    }

    /// Un-hovering mid-transition must ease back from where the element ACTUALLY
    /// is, not snap to the full hover value first. Getting this wrong produces
    /// a visible pop that reads as a bug.
    #[test]
    fn interrupting_a_transition_starts_from_the_current_value() {
        let sheet = button_sheet();
        let tk = tokens();
        let mut rt = StyleRuntime::default();
        let make = || {
            vec![node(
                1,
                ElementSpec {
                    style: "button".into(),
                    shape: Some(ShapeSpec::default()),
                    ..Default::default()
                },
            )]
        };
        // Settle on base first. An element seen for the FIRST time in a state
        // snaps to it (a menu that opens already-hovered must not animate in),
        // so without this the "hover" frame below would start from hover
        // rather than transition into it.
        let mut roots = make();
        apply_styles(&mut roots, &sheet, &tk, &StateInput::default(), &mut rt, 1.0);

        let hover = StateInput { hovered: Some(1), ..Default::default() };
        let mut roots = make();
        apply_styles(&mut roots, &sheet, &tk, &hover, &mut rt, 0.05); // → 1.025
        assert!((roots[0].spec.scale[0] - 1.025).abs() < 1e-4);

        // Leave immediately; one frame later we must be BETWEEN 1.0 and 1.025 —
        // never above it, which is what snapping to the full hover value first
        // would produce, and what reads on screen as a pop.
        let mut roots = make();
        apply_styles(&mut roots, &sheet, &tk, &StateInput::default(), &mut rt, 0.01);
        let s = roots[0].spec.scale[0];
        assert!(s > 1.0 && s < 1.025, "should ease back down from 1.025, got {s}");
    }

    /// An element appearing already in a state settles there instead of
    /// animating in from nothing — opening a menu should not make every
    /// element visibly slide into place.
    #[test]
    fn first_sight_snaps_instead_of_animating() {
        let mut rt = StyleRuntime::default();
        let mut roots = vec![node(
            1,
            ElementSpec {
                style: "button".into(),
                shape: Some(ShapeSpec::default()),
                ..Default::default()
            },
        )];
        apply_styles(
            &mut roots,
            &button_sheet(),
            &tokens(),
            &StateInput { hovered: Some(1), ..Default::default() },
            &mut rt,
            0.0,
        );
        assert_eq!(roots[0].spec.scale, [1.05, 1.05]);
    }

    #[test]
    fn a_zero_duration_transition_snaps() {
        let mut sheet = button_sheet();
        sheet.styles.get_mut("button").unwrap().transition.duration = 0.0;
        let mut rt = StyleRuntime::default();
        let mut roots = vec![node(
            1,
            ElementSpec {
                style: "button".into(),
                shape: Some(ShapeSpec::default()),
                ..Default::default()
            },
        )];
        apply_styles(
            &mut roots,
            &sheet,
            &tokens(),
            &StateInput { hovered: Some(1), ..Default::default() },
            &mut rt,
            0.0,
        );
        assert_eq!(roots[0].spec.scale, [1.05, 1.05]);
    }

    /// Rule 2, the one that keeps this from becoming CSS: a property the style
    /// does not mention is left exactly as the designer authored it.
    #[test]
    fn unmentioned_properties_are_untouched() {
        let mut roots = vec![node(
            1,
            ElementSpec {
                style: "button".into(),
                shape: Some(ShapeSpec { radius: Corners::all(7.0), ..Default::default() }),
                ..Default::default()
            },
        )];
        let mut rt = StyleRuntime::default();
        apply_styles(&mut roots, &button_sheet(), &tokens(), &StateInput::default(), &mut rt, 1.0);
        assert_eq!(roots[0].spec.shape.unwrap().radius.0, [7.0; 4], "radius was not in the style");
    }

    /// An element naming a style that doesn't exist keeps its authored look
    /// rather than turning into a default rectangle.
    #[test]
    fn an_unknown_style_name_changes_nothing() {
        let mut roots = vec![node(
            1,
            ElementSpec {
                style: "nope".into(),
                shape: Some(ShapeSpec { fill: [0.2, 0.3, 0.4, 1.0], ..Default::default() }),
                ..Default::default()
            },
        )];
        let mut rt = StyleRuntime::default();
        apply_styles(&mut roots, &button_sheet(), &tokens(), &StateInput::default(), &mut rt, 1.0);
        assert_eq!(roots[0].spec.shape.unwrap().fill, [0.2, 0.3, 0.4, 1.0]);
    }

    #[test]
    fn merging_sheets_reports_clashes() {
        let mut a = button_sheet();
        let b = button_sheet();
        assert_eq!(a.merge(b), vec!["button".to_string()]);
    }

    #[test]
    fn easings_hit_their_endpoints() {
        for e in Ease::ALL {
            assert!((e.apply(0.0)).abs() < 1e-5, "{} at 0", e.label());
            assert!((e.apply(1.0) - 1.0).abs() < 1e-4, "{} at 1", e.label());
        }
        // OutBack is supposed to overshoot — that IS the effect.
        assert!(Ease::OutBack.apply(0.7) > 1.0);
    }

    #[test]
    fn ease_names_round_trip() {
        for e in Ease::ALL {
            assert_eq!(Ease::parse(e.label()), Some(e));
        }
        assert_eq!(Ease::parse("bogus"), None);
    }

    /// A style sheet is hand-edited, so its RON has to be pleasant.
    #[test]
    fn a_style_sheet_parses_from_readable_ron() {
        let text = r#"{
            "button/primary": (
                base: ( fill: "accent", radius: 10.0, case: Upper, tracking: 1.5 ),
                hover: ( scale: (1.03, 1.03) ),
                pressed: ( scale: (0.98, 0.98) ),
                transition: ( duration: 0.09, ease: OutCubic ),
            ),
        }"#;
        let sheet = StyleSheet::parse(text).unwrap();
        let s = sheet.get("button/primary").unwrap();
        // Note: NO `Some(...)` anywhere above. That is the whole point of
        // parsing these files with implicit-some.
        assert_eq!(s.base.fill, Some(ColorRef::Token("accent".into())));
        assert_eq!(s.base.case, Some(Case::Upper));
        assert_eq!(s.base.radius, Some(Corners::all(10.0)));
        assert_eq!(s.transition.duration, 0.09);
        assert!(s.disabled.is_none(), "a state with no block stays None");
    }

    /// Token files are hand-edited too.
    #[test]
    fn a_token_file_parses() {
        let text = r#"(
            colors: { "accent": (1.0, 0.85, 0.35, 1.0), "bg": (0.02, 0.03, 0.05, 1.0) },
            spacing: { "xs": 4.0, "sm": 8.0, "md": 12.0, "lg": 20.0, "xl": 32.0 },
            radii: { "sm": 4.0, "md": 10.0, "pill": 999.0 },
            text: { "caption": 14.0, "body": 18.0, "title": 28.0 },
            fonts: { "ui": "fonts/Inter.ttf" },
        )"#;
        let tk = Tokens::parse(text).unwrap();
        assert_eq!(tk.colors["accent"], [1.0, 0.85, 0.35, 1.0]);
        assert_eq!(tk.spacing["md"], 12.0);
        assert_eq!(tk.font("ui"), "fonts/Inter.ttf");
        // A name that looks like a path is taken literally, so a style can
        // point at a font without declaring a token for it.
        assert_eq!(tk.font("fonts/Other.otf"), "fonts/Other.otf");
        assert_eq!(tk.font("nope"), "", "an unknown bare name = the fallback font");
    }

    /// The whole seam, end to end: style → solve → draw list. Proves the
    /// styled values actually reach the quads the renderer packs, not just the
    /// spec — the two are separated by the layout pass, and a style that sets
    /// padding changes what that pass measures.
    #[test]
    fn styles_reach_the_draw_list_through_layout() {
        use crate::{draw_list, solve, Size, StackCfg};
        let mut styles = BTreeMap::new();
        styles.insert(
            "card".to_string(),
            Style {
                base: StyleBlock {
                    fill: Some(ColorRef::Token("accent".into())),
                    radius: Some(Corners::all(6.0)),
                    // A style owning padding is the case that forces styles to
                    // resolve BEFORE layout.
                    pad: Some(NumRef::Token("md".into())),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let sheet = StyleSheet { styles };

        let child = node(
            2,
            ElementSpec {
                size: [Size::Fixed(20.0), Size::Fixed(20.0)],
                shape: Some(ShapeSpec::default()),
                ..Default::default()
            },
        );
        let mut roots = vec![Node {
            id: 1,
            spec: ElementSpec {
                style: "card".into(),
                size: [Size::Fixed(200.0), Size::Fixed(100.0)],
                stack: Some(StackCfg { pad: 0.0, gap: 0.0, ..Default::default() }),
                shape: Some(ShapeSpec::default()),
                ..Default::default()
            },
            children: vec![child],
        }];

        let mut rt = StyleRuntime::default();
        apply_styles(&mut roots, &sheet, &tokens(), &StateInput::default(), &mut rt, 1.0);
        let placed = solve(&roots, [400.0, 300.0], &|_| [0.0, 0.0]);
        let dl = draw_list(&roots, &placed, &[]);

        // The card is painted from the token.
        assert_eq!(dl.quads[0].color, [1.0, 0.85, 0.35, 1.0]);
        assert_eq!(dl.quads[0].radius, [6.0; 4]);
        // …and the child sits inset by the style's padding token, which only
        // works because the style resolved before the solver ran.
        let child_rect = placed.iter().find(|p| p.id == 2).unwrap().rect;
        assert_eq!([child_rect[0], child_rect[1]], [12.0, 12.0]);
    }

    #[test]
    fn runtime_state_can_be_pruned() {
        let mut rt = StyleRuntime::default();
        let mut roots = vec![node(
            1,
            ElementSpec {
                style: "button".into(),
                shape: Some(ShapeSpec::default()),
                ..Default::default()
            },
        )];
        apply_styles(&mut roots, &button_sheet(), &tokens(), &StateInput::default(), &mut rt, 0.1);
        assert_eq!(rt.live.len(), 1);
        rt.retain(&|_| false);
        assert!(rt.live.is_empty());
    }
}
