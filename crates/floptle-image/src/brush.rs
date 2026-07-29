//! The brush engine: dabs, strokes, pixel-perfect lines, and the pixel
//! operations that ride the same profile (bucket fill, gradients, shape stamps).
//!
//! One [`Brush`] describes *how a stroke feels* — radius, hardness, flow,
//! spacing, blend, and which of the eight [`BrushMode`]s it performs — and both
//! the 2D canvas and (eventually, §13) the 3D texture brush read it, so tuning a
//! brush is one job rather than two.
//!
//! Every write is clipped by the live selection and, in tiling mode, wraps at
//! the canvas edges: a stroke that leaves the right edge enters at the left,
//! which is the thing that actually *makes* a texture seamless.

use crate::select::Mask;
use crate::tiles::TileGrid;
use crate::{blend, u8c, Blend, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BrushMode {
    #[default]
    Paint,
    /// Removes coverage rather than painting a colour.
    Erase,
    /// Smears pixels along the stroke direction.
    Smudge,
    Blur,
    Sharpen,
    /// Lightens (dodge) / darkens (burn) preserving hue.
    Dodge,
    Burn,
    /// Copies from an offset source point.
    Clone,
}

impl BrushMode {
    pub const ALL: [BrushMode; 8] = [
        BrushMode::Paint,
        BrushMode::Erase,
        BrushMode::Smudge,
        BrushMode::Blur,
        BrushMode::Sharpen,
        BrushMode::Dodge,
        BrushMode::Burn,
        BrushMode::Clone,
    ];
    pub fn label(self) -> &'static str {
        match self {
            BrushMode::Paint => "Paint",
            BrushMode::Erase => "Erase",
            BrushMode::Smudge => "Smudge",
            BrushMode::Blur => "Blur",
            BrushMode::Sharpen => "Sharpen",
            BrushMode::Dodge => "Dodge",
            BrushMode::Burn => "Burn",
            BrushMode::Clone => "Clone stamp",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Brush {
    /// Radius in canvas pixels. A radius of 0.5 is the single-pixel pencil.
    pub radius: f32,
    /// 0 = fully soft falloff, 1 = hard edge (1 px of anti-aliasing).
    pub hardness: f32,
    /// Per-dab opacity, 0..1.
    pub flow: f32,
    /// Dab spacing as a fraction of the diameter. 0.15 is the paint-program norm.
    pub spacing: f32,
    pub blend: Blend,
    pub mode: BrushMode,
    /// Hard, un-anti-aliased, integer dabs — pixel mode's pencil.
    pub pixel_perfect: bool,
    /// Strength for the non-painting modes (smudge/blur/dodge/burn), 0..1.
    pub strength: f32,
}

impl Default for Brush {
    fn default() -> Self {
        Brush {
            radius: 0.5,
            hardness: 1.0,
            flow: 1.0,
            spacing: 0.15,
            blend: Blend::Mix,
            mode: BrushMode::Paint,
            pixel_perfect: true,
            strength: 0.5,
        }
    }
}

impl Brush {
    /// Coverage of the dab at a pixel `d` away from its centre, 0..1.
    fn coverage(&self, d: f32) -> f32 {
        let r = self.radius.max(0.5);
        if self.pixel_perfect {
            return if d <= r { 1.0 } else { 0.0 };
        }
        if self.hardness >= 0.99 {
            // A hard round brush still wants one pixel of edge, or it crawls.
            return (r - d + 0.5).clamp(0.0, 1.0);
        }
        let inner = r * self.hardness;
        if d <= inner {
            1.0
        } else if d >= r {
            0.0
        } else {
            let t = 1.0 - (d - inner) / (r - inner).max(1e-4);
            t * t * (3.0 - 2.0 * t)
        }
    }

    /// The pixel box one dab touches, centred on (x, y) in *canvas* space.
    pub fn dab_rect(&self, x: f32, y: f32) -> Rect {
        let r = self.radius.max(0.5) + 1.0;
        Rect::from_points(
            (x - r).floor() as i32,
            (y - r).floor() as i32,
            (x + r).ceil() as i32,
            (y + r).ceil() as i32,
        )
    }
}

/// Everything a dab needs beyond the brush itself.
pub struct DabCtx<'a> {
    /// Clips every write. `None` = the whole canvas.
    pub sel: Option<&'a Mask>,
    /// Canvas position of the layer's (0, 0) — dabs are given canvas coords.
    pub origin: (i32, i32),
    /// Canvas size, for wrap-around brushing.
    pub canvas: (u32, u32),
    /// Wrap the stroke at the canvas edges (tiling mode).
    pub wrap: bool,
    /// Clone-stamp source offset (source = point − offset).
    pub clone_offset: (f32, f32),
}

impl DabCtx<'_> {
    pub fn simple(w: u32, h: u32) -> DabCtx<'static> {
        DabCtx { sel: None, origin: (0, 0), canvas: (w, h), wrap: false, clone_offset: (0.0, 0.0) }
    }
}

