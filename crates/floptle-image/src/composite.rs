//! The reference compositor: a layer stack → flat straight-RGBA8.
//!
//! Everything the editor shows, every PNG it exports and every sprite-sheet cell
//! it packs comes out of here, so there is exactly one answer to "what does this
//! document look like".
//!
//! Two properties are load-bearing:
//!
//! - **Dirty rects.** [`composite_rect`] draws only the region asked for, which
//!   is what keeps a brush at 60 Hz on a 2048² canvas — a dab recomposites the
//!   tiles it touched, not the document.
//! - **Rect-independence.** A sub-rect must be pixel-identical to the same region
//!   of a full-canvas render, or painting would leave visible tile seams. The one
//!   operation that can't honour that (Floyd–Steinberg dithering) declares itself
//!   via [`Adjustment::needs_full_canvas`](crate::Adjustment::needs_full_canvas)
//!   and [`needs_full_canvas`] tells the caller to widen the rect.

use crate::doc::{Image, LayerKind, VectorCache};
use crate::{blend, Rect};

/// True when this document contains something whose result depends on the whole
/// canvas, so the caller must recomposite everything rather than a dirty rect.
pub fn needs_full_canvas(img: &Image) -> bool {
    img.layers.iter().any(|l| {
        l.visible && matches!(&l.kind, LayerKind::Adjust(a) if a.needs_full_canvas())
    })
}

/// Composite `rect` of the document at `frame` into a fresh tightly-packed
/// buffer of `rect.w * rect.h * 4` bytes.
pub fn composite_rect(img: &Image, frame: usize, rect: Rect, vcache: &mut VectorCache) -> Vec<u8> {
    let n = rect.w as usize * rect.h as usize;
    let mut acc = vec![0u8; n * 4];
    if rect.is_empty() {
        return acc;
    }
    let aa = img.mode.antialias();
    // Alpha of the last non-clipping layer — what `clip_below` clips against.
    let mut clip_alpha: Option<Vec<u8>> = None;

    for layer in &img.layers {
        if !layer.visible || layer.opacity <= 0.0 {
            continue;
        }
        match &layer.kind {
            LayerKind::Adjust(a) => {
                let mut adjusted = acc.clone();
                a.apply(&mut adjusted, rect.w, rect.h, rect.x, rect.y);
                for i in 0..n {
                    let mut k = layer.opacity.clamp(0.0, 1.0);
                    if layer.mask_enabled && let Some(m) = &layer.mask {
                        let (x, y) = (rect.x + (i % rect.w as usize) as i32, rect.y + (i / rect.w as usize) as i32);
                        k *= m.at(x, y);
                    }
                    if layer.clip_below && let Some(ca) = &clip_alpha {
                        k *= ca[i] as f32 / 255.0;
                    }
                    if k <= 0.0 {
                        continue;
                    }
                    for c in 0..4 {
                        let o = i * 4 + c;
                        acc[o] = crate::u8c(
                            acc[o] as f32 + (adjusted[o] as f32 - acc[o] as f32) * k,
                        );
                    }
                }
            }
            _ => {
                // Effects reach outside the rect, so render with a margin and crop.
                let margin = layer.effect_margin() as i32;
                let r2 = rect.expand(margin);
                let mut src = layer.render_rect(r2, frame, (img.w, img.h), aa, vcache);
                for e in &layer.effects {
                    e.apply(&mut src, r2.w, r2.h);
                }
                let mut own_alpha = vec![0u8; n];
                for i in 0..n {
                    let x = (i % rect.w as usize) as i32;
                    let y = (i / rect.w as usize) as i32;
                    let so = ((y + margin) as usize * r2.w as usize + (x + margin) as usize) * 4;
                    let s = [src[so], src[so + 1], src[so + 2], src[so + 3]];
                    own_alpha[i] = s[3];
                    let mut k = layer.opacity.clamp(0.0, 1.0);
                    if layer.mask_enabled && let Some(m) = &layer.mask {
                        k *= m.at(rect.x + x, rect.y + y);
                    }
                    if layer.clip_below && let Some(ca) = &clip_alpha {
                        k *= ca[i] as f32 / 255.0;
                    }
                    if k <= 0.0 {
                        continue;
                    }
                    let o = i * 4;
                    let d = [acc[o], acc[o + 1], acc[o + 2], acc[o + 3]];
                    let r = blend::over(d, s, layer.blend, k);
                    acc[o..o + 4].copy_from_slice(&r);
                }
                if !layer.clip_below {
                    clip_alpha = Some(own_alpha);
                }
            }
        }
    }
    acc
}

