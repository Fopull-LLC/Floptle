//! The document model: an [`Image`] is a stack of [`Layer`]s over a canvas size,
//! plus a mode, a frame count and an optional palette.
//!
//! **Mode is a preference, not a fork.** Pixel / Painterly / Vector set tool
//! defaults — anti-aliasing, snapping, the export sampler — and nothing
//! structural. Any document can hold any mix of layer kinds (proposal §7), which
//! is the whole reason this is one editor instead of three.
//!
//! **Layers are canvas-aligned, with an offset.** Moving a layer changes
//! `offset` rather than rewriting pixels, so dragging a sprite half off the
//! canvas and back loses nothing.

use serde::{Deserialize, Serialize};

use crate::adjust::Adjustment;
use crate::effect::Effect;
use crate::palette::Palette;
use crate::select::Mask;
use crate::tiles::TileGrid;
use crate::vector::VPath;
use crate::{Blend, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Mode {
    /// Crisp: no anti-aliasing, integer zoom, nearest export sampling.
    #[default]
    Pixel,
    /// Soft: anti-aliased brushes, continuous zoom, mipmapped export.
    Painterly,
    /// Shapes first: anti-aliased, continuous zoom, vector tools to hand.
    Vector,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Pixel, Mode::Painterly, Mode::Vector];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Pixel => "Pixel",
            Mode::Painterly => "Painterly",
            Mode::Vector => "Vector",
        }
    }

    /// Whether drawing anti-aliases by default in this mode.
    pub fn antialias(self) -> bool {
        !matches!(self, Mode::Pixel)
    }
}

/// What a layer actually holds.
#[derive(Clone, Debug)]
pub enum LayerKind {
    /// Pixels — one [`TileGrid`] per animation frame (a single grid = a static
    /// layer shown on every frame).
    Raster { frames: Vec<TileGrid> },
    /// Resolution-independent shapes, rasterized at composite and at export.
    Vector { paths: Vec<VPath> },
    /// A colour operation over everything beneath it in the stack.
    Adjust(Adjustment),
}

impl LayerKind {
    pub fn is_raster(&self) -> bool {
        matches!(self, LayerKind::Raster { .. })
    }
    pub fn is_vector(&self) -> bool {
        matches!(self, LayerKind::Vector { .. })
    }
    pub fn is_adjust(&self) -> bool {
        matches!(self, LayerKind::Adjust(_))
    }
    pub fn glyph(&self) -> &'static str {
        match self {
            LayerKind::Raster { .. } => "▣",
            LayerKind::Vector { .. } => "◆",
            LayerKind::Adjust(_) => "◑",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub name: String,
    pub kind: LayerKind,
    pub blend: Blend,
    /// 0..1.
    pub opacity: f32,
    pub visible: bool,
    /// Refuses edits (the "don't paint on the line art" switch).
    pub locked: bool,
    /// Clip to the alpha of the first non-clipping layer below.
    pub clip_below: bool,
    /// Per-layer 8-bit mask, painted like any other surface.
    pub mask: Option<Mask>,
    pub mask_enabled: bool,
    pub effects: Vec<Effect>,
    /// Canvas-space position of the layer's own (0, 0).
    pub offset: (i32, i32),
}

impl Layer {
    pub fn raster(name: impl Into<String>, w: u32, h: u32) -> Self {
        Layer {
            name: name.into(),
            kind: LayerKind::Raster { frames: vec![TileGrid::new(w, h)] },
            blend: Blend::Mix,
            opacity: 1.0,
            visible: true,
            locked: false,
            clip_below: false,
            mask: None,
            mask_enabled: true,
            effects: Vec::new(),
            offset: (0, 0),
        }
    }

    pub fn vector(name: impl Into<String>) -> Self {
        Layer { kind: LayerKind::Vector { paths: Vec::new() }, ..Layer::raster(name, 1, 1) }
    }

    pub fn adjust(a: Adjustment) -> Self {
        Layer { name: a.label().to_string(), kind: LayerKind::Adjust(a), ..Layer::raster("", 1, 1) }
    }

    /// The pixel grid for `frame` (static layers ignore the index).
    pub fn grid(&self, frame: usize) -> Option<&TileGrid> {
        match &self.kind {
            LayerKind::Raster { frames } => frames.get(frame.min(frames.len().saturating_sub(1))),
            _ => None,
        }
    }

