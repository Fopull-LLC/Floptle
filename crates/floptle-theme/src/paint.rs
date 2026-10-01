//! Painting a region: what a host calls to fill a panel with its theme.
//!
//! A host (the editor, the Hub) names a region and a rectangle, and this draws
//! the region's layers and then its fill. It is the only place a theme's
//! imagery is drawn, so the Effects setting is honoured in one place.
//!
//! Drawing order, bottom to top:
//!
//! 1. If the region has layers: the theme's opaque `backdrop` colour, so a
//!    translucent layer never shows whatever was under the window (in the
//!    editor, that is the 3D view).
//! 2. The layers: shader pictures, images, gradients, solids.
//! 3. The region's fill. Over layers it is a veil, which is why a theme with
//!    a backdrop gives its ground some transparency. With no layers it is
//!    painted solid.
//!
//! The default theme has no layers anywhere, so for it this is one rectangle
//! per region, which is what the panel was drawing already.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use egui::{Color32, Mesh, Pos2, Rect, Shape, TextureId, Vec2, pos2};

use crate::color::Rgba;
use crate::model::{Blend, Fit, Layer, Space, Theme};
use crate::source::{Effects, Prefs};

/// One shader layer some panel showed this frame, and the area it covered.
#[derive(Clone, Debug)]
pub(crate) struct ShaderUse {
    pub(crate) layer: Layer,
    pub(crate) rect: Rect,
}

/// The theme a context is drawing with, and what drawing it needs: decoded
/// images, the backdrop textures the renderer has published, and what the
/// panels asked for this frame.
pub struct Runtime {
    pub theme: Arc<Theme>,
    pub effects: Effects,
    pub(crate) generation: u64,
    images: HashMap<(String, bool, bool), Option<(egui::TextureHandle, Vec2)>>,
    pub(crate) shader_tex: HashMap<u64, TextureId>,
    pub(crate) uses: HashMap<u64, ShaderUse>,
    /// A shader that would not compile, and naga's reason, by the path the
    /// theme named it with.
    pub shader_errors: BTreeMap<String, String>,
    /// Backdrop draws in the last second: 0 when nothing moves.
    pub draws_per_second: u32,
    /// Is a backdrop animating right now? (The host keeps repainting if so.)
    pub moving: bool,
    /// Light added over every region: the editor's play-mode tint.
    pub tint: Color32,
}

/// A context's [`Runtime`], shared between the UI pass and the renderer.
#[derive(Clone)]
pub struct Shared(Arc<Mutex<Runtime>>);

impl Shared {
    pub fn lock(&self) -> MutexGuard<'_, Runtime> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn key() -> egui::Id {
    egui::Id::new("floptle-theme-runtime")
}

/// The runtime installed in `ctx`, if [`apply`] has run.
pub fn shared(ctx: &egui::Context) -> Option<Shared> {
    ctx.data(|d| d.get_temp::<Shared>(key()))
}

/// The theme `ctx` is drawing with: the applied one, or Floptle Dark.
pub fn theme(ctx: &egui::Context) -> Arc<Theme> {
    shared(ctx).map(|s| s.lock().theme.clone()).unwrap_or_else(crate::default_theme)
}

/// Install `theme` in `ctx`: its style on every style egui keeps, and its
/// imagery for [`paint_region`]. Fonts are **not** set here, because
/// `set_fonts` rebuilds the glyph atlas and the editor adds its packages'
/// faces to the stack: see [`crate::fonts::definitions`] and
/// [`crate::fonts::fingerprint`].
///
/// Cheap to call with the theme already applied (only the style is
/// re-written), so a host can call it whenever its prefs change.
pub fn apply(ctx: &egui::Context, theme: Arc<Theme>, prefs: &Prefs) {
    let display = crate::fonts::display_family(ctx);
    ctx.all_styles_mut(|s| crate::style::apply(s, &theme, display.clone()));
    match shared(ctx) {
        Some(sh) => {
            let mut rt = sh.lock();
            if !Arc::ptr_eq(&rt.theme, &theme) {
                rt.theme = theme;
                rt.generation = rt.generation.wrapping_add(1);
                rt.images.clear();
                rt.shader_errors.clear();
                rt.uses.clear();
            }
            rt.effects = prefs.effects;
        }
        None => {
            let rt = Runtime {
                theme,
                effects: prefs.effects,
                generation: 0,
                images: HashMap::new(),
                shader_tex: HashMap::new(),
                uses: HashMap::new(),
                shader_errors: BTreeMap::new(),
                draws_per_second: 0,
                moving: false,
                tint: Color32::TRANSPARENT,
            };
            ctx.data_mut(|d| d.insert_temp(key(), Shared(Arc::new(Mutex::new(rt)))));
        }
    }
}

