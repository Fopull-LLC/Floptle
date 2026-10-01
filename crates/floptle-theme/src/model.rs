//! The theme file, and the theme it resolves to.
//!
//! A theme is `theme.ron`, plus whatever images, shaders and fonts it names,
//! in a folder or zipped into a `.floptletheme`. Every key is optional. What a
//! theme leaves out comes from the theme it `extends`, and every chain ends at
//! **Floptle Dark**, which sets everything — so a missing key never falls back
//! to egui's stock look (contract `floptle-brand` §7.1).
//!
//! ```ron
//! (
//!     format: 1,
//!     id: "galaxy",
//!     name: "Galaxy",
//!     colors: (accent: "#9b8cff", on_accent: "#0d0a1f", surface: "#16142acc"),
//!     surfaces: {
//!         "ground": (layers: Some([Shader(shader: "builtin:galaxy")])),
//!         "tab.inspector": (layers: Some([Image(path: "images/me.png", fit: Contain, align: BottomRight)])),
//!     },
//! )
//! ```
//!
//! Resolution is strict where it is cheap to be: an unknown key, a colour that
//! does not parse, a `$reference` to nothing, or an `accent` with no
//! `on_accent` refuses the whole file with one sentence naming the key. A
//! theme never half-applies (§7.6).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::color::{self, ColorExpr, Rgba};

/// The format this build writes and reads. A file with a higher number was
/// made by a newer Floptle and is refused with that sentence, rather than
/// read with keys silently ignored.
pub const FORMAT: u32 = 1;

/// Every colour token, in the contract's order (§2), plus the two the editor
/// needs that the site does not: `backdrop` (what is under a translucent
/// panel) and `selection` (selected text).
pub const TOKENS: &[&str] = &[
    "ground",
    "surface",
    "surface_2",
    "well",
    "hairline",
    "hairline_quiet",
    "text",
    "dim",
    "faint",
    "accent",
    "accent_hi",
    "accent_wash",
    "accent_edge",
    "on_accent",
    "backdrop",
    "selection",
];

/// The regions a surface can be set for, with the region each inherits its
/// layers from when it sets none. `tab.<name>` is open-ended (one per editor
/// tab) and inherits from `panel`.
pub const REGIONS: &[(&str, Option<&str>, &str)] = &[
    ("ground", None, "the window behind everything"),
    ("panel", Some("ground"), "every docked panel's body"),
    ("menu_bar", Some("ground"), "the editor's menu bar"),
    ("tab_bar", Some("ground"), "the strip of tabs above each panel"),
    ("code_editor", None, "the script editor's text area"),
    ("window", None, "floating windows and dialogs"),
    ("hub.header", Some("ground"), "the Hub's top bar"),
    ("hub.content", Some("ground"), "the Hub's main area"),
    ("hub.card", None, "a project or version card in the Hub"),
];

/// The token a region is filled with when it does not say.
pub fn default_fill_token(region: &str) -> &'static str {
    match region {
        "code_editor" => "well",
        "window" | "hub.card" => "surface",
        "tab_bar" => "ground",
        _ => "ground",
    }
}

/// The region `region` takes its layers from when it sets none.
pub fn parent_region(region: &str) -> Option<&'static str> {
    if region.starts_with("tab.") {
        return Some("panel");
    }
    REGIONS.iter().find(|r| r.0 == region).and_then(|r| r.1)
}

// ---------------------------------------------------------------------------
// The file, as written. Every field optional; `deny_unknown_fields` so a typo
// is refused by name instead of quietly doing nothing.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ThemeFile {
    pub format: u32,
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A built-in theme's id. Absent means `floptle-dark`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dark: Option<bool>,
    /// An image shown on the theme's card in the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub colors: ColorsFile,
    pub code: CodeFile,
    pub fonts: FontsFile,
    pub shape: ShapeFile,
    pub motion: MotionFile,
    pub surfaces: BTreeMap<String, SurfaceFile>,
}