    pub fn grid_mut(&mut self, frame: usize) -> Option<&mut TileGrid> {
        match &mut self.kind {
            LayerKind::Raster { frames } => {
                let i = frame.min(frames.len().saturating_sub(1));
                frames.get_mut(i)
            }
            _ => None,
        }
    }

    /// True when this layer draws its own pixels on `frame` rather than sharing
    /// one grid across all of them.
    pub fn is_animated(&self) -> bool {
        matches!(&self.kind, LayerKind::Raster { frames } if frames.len() > 1)
    }

    /// Effects can reach this far outside the layer's own pixels.
    pub fn effect_margin(&self) -> u32 {
        self.effects.iter().map(|e| e.margin()).max().unwrap_or(0)
    }

    /// Render this layer's own pixels for `rect` (canvas space) into a fresh
    /// buffer. Adjustment layers render nothing.
    pub fn render_rect(
        &self,
        rect: Rect,
        frame: usize,
        canvas: (u32, u32),
        aa: bool,
        vcache: &mut VectorCache,
    ) -> Vec<u8> {
        let mut out = vec![0u8; rect.w as usize * rect.h as usize * 4];
        match &self.kind {
            LayerKind::Raster { .. } => {
                if let Some(g) = self.grid(frame) {
                    let local = Rect::new(rect.x - self.offset.0, rect.y - self.offset.1, rect.w, rect.h);
                    out = g.read_rect(local);
                }
            }
            LayerKind::Vector { paths } => {
                let full = vcache.get(paths, canvas.0, canvas.1, aa);
                // Crop the cached full-canvas render (plus the layer offset).
                for y in 0..rect.h as i32 {
                    for x in 0..rect.w as i32 {
                        let sx = rect.x + x - self.offset.0;
                        let sy = rect.y + y - self.offset.1;
                        if sx < 0 || sy < 0 || sx >= canvas.0 as i32 || sy >= canvas.1 as i32 {
                            continue;
                        }
                        let so = (sy as usize * canvas.0 as usize + sx as usize) * 4;
                        let o = (y as usize * rect.w as usize + x as usize) * 4;
                        out[o..o + 4].copy_from_slice(&full[so..so + 4]);
                    }
                }
            }
            LayerKind::Adjust(_) => {}
        }
        out
    }
}

/// One cached render: the paths it came from, the anti-alias flag and canvas
/// size it was rendered for, and the pixels.
type VectorEntry = (Vec<VPath>, bool, u32, u32, std::sync::Arc<Vec<u8>>);

/// Full-canvas renders of vector layers, keyed by the paths themselves.
///
/// Comparing the path list (a handful of nodes) is cheaper than re-rasterizing
/// and — unlike a revision counter — cannot go stale when some new edit path
/// forgets to bump it.
#[derive(Default)]
pub struct VectorCache {
    entries: Vec<VectorEntry>,
}

impl VectorCache {
    pub fn get(&mut self, paths: &[VPath], w: u32, h: u32, aa: bool) -> std::sync::Arc<Vec<u8>> {
        if let Some((_, _, _, _, buf)) = self
            .entries
            .iter()
            .find(|(p, a, cw, ch, _)| p == paths && *a == aa && *cw == w && *ch == h)
        {
            return buf.clone();
        }
        let buf = std::sync::Arc::new(crate::vector::render(paths, w, h, aa));
        self.entries.push((paths.to_vec(), aa, w, h, buf.clone()));
        // A handful of vector layers is the realistic case; keep the newest few.
        if self.entries.len() > 8 {
            self.entries.remove(0);
        }
        buf
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Clone, Debug)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub mode: Mode,
    /// Bottom-first.
    pub layers: Vec<Layer>,
    pub active: usize,
    /// Animation frame count (1 = a still image).
    pub frames: usize,
    pub fps: f32,
    /// The document's palette, embedded so a `.flimg` is self-contained.
    pub palette: Option<Palette>,
    /// Every colour placed snaps to the palette.
    pub palette_lock: bool,
    /// The live selection — clips every operation while set.
    pub selection: Option<Mask>,
    /// Editing wraps at the canvas edges (tiling mode).
    pub tiling: bool,
    /// The uniform cell grid this image is cut into, if it is a sheet —
    /// `(cols, rows)`, the one layout the engine can address.
    ///
    /// A property of the IMAGE and not of the view, because it is a fact about
    /// the art: a 16x16 tileset is 16x16 whoever opens it. Kept here so the
    /// grid you drew against is the grid you get back, and so a material and a
    /// tileset can be told the numbers rather than have them re-typed (and
    /// mistyped) in three places. `None` = an ordinary image, no cell grid.
    pub sheet: Option<(u32, u32)>,
}

impl Image {
    /// A new document with one empty raster layer.
    pub fn new(w: u32, h: u32, mode: Mode) -> Self {
        let (w, h) = (w.clamp(1, 16384), h.clamp(1, 16384));
        Image {
            w,
            h,
            mode,
            layers: vec![Layer::raster("Layer 1", w, h)],
            active: 0,
            frames: 1,
            fps: 12.0,
            palette: None,
            palette_lock: false,
            selection: None,
            tiling: false,
            sheet: None,
        }
    }