/// Live state across one stroke (spacing carry, direction, pixel-perfect history).
#[derive(Default, Clone, Debug)]
pub struct StrokeState {
    pub last: Option<(f32, f32)>,
    /// Distance carried over from the previous segment, so spacing is even.
    carry: f32,
    /// The last three accepted pixel-perfect points, each with the pixel value
    /// it covered — so retracting a staircase corner restores what was there
    /// rather than punching a hole in the layer.
    pp: Vec<((i32, i32), [u8; 4])>,
    /// Alpha already laid down by this stroke, per pixel — so overlapping dabs
    /// don't build up ("build-up" belongs to flow, not to dab spacing).
    painted: Option<Mask>,
    pub dabs: u32,
}

impl StrokeState {
    pub fn begin(&mut self, x: f32, y: f32, w: u32, h: u32) {
        self.last = Some((x, y));
        self.carry = 0.0;
        // Seed the pixel-perfect history with the point the first dab lands on,
        // or the very first corner of a stroke can never be recognised.
        self.pp = vec![((x.floor() as i32, y.floor() as i32), [0, 0, 0, 0])];
        self.painted = Some(Mask::new(w, h, 0));
        self.dabs = 0;
    }

    pub fn end(&mut self) {
        self.last = None;
        self.pp.clear();
        self.painted = None;
    }
}

/// Stamp one dab at canvas (x, y). Returns the dirtied canvas rect.
pub fn stamp(
    grid: &mut TileGrid,
    b: &Brush,
    x: f32,
    y: f32,
    color: [u8; 4],
    ctx: &DabCtx,
    state: &mut StrokeState,
) -> Rect {
    let mut dirty = Rect::EMPTY;
    if ctx.wrap {
        // Nine placements: whichever ones overlap the canvas actually draw, and
        // together they are exactly wrap-around brushing.
        let (cw, ch) = (ctx.canvas.0 as f32, ctx.canvas.1 as f32);
        for my in -1..=1 {
            for mx in -1..=1 {
                let (px, py) = (x + mx as f32 * cw, y + my as f32 * ch);
                if b.dab_rect(px, py).intersect(Rect::size(ctx.canvas.0, ctx.canvas.1)).is_empty() {
                    continue;
                }
                dirty = dirty.union(stamp_one(grid, b, px, py, color, ctx, state));
            }
        }
    } else {
        dirty = stamp_one(grid, b, x, y, color, ctx, state);
    }
    state.dabs += 1;
    dirty
}

