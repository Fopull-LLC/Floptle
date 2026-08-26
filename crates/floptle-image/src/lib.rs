//! The image-document kernel behind the editor's 🖼 Image tab
//! (docs/image-editor.md).
//!
//! Everything here is pure CPU and has no window, no egui and no GPU: the
//! document model, the compositor, the brush engine, selections, adjustments,
//! filters, the path rasterizer, palettes, the sprite-sheet packer and the
//! `.flimg` container. That's the house pattern (`floptle-map` behind
//! `map_edit.rs`, `floptle-shader` behind `shader_graph.rs`) and it's why the
//! interesting half of an image editor is covered by ordinary `cargo test`.
//!
//! **Colour convention.** Everything is straight (non-premultiplied) RGBA8 —
//! the same thing PNG stores and the renderer uploads, so nothing converts on
//! the way in or out. Blend math runs in f32 and lands back in u8.
//!
//! **Storage.** Raster pixels live in [`tiles::TileGrid`]: 128² tiles allocated
//! on write, each behind an `Arc`. That makes a document *clone* (the undo
//! snapshot) proportional to the number of tiles rather than the number of
//! pixels, and a stroke's real cost proportional to the tiles it dirtied.

pub mod adjust;
pub mod blend;
pub mod brush;
pub mod composite;
pub mod doc;
pub mod effect;
pub mod filter;
pub mod io;
pub mod palette;
pub mod select;
pub mod sheet;
pub mod tiles;
pub mod transform;
pub mod vector;

pub use adjust::Adjustment;
pub use blend::Blend;
pub use brush::{Brush, BrushMode, StrokeState};
pub use doc::{Image, Layer, LayerKind, Mode};
pub use effect::Effect;
pub use palette::Palette;
pub use select::Mask;
pub use tiles::TileGrid;
pub use vector::{NodeKind, Paint, Stroke, VNode, VPath};

/// An integer pixel rectangle, `x`/`y` inclusive and `w`/`h` a size. The dirty-rect
/// currency of the whole crate: brushes report one, the compositor consumes one, and
/// the editor uploads exactly that sub-rect to the GPU.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x: 0, y: 0, w: 0, h: 0 };

    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Rect { x, y, w, h }
    }

    /// A rect spanning two corner points (either order), inclusive of both.
    pub fn from_points(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        let (lo_x, hi_x) = (x0.min(x1), x0.max(x1));
        let (lo_y, hi_y) = (y0.min(y1), y0.max(y1));
        Rect {
            x: lo_x,
            y: lo_y,
            w: (hi_x - lo_x + 1) as u32,
            h: (hi_y - lo_y + 1) as u32,
        }
    }

    pub fn size(w: u32, h: u32) -> Self {
        Rect { x: 0, y: 0, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn right(&self) -> i32 {
        self.x + self.w as i32
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h as i32
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    /// The smallest rect containing both. An empty rect is the identity.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect { x, y, w: (r - x) as u32, h: (b - y) as u32 }
    }

    pub fn intersect(self, other: Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r <= x || b <= y {
            Rect::EMPTY
        } else {
            Rect { x, y, w: (r - x) as u32, h: (b - y) as u32 }
        }
    }

    /// Grow by `n` pixels on every side (shrinks for a negative `n`).
    pub fn expand(self, n: i32) -> Rect {
        if self.is_empty() {
            return self;
        }
        let w = self.w as i32 + n * 2;
        let h = self.h as i32 + n * 2;
        if w <= 0 || h <= 0 {
            return Rect::EMPTY;
        }
        Rect { x: self.x - n, y: self.y - n, w: w as u32, h: h as u32 }
    }
}

/// Linear-ish luminance of an sRGB8 triple (Rec.709 weights on the raw bytes —
/// what every paint program means by "brightness").
pub fn luma(px: [u8; 4]) -> f32 {
    0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32
}

/// Clamp + round an f32 0..255 to u8.
#[inline]
pub fn u8c(v: f32) -> u8 {
    v.clamp(0.0, 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_algebra() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.union(b), Rect::new(0, 0, 15, 15));
        assert_eq!(a.intersect(b), Rect::new(5, 5, 5, 5));
        assert!(a.intersect(Rect::new(50, 50, 2, 2)).is_empty());
        assert_eq!(Rect::EMPTY.union(a), a);
        assert_eq!(a.expand(2), Rect::new(-2, -2, 14, 14));
        assert!(a.expand(-6).is_empty());
        assert_eq!(Rect::from_points(4, 9, 2, 3), Rect::new(2, 3, 3, 7));
    }
}