/// Add `rgb` over every region, or stop (`None`). The editor's play-mode
/// tint: additive, so it brightens a backdrop the way it brightens a fill.
pub fn set_tint(ctx: &egui::Context, rgb: Option<[u8; 3]>) {
    if let Some(sh) = shared(ctx) {
        sh.lock().tint = rgb.map_or(Color32::TRANSPARENT, |[r, g, b]| Color32::from_rgba_premultiplied(r, g, b, 0));
    }
}

/// How soon the host should repaint for a moving backdrop, if one is moving.
/// The editor redraws every frame anyway; the Hub, which otherwise sleeps
/// until something happens, asks this.
pub fn repaint_after(ctx: &egui::Context, prefs: &Prefs) -> Option<std::time::Duration> {
    let sh = shared(ctx)?;
    let rt = sh.lock();
    (rt.effects == Effects::Full && rt.theme.is_animated())
        .then(|| std::time::Duration::from_secs_f32(1.0 / prefs.backdrop_fps.clamp(5.0, 60.0)))
}

/// Paint `region` over `rect` with `ui`'s painter, clipped to `rect`.
pub fn paint_region(ui: &egui::Ui, region: &str, rect: Rect) {
    let shapes = region_shapes(ui.ctx(), region, rect);
    let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    p.extend(shapes);
}

/// Does `region` have anything to draw beyond a solid fill under the current
/// effects setting? A host can skip its own fill when not.
pub fn region_has_layers(ctx: &egui::Context, region: &str) -> bool {
    let Some(sh) = shared(ctx) else { return false };
    let rt = sh.lock();
    let (_, layers) = rt.theme.surface(region);
    layers.iter().any(|l| rt.effects != Effects::Off || !l.is_effect())
}

/// The fill `region` ends up as where nothing else is drawn under it.
pub fn region_fill(ctx: &egui::Context, region: &str) -> Color32 {
    let t = theme(ctx);
    let (fill, _) = t.surface(region);
    t.tokens.solid(fill).to_egui()
}