    /// The pixel size of one cell, when this image is a sheet. `None` when it is
    /// not one, or when the grid does not divide the canvas evenly — a cell that
    /// is 10.6 px wide is a mistake to draw against, not a number to round.
    pub fn cell_size(&self) -> Option<(u32, u32)> {
        let (c, r) = self.sheet?;
        (c > 0 && r > 0 && self.w.is_multiple_of(c) && self.h.is_multiple_of(r))
            .then(|| (self.w / c, self.h / r))
    }

    /// Wrap a flat RGBA8 image (an opened PNG) as a one-layer document.
    pub fn from_rgba(w: u32, h: u32, px: &[u8], mode: Mode) -> Self {
        let mut img = Image::new(w, h, mode);
        if let Some(g) = img.layers[0].grid_mut(0) {
            *g = TileGrid::from_rgba(w, h, px);
        }
        img.layers[0].name = "Background".into();
        img
    }

    pub fn bounds(&self) -> Rect {
        Rect::size(self.w, self.h)
    }

    pub fn active_layer(&self) -> Option<&Layer> {
        self.layers.get(self.active)
    }

    pub fn active_layer_mut(&mut self) -> Option<&mut Layer> {
        let i = self.active;
        self.layers.get_mut(i)
    }

    /// The active layer's pixel grid for `frame`, if it's a raster layer and not
    /// locked. This is the one door every pixel edit goes through.
    pub fn paint_target(&mut self, frame: usize) -> Option<(&mut TileGrid, (i32, i32))> {
        let i = self.active;
        let layer = self.layers.get_mut(i)?;
        if layer.locked || !layer.visible {
            return None;
        }
        let off = layer.offset;
        layer.grid_mut(frame).map(|g| (g, off))
    }

    // --- layer stack ops --------------------------------------------------

    /// Insert `layer` above the active one and select it. Returns its index.
    pub fn add_layer(&mut self, layer: Layer) -> usize {
        let at = (self.active + 1).min(self.layers.len());
        self.layers.insert(at, layer);
        self.active = at;
        at
    }

    pub fn add_raster_layer(&mut self) -> usize {
        let n = self.next_layer_name();
        let (w, h, frames) = (self.w, self.h, self.frames);
        let mut l = Layer::raster(n, w, h);
        // Match the document's frame count so painting on frame 3 doesn't
        // silently land on frame 0.
        if frames > 1
            && let LayerKind::Raster { frames: f } = &mut l.kind
        {
            *f = (0..frames).map(|_| TileGrid::new(w, h)).collect();
        }
        self.add_layer(l)
    }

    fn next_layer_name(&self) -> String {
        let mut n = self.layers.len() + 1;
        loop {
            let name = format!("Layer {n}");
            if !self.layers.iter().any(|l| l.name == name) {
                return name;
            }
            n += 1;
        }
    }

    pub fn delete_layer(&mut self, i: usize) {
        if self.layers.len() <= 1 || i >= self.layers.len() {
            return;
        }
        self.layers.remove(i);
        self.active = self.active.min(self.layers.len() - 1);
    }

    pub fn duplicate_layer(&mut self, i: usize) {
        let Some(l) = self.layers.get(i).cloned() else { return };
        let mut copy = l;
        copy.name = format!("{} copy", copy.name);
        self.layers.insert(i + 1, copy);
        self.active = i + 1;
    }