fn stamp_one(
    grid: &mut TileGrid,
    b: &Brush,
    x: f32,
    y: f32,
    color: [u8; 4],
    ctx: &DabCtx,
    state: &mut StrokeState,
) -> Rect {
    let canvas_rect = b.dab_rect(x, y).intersect(Rect::size(ctx.canvas.0, ctx.canvas.1));
    if canvas_rect.is_empty() {
        return Rect::EMPTY;
    }
    let (ox, oy) = ctx.origin;
    let layer_rect = Rect::new(canvas_rect.x - ox, canvas_rect.y - oy, canvas_rect.w, canvas_rect.h);

    // Modes that read neighbouring pixels work off a snapshot, so a dab can't
    // feed on its own output halfway through.
    let snap_rect = layer_rect.expand(2);
    let snapshot = match b.mode {
        BrushMode::Smudge | BrushMode::Blur | BrushMode::Sharpen => Some(grid.read_rect(snap_rect)),
        _ => None,
    };
    let snap_at = |sx: i32, sy: i32| -> [u8; 4] {
        let Some(s) = &snapshot else { return [0, 0, 0, 0] };
        let (lx, ly) = (sx - snap_rect.x, sy - snap_rect.y);
        if lx < 0 || ly < 0 || lx >= snap_rect.w as i32 || ly >= snap_rect.h as i32 {
            return [0, 0, 0, 0];
        }
        let o = (ly as usize * snap_rect.w as usize + lx as usize) * 4;
        [s[o], s[o + 1], s[o + 2], s[o + 3]]
    };
    // Clone reads live from wherever the source point sits.
    let clone_src: Option<Vec<u8>> = if b.mode == BrushMode::Clone {
        let r = Rect::new(
            layer_rect.x - ctx.clone_offset.0.round() as i32,
            layer_rect.y - ctx.clone_offset.1.round() as i32,
            layer_rect.w,
            layer_rect.h,
        );
        Some(grid.read_rect(r))
    } else {
        None
    };

    let dir = state
        .last
        .map(|(lx, ly)| (x - lx, y - ly))
        .filter(|(dx, dy)| dx.abs() + dy.abs() > 1e-4)
        .unwrap_or((0.0, 0.0));
    let dlen = (dir.0 * dir.0 + dir.1 * dir.1).sqrt().max(1e-4);
    let step = (dir.0 / dlen, dir.1 / dlen);

    // Soft brushes must not build up where consecutive dabs overlap within one
    // stroke; track the maximum coverage already applied per pixel.
    let mut applied: Vec<(i32, i32, u8)> = Vec::new();
    let painted = state.painted.as_ref();

    grid.edit_rect(layer_rect, |lx, ly, px| {
        let (cx, cy) = (lx + ox, ly + oy);
        let d = ((cx as f32 + 0.5 - x).powi(2) + (cy as f32 + 0.5 - y).powi(2)).sqrt();
        let mut cov = b.coverage(d);
        if cov <= 0.0 {
            return;
        }
        if let Some(sel) = ctx.sel {
            cov *= sel.at(cx, cy);
            if cov <= 0.0 {
                return;
            }
        }
        // Paint accumulates to a TARGET alpha (coverage × flow) rather than
        // adding per dab: overlapping dabs within one stroke must not build up,
        // or a soft 20 %-flow brush goes opaque the moment you slow down.
        // Solving 1-(1-a_prev)(1-k) = a_target gives the increment to apply.
        let flow = b.flow.clamp(0.0, 1.0);
        let k = if b.mode == BrushMode::Paint {
            let a_prev = painted.map_or(0.0, |m| m.at(cx, cy));
            let a_target = cov * flow;
            if a_target <= a_prev {
                return;
            }
            applied.push((cx, cy, u8c(a_target * 255.0)));
            ((a_target - a_prev) / (1.0 - a_prev).max(1e-4)).clamp(0.0, 1.0)
        } else {
            cov * flow
        };
        match b.mode {
            BrushMode::Paint => {
                *px = blend::over(*px, color, b.blend, k);
            }
            BrushMode::Erase => {
                px[3] = u8c(px[3] as f32 * (1.0 - k));
            }
            BrushMode::Smudge => {
                let s = snap_at(
                    lx - (step.0 * b.radius.max(1.0) * 0.5).round() as i32,
                    ly - (step.1 * b.radius.max(1.0) * 0.5).round() as i32,
                );
                let t = k * b.strength.clamp(0.0, 1.0);
                for i in 0..4 {
                    px[i] = u8c(px[i] as f32 + (s[i] as f32 - px[i] as f32) * t);
                }
            }
            BrushMode::Blur | BrushMode::Sharpen => {
                let mut acc = [0f32; 4];
                for dy in -1..=1i32 {
                    for dx in -1..=1i32 {
                        let s = snap_at(lx + dx, ly + dy);
                        let a = s[3] as f32 / 255.0;
                        acc[0] += s[0] as f32 * a;
                        acc[1] += s[1] as f32 * a;
                        acc[2] += s[2] as f32 * a;
                        acc[3] += s[3] as f32;
                    }
                }
                let a = acc[3] / 9.0;
                let inv = if a > 0.5 { 255.0 / a } else { 0.0 };
                let soft = [acc[0] / 9.0 * inv, acc[1] / 9.0 * inv, acc[2] / 9.0 * inv, a];
                let t = k * b.strength.clamp(0.0, 1.0);
                for i in 0..4 {
                    let target = if b.mode == BrushMode::Blur {
                        soft[i]
                    } else {
                        px[i] as f32 + (px[i] as f32 - soft[i]) // unsharp
                    };
                    px[i] = u8c(px[i] as f32 + (target - px[i] as f32) * t);
                }
            }
            BrushMode::Dodge | BrushMode::Burn => {
                let t = k * b.strength.clamp(0.0, 1.0) * 0.6;
                for v in px.iter_mut().take(3) {
                    let f = *v as f32 / 255.0;
                    let n = if b.mode == BrushMode::Dodge {
                        f + (1.0 - f) * t
                    } else {
                        f * (1.0 - t)
                    };
                    *v = u8c(n * 255.0);
                }
            }
            BrushMode::Clone => {
                if let Some(src) = &clone_src {
                    let o = ((ly - layer_rect.y) as usize * layer_rect.w as usize
                        + (lx - layer_rect.x) as usize)
                        * 4;
                    if o + 4 <= src.len() {
                        let s = [src[o], src[o + 1], src[o + 2], src[o + 3]];
                        *px = blend::over(*px, s, b.blend, k);
                    }
                }
            }
        }
    });
    if let Some(m) = state.painted.as_mut() {
        for (cx, cy, v) in applied {
            if m.get(cx, cy) < v {
                m.set(cx, cy, v);
            }
        }
    }
    canvas_rect
}

