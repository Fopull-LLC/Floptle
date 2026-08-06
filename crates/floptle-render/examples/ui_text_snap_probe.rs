//! **Text snapping** (`floptle/0120`) — the same label, off the grid and on it.
//!
//! A pixel font's art is a grid. It only *looks* like a pixel font when one of
//! its cells is a whole number of screen pixels, and what reaches the rasterizer
//! is `text_size × layer scale` — a scale that belongs to the window, not to the
//! author. At 1252 rows against a 720-unit design that scale is 1.7389, so a
//! `size: 24` label rasterizes at 41.7 → 42 px. For a ten-cell em that is 4.2
//! pixels per cell: every vertical stem straddles a pixel boundary by a
//! different fraction, takes a different amount of antialiasing, and the same
//! letter comes out different in two different words.
//!
//! It was reported as *"each character looks like it's just not positioned
//! exactly correctly"*, and **nothing is mispositioned** — the distortion is
//! inside each glyph rather than between them. That is the whole reason this
//! probe writes pictures: the assertions below can only prove the SIZE landed on
//! the grid. Whether the result reads as a pixel font is a thing to look at.
//!
//! * `ui_text_snap_off.png` — today's answer at three window heights. The same
//!   string, rasterized at three sizes that are on no grid at all.
//! * `ui_text_snap_on.png` — `text_snap = 10`, the same three windows. Every
//!   size is a multiple of ten, so a cell is a whole number of pixels in all of
//!   them.
//!
//! Run: cargo run -p floptle-render --example ui_text_snap_probe -- <outdir>

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{Align, DrawList, TextRun, UiLayer, UiScaleMode};

const W: u32 = 640;
/// Window heights that give awkward scales against a 720 design: 1.7389, 1.2222
/// and 0.9028 — none of them a whole anything.
const HEIGHTS: [f32; 3] = [1252.0, 880.0, 650.0];
/// Cells in an em for the font this stands in for. A tenth-of-an-em grid is the
/// usual shape and the one the report came from.
const CELLS_PER_EM: f32 = 10.0;
const SIZE: f32 = 24.0;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let h = 96u32 * HEIGHTS.len() as u32;
    let gpu = Gpu::headless(W, h);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);

    let layer = |snap: f32| UiLayer {
        design_height: 720.0,
        scale_mode: UiScaleMode::MatchHeight,
        text_snap: snap,
        ..Default::default()
    };

    for (name, snap) in [("off", 0.0), ("on", CELLS_PER_EM)] {
        let l = layer(snap);
        let mut sizes = Vec::new();
        let mut px = vec![0u8; (W * h * 4) as usize];
        for (i, vh) in HEIGHTS.iter().enumerate() {
            let scale = l.scale_for([W as f32, *vh]);
            let want = floptle_ui::text_px(SIZE * scale, l.text_snap);
            sizes.push((scale, want));
            let list = DrawList {
                texts: vec![TextRun {
                    rect: [8.0 / scale, 8.0 / scale, (W as f32 - 16.0) / scale, 64.0 / scale],
                    text: format!("Handgloves {want}px  (scale {scale:.4})"),
                    size: SIZE,
                    color: [0.95, 0.96, 0.98, 1.0],
                    align: Align::Start,
                    valign: Align::Start,
                    font: String::new(),
                    ..Default::default()
                }],
                text_snap: l.text_snap,
                ..Default::default()
            };
            let band = shot(&gpu, &raster, &mut ui, &list, scale, W, 96);
            let off = (i as u32 * 96 * W * 4) as usize;
            px[off..off + band.len()].copy_from_slice(&band);
        }
        let out = format!("{dir}/ui_text_snap_{name}.png");
        save_png(&px, W, h, &out);
        let listed: Vec<String> =
            sizes.iter().map(|(s, p)| format!("{s:.4}→{p}px")).collect();
        println!("{name}: {} — wrote {out}", listed.join("  "));

        if snap >= 1.0 {
            for (scale, got) in &sizes {
                assert_eq!(
                    got % CELLS_PER_EM as u32,
                    0,
                    "a scale of {scale:.4} rasterized at {got} px, which is not a whole \
                     number of {CELLS_PER_EM}-cell ems — the font is being resampled off \
                     its own grid and every stem softens by a different fraction"
                );
            }
        } else {
            // …and the control: without it, none of these three land on the
            // grid, which is the whole complaint. If they all did, this probe
            // would be proving nothing.
            assert!(
                sizes.iter().any(|(_, p)| p % CELLS_PER_EM as u32 != 0),
                "every unsnapped size happened to be on the grid, so this probe cannot \
                 tell whether snapping does anything: {sizes:?}"
            );
        }
    }

    println!("text snapping OK — now LOOK at the two images; only they say whether it reads as a pixel font");
}

/// One band: draw `list` at `scale` into a `w × band` target and read it back.
fn shot(
    gpu: &Gpu,
    raster: &Raster,
    ui: &mut Ui,
    list: &DrawList,
    scale: f32,
    w: u32,
    band: u32,
) -> Vec<u8> {
    let size = wgpu::Extent3d { width: w, height: band, depth_or_array_layers: 1 };
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("text-snap"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let (mut instances, mut batches) = (Vec::new(), Vec::new());
    ui.pack(
        gpu,
        list,
        [0.0, 0.0],
        scale,
        &mut |_| None,
        &|_| None,
        &mut |_, _| None,
        &mut instances,
        &mut batches,
    );
    ui.draw(gpu, &view, [w as f32, band as f32], &instances, &batches, raster);
    readback(gpu, &color, w, band)
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<u8> {
    let padded =
        (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let row = (y * padded) as usize;
        for x in 0..w {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.extend_from_slice(&if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn save_png(px: &[u8], w: u32, h: u32, path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(px).unwrap();
}