    /// Move a layer one slot up (`+1`) or down (`-1`) the stack.
    pub fn move_layer(&mut self, i: usize, delta: i32) {
        let j = i as i32 + delta;
        if i >= self.layers.len() || j < 0 || j as usize >= self.layers.len() {
            return;
        }
        self.layers.swap(i, j as usize);
        if self.active == i {
            self.active = j as usize;
        } else if self.active == j as usize {
            self.active = i;
        }
    }

    /// Merge layer `i` down into `i-1`, rasterizing whatever it was.
    pub fn merge_down(&mut self, i: usize, frame: usize) {
        if i == 0 || i >= self.layers.len() {
            return;
        }
        // Composite exactly these two layers, then replace the lower one with the
        // result — so blend modes, opacity, masks and effects all bake correctly.
        let mut pair = self.clone();
        pair.layers = vec![self.layers[i - 1].clone(), self.layers[i].clone()];
        pair.active = 0;
        pair.selection = None;
        let flat = crate::composite::flatten(&pair, frame);
        let mut merged = Layer::raster(self.layers[i - 1].name.clone(), self.w, self.h);
        if let Some(g) = merged.grid_mut(0) {
            *g = TileGrid::from_rgba(self.w, self.h, &flat);
        }
        self.layers[i - 1] = merged;
        self.layers.remove(i);
        self.active = (i - 1).min(self.layers.len() - 1);
    }

    /// Collapse the whole stack to one raster layer — **per frame**, so
    /// flattening an animation doesn't quietly throw away every frame but the
    /// one you happened to be looking at.
    pub fn flatten_all(&mut self, _frame: usize) {
        let grids: Vec<TileGrid> = (0..self.frames.max(1))
            .map(|f| {
                let flat = crate::composite::flatten(self, f);
                TileGrid::from_rgba(self.w, self.h, &flat)
            })
            .collect();
        let mut l = Layer::raster("Flattened", self.w, self.h);
        l.kind = LayerKind::Raster { frames: grids };
        self.layers = vec![l];
        self.active = 0;
    }

    // --- canvas ops -------------------------------------------------------

    /// Resize the canvas without resampling (crop / extend), anchoring existing
    /// content at (`dx`, `dy`).
    pub fn resize_canvas(&mut self, w: u32, h: u32, dx: i32, dy: i32) {
        let (w, h) = (w.clamp(1, 16384), h.clamp(1, 16384));
        for l in &mut self.layers {
            match &mut l.kind {
                LayerKind::Raster { frames } => {
                    for g in frames.iter_mut() {
                        *g = g.recanvased(w, h, dx + l.offset.0, dy + l.offset.1);
                    }
                    l.offset = (0, 0);
                }
                LayerKind::Vector { paths } => {
                    for p in paths.iter_mut() {
                        p.translate(dx as f32, dy as f32);
                    }
                }
                LayerKind::Adjust(_) => {}
            }
            if let Some(m) = &l.mask {
                l.mask = Some(m.recanvased(w, h, dx, dy));
            }
        }
        if let Some(s) = &self.selection {
            self.selection = Some(s.recanvased(w, h, dx, dy));
        }
        self.w = w;
        self.h = h;
    }

    /// Resample every layer to a new pixel size. `nearest` keeps pixel art crisp.
    pub fn scale_to(&mut self, w: u32, h: u32, nearest: bool) {
        let (w, h) = (w.clamp(1, 16384), h.clamp(1, 16384));
        let (sx, sy) = (w as f32 / self.w as f32, h as f32 / self.h as f32);
        for l in &mut self.layers {
            match &mut l.kind {
                LayerKind::Raster { frames } => {
                    for g in frames.iter_mut() {
                        let src = g.to_rgba();
                        let out = crate::transform::resample(
                            &src,
                            g.width(),
                            g.height(),
                            w,
                            h,
                            nearest,
                        );
                        *g = TileGrid::from_rgba(w, h, &out);
                    }
                    l.offset = ((l.offset.0 as f32 * sx) as i32, (l.offset.1 as f32 * sy) as i32);
                }
                LayerKind::Vector { paths } => {
                    for p in paths.iter_mut() {
                        for n in &mut p.nodes {
                            n.p[0] *= sx;
                            n.p[1] *= sy;
                            n.h_in[0] *= sx;
                            n.h_in[1] *= sy;
                            n.h_out[0] *= sx;
                            n.h_out[1] *= sy;
                        }
                        if let Some(s) = &mut p.stroke {
                            s.width *= (sx + sy) * 0.5;
                        }
                    }
                }
                LayerKind::Adjust(_) => {}
            }
            l.mask = None; // a resampled mask is rarely what anyone wanted
        }
        self.selection = None;
        self.w = w;
        self.h = h;
    }

