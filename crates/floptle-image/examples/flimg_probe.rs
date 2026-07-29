//! Visual probe for the image kernel — the house pattern for anything whose
//! bugs are *visual* rather than logical (`blob_probe`, `perf_probe`).
//!
//! Builds a document that exercises the parts a unit test can only assert
//! numerically — the path rasterizer's anti-aliasing, stroke joins, blend
//! modes, layer effects, palette quantization and the tiled view — and writes
//! PNGs you can actually look at.
//!
//! `cargo run -p floptle-image --example flimg_probe -- <out-dir>`

use floptle_image::adjust::{Adjustment, Dither};
use floptle_image::doc::{Image, Layer, LayerKind, Mode};
use floptle_image::effect::Effect;
use floptle_image::vector::{Cap, Join, Paint, Stroke, VNode, VPath};
use floptle_image::{composite, io, Blend, Rect, TileGrid};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir).expect("out dir");

    write(dir, "probe_vector.png", &vector_doc());
    write(dir, "probe_pixel.png", &pixel_doc());
    write(dir, "probe_blends.png", &blend_doc());

    // The tiled view: the same document, repeated, so a seam is obvious.
    let doc = tiling_doc();
    write(dir, "probe_tile_one.png", &doc);
    let (px, w, h) = composite::tiled(&doc, 0, 3);
    io::save_png(&dir.join("probe_tile_3x3.png"), &px, w, h).expect("write");
    println!("wrote probes to {}", dir.display());
}

fn write(dir: &std::path::Path, name: &str, img: &Image) {
    let px = composite::flatten(img, 0);
    io::save_png(&dir.join(name), &px, img.w, img.h).expect("write");
}

/// Fills, strokes, joins, caps, gradients and curve nodes.
fn vector_doc() -> Image {
    let mut img = Image::new(320, 200, Mode::Vector);
    img.layers[0].grid_mut(0).unwrap().fill([28, 30, 38, 255]);

    let mut paths = Vec::new();
    // A gradient-filled rounded blob from four auto-handled curve nodes.
    let mut blob = VPath::ellipse(70.0, 60.0, 50.0, 38.0);
    blob.fill = Some(Paint::Linear {
        a: [20.0, 20.0],
        b: [120.0, 100.0],
        stops: vec![(0.0, [255, 120, 90, 255]), (1.0, [90, 60, 220, 255])],
    });
    blob.stroke = Some(Stroke { color: [255, 255, 255, 255], width: 2.0, cap: Cap::Round, join: Join::Round });
    paths.push(blob);

    // A star, testing miter joins and self-overlapping strokes.
    let mut star = VPath { nodes: Vec::new(), closed: true, fill: None, stroke: None, even_odd: true };
    for i in 0..10 {
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::TAU / 10.0;
        let r = if i % 2 == 0 { 45.0 } else { 18.0 };
        star.nodes.push(VNode::corner(210.0 + r * a.cos(), 60.0 + r * a.sin()));
    }
    star.fill = Some(Paint::Solid([250, 210, 90, 255]));
    star.stroke = Some(Stroke { color: [40, 30, 10, 255], width: 3.0, cap: Cap::Butt, join: Join::Miter });
    paths.push(star);

    // An open, thick, round-capped squiggle.
    let mut wave = VPath {
        nodes: (0..6)
            .map(|i| {
                let x = 24.0 + i as f32 * 55.0;
                let y = 150.0 + if i % 2 == 0 { -22.0 } else { 22.0 };
                VNode::curve(x, y)
            })
            .collect(),
        closed: false,
        fill: None,
        stroke: Some(Stroke { color: [120, 230, 200, 255], width: 9.0, cap: Cap::Round, join: Join::Round }),
        even_odd: false,
    };
    wave.nodes[0].kind = floptle_image::NodeKind::Curve;

    paths.push(wave);
    let mut l = Layer::vector("shapes");
    l.kind = LayerKind::Vector { paths };
    img.add_layer(l);
    img
}

