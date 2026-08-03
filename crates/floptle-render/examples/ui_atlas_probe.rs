//! Overflow the glyph atlas on purpose, and LOOK at what the player gets.
//!
//! The bug this exists for (floptle/0033) was not a crash or a stutter — it was
//! text quietly losing letters, forever, in a build nobody was watching a log
//! for. So the fix cannot be verified by a green test alone: the two frames
//! below are the actual evidence.
//!
//! * `ui_atlas_overflow.png` — the first frame after the atlas fills. Whatever
//!   could not be placed draws as ▯, which reads as "something is wrong here"
//!   rather than as a label somebody left blank.
//! * `ui_atlas_recovered.png` — the next frame, after `maintain` doubled the
//!   atlas. The same text, complete.
//!
//! Run: cargo run --release -p floptle-render --example ui_atlas_probe

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{Align, DrawList, TextRun};

/// Enough distinct pixel sizes to blow past 1024², which is the point: this is
/// a plausible amount of text, not a stress test. Sixteen sizes of printable
/// ASCII measures at ~2100 rows of 1024 — see the packer's unit tests.
const SIZES: [f32; 16] =
    [11.0, 13.0, 16.0, 19.0, 22.0, 26.0, 30.0, 35.0, 40.0, 46.0, 52.0, 58.0, 64.0, 72.0, 80.0, 90.0];

/// The whole printable range, because a real UI eventually uses most of it and
/// one pangram does not: 95 characters at 16 sizes is ~1500 glyphs, which is
/// where 1024² actually runs out.
fn line() -> String {
    ('!'..='~').collect()
}

fn run(size: f32, y: f32, w: f32) -> TextRun {
    TextRun {
        rect: [16.0, y, w - 32.0, size * 1.4],
        text: format!("{size:.0}px {}", line()),
        size,
        color: [0.92, 0.94, 0.97, 1.0],
        align: Align::Start,
        valign: Align::Start,
        font: String::new(),
        line_height: 1.0,
        ..Default::default()
    }
}

fn main() {
    let (w, h) = (1600u32, 1080u32);
    let gpu = Gpu::headless(64, 64);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);

    // One list holding every size — a single frame that cannot fit in 1024².
    let mut list = DrawList::default();
    let mut y = 12.0;
    for s in SIZES {
        list.texts.push(run(s, y, w as f32));
        y += s * 1.35 + 4.0;
    }

    for (frame, out) in ["ui_atlas_overflow.png", "ui_atlas_recovered.png"].iter().enumerate() {
        // `set_time` is the frame tick the glyph cache dates entries by.
        ui.set_time(frame as f32 / 60.0);

        let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas-probe"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
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
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.03, g: 0.04, b: 0.06, a: 1.0 }),
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

        let (mut instances, mut batches) = (Vec::new(), Vec::new());
        ui.clear_backdrop();
        ui.pack(
            &gpu,
            &list,
            [0.0, 0.0],
            1.0,
            &mut |_| None,
            &|_| None,
            &mut |_, _| None,
            &mut instances,
            &mut batches,
        );
        ui.draw(&gpu, &view, [w as f32, h as f32], &instances, &batches, &raster);

        let px = readback(&gpu, &tex, w, h);
        save_png(&px, w, h, out);
        println!(
            "  frame {frame}: {} glyph quads, atlas {:.0}% used",
            instances.len(),
            ui.atlas_utilisation() * 100.0
        );
    }
    println!(
        "\nOK — look at ui_atlas_overflow.png (boxes, not blanks) and \
         ui_atlas_recovered.png (the same text, complete)."
    );
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<[u8; 4]> {
    let row = (w * 4).next_multiple_of(256);
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (row * h) as u64,
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
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let data = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let i = (y * row + x * 4) as usize;
            out.push([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        }
    }
    drop(data);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], w: u32, h: u32, path: &str) {
    let mut flat = Vec::with_capacity(px.len() * 4);
    for p in px {
        flat.extend_from_slice(p);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
    println!("wrote {path}");
}
