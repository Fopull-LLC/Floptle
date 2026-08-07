//! **A warm ramp must not band into hues** (`floptle/0126`).
//!
//! A retro project posterizes. Quantize each colour channel on its own and a
//! smooth warm ramp crosses each channel's band boundary at a *different* value,
//! so it lands as a stack of bands in colours nobody chose: an olive one where
//! red and green have stepped and blue has not, a maroon one where only red has.
//!
//! The report that produced this card was *"it looks like a circle around the
//! player"* — three coloured rings around a warm lamp, and no way at all to find
//! the cause from that. The workaround was a rule that a light's colour must be
//! exact grey or strongly saturated, which rules out a torch, a lamp, a fire and
//! a muzzle flash.
//!
//! **The subject moved in `floptle/0127`.** Posterize now quantizes the palette —
//! the art — and runs before the light, so a light is never quantized at all and
//! cannot band into anything (`light2d_smooth_probe` is the assertion for that).
//! What is still quantized, and still has to keep its hue, is warm *art*: a
//! sunset gradient, a torch-lit wall texture, any sprite with a tint that is not
//! grey. So the ramp under test here is painted, not lit.
//!
//! * `posterize_per_channel.png` — the per-channel quantizer, and the bug. There
//!   must be values where some channels step and others do not, or this probe is
//!   proving nothing about the fix.
//! * `posterize_chroma.png` — brightness stepped once and the colour carried
//!   along. **Zero** such values, because chroma is never quantized.
//!
//! Run: cargo run -p floptle-render --example posterize_chroma_probe -- <outdir>

use floptle_render::{
    Globals, Gpu, MaterialParams, PostSettings, PostStack, Projection, Raster, RenderCamera, TexId,
    TexSampling, TextureData, instance_of_mat, mesh,
};
use glam::{DVec3, Mat4, Quat};

const S: u32 = 256;
const ORTHO_HEIGHT: f32 = 16.0;
/// The colour from the report. A mild warm white — spread 0.38, which is the
/// band-error size that made it look deliberate.
const WARM: [f32; 3] = [1.0, 0.86, 0.62];
const BANDS: u32 = 8;
/// A channel has to move by more than this for the sample to count as a step, so
/// 8-bit rounding noise is not read as one.
const STEP: i32 = 2;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    // A warm ramp, painted: 256 values from black to `WARM`. One quad filling the
    // view, so it lands about 1:1 on the frame and every value the quantizer sees
    // is a value the author put there.
    let pixels: Vec<u8> = (0..256u32)
        .flat_map(|x| {
            let t = x as f32 / 255.0;
            [
                (t * WARM[0] * 255.0).round() as u8,
                (t * WARM[1] * 255.0).round() as u8,
                (t * WARM[2] * 255.0).round() as u8,
                255,
            ]
        })
        .collect();
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels, width: 256, height: 1 },
        TexSampling::default(),
    );
    let quad = raster.register(&gpu, &mesh::plane(ORTHO_HEIGHT * 0.5), None);

    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::IDENTITY, &mat);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.0, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);

    let settings = |chroma: bool| PostSettings {
        bloom: false,
        bloom_threshold: 1.0,
        bloom_intensity: 0.7,
        vignette: false,
        vignette_strength: 0.0,
        vignette_radius: 1.0,
        ssao: false,
        ssao_strength: 0.0,
        ssao_radius: 0.5,
        posterize_bands: BANDS,
        // Undithered: dither trades the step for a stipple, and the step is
        // exactly what this probe is looking at.
        posterize_dither: false,
        posterize_chroma: chroma,
        color_filter: 0,
        color_filter_strength: 1.0,
        simulate_deficiency: false,
    };

    let report = |name: &str, chroma: bool, raster: &mut Raster| -> (usize, usize) {
        let px = shot(&gpu, raster, view_proj, quad, tex, &raw, &settings(chroma));
        let out = format!("{dir}/posterize_{name}.png");
        save_png(&px, &out);
        // Walk the ramp. Every adjacent pair is one place the frame could step.
        let (mut apart, mut together) = (0usize, 0usize);
        let y = S / 2;
        for x in 1..(S - 2) {
            let a = px[(y * S + x) as usize];
            let b = px[(y * S + x + 1) as usize];
            let moved: Vec<bool> =
                (0..3).map(|c| (a[c] as i32 - b[c] as i32).abs() > STEP).collect();
            let n = moved.iter().filter(|m| **m).count();
            match n {
                0 => {}
                3 => together += 1,
                _ => apart += 1,
            }
        }
        println!("{name}: {apart} values step APART, {together} step together — wrote {out}");
        (apart, together)
    };

    let per_channel = report("per_channel", false, &mut raster);
    let preserved = report("chroma", true, &mut raster);

    // The control, and it is the whole reason this probe is trustworthy: if the
    // default quantizer did NOT produce split steps in this scene, a passing
    // assertion below would mean nothing at all. The card this came from records
    // a first attempt whose threshold was loose enough that the test passed with
    // the bug still wired up.
    assert!(
        per_channel.0 > 0,
        "the per-channel quantizer produced no split steps here, so this probe cannot tell \
         whether preserving chroma does anything. Pick a colour or a band count that bands."
    );
    assert_eq!(
        preserved.0, 0,
        "a warm ramp still banded into hues with chroma preserved: {} values where some \
         channels stepped and others did not. Chroma is never quantized, so this cannot \
         happen — unless the step is being applied per channel somewhere after all.",
        preserved.0
    );
    // …and it must still POSTERIZE. A fix that quietly stopped banding at all
    // would pass the assertion above and destroy the look the project is made of.
    assert!(
        preserved.1 > 0,
        "nothing stepped together either: preserving chroma turned the posterize off rather \
         than making it step in brightness"
    );

    println!("posterize chroma OK — the bands in per_channel.png are the bug, in colour");
}

/// Draw the ramp into the post chain's input, quantize the palette, run the
/// chain. The palette pass is where the quantize lives now (`floptle/0127`).
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    view_proj: Mat4,
    quad: floptle_render::MeshId,
    tex: TexId,
    raw: &floptle_render::InstanceRaw,
    settings: &PostSettings,
) -> Vec<[u8; 4]> {
    let size = wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 };
    let make = |label: &str, format: wgpu::TextureFormat, extra: wgpu::TextureUsages| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | extra,
            view_formats: &[],
        })
    };
    let out_tex = make("posterize-out", gpu.surface_format(), wgpu::TextureUsages::COPY_SRC);
    let depth = make("posterize-depth", Gpu::DEPTH_FORMAT, wgpu::TextureUsages::empty());
    let out_view = out_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let dview = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let post = PostStack::new(gpu, S, S);
    let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(
        gpu,
        post.input_view(),
        &dview,
        globals,
        &[(quad, Some(tex), *raw)],
        Some([0.02, 0.02, 0.04, 1.0]),
        None,
    );
    if let Some(q) = settings.palette() {
        raster.quantize_palette(gpu, post.input_view(), (S, S), q);
    }
    post.run(gpu, settings, None, &out_view);
    readback(gpu, &out_tex)
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (S * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
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
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut o = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            o.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    o
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