/// Pixel art: a hard-edged sprite with an outline effect and a palette quantize.
fn pixel_doc() -> Image {
    let mut img = Image::new(64, 64, Mode::Pixel);
    let g = img.layers[0].grid_mut(0).unwrap();
    // A smooth radial ramp — something a palette can visibly bite into.
    for y in 0..64i64 {
        for x in 0..64i64 {
            let d = (((x - 32) * (x - 32) + (y - 32) * (y - 32)) as f32).sqrt() / 30.0;
            if d <= 1.0 {
                let v = (1.0 - d) * 255.0;
                g.set(x, y, [floptle_image::u8c(v), floptle_image::u8c(v * 0.5), 90, 255]);
            }
        }
    }
    img.layers[0].effects.push(Effect::Outline { color: [16, 16, 24, 255], width: 1, outside: true });
    img.layers[0].effects.push(Effect::DropShadow {
        color: [0, 0, 0, 255],
        dx: 2.0,
        dy: 3.0,
        blur: 1.5,
        opacity: 0.55,
    });
    let pal = floptle_image::palette::builtin()[0].clone(); // Sweetie 16
    img.palette = Some(pal.clone());
    img.add_layer(Layer::adjust(Adjustment::Quantize {
        palette: pal,
        dither: Dither::Ordered,
        amount: 1.0,
    }));
    img
}

/// Every blend mode as a swatch over a gradient backdrop.
fn blend_doc() -> Image {
    let cols = 6u32;
    let cell = 48u32;
    let rows = Blend::ALL.len() as u32 / cols + 1;
    let mut img = Image::new(cols * cell, rows * cell, Mode::Painterly);
    // Backdrop: a diagonal ramp, so every mode has something to interact with.
    let g = img.layers[0].grid_mut(0).unwrap();
    let (w, h) = (img.w as i64, img.h as i64);
    for y in 0..h {
        for x in 0..w {
            let t = (x as f32 / w as f32 + y as f32 / h as f32) * 0.5;
            g.set(x, y, [floptle_image::u8c(t * 255.0), 90, floptle_image::u8c(255.0 - t * 255.0), 255]);
        }
    }
    for (i, m) in Blend::ALL.iter().enumerate() {
        let (cx, cy) = ((i as u32 % cols) * cell, (i as u32 / cols) * cell);
        let mut l = Layer::raster(m.label(), img.w, img.h);
        l.blend = *m;
        if let Some(g) = l.grid_mut(0) {
            g.edit_rect(Rect::new(cx as i32 + 6, cy as i32 + 6, cell - 12, cell - 12), |_, _, p| {
                *p = [200, 170, 80, 255]
            });
        }
        img.add_layer(l);
    }
    img
}

/// A tileable texture: wrap-brushed noise plus a seam-crossing stroke.
fn tiling_doc() -> Image {
    let mut img = Image::new(96, 96, Mode::Painterly);
    img.tiling = true;
    let mut base = TileGrid::filled(96, 96, [70, 92, 64, 255]);
    // Deterministic speckle.
    for i in 0..1400u32 {
        let x = (i.wrapping_mul(2654435761) % 96) as i64;
        let y = (i.wrapping_mul(40503) % 96) as i64;
        let v = (i % 40) as i32 - 20;
        let p = base.get(x, y);
        base.set(
            x,
            y,
            [
                floptle_image::u8c(p[0] as f32 + v as f32),
                floptle_image::u8c(p[1] as f32 + v as f32),
                floptle_image::u8c(p[2] as f32 + v as f32),
                255,
            ],
        );
    }
    *img.layers[0].grid_mut(0).unwrap() = base;

    // A wrap-around stroke straight through the right edge.
    let brush = floptle_image::Brush {
        radius: 5.0,
        hardness: 0.4,
        flow: 1.0,
        spacing: 0.1,
        pixel_perfect: false,
        ..Default::default()
    };
    let ctx = floptle_image::brush::DabCtx {
        sel: None,
        origin: (0, 0),
        canvas: (96, 96),
        wrap: true,
        clone_offset: (0.0, 0.0),
    };
    let mut st = floptle_image::StrokeState::default();
    let g = img.layers[0].grid_mut(0).unwrap();
    st.begin(60.0, 40.0, 96, 96);
    floptle_image::brush::stamp(g, &brush, 60.0, 40.0, [180, 150, 90, 255], &ctx, &mut st);
    floptle_image::brush::stroke_to(g, &brush, 130.0, 55.0, [180, 150, 90, 255], &ctx, &mut st);
    img
}
