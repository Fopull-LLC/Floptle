//! Headless probe for the UI paint box (docs/ui-system-2-proposal.md §A).
//!
//! Renders one PNG per feature through the real production path — `DrawList` →
//! `Ui::pack` → `Ui::draw` against the actual `ui.wgsl` — and then ASSERTS on
//! the pixels. Both halves matter: the image is for looking at, the assertions
//! are because a harness that only writes a file will happily write a black
//! one and report success.
//!
//! Run: cargo run --release -p floptle-render --example ui_paint_probe

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{
    Blend, Corners, DrawList, GlowSpec, Gradient, GradientKind, GrainSpec, Quad, QuadKind,
    ShadowSpec, Sides, TextRun, TextShadow, TextStroke, Xform,
};

const W: u32 = 480;
const H: u32 = 320;

/// The mid-grey backdrop every probe draws over, so shadows have something to
/// darken and glows have something to lift off.
fn bg() -> Quad {
    Quad {
        rect: [0.0, 0.0, W as f32, H as f32],
        color: [0.35, 0.36, 0.40, 1.0],
        ..Default::default()
    }
}

struct Probe<'a> {
    gpu: &'a Gpu,
    raster: &'a Raster,
    ui: &'a mut Ui,
}

impl Probe<'_> {
    /// Draw a list and read the result back as RGBA rows.
    fn render(&mut self, list: &DrawList, out: &str) -> Vec<[u8; 4]> {
        let tex = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("paint-probe"),
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.gpu.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let mut instances = Vec::new();
        let mut batches = Vec::new();
        self.ui.clear_backdrop();
        self.ui.pack(
            self.gpu,
            list,
            [0.0, 0.0],
            1.0,
            &mut |_| None,
            &|_| None,
            &mut |_, _| None,
            &mut instances,
            &mut batches,
        );
        self.ui.draw(
            self.gpu,
            &view,
            [W as f32, H as f32],
            &instances,
            &batches,
            self.raster,
        );
        let px = readback(self.gpu, &tex);
        save_png(&px, out);
        px
    }
}

fn at(px: &[[u8; 4]], x: u32, y: u32) -> [u8; 4] {
    px[(y * W + x) as usize]
}