macro_rules! opt_struct {
    ($(#[$m:meta])* $name:ident { $($(#[$fm:meta])* $f:ident : $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields, default)]
        pub struct $name {
            $($(#[$fm])* #[serde(skip_serializing_if = "Option::is_none")] pub $f: Option<$t>,)*
        }
        impl $name {
            /// `self` over `under`: each key this one sets wins.
            pub fn over(&self, under: &Self) -> Self {
                Self { $($f: self.$f.clone().or_else(|| under.$f.clone()),)* }
            }
        }
    };
}

opt_struct!(
    /// The colour tokens (contract §2). Strings: see [`crate::color`].
    ColorsFile {
        ground: String,
        surface: String,
        surface_2: String,
        well: String,
        hairline: String,
        hairline_quiet: String,
        text: String,
        dim: String,
        faint: String,
        accent: String,
        accent_hi: String,
        accent_wash: String,
        accent_edge: String,
        on_accent: String,
        backdrop: String,
        selection: String,
    }
);

impl ColorsFile {
    pub fn get(&self, token: &str) -> Option<&String> {
        match token {
            "ground" => self.ground.as_ref(),
            "surface" => self.surface.as_ref(),
            "surface_2" => self.surface_2.as_ref(),
            "well" => self.well.as_ref(),
            "hairline" => self.hairline.as_ref(),
            "hairline_quiet" => self.hairline_quiet.as_ref(),
            "text" => self.text.as_ref(),
            "dim" => self.dim.as_ref(),
            "faint" => self.faint.as_ref(),
            "accent" => self.accent.as_ref(),
            "accent_hi" => self.accent_hi.as_ref(),
            "accent_wash" => self.accent_wash.as_ref(),
            "accent_edge" => self.accent_edge.as_ref(),
            "on_accent" => self.on_accent.as_ref(),
            "backdrop" => self.backdrop.as_ref(),
            "selection" => self.selection.as_ref(),
            _ => None,
        }
    }

    pub fn set(&mut self, token: &str, v: Option<String>) {
        let slot = match token {
            "ground" => &mut self.ground,
            "surface" => &mut self.surface,
            "surface_2" => &mut self.surface_2,
            "well" => &mut self.well,
            "hairline" => &mut self.hairline,
            "hairline_quiet" => &mut self.hairline_quiet,
            "text" => &mut self.text,
            "dim" => &mut self.dim,
            "faint" => &mut self.faint,
            "accent" => &mut self.accent,
            "accent_hi" => &mut self.accent_hi,
            "accent_wash" => &mut self.accent_wash,
            "accent_edge" => &mut self.accent_edge,
            "on_accent" => &mut self.on_accent,
            "backdrop" => &mut self.backdrop,
            "selection" => &mut self.selection,
            _ => return,
        };
        *slot = v;
    }
}

opt_struct!(
    /// The script editor's colours (what `CodeTheme` held).
    CodeFile {
        background: String,
        gutter: String,
        keyword: String,
        api: String,
        string: String,
        number: String,
        comment: String,
        text: String,
        current_line: String,
    }
);

opt_struct!(
    /// Typefaces. A face is a built-in name ([`crate::fonts::BUILTIN_FACES`])
    /// or a `.ttf`/`.otf` path inside the theme.
    FontsFile {
        ui: String,
        mono: String,
        display: String,
        /// Weight for variable faces, 100–900.
        ui_weight: f32,
        display_weight: f32,
        /// Body text, in points.
        size: f32,
        /// Code and data, in points. Defaults to `size`.
        mono_size: f32,
    }
);

opt_struct!(
    ShapeFile {
        /// Panels, windows and buttons.
        radius: f32,
        /// Chips and small data boxes.
        radius_small: f32,
        /// Text fields, sliders, checkboxes. Defaults to `radius_small`.
        widget_radius: f32,
        /// Hairline width.
        stroke: f32,
        /// Space between and inside widgets; 1.0 is the editor's usual.
        density: f32,
        /// A soft shadow under windows and menus. Floptle Dark has none.
        shadow: ShadowFile,
    }
);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShadowFile {
    pub blur: f32,
    pub spread: f32,
    pub offset: (f32, f32),
    pub color: String,
}

impl Default for ShadowFile {
    fn default() -> Self {
        Self { blur: 16.0, spread: 0.0, offset: (0.0, 6.0), color: "#00000066".into() }
    }
}

opt_struct!(
    MotionFile {
        /// How long a widget's hover/open animation takes. 0 turns them off.
        animation_ms: f32,
    }
);

/// What fills one region: layers drawn bottom to top, then `fill` over them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SurfaceFile {
    /// The region's own colour. Over layers, it is a veil, so give it alpha.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// `None` takes the parent region's layers; `Some([])` means none here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<LayerFile>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Blend {
    #[default]
    Normal,
    /// Light added to what is under it: glows, stars, scanline sheen.
    Add,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fit {
    /// Fill the region, cropping the image.
    #[default]
    Cover,
    /// The whole image, inside the region.
    Contain,
    /// Fill the region, squashing the image.
    Stretch,
    /// Repeat at the image's own size (times `scale`).
    Tile,
    /// The image's own size (times `scale`), placed by `align`.
    Natural,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Align {
    /// 0–1 position along each axis.
    pub fn factors(self) -> (f32, f32) {
        match self {
            Align::Center => (0.5, 0.5),
            Align::Top => (0.5, 0.0),
            Align::Bottom => (0.5, 1.0),
            Align::Left => (0.0, 0.5),
            Align::Right => (1.0, 0.5),
            Align::TopLeft => (0.0, 0.0),
            Align::TopRight => (1.0, 0.0),
            Align::BottomLeft => (0.0, 1.0),
            Align::BottomRight => (1.0, 1.0),
        }
    }
}

/// Whether an image or gradient is laid out against the region it fills, or
/// against the whole window (so it reads as one picture across every panel
/// that shows it, the way a shader backdrop does).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Space {
    #[default]
    Region,
    Window,
}

fn one() -> f32 {
    1.0
}
fn ninety() -> f32 {
    90.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum LayerFile {
    /// A WGSL fragment shader, animated. `builtin:<name>` or a `.wgsl` path
    /// inside the theme; see [`crate::backdrop`] for what it is handed.
    Shader {
        shader: String,
        #[serde(default = "one")]
        opacity: f32,
        /// Animation speed; 0 holds one frame.
        #[serde(default = "one")]
        speed: f32,
        /// Pattern scale, handed to the shader.
        #[serde(default = "one")]
        scale: f32,
        /// Up to four colours for the shader. Default: accent, accent_hi,
        /// ground, text.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        colors: Vec<String>,
        /// Up to eight numbers the shader may read.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        params: Vec<f32>,
        /// An image the shader can sample.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<String>,
        #[serde(default)]
        blend: Blend,
    },
    Image {
        path: String,
        #[serde(default)]
        fit: Fit,
        #[serde(default)]
        align: Align,
        #[serde(default = "one")]
        scale: f32,
        /// Points, after alignment.
        #[serde(default)]
        offset: (f32, f32),
        #[serde(default = "one")]
        opacity: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tint: Option<String>,
        #[serde(default)]
        blend: Blend,
        #[serde(default)]
        space: Space,
        /// Nearest-neighbour sampling, for pixel art.
        #[serde(default)]
        pixelated: bool,
    },
    Gradient {
        /// `(position 0–1, colour)`.
        stops: Vec<(f32, String)>,
        /// Degrees; 90 runs top to bottom, 0 left to right.
        #[serde(default = "ninety")]
        angle: f32,
        #[serde(default)]
        radial: bool,
        #[serde(default = "one")]
        opacity: f32,
        #[serde(default)]
        blend: Blend,
        #[serde(default)]
        space: Space,
    },
    Solid {
        color: String,
    },
}

// ---------------------------------------------------------------------------
// The resolved theme.
// ---------------------------------------------------------------------------

/// Every colour token, resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct Tokens {
    pub ground: Rgba,
    pub surface: Rgba,
    pub surface_2: Rgba,
    pub well: Rgba,
    pub hairline: Rgba,
    pub hairline_quiet: Rgba,
    pub text: Rgba,
    pub dim: Rgba,
    pub faint: Rgba,
    pub accent: Rgba,
    pub accent_hi: Rgba,
    pub accent_wash: Rgba,
    pub accent_edge: Rgba,
    pub on_accent: Rgba,
    pub backdrop: Rgba,
    pub selection: Rgba,
}

impl Tokens {
    pub fn get(&self, token: &str) -> Option<Rgba> {
        Some(match token {
            "ground" => self.ground,
            "surface" => self.surface,
            "surface_2" => self.surface_2,
            "well" => self.well,
            "hairline" => self.hairline,
            "hairline_quiet" => self.hairline_quiet,
            "text" => self.text,
            "dim" => self.dim,
            "faint" => self.faint,
            "accent" => self.accent,
            "accent_hi" => self.accent_hi,
            "accent_wash" => self.accent_wash,
            "accent_edge" => self.accent_edge,
            "on_accent" => self.on_accent,
            "backdrop" => self.backdrop,
            "selection" => self.selection,
            _ => return None,
        })
    }

    /// `c` as it lands on an opaque screen: over the backdrop if it is
    /// translucent. What to paint where nothing else is underneath.
    pub fn solid(&self, c: Rgba) -> Rgba {
        if c.is_opaque() { c } else { c.over(self.backdrop.over(Rgba::rgb(0, 0, 0))) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodeColors {
    pub background: Rgba,
    pub gutter: Rgba,
    pub keyword: Rgba,
    pub api: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub comment: Rgba,
    pub text: Rgba,
    pub current_line: Rgba,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fonts {
    pub ui: String,
    pub mono: String,
    pub display: String,
    pub ui_weight: f32,
    pub display_weight: f32,
    pub size: f32,
    pub mono_size: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shadow {
    pub blur: f32,
    pub spread: f32,
    pub offset: (f32, f32),
    pub color: Rgba,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub radius: f32,
    pub radius_small: f32,
    pub widget_radius: f32,
    pub stroke: f32,
    pub density: f32,
    pub shadow: Option<Shadow>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Layer {
    Shader {
        shader: String,
        opacity: f32,
        speed: f32,
        scale: f32,
        colors: [Rgba; 4],
        params: [f32; 8],
        image: Option<String>,
        blend: Blend,
    },
    Image {
        path: String,
        fit: Fit,
        align: Align,
        scale: f32,
        offset: (f32, f32),
        opacity: f32,
        tint: Rgba,
        blend: Blend,
        space: Space,
        pixelated: bool,
    },
    Gradient {
        stops: Vec<(f32, Rgba)>,
        angle: f32,
        radial: bool,
        opacity: f32,
        blend: Blend,
        space: Space,
    },
    Solid(Rgba),
}

impl Layer {
    /// Shader and image layers are "effects": the ones the user's Effects
    /// setting turns off. A gradient is as cheap as a fill and stays.
    pub fn is_effect(&self) -> bool {
        matches!(self, Layer::Shader { .. } | Layer::Image { .. })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Surface {
    pub fill: Option<Rgba>,
    pub layers: Option<Vec<Layer>>,
}

/// Where a theme came from: what the picker shows beside it, and what decides
/// whether it can be edited in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Builtin,
    /// The user's themes folder.
    User(std::path::PathBuf),
    /// Shipped inside an installed package (its id).
    Package { package: String, path: std::path::PathBuf },
}

/// The bytes a theme's layers and fonts refer to, read once at load.
#[derive(Clone, Debug, Default)]
pub struct Assets {
    pub files: BTreeMap<String, Arc<[u8]>>,
}

/// A theme, resolved: every value known, every reference followed, every file
/// it names read.
#[derive(Clone, Debug)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub dark: bool,
    pub tokens: Tokens,
    pub code: CodeColors,
    pub fonts: Fonts,
    pub shape: Shape,
    pub animation_ms: f32,
    pub surfaces: BTreeMap<String, Surface>,
    pub preview: Option<String>,
    pub assets: Assets,
    pub origin: Origin,
    /// Things worth saying that did not stop it loading: low contrast, a
    /// region name nothing draws.
    pub warnings: Vec<String>,
    /// The merged file, kept so the theme editor can start from it.
    pub file: ThemeFile,
}

impl Theme {
    /// The surface for `region`: its own fill (or its default token), and the
    /// layers of the nearest region up its chain that sets any.
    pub fn surface(&self, region: &str) -> (Rgba, &[Layer]) {
        let own = self.surfaces.get(region);
        let fill = own
            .and_then(|s| s.fill)
            .unwrap_or_else(|| self.tokens.get(default_fill_token(region)).unwrap_or(self.tokens.ground));
        let mut r = Some(region);
        let mut layers: &[Layer] = &[];
        let mut guard = 0;
        while let Some(name) = r {
            if let Some(Some(l)) = self.surfaces.get(name).map(|s| s.layers.as_ref()) {
                layers = l;
                break;
            }
            r = parent_region(name);
            guard += 1;
            if guard > 8 {
                break;
            }
        }
        (fill, layers)
    }

    /// Does any region carry a shader? (Then the host must keep repainting.)
    pub fn is_animated(&self) -> bool {
        self.surfaces.values().any(|s| {
            s.layers
                .as_ref()
                .is_some_and(|l| l.iter().any(|l| matches!(l, Layer::Shader { speed, .. } if *speed != 0.0)))
        })
    }

    pub fn has_effects(&self) -> bool {
        self.surfaces
            .values()
            .any(|s| s.layers.as_ref().is_some_and(|l| l.iter().any(Layer::is_effect)))
    }
}

/// Why a theme was refused: the file and one sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeError {
    pub file: String,
    pub message: String,
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file, self.message)
    }
}

impl std::error::Error for ThemeError {}

/// Read `theme.ron` text into the file shape. `file` names it in an error.
pub fn parse_file(text: &str, file: &str) -> Result<ThemeFile, ThemeError> {
    let err = |message: String| ThemeError { file: file.to_string(), message };
    let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
    let tf: ThemeFile = opts.from_str(text).map_err(|e| {
        let pos = e.position;
        err(format!("line {}, column {}: {}", pos.line, pos.col, e.code))
    })?;
    if tf.format > FORMAT {
        return Err(err(format!(
            "this theme is format {}, made by a newer Floptle (this one reads format {FORMAT}); update Floptle to use it",
            tf.format
        )));
    }
    if tf.id.is_empty() {
        return Err(err("the theme has no id; add one, such as id: \"my-theme\"".into()));
    }
    if !tf.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err(err(format!(
            "the id {:?} may use only lowercase letters, digits, '-' and '_'",
            tf.id
        )));
    }
    if tf.name.trim().is_empty() {
        return Err(err("the theme has no name; add one, such as name: \"My Theme\"".into()));
    }
    // §7.3: a theme that chooses its own accent says what text goes on it.
    if tf.colors.accent.is_some() && tf.colors.on_accent.is_none() {
        return Err(err(
            "colors.accent is set but colors.on_accent is not; a filled button needs to know what colour its text is"
                .into(),
        ));
    }
    Ok(tf)
}

/// `child` over `parent`, key by key. Surfaces merge per region.
pub fn merge(child: &ThemeFile, parent: &ThemeFile) -> ThemeFile {
    let mut surfaces = parent.surfaces.clone();
    for (k, v) in &child.surfaces {
        surfaces.insert(k.clone(), v.clone());
    }
    ThemeFile {
        format: child.format.max(parent.format),
        id: child.id.clone(),
        name: child.name.clone(),
        author: child.author.clone(),
        description: child.description.clone(),
        extends: child.extends.clone(),
        dark: child.dark.or(parent.dark),
        preview: child.preview.clone(),
        colors: child.colors.over(&parent.colors),
        code: child.code.over(&parent.code),
        fonts: child.fonts.over(&parent.fonts),
        shape: child.shape.over(&parent.shape),
        motion: child.motion.over(&parent.motion),
        surfaces,
    }
}

/// Resolve a merged file (one whose chain reached Floptle Dark, so every key
/// is present) into a [`Theme`]. `assets` holds the bytes of every path the
/// file names; a path with no bytes is an error naming it.
pub fn resolve(merged: &ThemeFile, file: &str, assets: Assets, origin: Origin) -> Result<Theme, ThemeError> {
    let err = |message: String| ThemeError { file: file.to_string(), message };

    // Colour tokens, references followed.
    let mut exprs: BTreeMap<&str, ColorExpr> = BTreeMap::new();
    for &t in TOKENS {
        let s = merged
            .colors
            .get(t)
            .ok_or_else(|| err(format!("colors.{t} is missing and nothing it extends sets it")))?;
        exprs.insert(t, color::parse(s).map_err(|e| err(format!("colors.{t}: {e}")))?);
    }
    fn follow(
        t: &str,
        exprs: &BTreeMap<&str, ColorExpr>,
        depth: u32,
    ) -> Result<Rgba, String> {
        if depth > 8 {
            return Err(format!("colors.{t} refers round in a circle"));
        }
        match exprs.get(t) {
            Some(ColorExpr::Lit(c)) => Ok(*c),
            Some(ColorExpr::Ref { token, alpha }) => {
                if !exprs.contains_key(token.as_str()) {
                    return Err(format!("colors.{t} refers to ${token}, which is not a colour token"));
                }
                Ok(follow(token, exprs, depth + 1)?.with_alpha_mul(*alpha))
            }
            None => Err(format!("${t} is not a colour token")),
        }
    }
    let tok = |t: &str| follow(t, &exprs, 0).map_err(&err);
    let tokens = Tokens {
        ground: tok("ground")?,
        surface: tok("surface")?,
        surface_2: tok("surface_2")?,
        well: tok("well")?,
        hairline: tok("hairline")?,
        hairline_quiet: tok("hairline_quiet")?,
        text: tok("text")?,
        dim: tok("dim")?,
        faint: tok("faint")?,
        accent: tok("accent")?,
        accent_hi: tok("accent_hi")?,
        accent_wash: tok("accent_wash")?,
        accent_edge: tok("accent_edge")?,
        on_accent: tok("on_accent")?,
        backdrop: tok("backdrop")?,
        selection: tok("selection")?,
    };

    // Any other colour in the file: a literal or a token reference.
    let any_color = |key: &str, s: &str| -> Result<Rgba, ThemeError> {
        match color::parse(s).map_err(|e| err(format!("{key}: {e}")))? {
            ColorExpr::Lit(c) => Ok(c),
            ColorExpr::Ref { token, alpha } => tokens
                .get(&token)
                .map(|c| c.with_alpha_mul(alpha))
                .ok_or_else(|| err(format!("{key} refers to ${token}, which is not a colour token"))),
        }
    };

    let cf = &merged.code;
    let code_c = |k: &str, v: &Option<String>| -> Result<Rgba, ThemeError> {
        let s = v.as_ref().ok_or_else(|| err(format!("code.{k} is missing")))?;
        any_color(&format!("code.{k}"), s)
    };
    let code = CodeColors {
        background: code_c("background", &cf.background)?,
        gutter: code_c("gutter", &cf.gutter)?,
        keyword: code_c("keyword", &cf.keyword)?,
        api: code_c("api", &cf.api)?,
        string: code_c("string", &cf.string)?,
        number: code_c("number", &cf.number)?,
        comment: code_c("comment", &cf.comment)?,
        text: code_c("text", &cf.text)?,
        current_line: code_c("current_line", &cf.current_line)?,
    };

    let ff = &merged.fonts;
    let need = |k: &str| err(format!("fonts.{k} is missing"));
    let size = ff.size.ok_or_else(|| need("size"))?;
    if !(8.0..=32.0).contains(&size) {
        return Err(err(format!("fonts.size is {size}; it must be between 8 and 32 points")));
    }
    let fonts = Fonts {
        ui: ff.ui.clone().ok_or_else(|| need("ui"))?,
        mono: ff.mono.clone().ok_or_else(|| need("mono"))?,
        display: ff.display.clone().ok_or_else(|| need("display"))?,
        ui_weight: ff.ui_weight.unwrap_or(400.0).clamp(100.0, 900.0),
        display_weight: ff.display_weight.unwrap_or(650.0).clamp(100.0, 900.0),
        size,
        mono_size: ff.mono_size.unwrap_or(size).clamp(8.0, 32.0),
    };
    for (k, face) in [("ui", &fonts.ui), ("mono", &fonts.mono), ("display", &fonts.display)] {
        if !crate::fonts::is_builtin(face) && !assets.files.contains_key(face.as_str()) {
            return Err(err(format!(
                "fonts.{k} is {face:?}, which is neither a built-in face ({}) nor a file in the theme",
                crate::fonts::BUILTIN_FACES.join(", ")
            )));
        }
    }

    let sf = &merged.shape;
    let radius = sf.radius.unwrap_or(6.0).clamp(0.0, 24.0);
    let radius_small = sf.radius_small.unwrap_or(4.0).clamp(0.0, 24.0);
    let shape = Shape {
        radius,
        radius_small,
        widget_radius: sf.widget_radius.unwrap_or(radius_small).clamp(0.0, 24.0),
        stroke: sf.stroke.unwrap_or(1.0).clamp(0.0, 4.0),
        density: sf.density.unwrap_or(1.0).clamp(0.6, 1.6),
        shadow: match &sf.shadow {
            None => None,
            Some(s) => Some(Shadow {
                blur: s.blur.clamp(0.0, 64.0),
                spread: s.spread.clamp(0.0, 32.0),
                offset: s.offset,
                color: any_color("shape.shadow.color", &s.color)?,
            }),
        },
    };

    // Surfaces.
    let mut surfaces = BTreeMap::new();
    let mut warnings = Vec::new();
    let need_asset = |key: &str, path: &str| -> Result<(), ThemeError> {
        if assets.files.contains_key(path) {
            Ok(())
        } else {
            Err(err(format!("{key} names {path:?}, which is not in the theme")))
        }
    };
    for (region, s) in &merged.surfaces {
        if !region.starts_with("tab.") && !REGIONS.iter().any(|r| r.0 == region) {
            warnings.push(format!(
                "surfaces has {region:?}, which is not a region; the regions are {} and tab.<name>",
                REGIONS.iter().map(|r| r.0).collect::<Vec<_>>().join(", ")
            ));
            continue;
        }
        let key = format!("surfaces.{region:?}");
        let fill = s.fill.as_ref().map(|f| any_color(&format!("{key}.fill"), f)).transpose()?;
        let layers = match &s.layers {
            None => None,
            Some(ls) => {
                let mut out = Vec::new();
                for (i, l) in ls.iter().enumerate() {
                    let lk = format!("{key}.layers[{i}]");
                    out.push(match l {
                        LayerFile::Shader { shader, opacity, speed, scale, colors, params, image, blend } => {
                            if !shader.starts_with("builtin:") {
                                need_asset(&lk, shader)?;
                            } else if crate::backdrop::builtin_source(shader).is_none() {
                                return Err(err(format!(
                                    "{lk} names {shader:?}; the built-in shaders are {}",
                                    crate::backdrop::BUILTIN_SHADERS
                                        .iter()
                                        .map(|(n, _)| format!("builtin:{n}"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )));
                            }
                            if let Some(img) = image {
                                need_asset(&lk, img)?;
                            }
                            if colors.len() > 4 || params.len() > 8 {
                                return Err(err(format!("{lk} has more than 4 colors or 8 params")));
                            }
                            let mut cs = [tokens.accent, tokens.accent_hi, tokens.ground, tokens.text];
                            for (j, c) in colors.iter().enumerate() {
                                cs[j] = any_color(&format!("{lk}.colors[{j}]"), c)?;
                            }
                            let mut ps = [0.0f32; 8];
                            ps[..params.len()].copy_from_slice(params);
                            Layer::Shader {
                                shader: shader.clone(),
                                opacity: opacity.clamp(0.0, 1.0),
                                speed: *speed,
                                scale: scale.max(0.01),
                                colors: cs,
                                params: ps,
                                image: image.clone(),
                                blend: *blend,
                            }
                        }
                        LayerFile::Image {
                            path, fit, align, scale, offset, opacity, tint, blend, space, pixelated,
                        } => {
                            need_asset(&lk, path)?;
                            Layer::Image {
                                path: path.clone(),
                                fit: *fit,
                                align: *align,
                                scale: scale.max(0.01),
                                offset: *offset,
                                opacity: opacity.clamp(0.0, 1.0),
                                tint: tint
                                    .as_ref()
                                    .map(|t| any_color(&format!("{lk}.tint"), t))
                                    .transpose()?
                                    .unwrap_or(Rgba::rgb(255, 255, 255)),
                                blend: *blend,
                                space: *space,
                                pixelated: *pixelated,
                            }
                        }
                        LayerFile::Gradient { stops, angle, radial, opacity, blend, space } => {
                            if stops.len() < 2 {
                                return Err(err(format!("{lk} needs at least two stops")));
                            }
                            let mut st = Vec::new();
                            for (j, (p, c)) in stops.iter().enumerate() {
                                st.push((p.clamp(0.0, 1.0), any_color(&format!("{lk}.stops[{j}]"), c)?));
                            }
                            st.sort_by(|a, b| a.0.total_cmp(&b.0));
                            Layer::Gradient {
                                stops: st,
                                angle: *angle,
                                radial: *radial,
                                opacity: opacity.clamp(0.0, 1.0),
                                blend: *blend,
                                space: *space,
                            }
                        }
                        LayerFile::Solid { color } => Layer::Solid(any_color(&format!("{lk}.color"), color)?),
                    });
                }
                Some(out)
            }
        };
        surfaces.insert(region.clone(), Surface { fill, layers });
    }
    if let Some(p) = &merged.preview {
        need_asset("preview", p)?;
    }

    // §7.7: readable or not, it loads; but somebody is told.
    let solid = |c: Rgba| tokens.solid(c);
    for (what, fg, bg) in [
        ("text on ground", tokens.text, tokens.ground),
        ("text on surface", tokens.text, tokens.surface),
        ("on_accent on accent", tokens.on_accent, tokens.accent),
    ] {
        let r = color::contrast(solid(fg), solid(bg));
        if r < 4.5 {
            warnings.push(format!("{what} has a contrast of {r:.1}:1, under the 4.5:1 that reads comfortably"));
        }
    }

    Ok(Theme {
        id: merged.id.clone(),
        name: merged.name.clone(),
        author: merged.author.clone(),
        description: merged.description.clone(),
        dark: merged.dark.unwrap_or(true),
        tokens,
        code,
        fonts,
        shape,
        animation_ms: merged.motion.animation_ms.unwrap_or(120.0).clamp(0.0, 1000.0),
        surfaces,
        preview: merged.preview.clone(),
        assets,
        origin,
        warnings,
        file: merged.clone(),
    })
}

/// Every path a merged file names, for the loader to read.
pub fn referenced_paths(f: &ThemeFile) -> Vec<String> {
    let mut out = Vec::new();
    for face in [&f.fonts.ui, &f.fonts.mono, &f.fonts.display].into_iter().flatten() {
        if !crate::fonts::is_builtin(face) {
            out.push(face.clone());
        }
    }
    if let Some(p) = &f.preview {
        out.push(p.clone());
    }
    for s in f.surfaces.values() {
        for l in s.layers.iter().flatten() {
            match l {
                LayerFile::Shader { shader, image, .. } => {
                    if !shader.starts_with("builtin:") {
                        out.push(shader.clone());
                    }
                    if let Some(i) = image {
                        out.push(i.clone());
                    }
                }
                LayerFile::Image { path, .. } => out.push(path.clone()),
                _ => {}
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
