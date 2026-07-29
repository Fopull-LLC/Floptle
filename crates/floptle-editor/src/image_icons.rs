//! Tool icons for the 🖼 Image tab, **drawn** rather than typed.
//!
//! The tool strip is the most-looked-at surface in the tab and it's keyed by
//! letters, not labels — so an icon that reads wrong is a tool nobody can find.
//! Font glyphs couldn't carry it: the bundled stack has no pencil, brush,
//! eraser, eyedropper or pen (✎ 🖌 ⌫ ⌾ ✒ are all tofu — see `icons.rs`), and the
//! geometric shapes that *do* exist are a poor mime of a paint program.
//!
//! So each icon is a few primitives in a unit box. They cost nothing, they can
//! never be a tofu square, and they scale with the button.

use egui::{Color32, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2};

use crate::image_edit::ImgTool;

/// A segment in unit space: `((x0, y0), (x1, y1))`.
type Seg = ((f32, f32), (f32, f32));

/// Draw `tool`'s icon inside `r`, in `col`.
pub(crate) fn draw_tool_icon(p: &Painter, r: Rect, tool: ImgTool, col: Color32) {
    let u = |x: f32, y: f32| Pos2::new(r.left() + x * r.width(), r.top() + y * r.height());
    let w = (r.width() * 0.09).clamp(1.2, 2.0);
    let s = Stroke::new(w, col);
    let thin = Stroke::new((w * 0.75).max(1.0), col);
    let line = |a: (f32, f32), b: (f32, f32)| p.line_segment([u(a.0, a.1), u(b.0, b.1)], s);
    let hairline = |a: (f32, f32), b: (f32, f32)| p.line_segment([u(a.0, a.1), u(b.0, b.1)], thin);
    let tri = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        p.add(egui::Shape::convex_polygon(
            vec![u(a.0, a.1), u(b.0, b.1), u(c.0, c.1)],
            col,
            Stroke::NONE,
        ));
    };
    let quad = |a: (f32, f32), b: (f32, f32), c: (f32, f32), d: (f32, f32)| {
        p.add(egui::Shape::convex_polygon(
            vec![u(a.0, a.1), u(b.0, b.1), u(c.0, c.1), u(d.0, d.1)],
            col,
            Stroke::NONE,
        ));
    };
    // A quad in a dimmer shade — a ferrule, a worn face, a lit side.
    let shade = |a: (f32, f32), b: (f32, f32), c: (f32, f32), d: (f32, f32), k: f32| {
        p.add(egui::Shape::convex_polygon(
            vec![u(a.0, a.1), u(b.0, b.1), u(c.0, c.1), u(d.0, d.1)],
            col.gamma_multiply(k),
            Stroke::NONE,
        ));
    };
    let dashes = |pts: &[Seg]| {
        for (a, b) in pts {
            p.line_segment([u(a.0, a.1), u(b.0, b.1)], thin);
        }
    };

    match tool {
        // A pencil: a shaft, a sharpened nib, and a shaded ferrule at the butt.
        // (An earlier version drew the ferrule as a crossing LINE — at 18 px it
        // read as an X. A perpendicular filled band reads as a ferrule; that is
        // the difference between a pencil and a kitchen knife.)
        ImgTool::Pencil => {
            quad((0.26, 0.66), (0.60, 0.32), (0.72, 0.44), (0.38, 0.78));
            tri((0.26, 0.66), (0.38, 0.78), (0.12, 0.92));
            shade((0.60, 0.32), (0.72, 0.44), (0.84, 0.32), (0.72, 0.20), 0.5);
        }
        // A brush: a handle, a shaded ferrule, and a WEDGE of bristles. The
        // round-headed version of this was indistinguishable from the
        // eyedropper two rows down in the strip.
        ImgTool::Brush => {
            quad((0.10, 0.90), (0.44, 0.56), (0.54, 0.66), (0.20, 1.0));
            shade((0.44, 0.56), (0.54, 0.66), (0.66, 0.54), (0.56, 0.44), 0.55);
            tri((0.52, 0.40), (0.68, 0.56), (0.94, 0.08));
            hairline((0.62, 0.34), (0.86, 0.14));
        }
        // An eraser: a block seen at an angle, its worn face shaded.
        ImgTool::Eraser => {
            quad((0.12, 0.62), (0.46, 0.28), (0.78, 0.28), (0.44, 0.62));
            quad((0.12, 0.62), (0.44, 0.62), (0.44, 0.84), (0.12, 0.84));
            p.add(egui::Shape::convex_polygon(
                vec![u(0.44, 0.62), u(0.78, 0.28), u(0.78, 0.5), u(0.44, 0.84)],
                col.gamma_multiply(0.55),
                Stroke::NONE,
            ));
        }
        // A bucket: a tipped pail with a handle, pouring.
        ImgTool::Bucket => {
            quad((0.18, 0.2), (0.62, 0.14), (0.7, 0.56), (0.3, 0.62));
            let c = u(0.42, 0.18);
            let rad = r.width() * 0.22;
            let arc: Vec<Pos2> = (0..=8)
                .map(|i| {
                    let a = std::f32::consts::PI + i as f32 * std::f32::consts::PI / 8.0;
                    Pos2::new(c.x + rad * a.cos(), c.y + rad * a.sin() * 0.8)
                })
                .collect();
            p.add(egui::Shape::line(arc, thin));
            // The pour.
            tri((0.62, 0.6), (0.78, 0.62), (0.72, 0.8));
            p.circle_filled(u(0.74, 0.86), r.width() * 0.08, col);
        }
        // A gradient: bands from solid to nothing.
        ImgTool::Gradient => {
            p.rect_stroke(
                Rect::from_min_max(u(0.14, 0.18), u(0.86, 0.82)),
                1.0,
                thin,
                StrokeKind::Inside,
            );
            for i in 0..5 {
                let t = i as f32 / 4.0;
                let y0 = 0.2 + t * 0.6 * 0.98;
                p.rect_filled(
                    Rect::from_min_max(u(0.16, y0), u(0.84, y0 + 0.12)),
                    0.0,
                    col.gamma_multiply(1.0 - t),
                );
            }
        }
        ImgTool::Line => {
            line((0.18, 0.82), (0.82, 0.18));
            p.circle_filled(u(0.18, 0.82), w, col);
            p.circle_filled(u(0.82, 0.18), w, col);
        }
        ImgTool::Rectangle => {
            p.rect_stroke(Rect::from_min_max(u(0.14, 0.22), u(0.86, 0.78)), 1.0, s, StrokeKind::Inside);
        }
        ImgTool::Ellipse => {
            p.circle_stroke(r.center(), r.width() * 0.36, s);
        }
        // Marquees are the same shapes, dashed.
        ImgTool::SelectRect => {
            dashes(&[
                ((0.14, 0.22), (0.36, 0.22)),
                ((0.48, 0.22), (0.7, 0.22)),
                ((0.78, 0.22), (0.86, 0.22)),
                ((0.86, 0.3), (0.86, 0.52)),
                ((0.86, 0.62), (0.86, 0.78)),
                ((0.78, 0.78), (0.56, 0.78)),
                ((0.44, 0.78), (0.22, 0.78)),
                ((0.14, 0.78), (0.14, 0.62)),
                ((0.14, 0.52), (0.14, 0.3)),
            ]);
        }
        ImgTool::SelectEllipse => {
            let c = r.center();
            let rad = r.width() * 0.36;
            for i in 0..10 {
                let a0 = i as f32 * std::f32::consts::TAU / 10.0;
                let a1 = a0 + std::f32::consts::TAU / 18.0;
                p.line_segment(
                    [
                        Pos2::new(c.x + rad * a0.cos(), c.y + rad * a0.sin()),
                        Pos2::new(c.x + rad * a1.cos(), c.y + rad * a1.sin()),
                    ],
                    thin,
                );
            }
        }
        // A lasso: a WIDE rope loop with the ends crossed and a tail hanging
        // off it. (A round loop with a straight tail is a magnifying glass.)
        ImgTool::Lasso => {
            let c = u(0.5, 0.36);
            let (rx, ry) = (r.width() * 0.34, r.height() * 0.2);
            let mut pts = Vec::new();
            for i in 0..=18 {
                let a = 0.55 + i as f32 * (std::f32::consts::TAU * 0.86) / 18.0;
                pts.push(Pos2::new(c.x + rx * a.cos(), c.y + ry * a.sin()));
            }
            p.add(egui::Shape::line(pts, thin));
            // The knot where the rope crosses itself, and the tail below it.
            hairline((0.38, 0.5), (0.56, 0.56));
            hairline((0.5, 0.54), (0.62, 0.92));
        }
        // A wand: a stick with a spark.
        ImgTool::Wand => {
            line((0.16, 0.86), (0.6, 0.42));
            let c = u(0.72, 0.3);
            let d = r.width() * 0.2;
            p.line_segment([Pos2::new(c.x - d, c.y), Pos2::new(c.x + d, c.y)], thin);
            p.line_segment([Pos2::new(c.x, c.y - d), Pos2::new(c.x, c.y + d)], thin);
            let e = d * 0.62;
            p.line_segment([Pos2::new(c.x - e, c.y - e), Pos2::new(c.x + e, c.y + e)], thin);
            p.line_segment([Pos2::new(c.x - e, c.y + e), Pos2::new(c.x + e, c.y - e)], thin);
        }
        // Move: a four-way arrow.
        ImgTool::Move => {
            line((0.5, 0.16), (0.5, 0.84));
            line((0.16, 0.5), (0.84, 0.5));
            tri((0.5, 0.08), (0.4, 0.24), (0.6, 0.24));
            tri((0.5, 0.92), (0.4, 0.76), (0.6, 0.76));
            tri((0.08, 0.5), (0.24, 0.4), (0.24, 0.6));
            tri((0.92, 0.5), (0.76, 0.4), (0.76, 0.6));
        }
        // An eyedropper: a squeeze bulb, a thin tapered shaft, a drop leaving
        // the tip. Mirrored against the brush (bulb top-LEFT, tip bottom-right)
        // so the two never read as the same object in the strip.
        ImgTool::Eyedropper => {
            p.circle_filled(u(0.25, 0.23), r.width() * 0.18, col);
            shade((0.22, 0.38), (0.36, 0.22), (0.48, 0.32), (0.34, 0.48), 0.6);
            tri((0.3, 0.44), (0.46, 0.3), (0.88, 0.88));
        }
        // Reshape: a curve with grabbable nodes.
        ImgTool::Reshape => {
            let pts: Vec<Pos2> = (0..=12)
                .map(|i| {
                    let t = i as f32 / 12.0;
                    u(0.14 + t * 0.72, 0.72 - (t * std::f32::consts::PI).sin() * 0.42)
                })
                .collect();
            p.add(egui::Shape::line(pts, thin));
            for at in [(0.14, 0.72), (0.86, 0.72)] {
                p.rect_filled(Rect::from_center_size(u(at.0, at.1), Vec2::splat(r.width() * 0.17)), 1.0, col);
            }
            p.circle_filled(u(0.5, 0.3), r.width() * 0.1, col);
        }
        // A pen nib.
        ImgTool::Pen => {
            quad((0.3, 0.16), (0.7, 0.16), (0.62, 0.6), (0.38, 0.6));
            tri((0.38, 0.6), (0.62, 0.6), (0.5, 0.9));
            p.circle_filled(u(0.5, 0.5), r.width() * 0.07, Color32::from_black_alpha(160));
        }
        // Free transform: a box with corner handles.
        ImgTool::Transform => {
            p.rect_stroke(Rect::from_min_max(u(0.2, 0.26), u(0.8, 0.78)), 0.0, thin, StrokeKind::Inside);
            for c in [(0.2, 0.26), (0.8, 0.26), (0.2, 0.78), (0.8, 0.78)] {
                p.rect_filled(Rect::from_center_size(u(c.0, c.1), Vec2::splat(r.width() * 0.2)), 0.0, col);
            }
        }
        // Text: a serif "A" over a baseline.
        ImgTool::Text => {
            line((0.24, 0.78), (0.5, 0.2));
            line((0.5, 0.2), (0.76, 0.78));
            hairline((0.34, 0.58), (0.66, 0.58));
            hairline((0.14, 0.9), (0.86, 0.9));
        }
    }
}