/// Continue a stroke to (x, y), laying evenly-spaced dabs from the last point.
/// Call [`StrokeState::begin`] first and [`StrokeState::end`] on release.
pub fn stroke_to(
    grid: &mut TileGrid,
    b: &Brush,
    x: f32,
    y: f32,
    color: [u8; 4],
    ctx: &DabCtx,
    state: &mut StrokeState,
) -> Rect {
    let Some((lx, ly)) = state.last else {
        state.last = Some((x, y));
        return stamp(grid, b, x, y, color, ctx, state);
    };
    if b.pixel_perfect && b.radius <= 1.0 {
        return pixel_stroke(grid, b, lx, ly, x, y, color, ctx, state);
    }
    let spacing = (b.spacing.max(0.02) * b.radius.max(0.5) * 2.0).max(0.5);
    let (dx, dy) = (x - lx, y - ly);
    let dist = (dx * dx + dy * dy).sqrt();
    let mut dirty = Rect::EMPTY;
    if dist < 1e-4 {
        return dirty;
    }
    let mut t = spacing - state.carry;
    while t <= dist {
        let f = t / dist;
        let (px, py) = (lx + dx * f, ly + dy * f);
        dirty = dirty.union(stamp(grid, b, px, py, color, ctx, state));
        state.last = Some((px, py));
        t += spacing;
    }
    state.carry = (dist - (t - spacing)).max(0.0);
    state.last = Some((x, y));
    dirty
}