    /// Mirror the whole document — every layer, every frame, every mask, the
    /// selection and any vector paths.
    ///
    /// This lives in the kernel rather than in the tab because the tab's version
    /// only knew about raster pixels: flipping an image silently threw away its
    /// layer masks, and a mask you didn't know you'd lost is the worst kind.
    pub fn flip(&mut self, horizontal: bool) {
        let (cw, ch) = (self.w, self.h);
        for l in &mut self.layers {
            let off = l.offset;
            match &mut l.kind {
                LayerKind::Raster { frames } => {
                    for g in frames.iter_mut() {
                        let (gw, gh) = (g.width(), g.height());
                        let mut buf = g.to_rgba();
                        if horizontal {
                            crate::transform::flip_h(&mut buf, gw, gh);
                        } else {
                            crate::transform::flip_v(&mut buf, gw, gh);
                        }
                        *g = TileGrid::from_rgba(gw, gh, &buf);
                    }
                    // An offset layer has to move to the other side too, or its
                    // content flips inside a box that didn't.
                    let (gw, gh) = frames
                        .first()
                        .map(|g| (g.width() as i32, g.height() as i32))
                        .unwrap_or((cw as i32, ch as i32));
                    l.offset = if horizontal {
                        (cw as i32 - (off.0 + gw), off.1)
                    } else {
                        (off.0, ch as i32 - (off.1 + gh))
                    };
                }
                LayerKind::Vector { paths } => {
                    for p in paths.iter_mut() {
                        for n in &mut p.nodes {
                            if horizontal {
                                n.p[0] = cw as f32 - n.p[0];
                                n.h_in[0] = -n.h_in[0];
                                n.h_out[0] = -n.h_out[0];
                            } else {
                                n.p[1] = ch as f32 - n.p[1];
                                n.h_in[1] = -n.h_in[1];
                                n.h_out[1] = -n.h_out[1];
                            }
                        }
                    }
                }
                LayerKind::Adjust(_) => {}
            }
            if let Some(m) = &mut l.mask {
                m.flip(horizontal);
            }
        }
        if let Some(s) = &mut self.selection {
            s.flip(horizontal);
        }
    }

    /// Rotate the whole document by `turns` quarter-turns clockwise — pixels,
    /// masks, the selection and vector paths together.
    pub fn rotate(&mut self, turns: i32) {
        let t = turns.rem_euclid(4);
        if t == 0 {
            return;
        }
        let (cw, ch) = (self.w as f32, self.h as f32);
        for l in &mut self.layers {
            match &mut l.kind {
                LayerKind::Raster { frames } => {
                    for g in frames.iter_mut() {
                        let (buf, nw, nh) =
                            crate::transform::rotate_quarter(&g.to_rgba(), g.width(), g.height(), t);
                        *g = TileGrid::from_rgba(nw, nh, &buf);
                    }
                    // Rotating a partially-offset layer is not expressible as an
                    // offset in general; bake it flat first would cost the whole
                    // canvas, so keep the common case exact: an unoffset layer.
                    l.offset = (0, 0);
                }
                LayerKind::Vector { paths } => {
                    for p in paths.iter_mut() {
                        for n in &mut p.nodes {
                            let (x, y) = (n.p[0], n.p[1]);
                            n.p = rot_point(x, y, cw, ch, t);
                            n.h_in = rot_vec(n.h_in, t);
                            n.h_out = rot_vec(n.h_out, t);
                        }
                    }
                }
                LayerKind::Adjust(_) => {}
            }
            if let Some(m) = &l.mask {
                l.mask = Some(m.rotated(t));
            }
        }
        if let Some(s) = &self.selection {
            self.selection = Some(s.rotated(t));
        }
        if t % 2 == 1 {
            std::mem::swap(&mut self.w, &mut self.h);
        }
    }

    /// Crop the canvas to the selection's bounding box. Returns false when
    /// there's nothing selected to crop to.
    pub fn crop_to_selection(&mut self) -> bool {
        let Some(b) = self.selection.as_ref().map(|s| s.selected_bounds()) else { return false };
        if b.is_empty() || (b.w == self.w && b.h == self.h) {
            return false;
        }
        self.resize_canvas(b.w, b.h, -b.x, -b.y);
        true
    }