/// Render every tool icon into one PNG, so they can be *looked at*.
///
/// egui has no headless raster backend, but it will happily tessellate a frame
/// into triangles — and the image kernel already has a scanline filler. Between
/// them that's a real visual harness for UI drawing, the same idea as
/// `flimg_probe` for the compositor.
#[cfg(test)]
pub(crate) fn render_icon_sheet(path: &std::path::Path, cell: u32, cols: u32) -> Vec<u8> {
    let ctx = crate::icons::test_context();
    // Feathering emits triangles whose outer vertices are fully TRANSPARENT —
    // with it on, an anti-aliased stroke tessellates into shapes this rasterizer
    // reads as invisible, and the sheet comes out blank while the test still
    // passes. Geometry, not gradients, is what we're inspecting here.
    ctx.tessellation_options_mut(|o| o.feathering = false);
    let rows = (ImgTool::ALL.len() as u32).div_ceil(cols);
    let (w, h) = (cell * cols, cell * rows);
    let out = ctx.run_ui(crate::icons::test_input(), |ui| {
        let p = ui.painter();
        for (i, t) in ImgTool::ALL.iter().enumerate() {
            let (cx, cy) = ((i as u32 % cols) * cell, (i as u32 / cols) * cell);
            let r = Rect::from_min_size(
                Pos2::new(cx as f32, cy as f32),
                Vec2::splat(cell as f32),
            )
            .shrink(cell as f32 * 0.22);
            draw_tool_icon(p, r, *t, Color32::WHITE);
        }
    });
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    // An OPAQUE dark ground, because the icons are drawn in white: on a
    // transparent background every viewer shows them against white paper, i.e.
    // shows nothing at all, and the sheet looks empty when it isn't.
    let mut px = vec![0u8; (w * h) as usize * 4];
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[28, 30, 34, 255]);
    }
    let mut cov = vec![0f32; (w * h) as usize];
    for prim in prims {
        let egui::epaint::Primitive::Mesh(mesh) = prim.primitive else { continue };
        for tri in mesh.indices.chunks_exact(3) {
            let v: Vec<_> = tri.iter().map(|&i| mesh.vertices[i as usize]).collect();
            let ring: Vec<(f32, f32)> = v.iter().map(|q| (q.pos.x, q.pos.y)).collect();
            cov.iter_mut().for_each(|c| *c = 0.0);
            floptle_image::vector::coverage(&[ring], w, h, false, &mut cov);
            // The most opaque corner wins: a triangle with one transparent
            // vertex is still ink.
            let c = v.iter().map(|q| q.color).max_by_key(|c| c.a()).unwrap_or(v[0].color);
            for (i, a) in cov.iter().enumerate() {
                let a = a.clamp(0.0, 1.0) * (c.a() as f32 / 255.0);
                if a <= 0.0 {
                    continue;
                }
                let o = i * 4;
                let src = [c.r(), c.g(), c.b(), floptle_image::u8c(a * 255.0)];
                let dst = [px[o], px[o + 1], px[o + 2], px[o + 3]];
                let r = floptle_image::blend::over(dst, src, floptle_image::Blend::Mix, 1.0);
                px[o..o + 4].copy_from_slice(&r);
            }
        }
    }
    let _ = floptle_image::io::save_png(path, &px, w, h);
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write the icon sheet somewhere you can look at it — and check that every
    /// cell actually has ink in it.
    ///
    /// The file-exists version of this test passed while the sheet was very
    /// nearly BLANK (feathered strokes tessellate to triangles with transparent
    /// corners, and the rasterizer was reading those as invisible). A harness
    /// you can't fail is not a harness, so the per-cell coverage is the
    /// assertion now.
    #[test]
    fn every_icon_has_ink_in_its_cell() {
        const CELL: u32 = 64;
        const COLS: u32 = 6;
        let out = std::env::temp_dir().join(format!("flimg-icons-{}.png", std::process::id()));
        let px = render_icon_sheet(&out, CELL, COLS);
        let w = CELL * COLS;
        let mut thin = Vec::new();
        for (i, t) in ImgTool::ALL.iter().enumerate() {
            let (cx, cy) = ((i as u32 % COLS) * CELL, (i as u32 / COLS) * CELL);
            let ink = (0..CELL)
                .flat_map(|y| (0..CELL).map(move |x| (x, y)))
                .filter(|(x, y)| {
                    // Ink = brighter than the ground it's drawn on.
                    let o = (((cy + y) * w + cx + x) * 4) as usize;
                    px.get(o).is_some_and(|r| *r > 90)
                })
                .count();
            if ink < 40 {
                thin.push(format!("{t:?}: {ink} texels"));
            }
        }
        assert!(thin.is_empty(), "icons that barely draw anything:\n  {}", thin.join("\n  "));
        println!("icon sheet: {}", out.display());
    }

    /// Every tool draws something, and nothing panics. (The match above has no
    /// fallback arm, so a new tool without an icon is a compile error — this
    /// covers the drawing itself.)
    #[test]
    fn every_tool_draws_an_icon() {
        let ctx = crate::icons::test_context();
        let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
            let p = ui.painter();
            for t in ImgTool::ALL {
                let before = p.clone();
                draw_tool_icon(&before, Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::splat(18.0)), t, Color32::WHITE);
            }
        });
    }
}