/// Pixel-perfect stroking: a Bresenham line, minus the "L" pixel a corner leaves
/// behind. That single rule is the difference between a pencil that feels like
/// Aseprite's and one that feels broken.
#[allow(clippy::too_many_arguments)]
fn pixel_stroke(
    grid: &mut TileGrid,
    b: &Brush,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: [u8; 4],
    ctx: &DabCtx,
    state: &mut StrokeState,
) -> Rect {
    let mut dirty = Rect::EMPTY;
    let pts = line_points(x0.floor() as i32, y0.floor() as i32, x1.floor() as i32, y1.floor() as i32);
    // The corner rule only makes sense for a one-pixel nib; a fat pencil still
    // walks the integer line (so it can't leave gaps) but keeps every step.
    let thin = b.radius <= 0.6;
    for (i, (px, py)) in pts.into_iter().enumerate() {
        if i == 0 && state.dabs > 0 {
            continue; // the segment's first point was stamped by the last call
        }
        let (ox, oy) = ctx.origin;
        let under = grid.get((px - ox) as i64, (py - oy) as i64);
        state.pp.push(((px, py), under));
        // Corner rule: with a, m, c the last three points, drop m when a and c
        // are diagonal neighbours — m is a redundant staircase pixel.
        if thin && state.pp.len() >= 3 {
            let n = state.pp.len();
            let (a, m, c) = (state.pp[n - 3].0, state.pp[n - 2], state.pp[n - 1].0);
            let diag = (a.0 - c.0).abs() == 1 && (a.1 - c.1).abs() == 1;
            let adj = (a.0 - m.0.0).abs() + (a.1 - m.0.1).abs() == 1
                && (m.0.0 - c.0).abs() + (m.0.1 - c.1).abs() == 1;
            if diag && adj {
                // Put back exactly what the retracted dab covered.
                grid.set((m.0.0 - ox) as i64, (m.0.1 - oy) as i64, m.1);
                if let Some(mask) = state.painted.as_mut() {
                    mask.set(m.0.0, m.0.1, 0);
                }
                state.pp.remove(n - 2);
                dirty = dirty.union(Rect::new(m.0.0, m.0.1, 1, 1));
            }
        }
        if state.pp.len() > 3 {
            state.pp.remove(0);
        }
        dirty = dirty.union(stamp(grid, b, px as f32 + 0.5, py as f32 + 0.5, color, ctx, state));
    }
    state.last = Some((x1, y1));
    dirty
}

/// Integer Bresenham line points, inclusive of both ends.
pub fn line_points(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let (mut x, mut y) = (x0, y0);
    let mut err = dx + dy;
    loop {
        out.push((x, y));
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
        if out.len() > 1 << 16 {
            break; // paranoia: never spin on a degenerate call
        }
    }
    out
}

/// Bucket fill from (x, y). `contiguous` floods; otherwise every matching pixel
/// on the layer changes. Returns the dirtied rect.
#[allow(clippy::too_many_arguments)]
pub fn flood_fill(
    grid: &mut TileGrid,
    x: i32,
    y: i32,
    color: [u8; 4],
    tolerance: u8,
    contiguous: bool,
    sel: Option<&Mask>,
    mode: Blend,
    opacity: f32,
) -> Rect {
    let m = crate::select::wand_mask(grid, x, y, tolerance, contiguous);
    let mut dirty = Rect::EMPTY;
    let bounds = m.selected_bounds();
    if bounds.is_empty() {
        return dirty;
    }
    grid.edit_rect(bounds, |px_x, px_y, px| {
        let mut k = m.at(px_x, px_y) * opacity;
        if let Some(s) = sel {
            k *= s.at(px_x, px_y);
        }
        if k <= 0.0 {
            return;
        }
        *px = blend::over(*px, color, mode, k);
    });
    dirty = dirty.union(bounds);
    dirty
}

/// Fill the whole selection (or the whole layer) with one colour.
pub fn fill_region(
    grid: &mut TileGrid,
    rect: Rect,
    color: [u8; 4],
    sel: Option<&Mask>,
    mode: Blend,
    opacity: f32,
) -> Rect {
    grid.edit_rect(rect, |x, y, px| {
        let mut k = opacity;
        if let Some(s) = sel {
            k *= s.at(x, y);
        }
        if k <= 0.0 {
            return;
        }
        *px = blend::over(*px, color, mode, k);
    });
    rect
}

