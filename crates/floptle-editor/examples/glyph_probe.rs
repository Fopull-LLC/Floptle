//! Renders every icon glyph the editor uses to a PNG, through the editor's real
//! font stack, so a human can LOOK at the result.
//!
//! Why not just ask egui? Because `Fonts::has_glyph` is unreliable on this font
//! stack — it has reported glyphs missing that demonstrably render, and a
//! previous pass acted on it and swapped away working icons. The two sources of
//! truth that don't lie are the fonts' own character maps (which `icons.rs`
//! tests against) and a picture. This is the picture.
//!
//! ```sh
//! cargo run -p floptle-editor --example glyph_probe
//! ```
//!
//! Writes `target/glyph_probe.png`: one cell per glyph, drawn large beside its
//! codepoint. The first row is a **control** of glyphs known to be absent from
//! every bundled font — whatever they look like is what "broken" looks like, so
//! there's no guessing about the rest.

use egui::{Color32, FontFamily, FontId};

/// Codepoints no font in the editor's stack maps. They are here to be drawn
/// wrong on purpose: the eye needs a reference for what a missing glyph looks
/// like before it can judge the cells below.
const CONTROL: &str = "⛰⬢🥊🪐🦴🧹";

fn main() {
    let scanned = scan_sources();
    let entries: Vec<(String, char)> = CONTROL
        .chars()
        .map(|c| ("CONTROL".to_owned(), c))
        .chain(scanned.iter().map(|c| (format!("U+{:04X}", *c as u32), *c)))
        .collect();

    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    if let Some(fam) = fonts.families.get_mut(&FontFamily::Proportional) {
        fam.push("Hack".into());
    }
    ctx.set_fonts(fonts);
    // Fonts don't exist until a frame has run.
    let _ = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        },
        |_| {},
    );

    let cols = 10usize;
    let cell = (128usize, 68usize);
    let rows = entries.len().div_ceil(cols);
    let (w, h) = (cols * cell.0, rows * cell.1);
    let mut buf = vec![0u8; w * h * 4];

    // A checkerboard of cells, so an empty glyph reads as an empty CELL rather
    // than an ambiguous patch of background.
    for y in 0..h {
        for x in 0..w {
            let (cx, cy) = (x / cell.0, y / cell.1);
            let shade = if (cx + cy) % 2 == 0 { 26u8 } else { 38 };
            let i = (y * w + x) * 4;
            buf[i..i + 4].copy_from_slice(&[shade, shade, shade, 255]);
        }
    }

    for (n, (label, glyph)) in entries.iter().enumerate() {
        let (cx, cy) = ((n % cols) * cell.0, (n / cols) * cell.1);
        let control = label == "CONTROL";
        let tint = if control {
            Color32::from_rgb(255, 130, 130)
        } else {
            Color32::WHITE
        };
        draw(&ctx, &mut buf, w, h, &glyph.to_string(), 40.0, cx + 12, cy + 6, tint);
        draw(
            &ctx,
            &mut buf,
            w,
            h,
            label,
            12.0,
            cx + 60,
            cy + 26,
            Color32::from_rgb(150, 170, 200),
        );
    }

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/glyph_probe.png");
    image::save_buffer(path, &buf, w as u32, h as u32, image::ColorType::Rgba8).unwrap();
    println!("wrote {path} — {} glyphs, {w}x{h}", entries.len());
}

/// Every non-ASCII character in a string literal under `src/`. Deliberately
/// re-derived from the sources rather than from a curated list: an icon nobody
/// remembered to register is exactly the one that ships broken.
fn scan_sources() -> Vec<char> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut seen: Vec<char> = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                let src = std::fs::read_to_string(&path).unwrap_or_default();
                for c in string_literal_chars(&src) {
                    if !c.is_ascii() && !seen.contains(&c) {
                        seen.push(c);
                    }
                }
            }
        }
    }
    seen.sort_unstable();
    seen
}

/// Characters inside double-quoted string literals — skipping comments, where a
/// glyph is prose rather than something a user will ever see.
fn string_literal_chars(src: &str) -> Vec<char> {
    #[derive(PartialEq)]
    enum S {
        Code,
        Line,
        Block,
        Str,
    }
    let mut state = S::Code;
    let mut out = Vec::new();
    let mut escaped = false;
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied().unwrap_or('\0');
        match state {
            S::Code => match (c, next) {
                ('/', '/') => {
                    state = S::Line;
                    i += 1;
                }
                ('/', '*') => {
                    state = S::Block;
                    i += 1;
                }
                ('"', _) => state = S::Str,
                _ => {}
            },
            S::Line if c == '\n' => state = S::Code,
            S::Block if c == '*' && next == '/' => {
                state = S::Code;
                i += 1;
            }
            S::Str => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    state = S::Code;
                } else {
                    out.push(c);
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Composite a laid-out string out of the font atlas into `buf`.
#[allow(clippy::too_many_arguments)]
fn draw(
    ctx: &egui::Context,
    buf: &mut [u8],
    w: usize,
    h: usize,
    text: &str,
    size: f32,
    ox: usize,
    oy: usize,
    color: Color32,
) {
    let galley = ctx.fonts_mut(|f| {
        f.layout_no_wrap(text.to_owned(), FontId::proportional(size), Color32::WHITE)
    });
    let atlas = ctx.fonts(|f| f.image());
    let (aw, ah) = (atlas.width(), atlas.height());

    for row in &galley.rows {
        for g in &row.row.glyphs {
            let uv = g.uv_rect;
            if uv.size.x <= 0.0 || uv.size.y <= 0.0 {
                continue; // no raster at all — a blank or unmapped glyph
            }
            let gx = ox as f32 + row.pos.x + g.pos.x + uv.offset.x;
            let gy = oy as f32 + row.pos.y + g.pos.y + uv.offset.y;
            let (sw, sh) = (
                (uv.max[0] - uv.min[0]) as usize,
                (uv.max[1] - uv.min[1]) as usize,
            );
            for sy in 0..sh {
                for sx in 0..sw {
                    let (ax, ay) = (uv.min[0] as usize + sx, uv.min[1] as usize + sy);
                    if ax >= aw || ay >= ah {
                        continue;
                    }
                    let a = atlas.pixels[ay * aw + ax].a() as f32 / 255.0;
                    if a <= 0.0 {
                        continue;
                    }
                    let (dx, dy) = (gx as isize + sx as isize, gy as isize + sy as isize);
                    if dx < 0 || dy < 0 || dx as usize >= w || dy as usize >= h {
                        continue;
                    }
                    let i = (dy as usize * w + dx as usize) * 4;
                    for c in 0..3 {
                        let src = [color.r(), color.g(), color.b()][c] as f32;
                        let dst = buf[i + c] as f32;
                        buf[i + c] = (dst + (src - dst) * a) as u8;
                    }
                }
            }
        }
    }
}