    /// Trim the canvas to the union of every layer's opaque pixels.
    pub fn trim(&mut self, frame: usize) {
        let mut b = Rect::EMPTY;
        for l in &self.layers {
            if let Some(g) = l.grid(frame) {
                let lb = g.opaque_bounds();
                if !lb.is_empty() {
                    b = b.union(Rect::new(lb.x + l.offset.0, lb.y + l.offset.1, lb.w, lb.h));
                }
            }
        }
        if b.is_empty() {
            return;
        }
        self.resize_canvas(b.w, b.h, -b.x, -b.y);
    }

    // --- frames -----------------------------------------------------------

    /// Set the frame count, extending animated layers with copies of their last
    /// frame (the onion-skin-friendly default) and truncating when shrinking.
    pub fn set_frames(&mut self, n: usize) {
        let n = n.clamp(1, 512);
        for l in &mut self.layers {
            if let LayerKind::Raster { frames } = &mut l.kind
                && frames.len() > 1
            {
                while frames.len() < n {
                    let last = frames.last().cloned().unwrap_or_else(|| TileGrid::new(self.w, self.h));
                    frames.push(last);
                }
                frames.truncate(n.max(1));
            }
        }
        self.frames = n;
    }

    /// Give a layer its own pixels per frame (or collapse back to one).
    pub fn set_layer_animated(&mut self, i: usize, animated: bool) {
        let frames_n = self.frames;
        let (w, h) = (self.w, self.h);
        let Some(l) = self.layers.get_mut(i) else { return };
        if let LayerKind::Raster { frames } = &mut l.kind {
            if animated && frames.len() == 1 {
                let base = frames[0].clone();
                *frames = (0..frames_n.max(1)).map(|_| base.clone()).collect();
            } else if !animated && frames.len() > 1 {
                frames.truncate(1);
            } else if animated {
                while frames.len() < frames_n {
                    let last = frames.last().cloned().unwrap_or_else(|| TileGrid::new(w, h));
                    frames.push(last);
                }
                frames.truncate(frames_n.max(1));
            }
        }
    }

    /// Insert a copy of `frame` after it, on every animated layer.
    pub fn duplicate_frame(&mut self, frame: usize) {
        for l in &mut self.layers {
            if let LayerKind::Raster { frames } = &mut l.kind
                && frames.len() > 1
                && frame < frames.len()
            {
                let copy = frames[frame].clone();
                frames.insert(frame + 1, copy);
            }
        }
        self.frames = (self.frames + 1).min(512);
    }

    pub fn delete_frame(&mut self, frame: usize) {
        if self.frames <= 1 {
            return;
        }
        for l in &mut self.layers {
            if let LayerKind::Raster { frames } = &mut l.kind
                && frames.len() > 1
                && frame < frames.len()
            {
                frames.remove(frame);
            }
        }
        self.frames -= 1;
    }