fn main() {
    let gpu = Gpu::headless(W, H);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);
    let mut p = Probe { gpu: &gpu, raster: &raster, ui: &mut ui };

    // ---- gradients ------------------------------------------------------
    // The headline: a panel that stops reading as a slab, with no shader.
    {
        let px = p.render(
            &DrawList {
            text_snap: 0.0,
                quads: vec![
                    bg(),
                    Quad {
                        rect: [40.0, 40.0, 400.0, 100.0],
                        color: [0.95, 0.80, 0.30, 1.0],
                        gradient: Some(Gradient {
                            kind: GradientKind::Linear,
                            to: [0.20, 0.10, 0.35, 1.0],
                            angle: 0.0, // left → right
                            ..Default::default()
                        }),
                        radius: Corners::all(12.0).0,
                        ..Default::default()
                    },
                    Quad {
                        rect: [40.0, 170.0, 180.0, 110.0],
                        color: [1.0, 1.0, 1.0, 1.0],
                        gradient: Some(Gradient {
                            kind: GradientKind::Radial,
                            to: [0.05, 0.05, 0.10, 1.0],
                            ..Default::default()
                        }),
                        radius: Corners::all(8.0).0,
                        ..Default::default()
                    },
                    Quad {
                        rect: [260.0, 170.0, 180.0, 110.0],
                        color: [1.0, 0.3, 0.2, 1.0],
                        gradient: Some(Gradient {
                            kind: GradientKind::Angular,
                            to: [0.2, 0.4, 1.0, 1.0],
                            ..Default::default()
                        }),
                        radius: Corners::all(8.0).0,
                        ..Default::default()
                    },
                ],
                texts: Vec::new(),
            },
            "ui_gradient_probe.png",
        );
        // Linear: warm on the left, cool on the right. Checks the sweep runs
        // the way `angle: 0` says it does, not the mirror of it.
        let left = at(&px, 60, 90);
        let right = at(&px, 420, 90);
        println!("linear gradient: left {left:?} right {right:?}");
        assert!(left[0] > right[0] + 60, "left should be much warmer: {left:?} {right:?}");
        // Hue actually crosses over, not just dims: the near stop is red-
        // dominant and the far stop is blue-dominant.
        assert!(left[0] > left[2], "near stop should be red-dominant: {left:?}");
        assert!(right[2] > right[0], "far stop should be blue-dominant: {right:?}");
        // Radial: bright at the centre, dark at the corner.
        let mid = at(&px, 130, 225);
        let corner = at(&px, 48, 178);
        println!("radial gradient: centre {mid:?} corner {corner:?}");
        assert!(mid[0] > corner[0] + 60, "radial should fall off outward");
        println!("gradients OK → ui_gradient_probe.png");
    }

    // ---- per-corner radius + per-side border ----------------------------
    // Fofighter's `Front Header` (square bottom corners) and `Front Rule`
    // (a bottom-only border) in one image.
    {
        let px = p.render(
            &DrawList {
            text_snap: 0.0,
                quads: vec![
                    bg(),
                    Quad {
                        rect: [60.0, 60.0, 360.0, 200.0],
                        color: [0.08, 0.09, 0.14, 1.0],
                        radius: [28.0, 28.0, 0.0, 0.0], // TL, TR square-bottom
                        border: [0.0, 0.0, 0.0, 6.0],   // bottom rule only
                        border_color: [1.0, 0.85, 0.35, 1.0],
                        ..Default::default()
                    },
                ],
                texts: Vec::new(),
            },
            "ui_corners_probe.png",
        );
        // Background is [89, 92, 102]; the panel fill is near-black; the rule
        // is gold. The top-left corner is rounded AWAY (background shows
        // through); the bottom-left is square, so the panel reaches the corner.
        let tl = at(&px, 64, 64);
        let bl_fill = at(&px, 64, 245); // inside the panel, above the rule
        println!("corners: top-left {tl:?} bottom-left fill {bl_fill:?}");
        assert!(tl[0] > 70, "top-left should be rounded away to background: {tl:?}");
        assert!(bl_fill[0] < 70, "bottom-left should be square panel fill: {bl_fill:?}");
        // The bottom edge carries the accent rule; the top edge has no border.
        let bottom_edge = at(&px, 240, 257);
        let top_edge = at(&px, 240, 63);
        println!("border: bottom {bottom_edge:?} top {top_edge:?}");
        assert!(
            bottom_edge[0] > 150 && bottom_edge[1] > 110,
            "bottom border should be the gold accent: {bottom_edge:?}"
        );
        assert!(top_edge[0] < 100, "top edge should have no border: {top_edge:?}");
        // And the rule reaches the square corner too — that is the whole
        // reason `Front Rule` can stop being its own node.
        let bl_rule = at(&px, 64, 257);
        assert!(bl_rule[0] > 150 && bl_rule[1] > 110, "the rule should reach the corner: {bl_rule:?}");
        println!("per-corner radius + per-side border OK → ui_corners_probe.png");
    }

    // ---- glow, inset shadow, grain --------------------------------------
    {
        let px = p.render(
            &DrawList {
            text_snap: 0.0,
                quads: vec![
                    bg(),
                    // Glow behind a small bright chip.
                    Quad {
                        rect: [40.0, 100.0, 120.0, 120.0],
                        color: [1.0, 0.75, 0.2, 0.9],
                        radius: [16.0; 4],
                        kind: QuadKind::Shadow,
                        feather: 30.0,
                        blend: Blend::Additive,
                        ..Default::default()
                    },
                    Quad {
                        rect: [60.0, 120.0, 80.0, 80.0],
                        color: [1.0, 0.9, 0.5, 1.0],
                        radius: [12.0; 4],
                        ..Default::default()
                    },
                    // A recessed well: light fill + inset shadow.
                    Quad {
                        rect: [200.0, 120.0, 100.0, 80.0],
                        color: [0.75, 0.77, 0.82, 1.0],
                        radius: [10.0; 4],
                        ..Default::default()
                    },
                    Quad {
                        rect: [200.0, 120.0, 100.0, 80.0],
                        color: [0.0, 0.0, 0.0, 0.9],
                        radius: [10.0; 4],
                        kind: QuadKind::InsetShadow,
                        feather: 10.0,
                        shadow_offset: [0.0, 3.0],
                        ..Default::default()
                    },
                    // Grain over a flat fill.
                    Quad {
                        rect: [340.0, 120.0, 100.0, 80.0],
                        color: [0.5, 0.5, 0.55, 1.0],
                        radius: [10.0; 4],
                        grain: Some(GrainSpec { amount: 0.35, scale: 2.0 }),
                        ..Default::default()
                    },
                ],
                texts: Vec::new(),
            },
            "ui_effects_probe.png",
        );
        // Glow: the area just outside the chip is brighter than plain bg.
        let halo = at(&px, 50, 160);
        let plain = at(&px, 20, 20);
        println!("glow: halo {halo:?} plain {plain:?}");
        assert!(halo[0] > plain[0] + 15, "glow should brighten outside the chip");
        // Inset: the top inside edge is darker than the middle of the well.
        let inset_edge = at(&px, 250, 126);
        let well_mid = at(&px, 250, 165);
        println!("inset: edge {inset_edge:?} middle {well_mid:?}");
        assert!(inset_edge[0] + 20 < well_mid[0], "inset shadow should darken the inner edge");
        // Grain: neighbouring pixels differ. A flat fill would be identical.
        let mut spread = 0i32;
        for x in 350..430 {
            let a = at(&px, x, 160)[0] as i32;
            let b = at(&px, x + 1, 160)[0] as i32;
            spread = spread.max((a - b).abs());
        }
        println!("grain: max neighbour delta {spread}");
        assert!(spread > 4, "grain should vary pixel to pixel, got {spread}");
        println!("glow + inset + grain OK → ui_effects_probe.png");
    }

    // ---- transform: rotate/scale about a pivot ---------------------------
    {
        let px = p.render(
            &DrawList {
            text_snap: 0.0,
                quads: vec![
                    bg(),
                    Quad {
                        rect: [190.0, 110.0, 100.0, 100.0],
                        color: [0.95, 0.35, 0.30, 1.0],
                        xform: Xform {
                            rotation: std::f32::consts::FRAC_PI_4,
                            scale: [1.0, 1.0],
                            pivot: [0.5, 0.5],
                        },
                        ..Default::default()
                    },
                ],
                texts: Vec::new(),
            },
            "ui_transform_probe.png",
        );
        // A 45°-rotated square: its corners now point at the edge midpoints.
        // The original corner position must be empty, the new tip must be red.
        let old_corner = at(&px, 195, 115);
        let new_tip = at(&px, 240, 122);
        println!("rotate: old corner {old_corner:?} new tip {new_tip:?}");
        assert!(old_corner[0] < 120, "the un-rotated corner should be background now");
        assert!(new_tip[0] > 150 && new_tip[1] < 120, "the rotated tip should be red");
        println!("transform OK → ui_transform_probe.png");
    }

    // ---- text: stroke, shadow, tracking ---------------------------------
    {
        let px = p.render(
            &DrawList {
            text_snap: 0.0,
                quads: vec![
                    // A bright background is the case plain text can't survive.
                    Quad {
                        rect: [0.0, 0.0, W as f32, H as f32],
                        color: [0.9, 0.88, 0.2, 1.0],
                        ..Default::default()
                    },
                ],
                texts: vec![
                    TextRun {
                        rect: [20.0, 40.0, 440.0, 60.0],
                        text: "OUTLINED".into(),
                        size: 44.0,
                        color: [1.0, 1.0, 1.0, 1.0],
                        stroke: Some(TextStroke { color: [0.0, 0.0, 0.0, 1.0], width: 2.5 }),
                        tracking: 6.0,
                        ..Default::default()
                    },
                    TextRun {
                        rect: [20.0, 140.0, 440.0, 60.0],
                        text: "shadowed".into(),
                        size: 40.0,
                        color: [1.0, 1.0, 1.0, 1.0],
                        shadow: Some(TextShadow {
                            color: [0.0, 0.0, 0.0, 0.9],
                            offset: [3.0, 3.0],
                        }),
                        ..Default::default()
                    },
                    TextRun {
                        rect: [20.0, 220.0, 440.0, 80.0],
                        text: "this line is long enough that it has to wrap somewhere".into(),
                        size: 20.0,
                        color: [0.1, 0.1, 0.1, 1.0],
                        wrap: true,
                        line_height: 1.2,
                        ..Default::default()
                    },
                ],
            },
            "ui_text_probe.png",
        );
        // Somewhere on the headline row there must be near-black pixels: that
        // is the outline, and without it white-on-yellow has none.
        let mut darkest = 255u8;
        for x in 20..460 {
            for y in 45..95 {
                darkest = darkest.min(at(&px, x, y)[0]);
            }
        }
        println!("text outline: darkest pixel on the headline row {darkest}");
        assert!(darkest < 60, "the stroke should put near-black around the glyphs, got {darkest}");
        // The wrapped paragraph must reach a second line — dark pixels well
        // below the first baseline.
        let mut second_line = false;
        for x in 20..460 {
            for y in 252..300 {
                if at(&px, x, y)[0] < 90 {
                    second_line = true;
                }
            }
        }
        assert!(second_line, "wrapped text should occupy a second line");
        println!("text stroke + shadow + wrap OK → ui_text_probe.png");
    }

    // ---- opacity cascade -------------------------------------------------
    // Not a shader feature — but it IS the change most likely to silently
    // regress, so it gets pixels too.
    {
        use floptle_ui::{ElementSpec, Node, ShapeSpec, Size};
        fn el(id: u32, spec: ElementSpec, children: Vec<Node>) -> Node {
            Node { id, spec, children }
        }
        let child = el(
            2,
            ElementSpec {
                size: [Size::Fixed(200.0), Size::Fixed(200.0)],
                shape: Some(ShapeSpec { fill: [1.0, 1.0, 1.0, 1.0], ..Default::default() }),
                ..Default::default()
            },
            vec![],
        );
        let root = el(
            1,
            ElementSpec {
                place: floptle_ui::Place::Free { pos: [140.0, 60.0] },
                size: [Size::Fixed(200.0), Size::Fixed(200.0)],
                opacity: 0.35,
                ..Default::default()
            },
            vec![child],
        );
        let placed = floptle_ui::solve(std::slice::from_ref(&root), [W as f32, H as f32], &|_| {
            [0.0, 0.0]
        });
        let mut list = floptle_ui::draw_list(&[root], &placed, &[]);
        list.quads.insert(0, bg());
        let px = p.render(&list, "ui_cascade_probe.png");
        let inside = at(&px, 240, 160);
        let outside = at(&px, 20, 20);
        println!("cascade: child under a 0.35 parent {inside:?} vs bg {outside:?}");
        assert!(
            inside[0] > outside[0] + 20 && inside[0] < 230,
            "an opaque child under a 0.35 parent must be PARTLY faded, not solid \
             white and not invisible: {inside:?}"
        );
        println!("opacity cascade OK → ui_cascade_probe.png");
    }

    // ---- 9-slice --------------------------------------------------------
    // The feature that makes a project's OWN panel art usable: one small frame
    // texture dressing a big element without smearing its corners.
    {
        // A 32×32 frame: an 8px border of gold with rounded-ish corner notches,
        // a dark middle. Slicing at 0.25 puts the border in the corner patches.
        const N: u32 = 32;
        const B: u32 = 8;
        let mut pixels = Vec::with_capacity((N * N * 4) as usize);
        for y in 0..N {
            for x in 0..N {
                let edge = x < B || y < B || x >= N - B || y >= N - B;
                // Notch the extreme corners so an unstretched corner is
                // visually identifiable in the output.
                let notch = !(3..N - 3).contains(&x) && !(3..N - 3).contains(&y);
                let p: [u8; 4] = if notch {
                    [255, 60, 60, 255] // red corner pips
                } else if edge {
                    [255, 210, 90, 255] // gold frame
                } else {
                    [24, 26, 38, 255] // dark middle
                };
                pixels.extend_from_slice(&p);
            }
        }
        let mut raster2 = Raster::new(&gpu);
        let id = raster2.register_texture(
            &gpu,
            &floptle_render::TextureData { pixels, width: N, height: N },
            Default::default(),
        );
        let sliced = |rect: [f32; 4]| Quad {
            rect,
            color: [1.0; 4],
            texture: "frame".into(),
            slice: [0.25, 0.25, 0.25, 0.25],
            ..Default::default()
        };
        let list = DrawList {
            text_snap: 0.0,
            quads: vec![
                bg(),
                sliced([30.0, 30.0, 260.0, 120.0]),  // wide
                sliced([30.0, 180.0, 60.0, 110.0]),  // tall and narrow
                sliced([330.0, 30.0, 120.0, 260.0]), // tall
            ],
            texts: Vec::new(),
        };
        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("slice-probe"),
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let mut instances = Vec::new();
        let mut batches = Vec::new();
        p.ui.clear_backdrop();
        p.ui.pack(
            &gpu,
            &list,
            [0.0, 0.0],
            1.0,
            &mut |_| Some(id),
            &|i| raster2.texture_size(i),
            &mut |_, _| None,
            &mut instances,
            &mut batches,
        );
        p.ui.draw(&gpu, &view, [W as f32, H as f32], &instances, &batches, &raster2);
        let px = readback(&gpu, &tex);
        save_png(&px, "ui_9slice_probe.png");

        // Nine patches per sliced quad, not one.
        println!("9-slice: {} instances for 3 sliced quads + bg", instances.len());
        assert_eq!(instances.len(), 1 + 9 * 3, "each sliced quad should expand to nine patches");
        // The corner pip must stay the SAME size on the wide box and the
        // narrow one — that is the whole promise of 9-slice, and the thing a
        // plain stretched quad gets wrong.
        let count_red = |x0: u32, y0: u32, x1: u32, y1: u32| -> i32 {
            let mut n = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let c = at(&px, x, y);
                    if c[0] > 180 && c[1] < 120 && c[2] < 120 {
                        n += 1;
                    }
                }
            }
            n
        };
        let wide_pip = count_red(28, 28, 50, 50);
        let narrow_pip = count_red(28, 178, 50, 200);
        println!("corner pip px: wide box {wide_pip}, narrow box {narrow_pip}");
        assert!(wide_pip > 4, "the wide box should have an unstretched corner pip");
        assert!(
            (wide_pip - narrow_pip).abs() <= 2,
            "corner size must not depend on the element's size: {wide_pip} vs {narrow_pip}"
        );
        println!("9-slice OK → ui_9slice_probe.png");
    }

    println!("\nall paint-box probes passed");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (W * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.config.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut outp = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            outp.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    outp
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}

// Silence the unused-import warning when a probe is edited out during triage.
#[allow(dead_code)]
fn _unused(_: ShadowSpec, _: GlowSpec, _: Sides) {}