/// The shapes for `region` over `rect`, for a host that has to place them
/// itself (the editor's dock reserves a slot before the dock draws, and fills
/// it after, once it knows where the tab bars landed). Unclipped: wrap them
/// in a clip of their own if they must not spill.
pub fn region_shapes(ctx: &egui::Context, region: &str, rect: Rect) -> Vec<Shape> {
    if !rect.is_positive() {
        return Vec::new();
    }
    let Some(sh) = shared(ctx) else {
        return vec![Shape::rect_filled(rect, 0.0, crate::default_theme().tokens.ground.to_egui())];
    };
    let window = ctx.content_rect();
    let mut rt = sh.lock();
    let theme = rt.theme.clone();
    let (fill, layers) = theme.surface(region);
    let effects = rt.effects;
    let layers: Vec<&Layer> = layers.iter().filter(|l| effects != Effects::Off || !l.is_effect()).collect();
    let mut out = Vec::new();
    let tint = rt.tint;
    if layers.is_empty() {
        out.push(Shape::rect_filled(rect, 0.0, theme.tokens.solid(fill).to_egui()));
        if tint != Color32::TRANSPARENT {
            out.push(Shape::rect_filled(rect, 0.0, tint));
        }
        return out;
    }
    out.push(Shape::rect_filled(rect, 0.0, theme.tokens.backdrop.over(Rgba::rgb(0, 0, 0)).to_egui()));
    for layer in layers {
        match layer {
            Layer::Solid(c) => out.push(Shape::rect_filled(rect, 0.0, c.to_egui())),
            Layer::Gradient { stops, angle, radial, opacity, blend, space } => {
                let area = if *space == Space::Window { window } else { rect };
                out.push(Shape::mesh(gradient_mesh(rect, area, stops, *angle, *radial, *opacity, *blend)));
            }
            Layer::Image { path, fit, align, scale, offset, opacity, tint, blend, space, pixelated } => {
                let tile = *fit == Fit::Tile;
                let Some((tex, size)) = rt.image(ctx, &theme, path, *pixelated, tile) else { continue };
                let area = if *space == Space::Window { window } else { rect };
                let tint = layer_tint(*tint, *opacity, *blend);
                let ppp = ctx.pixels_per_point();
                let natural = size / ppp * *scale;
                let off = Vec2::new(offset.0, offset.1);
                if tile {
                    let tile_size = natural.max(Vec2::splat(1.0));
                    let uv_min = (rect.min - area.min - off) / tile_size;
                    let uv_max = uv_min + rect.size() / tile_size;
                    out.push(image_shape(tex, rect, Rect::from_min_max(uv_min.to_pos2(), uv_max.to_pos2()), tint));
                    continue;
                }
                let placed_size = match fit {
                    Fit::Cover => size * (area.width() / size.x).max(area.height() / size.y) * *scale,
                    Fit::Contain => size * (area.width() / size.x).min(area.height() / size.y) * *scale,
                    Fit::Stretch => area.size(),
                    Fit::Natural | Fit::Tile => natural,
                };
                let (fx, fy) = align.factors();
                let min = area.min + Vec2::new((area.width() - placed_size.x) * fx, (area.height() - placed_size.y) * fy) + off;
                let placed = Rect::from_min_size(min, placed_size);
                // Only the part inside this region: the rest belongs to the
                // neighbours that share a window-space picture.
                let shown = placed.intersect(rect);
                if !shown.is_positive() {
                    continue;
                }
                let uv = Rect::from_min_max(
                    ((shown.min - placed.min) / placed.size()).to_pos2(),
                    ((shown.max - placed.min) / placed.size()).to_pos2(),
                );
                out.push(image_shape(tex, shown, uv, tint));
            }
            Layer::Shader { opacity, blend, .. } => {
                if effects == Effects::Off {
                    continue;
                }
                let Some(k) = crate::backdrop::layer_key(layer) else { continue };
                let shown = rect.intersect(window);
                rt.uses
                    .entry(k)
                    .and_modify(|u| u.rect = u.rect.union(shown))
                    .or_insert(ShaderUse { layer: layer.clone(), rect: shown });
                if let Some(&tex) = rt.shader_tex.get(&k) {
                    let uv = Rect::from_min_max(
                        ((shown.min - window.min) / window.size()).to_pos2(),
                        ((shown.max - window.min) / window.size()).to_pos2(),
                    );
                    out.push(image_shape(tex, shown, uv, layer_tint(Rgba::rgb(255, 255, 255), *opacity, *blend)));
                }
            }
        }
    }
    out.push(Shape::rect_filled(rect, 0.0, fill.to_egui()));
    if tint != Color32::TRANSPARENT {
        out.push(Shape::rect_filled(rect, 0.0, tint));
    }
    out
}

fn image_shape(tex: TextureId, rect: Rect, uv: Rect, tint: Color32) -> Shape {
    let mut mesh = Mesh::with_texture(tex);
    mesh.add_rect_with_uv(rect, uv, tint);
    Shape::mesh(mesh)
}

/// The tint a layer is drawn with. An additive layer is premultiplied colour
/// with zero alpha: egui blends premultiplied, so that adds light without
/// covering anything.
fn layer_tint(tint: Rgba, opacity: f32, blend: Blend) -> Color32 {
    let c = tint.with_alpha_mul(opacity).to_egui();
    match blend {
        Blend::Normal => c,
        Blend::Add => Color32::from_rgba_premultiplied(c.r(), c.g(), c.b(), 0),
    }
}

