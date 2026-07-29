//! Headless probe for the ◫ UI tab's canvas (docs/ui-system-2-proposal.md §C).
//!
//! The tab's canvas is not an approximation of the UI — it is the shipping path
//! (`solve` → `draw_list` → `Ui::pack` → `Ui::draw`) rendered into an offscreen
//! sRGB target that was first cleared to the author's chosen backdrop. Two
//! things there can silently go wrong and both look plausible on screen:
//!
//! 1. **The backdrop colour.** wgpu clear values are LINEAR and the target is
//!    sRGB, so a colour handed straight to `LoadOp::Clear` comes out visibly
//!    too light. The editor pre-encodes it; this checks the byte that lands.
//! 2. **The multi-resolution preview.** The whole claim of the resolution
//!    dropdown is that re-solving at another shape shows what `Pin` and
//!    `Stretch` actually do. If the canvas re-solved at the reference and then
//!    merely stretched the image, every layout would look responsive and none
//!    would be — so this renders the SAME layer at 16:9 and 21:9 and asserts
//!    the elements moved the way each placement promises.
//!
//! Run: cargo run --release -p floptle-render --example ui_canvas_probe

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{
    Anchor, Corners, ElementSpec, Gradient, GradientKind, Node, Place, ShapeSpec, Size, TextSpec,
    UiLayer,
};

/// sRGB encode — what the editor does to a picked backdrop colour before
/// handing it to a clear op.
fn lin(c: f32) -> f64 {
    (if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }) as f64
}

fn shape(fill: [f32; 4]) -> ShapeSpec {
    ShapeSpec {
        fill,
        gradient: None,
        radius: Corners::all(8.0),
        border: Default::default(),
        border_color: [0.0; 4],
        shadow: None,
        glow: None,
        grain: None,
        blend: Default::default(),
    }
}

fn el(id: u32, place: Place, size: [Size; 2], fill: [f32; 4]) -> Node {
    Node::with_children(
        id,
        ElementSpec { place, size, shape: Some(shape(fill)), ..Default::default() },
        vec![],
    )
}

