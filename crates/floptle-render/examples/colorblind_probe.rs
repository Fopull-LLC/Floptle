//! Headless colour-vision probe (`floptle/0079`) — renders a chart of the colour
//! pairs a deficiency confuses, runs it through each filter, and checks the
//! correction actually SEPARATES them.
//!
//! Run: `cargo run -p floptle-render --example colorblind_probe -- <out-prefix>`
//! (writes `<prefix>-plain.png`, `-deutan.png`, `-deutan-sim.png`, `-protan.png`,
//! `-tritan.png`).
//!
//! **Why a probe and not a unit test.** "Corrects for colour blindness" is a
//! claim about a picture, and the only honest check is a number taken from the
//! rendered pixels plus a look at the file. This asserts three things:
//!
//! 1. the filter CHANGES the image (a shader that silently no-ops would
//!    otherwise pass every test anyone would think to write),
//! 2. simulating deuteranopia collapses the red/green pair toward each other —
//!    which is what the deficiency does, so it proves the matrices are wired the
//!    right way round, and
//! 3. correcting for it moves that same pair further apart than simulating does,
//!    which is the entire point of daltonization.

use floptle_render::{Gpu, PostSettings, PostStack, Raster};

const W: u32 = 640;
const H: u32 = 240;

/// The pairs that matter, left to right across the chart. Each is a colour a
/// deuteranope/protanope reports as "the same as" its partner.
const PAIRS: &[([f32; 3], [f32; 3])] = &[
    ([0.85, 0.15, 0.15], [0.15, 0.65, 0.15]), // red / green — the classic
    ([0.80, 0.55, 0.10], [0.55, 0.70, 0.10]), // amber / olive
    ([0.60, 0.25, 0.55], [0.30, 0.35, 0.70]), // purple / blue
];