/// A gradient over `rect`, laid out against `area`, as a grid of vertices
/// coloured by evaluating it, so any number of stops comes out right.
fn gradient_mesh(rect: Rect, area: Rect, stops: &[(f32, Rgba)], angle: f32, radial: bool, opacity: f32, blend: Blend) -> Mesh {
    const N: usize = 12;
    let dir = Vec2::angled(angle.to_radians());
    let corners = [area.left_top(), area.right_top(), area.left_bottom(), area.right_bottom()];
    let proj = |p: Pos2| (p - area.center()).dot(dir);
    let (lo, hi) = corners.iter().fold((f32::MAX, f32::MIN), |(lo, hi), c| (lo.min(proj(*c)), hi.max(proj(*c))));
    let reach = (area.size() * 0.5).length().max(1.0);
    let at = |p: Pos2| -> f32 {
        if radial {
            ((p - area.center()).length() / reach).clamp(0.0, 1.0)
        } else {
            ((proj(p) - lo) / (hi - lo).max(1e-3)).clamp(0.0, 1.0)
        }
    };
    let eval = |t: f32| -> Rgba {
        let i = stops.iter().position(|s| s.0 >= t).unwrap_or(stops.len() - 1);
        if i == 0 {
            return stops[0].1;
        }
        let (a, b) = (stops[i - 1], stops[i]);
        let f = ((t - a.0) / (b.0 - a.0).max(1e-5)).clamp(0.0, 1.0);
        let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
        Rgba([l(a.1.0[0], b.1.0[0]), l(a.1.0[1], b.1.0[1]), l(a.1.0[2], b.1.0[2]), l(a.1.0[3], b.1.0[3])])
    };
    let mut mesh = Mesh::default();
    for j in 0..=N {
        for i in 0..=N {
            let p = pos2(
                rect.min.x + rect.width() * i as f32 / N as f32,
                rect.min.y + rect.height() * j as f32 / N as f32,
            );
            mesh.colored_vertex(p, layer_tint(eval(at(p)), opacity, blend));
        }
    }
    let w = (N + 1) as u32;
    for j in 0..N as u32 {
        for i in 0..N as u32 {
            let a = j * w + i;
            mesh.add_triangle(a, a + 1, a + w);
            mesh.add_triangle(a + 1, a + w + 1, a + w);
        }
    }
    mesh
}

impl Runtime {
    fn image(
        &mut self,
        ctx: &egui::Context,
        theme: &Theme,
        path: &str,
        pixelated: bool,
        tile: bool,
    ) -> Option<(TextureId, Vec2)> {
        let k = (path.to_string(), pixelated, tile);
        if !self.images.contains_key(&k) {
            let loaded = theme.assets.files.get(path).and_then(|b| decode(b)).map(|img| {
                let size = Vec2::new(img.size[0] as f32, img.size[1] as f32);
                let filter = if pixelated { egui::TextureFilter::Nearest } else { egui::TextureFilter::Linear };
                let opts = egui::TextureOptions {
                    magnification: filter,
                    minification: egui::TextureFilter::Linear,
                    wrap_mode: if tile { egui::TextureWrapMode::Repeat } else { egui::TextureWrapMode::ClampToEdge },
                    mipmap_mode: (!pixelated).then_some(egui::TextureFilter::Linear),
                };
                (ctx.load_texture(format!("theme:{}:{path}", theme.id), img, opts), size)
            });
            self.images.insert(k.clone(), loaded);
        }
        self.images[&k].as_ref().map(|(h, s)| (h.id(), *s))
    }
}

/// Decode an image, shrinking anything past 4096 pixels on a side: no panel
/// shows more, and an 8K wallpaper is 128 MB of video memory otherwise.
fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width().max(img.height()) > 4096 {
        img.resize(4096, 4096, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}