/// The whole canvas at `frame`.
pub fn flatten(img: &Image, frame: usize) -> Vec<u8> {
    let mut cache = VectorCache::default();
    composite_rect(img, frame, img.bounds(), &mut cache)
}

/// Composite one layer alone (its own pixels, effects applied, opacity and blend
/// ignored) — what the layer-panel thumbnail and "export current layer" show.
pub fn layer_only(img: &Image, layer: usize, frame: usize) -> Vec<u8> {
    let Some(l) = img.layers.get(layer) else {
        return vec![0; img.w as usize * img.h as usize * 4];
    };
    let mut cache = VectorCache::default();
    let mut buf = l.render_rect(img.bounds(), frame, (img.w, img.h), img.mode.antialias(), &mut cache);
    for e in &l.effects {
        e.apply(&mut buf, img.w, img.h);
    }
    buf
}

/// Composite with the canvas repeated `n`×`n` — the tiled view (§6.4), which is
/// how you find out a texture repeats visibly before the wall does.
pub fn tiled(img: &Image, frame: usize, n: u32) -> (Vec<u8>, u32, u32) {
    let n = n.clamp(1, 8);
    let one = flatten(img, frame);
    let (w, h) = (img.w as usize, img.h as usize);
    let (tw, th) = (w * n as usize, h * n as usize);
    let mut out = vec![0u8; tw * th * 4];
    for y in 0..th {
        let sy = y % h;
        for x in 0..tw {
            let sx = x % w;
            let so = (sy * w + sx) * 4;
            let o = (y * tw + x) * 4;
            out[o..o + 4].copy_from_slice(&one[so..so + 4]);
        }
    }
    (out, tw as u32, th as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adjust::Adjustment;
    use crate::doc::{Layer, Mode};
    use crate::effect::Effect;
    use crate::select::Mask;
    use crate::Blend;

    fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * w + x) * 4) as usize;
        [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
    }

    fn two_layer_doc() -> Image {
        let mut img = Image::new(16, 16, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().fill([0, 0, 255, 255]);
        img.add_raster_layer();
        img.layers[1]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(4, 4, 8, 8), |_, _, p| *p = [255, 0, 0, 255]);
        img
    }

    #[test]
    fn stack_order_is_bottom_first() {
        let img = two_layer_doc();
        let flat = flatten(&img, 0);
        assert_eq!(px(&flat, 16, 0, 0), [0, 0, 255, 255], "background shows through");
        assert_eq!(px(&flat, 16, 6, 6), [255, 0, 0, 255], "top layer wins");
    }

    /// The property the whole dirty-rect scheme rests on.
    #[test]
    fn a_sub_rect_matches_the_full_render() {
        let mut img = two_layer_doc();
        img.layers[1].opacity = 0.6;
        img.layers[1].blend = Blend::Multiply;
        img.add_layer(Layer::adjust(Adjustment::Hsl { hue: 40.0, sat: 0.2, light: 0.1 }));
        let full = flatten(&img, 0);
        let mut cache = VectorCache::default();
        let sub = composite_rect(&img, 0, Rect::new(4, 4, 6, 6), &mut cache);
        for y in 0..6u32 {
            for x in 0..6u32 {
                assert_eq!(
                    px(&sub, 6, x, y),
                    px(&full, 16, x + 4, y + 4),
                    "mismatch at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn effects_that_reach_outside_still_match_a_sub_rect() {
        let mut img = two_layer_doc();
        img.layers[1].effects.push(Effect::Outline {
            color: [0, 255, 0, 255],
            width: 2,
            outside: true,
        });
        let full = flatten(&img, 0);
        let mut cache = VectorCache::default();
        // A rect that touches only the outline, not the shape that casts it.
        let sub = composite_rect(&img, 0, Rect::new(2, 6, 2, 2), &mut cache);
        assert_eq!(px(&sub, 2, 0, 0), px(&full, 16, 2, 6));
        assert_eq!(px(&sub, 2, 0, 0)[1], 255, "the outline must be present in the sub-rect");
    }

    #[test]
    fn hidden_layers_and_zero_opacity_contribute_nothing() {
        let mut img = two_layer_doc();
        img.layers[1].visible = false;
        assert_eq!(px(&flatten(&img, 0), 16, 6, 6), [0, 0, 255, 255]);
        img.layers[1].visible = true;
        img.layers[1].opacity = 0.0;
        assert_eq!(px(&flatten(&img, 0), 16, 6, 6), [0, 0, 255, 255]);
    }

    #[test]
    fn masks_gate_a_layer() {
        let mut img = two_layer_doc();
        let mut m = Mask::new(16, 16, 0);
        for y in 0..8 {
            for x in 0..16 {
                m.set(x, y, 255);
            }
        }
        img.layers[1].mask = Some(m);
        let flat = flatten(&img, 0);
        assert_eq!(px(&flat, 16, 6, 5), [255, 0, 0, 255], "inside the mask");
        assert_eq!(px(&flat, 16, 6, 10), [0, 0, 255, 255], "outside it");
        img.layers[1].mask_enabled = false;
        assert_eq!(px(&flatten(&img, 0), 16, 6, 10), [255, 0, 0, 255]);
    }

    #[test]
    fn clip_below_confines_a_layer_to_the_one_under_it() {
        let mut img = Image::new(16, 16, Mode::Pixel);
        // Bottom: a small opaque square. Middle: full-canvas colour, clipped to it.
        img.layers[0]
            .grid_mut(0)
            .unwrap()
            .edit_rect(Rect::new(4, 4, 4, 4), |_, _, p| *p = [255, 255, 255, 255]);
        img.add_raster_layer();
        img.layers[1].grid_mut(0).unwrap().fill([255, 0, 0, 255]);
        img.layers[1].clip_below = true;
        let flat = flatten(&img, 0);
        assert_eq!(px(&flat, 16, 5, 5), [255, 0, 0, 255], "inside the base");
        assert_eq!(px(&flat, 16, 12, 12), [0, 0, 0, 0], "clipped away outside it");
    }

    #[test]
    fn adjustment_layers_affect_everything_beneath() {
        let mut img = two_layer_doc();
        img.add_layer(Layer::adjust(Adjustment::Invert));
        let flat = flatten(&img, 0);
        assert_eq!(px(&flat, 16, 0, 0), [255, 255, 0, 255], "blue inverted");
        assert_eq!(px(&flat, 16, 6, 6), [0, 255, 255, 255], "red inverted");
    }

    #[test]
    fn adjustment_opacity_blends_the_effect() {
        let mut img = Image::new(4, 4, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().fill([0, 0, 0, 255]);
        let mut adj = Layer::adjust(Adjustment::Invert);
        adj.opacity = 0.5;
        img.add_layer(adj);
        let flat = flatten(&img, 0);
        assert!((px(&flat, 4, 1, 1)[0] as i32 - 128).abs() <= 2);
    }

    #[test]
    fn full_canvas_flag_reports_floyd_steinberg() {
        let mut img = Image::new(8, 8, Mode::Pixel);
        assert!(!needs_full_canvas(&img));
        img.add_layer(Layer::adjust(Adjustment::Quantize {
            palette: crate::Palette::new("x"),
            dither: crate::adjust::Dither::FloydSteinberg,
            amount: 1.0,
        }));
        assert!(needs_full_canvas(&img));
    }

    #[test]
    fn vector_layers_composite_and_cache() {
        let mut img = Image::new(32, 32, Mode::Vector);
        let mut l = Layer::vector("shape");
        l.kind = LayerKind::Vector { paths: vec![crate::VPath::rect(8.0, 8.0, 16.0, 16.0)] };
        img.add_layer(l);
        let flat = flatten(&img, 0);
        assert_eq!(px(&flat, 32, 16, 16)[3], 255);
        assert_eq!(px(&flat, 32, 2, 2)[3], 0);
        // Second render comes from the cache and matches byte-for-byte.
        assert_eq!(flatten(&img, 0), flat);
    }

    #[test]
    fn layer_offset_moves_content_without_losing_it() {
        let mut img = two_layer_doc();
        img.layers[1].offset = (-100, 0); // dragged fully off-canvas
        let flat = flatten(&img, 0);
        assert_eq!(px(&flat, 16, 6, 6), [0, 0, 255, 255], "nothing visible");
        img.layers[1].offset = (0, 0);
        let flat = flatten(&img, 0);
        assert_eq!(px(&flat, 16, 6, 6), [255, 0, 0, 255], "and it comes back intact");
    }

    #[test]
    fn tiled_view_repeats_the_canvas() {
        let img = two_layer_doc();
        let (buf, w, h) = tiled(&img, 0, 3);
        assert_eq!((w, h), (48, 48));
        assert_eq!(px(&buf, 48, 6, 6), px(&buf, 48, 22, 22));
    }

    #[test]
    fn frames_pick_the_right_grid() {
        let mut img = Image::new(4, 4, Mode::Pixel);
        img.set_frames(2);
        img.set_layer_animated(0, true);
        img.layers[0].grid_mut(0).unwrap().fill([255, 0, 0, 255]);
        img.layers[0].grid_mut(1).unwrap().fill([0, 255, 0, 255]);
        assert_eq!(px(&flatten(&img, 0), 4, 1, 1), [255, 0, 0, 255]);
        assert_eq!(px(&flatten(&img, 1), 4, 1, 1), [0, 255, 0, 255]);
    }
}