    /// Memory actually resident, in bytes (the status bar's honesty check).
    pub fn resident_bytes(&self) -> usize {
        let mut n = 0;
        for l in &self.layers {
            if let LayerKind::Raster { frames } = &l.kind {
                for g in frames {
                    n += g.resident_tiles() * crate::tiles::TILE * crate::tiles::TILE * 4;
                }
            }
            if let Some(m) = &l.mask {
                n += m.data.len();
            }
        }
        n
    }
}

/// A point rotated `t` quarter-turns clockwise inside a `w`×`h` canvas.
fn rot_point(x: f32, y: f32, w: f32, h: f32, t: i32) -> [f32; 2] {
    match t {
        1 => [h - y, x],
        2 => [w - x, h - y],
        _ => [y, w - x],
    }
}

/// The same for a direction (a bezier handle) — no canvas offset involved.
fn rot_vec(v: [f32; 2], t: i32) -> [f32; 2] {
    match t {
        1 => [-v[1], v[0]],
        2 => [-v[0], -v[1]],
        _ => [v[1], -v[0]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canvas flip takes the layer's MASK with it. The tab used to flip only
    /// the pixels and drop every mask on the floor — silently.
    #[test]
    fn flipping_carries_masks_and_the_selection() {
        let mut img = Image::new(8, 4, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().edit_rect(Rect::new(0, 0, 2, 4), |_, _, p| {
            *p = [255, 0, 0, 255]
        });
        let mut m = crate::select::Mask::new(8, 4, 0);
        m.set(0, 0, 255);
        img.layers[0].mask = Some(m);
        img.selection = Some(crate::select::rect_mask(8, 4, Rect::new(0, 0, 2, 4)));

        img.flip(true);
        assert_eq!(img.layers[0].grid(0).unwrap().get(7, 0), [255, 0, 0, 255], "pixels mirrored");
        assert_eq!(img.layers[0].grid(0).unwrap().get(0, 0)[3], 0);
        assert_eq!(img.layers[0].mask.as_ref().unwrap().get(7, 0), 255, "the mask mirrored too");
        assert_eq!(
            img.selection.as_ref().unwrap().selected_bounds(),
            Rect::new(6, 0, 2, 4),
            "and so did the selection"
        );
    }

    #[test]
    fn rotating_turns_the_canvas_masks_and_vectors_together() {
        let mut img = Image::new(8, 4, Mode::Pixel);
        img.layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(0, 0, 1, 1), |_, _, p| *p = [1, 2, 3, 255]);
        let mut m = crate::select::Mask::new(8, 4, 0);
        m.set(0, 0, 255);
        img.layers[0].mask = Some(m);
        let mut l = Layer::vector("v");
        l.kind = LayerKind::Vector { paths: vec![crate::vector::VPath::rect(0.0, 0.0, 2.0, 2.0)] };
        img.add_layer(l);

        img.rotate(1);
        assert_eq!((img.w, img.h), (4, 8), "the canvas turned");
        // (0,0) rotates clockwise to the top-right corner.
        assert_eq!(img.layers[0].grid(0).unwrap().get(3, 0), [1, 2, 3, 255]);
        assert_eq!(img.layers[0].mask.as_ref().unwrap().get(3, 0), 255);
        let pts: Vec<[f32; 2]> = match &img.layers[1].kind {
            LayerKind::Vector { paths } => paths[0].nodes.iter().map(|n| n.p).collect(),
            _ => unreachable!(),
        };
        // (0,0)→(4,0), (2,0)→(4,2), (2,2)→(2,2), (0,2)→(2,0) in the turned canvas.
        assert_eq!(pts, vec![[4.0, 0.0], [4.0, 2.0], [2.0, 2.0], [2.0, 0.0]], "the path turned too");
        // Four turns is the identity.
        img.rotate(3);
        assert_eq!((img.w, img.h), (8, 4));
        assert_eq!(img.layers[0].grid(0).unwrap().get(0, 0), [1, 2, 3, 255]);
    }

    #[test]
    fn cropping_to_the_selection_keeps_what_was_selected() {
        let mut img = Image::new(16, 16, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().edit_rect(Rect::new(4, 4, 4, 4), |_, _, p| {
            *p = [9, 9, 9, 255]
        });
        img.selection = Some(crate::select::rect_mask(16, 16, Rect::new(4, 4, 4, 4)));
        assert!(img.crop_to_selection());
        assert_eq!((img.w, img.h), (4, 4));
        assert_eq!(img.layers[0].grid(0).unwrap().get(0, 0), [9, 9, 9, 255]);
        // Nothing selected, nothing to crop to.
        img.selection = None;
        assert!(!img.crop_to_selection());
    }

    #[test]
    fn new_document_has_one_empty_layer() {
        let img = Image::new(32, 32, Mode::Pixel);
        assert_eq!(img.layers.len(), 1);
        assert!(img.layers[0].grid(0).unwrap().is_blank());
        assert_eq!(img.active, 0);
    }

    #[test]
    fn layer_stack_ops_keep_the_active_index_sane() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        img.add_raster_layer();
        img.add_raster_layer();
        assert_eq!(img.layers.len(), 3);
        assert_eq!(img.active, 2);
        img.move_layer(2, -1);
        assert_eq!(img.active, 1, "the active layer follows the move");
        img.delete_layer(1);
        assert_eq!(img.layers.len(), 2);
        assert!(img.active < img.layers.len());
        // The last layer can never be deleted out from under the document.
        img.delete_layer(0);
        img.delete_layer(0);
        assert_eq!(img.layers.len(), 1);
    }

    #[test]
    fn locked_and_hidden_layers_refuse_paint() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        assert!(img.paint_target(0).is_some());
        img.layers[0].locked = true;
        assert!(img.paint_target(0).is_none());
        img.layers[0].locked = false;
        img.layers[0].visible = false;
        assert!(img.paint_target(0).is_none());
    }

    #[test]
    fn adjustment_layers_are_not_paint_targets() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        img.add_layer(Layer::adjust(Adjustment::Invert));
        assert!(img.paint_target(0).is_none(), "you can't brush on an adjustment");
    }

    #[test]
    fn resize_canvas_keeps_content_at_the_anchor() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().set(0, 0, [255, 0, 0, 255]);
        img.resize_canvas(16, 16, 4, 4);
        assert_eq!(img.w, 16);
        assert_eq!(img.layers[0].grid(0).unwrap().get(4, 4), [255, 0, 0, 255]);
    }