/// Clear (to transparent) inside the selection.
pub fn clear_region(grid: &mut TileGrid, rect: Rect, sel: Option<&Mask>) -> Rect {
    grid.edit_rect(rect, |x, y, px| {
        let k = sel.map_or(1.0, |s| s.at(x, y));
        if k <= 0.0 {
            return;
        }
        px[3] = u8c(px[3] as f32 * (1.0 - k));
    });
    rect
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GradientKind {
    #[default]
    Linear,
    Radial,
}

/// Draw a gradient across `rect`, from (x0, y0) to (x1, y1).
#[allow(clippy::too_many_arguments)]
pub fn gradient_fill(
    grid: &mut TileGrid,
    rect: Rect,
    kind: GradientKind,
    from: (f32, f32),
    to: (f32, f32),
    a: [u8; 4],
    b: [u8; 4],
    sel: Option<&Mask>,
    mode: Blend,
    opacity: f32,
) -> Rect {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let len2 = (dx * dx + dy * dy).max(1e-6);
    let radius = len2.sqrt().max(1e-3);
    grid.edit_rect(rect, |x, y, px| {
        let (fx, fy) = (x as f32 + 0.5 - from.0, y as f32 + 0.5 - from.1);
        let t = match kind {
            GradientKind::Linear => ((fx * dx + fy * dy) / len2).clamp(0.0, 1.0),
            GradientKind::Radial => ((fx * fx + fy * fy).sqrt() / radius).clamp(0.0, 1.0),
        };
        let mut c = [0u8; 4];
        for i in 0..4 {
            c[i] = u8c(a[i] as f32 + (b[i] as f32 - a[i] as f32) * t);
        }
        let mut k = opacity;
        if let Some(s) = sel {
            k *= s.at(x, y);
        }
        if k <= 0.0 {
            return;
        }
        *px = blend::over(*px, c, mode, k);
    });
    rect
}

/// Rasterize vector paths straight into a raster layer — how the shape tools
/// stamp pixels without a second rasterizer.
#[allow(clippy::too_many_arguments)]
pub fn stamp_paths(
    grid: &mut TileGrid,
    paths: &[crate::VPath],
    aa: bool,
    origin: (i32, i32),
    canvas: (u32, u32),
    sel: Option<&Mask>,
    mode: Blend,
    opacity: f32,
) -> Rect {
    let mut bounds = Rect::EMPTY;
    for p in paths {
        bounds = bounds.union(p.bounds());
    }
    let bounds = bounds.intersect(Rect::size(canvas.0, canvas.1));
    if bounds.is_empty() {
        return bounds;
    }
    let full = crate::vector::render(paths, canvas.0, canvas.1, aa);
    let layer_rect = Rect::new(bounds.x - origin.0, bounds.y - origin.1, bounds.w, bounds.h);
    grid.edit_rect(layer_rect, |lx, ly, px| {
        let (cx, cy) = (lx + origin.0, ly + origin.1);
        if cx < 0 || cy < 0 || cx >= canvas.0 as i32 || cy >= canvas.1 as i32 {
            return;
        }
        let o = (cy as usize * canvas.0 as usize + cx as usize) * 4;
        let s = [full[o], full[o + 1], full[o + 2], full[o + 3]];
        if s[3] == 0 {
            return;
        }
        let mut k = opacity;
        if let Some(m) = sel {
            k *= m.at(cx, cy);
        }
        if k <= 0.0 {
            return;
        }
        *px = blend::over(*px, s, mode, k);
    });
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pencil() -> Brush {
        Brush { radius: 0.5, pixel_perfect: true, ..Default::default() }
    }

    #[test]
    fn a_pencil_dab_paints_exactly_one_pixel() {
        let mut g = TileGrid::new(16, 16);
        let ctx = DabCtx::simple(16, 16);
        let mut st = StrokeState::default();
        st.begin(4.5, 4.5, 16, 16);
        let d = stamp(&mut g, &pencil(), 4.5, 4.5, [255, 0, 0, 255], &ctx, &mut st);
        assert_eq!(g.get(4, 4), [255, 0, 0, 255]);
        assert_eq!(g.get(5, 4), [0, 0, 0, 0]);
        assert!(d.contains(4, 4));
    }

    #[test]
    fn a_soft_brush_falls_off() {
        let mut g = TileGrid::new(32, 32);
        let b = Brush {
            radius: 6.0,
            hardness: 0.0,
            pixel_perfect: false,
            ..Default::default()
        };
        let ctx = DabCtx::simple(32, 32);
        let mut st = StrokeState::default();
        st.begin(16.5, 16.5, 32, 32);
        stamp(&mut g, &b, 16.5, 16.5, [255, 255, 255, 255], &ctx, &mut st);
        let mid = g.get(16, 16)[3];
        let edge = g.get(21, 16)[3];
        assert_eq!(mid, 255, "the dab centre reaches full coverage");
        assert!(edge > 0 && edge < mid, "soft edge: mid {mid}, edge {edge}");
        assert_eq!(g.get(24, 16)[3], 0, "nothing beyond the radius");
    }

    #[test]
    fn a_stroke_is_continuous() {
        let mut g = TileGrid::new(32, 32);
        let ctx = DabCtx::simple(32, 32);
        let mut st = StrokeState::default();
        st.begin(2.5, 2.5, 32, 32);
        stamp(&mut g, &pencil(), 2.5, 2.5, [0, 0, 0, 255], &ctx, &mut st);
        stroke_to(&mut g, &pencil(), 20.5, 2.5, [0, 0, 0, 255], &ctx, &mut st);
        for x in 2..=20 {
            assert_eq!(g.get(x, 2)[3], 255, "gap at x={x}");
        }
    }

    /// The pixel-perfect rule: a 45° drag must not leave staircase doubles.
    #[test]
    fn pixel_perfect_drops_corner_pixels() {
        let mut g = TileGrid::new(16, 16);
        let ctx = DabCtx::simple(16, 16);
        let mut st = StrokeState::default();
        st.begin(1.5, 1.5, 16, 16);
        stamp(&mut g, &pencil(), 1.5, 1.5, [0, 0, 0, 255], &ctx, &mut st);
        // Drag right one, then down one — the classic L.
        stroke_to(&mut g, &pencil(), 2.5, 1.5, [0, 0, 0, 255], &ctx, &mut st);
        stroke_to(&mut g, &pencil(), 2.5, 2.5, [0, 0, 0, 255], &ctx, &mut st);
        assert_eq!(g.get(1, 1)[3], 255);
        assert_eq!(g.get(2, 2)[3], 255);
        assert_eq!(g.get(2, 1)[3], 0, "the corner pixel must be dropped");
    }

    #[test]
    fn a_selection_clips_the_brush() {
        let mut g = TileGrid::new(16, 16);
        let sel = crate::select::rect_mask(16, 16, Rect::new(0, 0, 8, 16));
        let ctx = DabCtx { sel: Some(&sel), ..DabCtx::simple(16, 16) };
        let b = Brush { radius: 6.0, pixel_perfect: false, hardness: 1.0, ..Default::default() };
        let mut st = StrokeState::default();
        st.begin(8.0, 8.0, 16, 16);
        stamp(&mut g, &b, 8.0, 8.0, [255, 0, 0, 255], &ctx, &mut st);
        assert_eq!(g.get(5, 8)[3], 255, "inside the selection");
        assert_eq!(g.get(11, 8)[3], 0, "outside it");
    }

    /// Wrap-around brushing: a dab at the right edge must appear at the left.
    #[test]
    fn tiling_wraps_the_stroke() {
        let mut g = TileGrid::new(32, 32);
        let ctx = DabCtx { wrap: true, ..DabCtx::simple(32, 32) };
        let b = Brush { radius: 4.0, pixel_perfect: false, hardness: 1.0, ..Default::default() };
        let mut st = StrokeState::default();
        st.begin(31.0, 16.0, 32, 32);
        stamp(&mut g, &b, 31.0, 16.0, [0, 255, 0, 255], &ctx, &mut st);
        assert!(g.get(31, 16)[3] > 0, "painted at the right edge");
        assert!(g.get(0, 16)[3] > 0, "and wrapped to the left edge");
    }

    #[test]
    fn eraser_removes_alpha_only() {
        let mut g = TileGrid::filled(16, 16, [10, 20, 30, 255]);
        let b = Brush { radius: 3.0, mode: BrushMode::Erase, pixel_perfect: false, hardness: 1.0, ..Default::default() };
        let ctx = DabCtx::simple(16, 16);
        let mut st = StrokeState::default();
        st.begin(8.0, 8.0, 16, 16);
        stamp(&mut g, &b, 8.0, 8.0, [0, 0, 0, 255], &ctx, &mut st);
        assert_eq!(g.get(8, 8)[3], 0);
        assert_eq!(g.get(15, 15), [10, 20, 30, 255]);
    }

    #[test]
    fn a_soft_stroke_does_not_build_up_where_dabs_overlap() {
        let b = Brush {
            radius: 5.0,
            hardness: 0.0,
            flow: 0.5,
            spacing: 0.05,
            pixel_perfect: false,
            ..Default::default()
        };
        let ctx = DabCtx::simple(64, 64);
        let mut g = TileGrid::new(64, 64);
        let mut st = StrokeState::default();
        st.begin(10.0, 32.0, 64, 64);
        stamp(&mut g, &b, 10.0, 32.0, [255, 255, 255, 255], &ctx, &mut st);
        stroke_to(&mut g, &b, 50.0, 32.0, [255, 255, 255, 255], &ctx, &mut st);
        let a = g.get(30, 32)[3];
        assert!(a <= 130, "flow 0.5 must not accumulate to opaque within one stroke: {a}");
        assert!(a >= 100, "…but should reach roughly the flow value: {a}");
    }

    #[test]
    fn flood_fill_respects_edges_and_tolerance() {
        let mut g = TileGrid::filled(16, 16, [255, 255, 255, 255]);
        // A vertical black wall at x=8.
        g.edit_rect(Rect::new(8, 0, 1, 16), |_, _, p| *p = [0, 0, 0, 255]);
        flood_fill(&mut g, 2, 2, [255, 0, 0, 255], 10, true, None, Blend::Mix, 1.0);
        assert_eq!(g.get(2, 2), [255, 0, 0, 255]);
        assert_eq!(g.get(12, 2), [255, 255, 255, 255], "the wall stopped it");
        assert_eq!(g.get(8, 2), [0, 0, 0, 255]);
    }

    #[test]
    fn gradient_ramps_between_the_endpoints() {
        let mut g = TileGrid::new(32, 4);
        gradient_fill(
            &mut g,
            Rect::size(32, 4),
            GradientKind::Linear,
            (0.0, 0.0),
            (32.0, 0.0),
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            None,
            Blend::Mix,
            1.0,
        );
        assert!(g.get(0, 0)[0] < 20);
        assert!(g.get(31, 0)[0] > 235);
        assert!((g.get(16, 0)[0] as i32 - 128).abs() < 20);
    }

    #[test]
    fn shape_stamping_reuses_the_vector_rasterizer() {
        let mut g = TileGrid::new(32, 32);
        let p = crate::VPath::rect(8.0, 8.0, 10.0, 10.0);
        let r = stamp_paths(&mut g, &[p], false, (0, 0), (32, 32), None, Blend::Mix, 1.0);
        assert!(!r.is_empty());
        assert_eq!(g.get(12, 12)[3], 255);
        assert_eq!(g.get(2, 2)[3], 0);
    }

    #[test]
    fn clone_stamp_copies_from_the_offset_source() {
        let mut g = TileGrid::new(32, 32);
        g.edit_rect(Rect::new(0, 0, 8, 8), |_, _, p| *p = [200, 30, 30, 255]);
        let b = Brush { radius: 3.0, mode: BrushMode::Clone, pixel_perfect: false, hardness: 1.0, ..Default::default() };
        let ctx = DabCtx { clone_offset: (20.0, 20.0), ..DabCtx::simple(32, 32) };
        let mut st = StrokeState::default();
        st.begin(24.0, 24.0, 32, 32);
        stamp(&mut g, &b, 24.0, 24.0, [0, 0, 0, 255], &ctx, &mut st);
        assert_eq!(g.get(24, 24), [200, 30, 30, 255]);
    }

    #[test]
    fn line_points_are_inclusive_and_connected() {
        let p = line_points(0, 0, 3, 1);
        assert_eq!(p.first(), Some(&(0, 0)));
        assert_eq!(p.last(), Some(&(3, 1)));
        for w in p.windows(2) {
            let d = (w[0].0 - w[1].0).abs().max((w[0].1 - w[1].1).abs());
            assert_eq!(d, 1);
        }
    }

    #[test]
    fn clear_region_honours_the_selection() {
        let mut g = TileGrid::filled(16, 16, [1, 2, 3, 255]);
        let sel = crate::select::rect_mask(16, 16, Rect::new(4, 4, 4, 4));
        clear_region(&mut g, Rect::size(16, 16), Some(&sel));
        assert_eq!(g.get(5, 5)[3], 0);
        assert_eq!(g.get(1, 1)[3], 255);
    }
}