/// The screen under test: one of each placement mode, so each resolution tells
/// a different story about a different element.
fn screen() -> Vec<Node> {
    vec![
        // 1 — a header that STRETCHES across whatever width it's given.
        {
            let mut n = el(
                1,
                Place::Stretch {
                    min: [0.0, 0.0],
                    max: [1.0, 0.0],
                    margin: [24.0, 24.0, 24.0, 0.0],
                },
                [Size::Fit, Size::Fixed(72.0)],
                [0.10, 0.12, 0.20, 1.0],
            );
            if let Some(s) = n.spec.shape.as_mut() {
                s.gradient = Some(Gradient {
                    kind: GradientKind::Linear,
                    to: [0.35, 0.20, 0.45, 1.0],
                    angle: 0.0,
                    ..Default::default()
                });
                s.radius = Corners([12.0, 12.0, 0.0, 0.0]);
            }
            n.children.push(Node::with_children(
                11,
                ElementSpec {
                    place: Place::Pin { anchor: Anchor::Left, offset: [20.0, 0.0] },
                    text: Some(TextSpec {
                        text: "STRETCH — follows the width".into(),
                        size: 26.0,
                        color: [0.92, 0.94, 1.0, 1.0],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                vec![],
            ));
            n
        },
        // 2 — a FREE panel: fixed where it was put, at any resolution.
        el(
            2,
            Place::Free { pos: [80.0, 160.0] },
            [Size::Fixed(280.0), Size::Fixed(180.0)],
            [0.85, 0.35, 0.25, 1.0],
        ),
        // 3 — PINNED to the bottom-right corner: tracks the corner.
        el(
            3,
            Place::Pin { anchor: Anchor::BottomRight, offset: [-32.0, -32.0] },
            [Size::Fixed(200.0), Size::Fixed(80.0)],
            [0.25, 0.70, 0.55, 1.0],
        ),
        // 4 — PINNED to the centre: tracks the middle.
        el(
            4,
            Place::Pin { anchor: Anchor::Center, offset: [0.0, 0.0] },
            [Size::Fixed(120.0), Size::Fixed(120.0)],
            [0.95, 0.80, 0.30, 1.0],
        ),
    ]
}

struct Shot {
    px: Vec<[u8; 4]>,
    w: u32,
    h: u32,
    /// Design units per rendered pixel — for turning a solved rect into a probe
    /// point.
    scale: f32,
}

impl Shot {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        self.px[(y.min(self.h - 1) * self.w + x.min(self.w - 1)) as usize]
    }
    /// Sample at a point given in DESIGN units.
    fn at_design(&self, x: f32, y: f32) -> [u8; 4] {
        self.at((x * self.scale) as u32, (y * self.scale) as u32)
    }
}

/// Render the layer exactly the way the UI tab's canvas does.
#[allow(clippy::too_many_arguments, reason = "one probe entry point; splitting it would hide what the canvas path actually takes")]
fn canvas(
    gpu: &Gpu,
    raster: &Raster,
    ui: &mut Ui,
    layer: &UiLayer,
    preview_px: [f32; 2],
    zoom: f32,
    backdrop: [f32; 3],
    out: &str,
) -> Shot {
    let layer_scale = layer.scale_for(preview_px);
    let design_vp = [preview_px[0] / layer_scale, preview_px[1] / layer_scale];
    let render_scale = layer_scale * zoom;
    let w = (design_vp[0] * render_scale).round() as u32;
    let h = (design_vp[1] * render_scale).round() as u32;

    let roots = screen();
    let measure = |t: &TextSpec| ui.measure_spec(t);
    let placed = floptle_ui::solve(&roots, design_vp, &measure);
    let dl = floptle_ui::draw_list(&roots, &placed, &[]);

    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui-canvas-probe"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    // 1. clear to the backdrop (pre-encoded, as the editor does)
    {
        let mut enc =
            gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: lin(backdrop[0]),
                        g: lin(backdrop[1]),
                        b: lin(backdrop[2]),
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.queue.submit(Some(enc.finish()));
    }
    // 2. the real UI pass over it
    let mut instances = Vec::new();
    let mut batches = Vec::new();
    ui.clear_backdrop();
    ui.pack(
        gpu,
        &dl,
        [0.0, 0.0],
        render_scale,
        &mut |_| None,
        &|_| None,
        &mut |_, _| None,
        &mut instances,
        &mut batches,
    );
    ui.draw(gpu, &view, [w as f32, h as f32], &instances, &batches, raster);

    let px = readback(gpu, &tex, w, h);
    save_png(&px, w, h, out);
    // Report the solved rects: the canvas's picking and snapping run on these
    // numbers, so a probe that only looked at pixels would miss a solver bug.
    for p in &placed {
        println!(
            "  #{}  x {:>6.1}  y {:>6.1}  w {:>6.1}  h {:>6.1}",
            p.id, p.rect[0], p.rect[1], p.rect[2], p.rect[3]
        );
    }
    Shot { px, w, h, scale: render_scale }
}

fn main() {
    let gpu = Gpu::headless(64, 64);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);

    let layer = UiLayer { design_height: 720.0, reference_width: 1280.0, ..Default::default() };
    // A deliberately un-neutral backdrop: a bug that ignores the encode shows
    // up as a wrong colour, not just a wrong brightness.
    let backdrop = [0.09, 0.11, 0.16];

    // ---- 16:9, the reference shape ----
    println!("16:9 @ 1280×720");
    let wide = canvas(
        &gpu,
        &raster,
        &mut ui,
        &layer,
        [1280.0, 720.0],
        1.0,
        backdrop,
        "ui_canvas_16x9.png",
    );

    // The backdrop survives the round trip. Rendered sRGB bytes are the picked
    // colour × 255 (±1 for rounding), NOT the ~40%-too-light value you get by
    // handing a display colour to a linear clear.
    let corner = wide.at(4, wide.h - 4);
    let want = backdrop.map(|c| (c * 255.0).round() as i32);
    println!("backdrop: got {corner:?} want {want:?}");
    for i in 0..3 {
        assert!(
            (corner[i] as i32 - want[i]).abs() <= 2,
            "backdrop channel {i} came out {} not {} — the sRGB encode on the clear is wrong",
            corner[i],
            want[i]
        );
    }

    // The pieces are where they were placed.
    let free = wide.at_design(200.0, 250.0);
    println!("free panel: {free:?}");
    assert!(free[0] > 150 && free[1] < 130, "the free panel should be red-orange: {free:?}");
    let centre = wide.at_design(640.0, 360.0);
    println!("centre pin: {centre:?}");
    assert!(centre[0] > 200 && centre[1] > 150 && centre[2] < 140, "centre badge is yellow");
    println!("16:9 OK → ui_canvas_16x9.png");

    // ---- 21:9, the same layer re-solved ----
    println!("21:9 @ 2560×1080");
    let ultra = canvas(
        &gpu,
        &raster,
        &mut ui,
        &layer,
        [2560.0, 1080.0],
        1.0,
        backdrop,
        "ui_canvas_21x9.png",
    );
    // MatchHeight: 720 design units still span the height, so the design
    // viewport got WIDER (1706 units) while staying 720 tall.
    let vp_w = ultra.w as f32 / ultra.scale;
    println!("21:9 design viewport: {vp_w:.0} × 720");
    assert!(vp_w > 1600.0, "the design viewport should widen, got {vp_w:.0}");

    // 1 — the STRETCH header now reaches the new right edge. Sampled 60 units
    // in from the right, which is off the end of the 16:9 header entirely.
    let header_far = ultra.at_design(vp_w - 60.0, 50.0);
    println!("stretch header at the far right: {header_far:?}");
    assert!(
        header_far != ultra.at(4, ultra.h - 4),
        "the header should have grown into the extra width, found backdrop: {header_far:?}"
    );

    // 2 — the FREE panel did NOT move: free placement is absolute, and a tool
    // that quietly re-flowed it would be lying about what Free means.
    let free_wide = ultra.at_design(200.0, 250.0);
    println!("free panel at 21:9: {free_wide:?}");
    assert!(
        free_wide[0] > 150 && free_wide[1] < 130,
        "the free panel must stay put: {free_wide:?}"
    );

    // 3 — the bottom-right PIN tracked the corner: it is no longer near where
    // it sat at 16:9, and it IS near the new corner.
    let old_corner = ultra.at_design(1280.0 - 100.0, 720.0 - 60.0);
    let new_corner = ultra.at_design(vp_w - 100.0, 720.0 - 60.0);
    println!("bottom-right pin: at old corner {old_corner:?}, at new corner {new_corner:?}");
    assert!(
        new_corner[1] > 120 && new_corner[2] > 90 && new_corner[0] < 120,
        "the pinned badge should be at the NEW corner (teal): {new_corner:?}"
    );
    assert_ne!(old_corner, new_corner, "the badge cannot be in both corners");

    // 4 — the centre pin re-centred.
    let mid = ultra.at_design(vp_w * 0.5, 360.0);
    println!("centre pin at 21:9: {mid:?}");
    assert!(mid[0] > 200 && mid[1] > 150 && mid[2] < 140, "centre badge re-centred: {mid:?}");
    println!("21:9 OK → ui_canvas_21x9.png");

    // ---- zoom renders MORE pixels, not a stretched image ----
    println!("16:9 @ 200% zoom");
    let zoomed = canvas(
        &gpu,
        &raster,
        &mut ui,
        &layer,
        [1280.0, 720.0],
        2.0,
        backdrop,
        "ui_canvas_zoom.png",
    );
    assert_eq!((zoomed.w, zoomed.h), (2560, 1440), "zoom must re-render, not upscale");
    // Same design point, same colour — the layout is untouched by zoom.
    let z = zoomed.at_design(200.0, 250.0);
    println!("free panel at 200%: {z:?}");
    assert!(z[0] > 150 && z[1] < 130, "zoom must not move anything: {z:?}");
    println!("zoom OK → ui_canvas_zoom.png");

    println!("\nall UI canvas probes passed");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<[u8; 4]> {
    let padded =
        (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
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
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let row = &data[(y * padded) as usize..];
        for x in 0..w {
            let i = (x * 4) as usize;
            out.push([row[i], row[i + 1], row[i + 2], row[i + 3]]);
        }
    }
    drop(data);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], w: u32, h: u32, path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