fn main() {
    let prefix = std::env::args().nth(1).unwrap_or_else(|| "colorblind".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let post = PostStack::new(&gpu, W, H);

    let off = PostSettings {
        bloom: false,
        bloom_threshold: 1.0,
        bloom_intensity: 0.0,
        vignette: false,
        vignette_strength: 0.0,
        vignette_radius: 1.0,
        ssao: false,
        ssao_strength: 0.0,
        ssao_radius: 0.5,
        posterize_bands: 0,
        posterize_dither: false,
        posterize_chroma: false,
        color_filter: 0,
        color_filter_strength: 1.0,
        simulate_deficiency: false,
    };

    // Paint the chart into the post input, once — every filter reads the same
    // source image, so the differences below are the filter and nothing else.
    paint_chart(&gpu, &post);

    let shot = |s: &PostSettings, name: &str| -> Vec<u8> {
        post.run(&gpu, s, None, &color_view);
        let px = readback(&gpu, &color_tex);
        save_png(&format!("{prefix}-{name}.png"), &px);
        px
    };

    let plain = shot(&off, "plain");
    let deutan = shot(&PostSettings { color_filter: 2, ..off }, "deutan");
    let deutan_sim =
        shot(&PostSettings { color_filter: 2, simulate_deficiency: true, ..off }, "deutan-sim");
    let protan = shot(&PostSettings { color_filter: 1, ..off }, "protan");
    let tritan = shot(&PostSettings { color_filter: 3, ..off }, "tritan");

    // 1. Each filter has to actually change the picture.
    for (name, px) in [("deutan", &deutan), ("protan", &protan), ("tritan", &tritan)] {
        let moved = px.iter().zip(&plain).filter(|(a, b)| a != b).count();
        assert!(
            moved > px.len() / 20,
            "the {name} filter changed {moved} of {} bytes — a filter that no-ops \
             silently is the failure this probe exists to catch",
            px.len()
        );
    }

    // 2. Simulating deuteranopia has to COLLAPSE the red/green pair: that is what
    //    the deficiency does, and it is how we know the matrices are not inverted.
    let sep = |px: &[u8], i: usize| -> f32 {
        let a = swatch(px, i, 0);
        let b = swatch(px, i, 1);
        (0..3).map(|c| (a[c] - b[c]).powi(2)).sum::<f32>().sqrt()
    };
    let (plain_rg, sim_rg, fix_rg) = (sep(&plain, 0), sep(&deutan_sim, 0), sep(&deutan, 0));
    println!("red/green separation — plain {plain_rg:.3}  simulated {sim_rg:.3}  corrected {fix_rg:.3}");
    assert!(
        sim_rg < plain_rg * 0.75,
        "simulating deuteranopia should bring red and green TOGETHER: {plain_rg:.3} → {sim_rg:.3}"
    );

    // 3. …and correcting for it has to push them back apart, further than the
    //    simulation leaves them. That is daltonization doing its job.
    assert!(
        fix_rg > sim_rg * 1.3,
        "correcting for deuteranopia should separate red and green again: \
         simulated {sim_rg:.3} vs corrected {fix_rg:.3}"
    );

    // The anti-lying-harness guard: a chart of black would satisfy everything
    // above by having no colours to confuse in the first place.
    let ink = plain.chunks(4).filter(|p| p[0] > 24 || p[1] > 24 || p[2] > 24).count();
    let coverage = ink as f32 / (W * H) as f32;
    assert!(coverage > 0.3, "the chart is nearly empty ({coverage:.3}) — nothing was measured");

    println!("wrote {prefix}-plain/deutan/deutan-sim/protan/tritan.png");
}

/// Draw the pair chart: each pair as two touching rectangles, top row = first
/// colour, bottom row = second, so a confused pair reads as one solid block.
fn paint_chart(gpu: &Gpu, post: &PostStack) {
    let mut quads: Vec<floptle_ui::Quad> = Vec::new();
    let cell = W as f32 / PAIRS.len() as f32;
    for (i, (a, b)) in PAIRS.iter().enumerate() {
        let x = i as f32 * cell;
        for (row, c) in [(0.0, a), (1.0, b)] {
            quads.push(floptle_ui::Quad {
                rect: [x, row * H as f32 * 0.5, cell, H as f32 * 0.5],
                color: [c[0], c[1], c[2], 1.0],
                ..Default::default()
            });
        }
    }
    let mut ui = floptle_render::Ui::new(gpu);
    let dl = floptle_ui::DrawList { quads, ..Default::default() };
    let mut instances = Vec::new();
    let mut batches = Vec::new();
    ui.pack(
        gpu,
        &dl,
        [0.0, 0.0],
        1.0,
        &mut |_| None,
        &|_| None,
        &mut |_, _| None,
        &mut instances,
        &mut batches,
    );
    // The UI pass does not clear, so the chart is drawn over a cleared target:
    // one empty raster pass with a black clear gives it a background.
    let mut raster = Raster::new(gpu);
    raster.draw_scene(
        gpu,
        post.input_view(),
        gpu.depth_view(),
        floptle_render::Globals::default(),
        &[],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );
    ui.draw(gpu, post.input_view(), [W as f32, H as f32], &instances, &batches, &raster);
}

/// The centre pixel of pair `i`'s `row`, as linear-ish 0..1 floats.
fn swatch(px: &[u8], i: usize, row: usize) -> [f32; 3] {
    let cell = W / PAIRS.len() as u32;
    let x = i as u32 * cell + cell / 2;
    let y = H / 4 + row as u32 * H / 2;
    let o = ((y * W + x) * 4) as usize;
    [px[o] as f32 / 255.0, px[o + 1] as f32 / 255.0, px[o + 2] as f32 / 255.0]
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let bpp = 4u32;
    let unpadded = W * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
    encoder.copy_texture_to_buffer(
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
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for row in 0..H {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    pixels
}

fn save_png(path: &str, pixels: &[u8]) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(pixels).unwrap();
}