    #[test]
    fn scale_resamples_every_layer() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().fill([1, 2, 3, 255]);
        img.scale_to(16, 16, true);
        assert_eq!((img.w, img.h), (16, 16));
        assert_eq!(img.layers[0].grid(0).unwrap().get(15, 15), [1, 2, 3, 255]);
    }

    #[test]
    fn trim_crops_to_the_painted_box() {
        let mut img = Image::new(32, 32, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().edit_rect(Rect::new(10, 12, 4, 5), |_, _, p| {
            *p = [9, 9, 9, 255]
        });
        img.trim(0);
        assert_eq!((img.w, img.h), (4, 5));
        assert_eq!(img.layers[0].grid(0).unwrap().get(0, 0), [9, 9, 9, 255]);
    }

    #[test]
    fn frames_extend_only_animated_layers() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        img.set_frames(4);
        assert_eq!(img.frames, 4);
        assert!(!img.layers[0].is_animated(), "a static layer stays static");
        img.set_layer_animated(0, true);
        assert!(img.layers[0].is_animated());
        match &img.layers[0].kind {
            LayerKind::Raster { frames } => assert_eq!(frames.len(), 4),
            _ => panic!(),
        }
        img.duplicate_frame(0);
        assert_eq!(img.frames, 5);
        img.delete_frame(0);
        assert_eq!(img.frames, 4);
    }

    #[test]
    fn a_new_layer_matches_the_document_frame_count_when_animated_work_exists() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        img.set_frames(3);
        let i = img.add_raster_layer();
        match &img.layers[i].kind {
            LayerKind::Raster { frames } => assert_eq!(frames.len(), 3),
            _ => panic!(),
        }
    }

    #[test]
    fn merge_down_bakes_opacity_and_blend() {
        let mut img = Image::new(4, 4, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().fill([0, 0, 0, 255]);
        img.add_raster_layer();
        img.layers[1].grid_mut(0).unwrap().fill([255, 255, 255, 255]);
        img.layers[1].opacity = 0.5;
        img.merge_down(1, 0);
        assert_eq!(img.layers.len(), 1);
        let px = img.layers[0].grid(0).unwrap().get(1, 1);
        assert!((px[0] as i32 - 128).abs() <= 2, "half-opacity white over black: {px:?}");
    }

    /// Flattening an animation keeps every frame — losing all but the visible
    /// one would be a silent, unrecoverable data loss.
    #[test]
    fn flatten_keeps_every_frame() {
        let mut img = Image::new(4, 4, Mode::Pixel);
        img.set_frames(3);
        img.set_layer_animated(0, true);
        img.layers[0].grid_mut(0).unwrap().fill([255, 0, 0, 255]);
        img.layers[0].grid_mut(2).unwrap().fill([0, 0, 255, 255]);
        img.add_raster_layer();
        img.flatten_all(0);
        assert_eq!(img.layers.len(), 1);
        assert!(img.layers[0].is_animated());
        assert_eq!(img.layers[0].grid(0).unwrap().get(1, 1), [255, 0, 0, 255]);
        assert_eq!(img.layers[0].grid(2).unwrap().get(1, 1), [0, 0, 255, 255]);
    }

    #[test]
    fn resident_memory_tracks_what_is_painted() {
        let mut img = Image::new(1024, 1024, Mode::Painterly);
        assert_eq!(img.resident_bytes(), 0);
        img.layers[0].grid_mut(0).unwrap().set(5, 5, [1, 1, 1, 255]);
        assert_eq!(img.resident_bytes(), 128 * 128 * 4);
    }
}
