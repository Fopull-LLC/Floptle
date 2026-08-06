//! **A warm light must not band into hues** (`floptle/0126`).
//!
//! A retro project posterizes. Put a 2D light on a posterized frame and the
//! quantizer — which steps each colour channel on its own — meets a smooth
//! radial ramp, and each channel crosses its band boundary at a *different
//! radius*. A light at `{1.0, 0.86, 0.62}`, which is the least exotic colour in
//! lighting, therefore lands as a stack of concentric rings in colours nobody
//! chose: an olive one where red and green have stepped and blue has not, a
//! maroon one where only red has.
//!
//! It was on screen for a release, and the report it produced was *"it looks
//! like a circle around the player"* — a completely reasonable way to describe
//! three coloured rings and no way at all to find the cause. The workaround was
//! a rule that a light's colour must be exact grey or strongly saturated, which
//! rules out a torch, a lamp, a fire and a muzzle flash.
//!
//! So: sample the same warm light along a radius under both quantizers.
//!
//! * `posterize_per_channel.png` — today's default, and the bug. There must be
//!   radii where some channels step and others do not, or this probe is proving
//!   nothing about the fix.
//! * `posterize_chroma.png` — brightness stepped once and the colour carried
//!   along. **Zero** such radii, because chroma is never quantized.
//!
//! Run: cargo run -p floptle-render --example posterize_chroma_probe -- <outdir>

use floptle_render::{
    Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, PostSettings, PostStack,
    Projection, Raster, RenderCamera, TexId, TexSampling, TextureData, instance_of_mat, mesh,
};
use glam::{DVec3, Mat4, Quat};

const S: u32 = 256;
const ORTHO_HEIGHT: f32 = 16.0;
const MAP_RANK: u32 = 1;
/// The colour from the report. A mild warm white — spread 0.38, which is the
/// band-error size that made it look deliberate.
const WARM: [f32; 3] = [1.0, 0.86, 0.62];
const BANDS: u32 = 8;
/// A channel has to move by more than this for the sample to count as a step,
/// so 8-bit rounding noise is not read as one.
const STEP: i32 = 2;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    // Flat mid-grey art, the way the reporting project's tiles are: every hue in
    // the output below therefore came from the light or from the quantizer, and
    // there is nothing else it could have come from.
    let n = 8u32;
    let pixels: Vec<u8> = (0..n * n).flat_map(|_| [150u8, 150, 150, 255]).collect();
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels, width: n, height: n },
        TexSampling::default(),
    );
    let data: Vec<u32> = (0..16 * 16).map(|_| 0).collect();
    let map = raster.register(&gpu, &mesh::tilemap(16, 16, 1.0, 1, 1, [0.0, 0.0], &data), None);

    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::IDENTITY, &mat);
    let flat = [(map, Some::<TexId>(tex), Light2dInstance::from_raster(&raw, MAP_RANK, false))];

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.0, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);

    let mut lights = Light2dUniform {
        count: [1.0, 0.0, 0.0, 0.0],
        ambient: [0.3, 0.3, 0.34, 0.0],
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };
    lights.pos[0] = [0.0, 0.0, 0.0, 8.0];
    lights.color[0] = [WARM[0] * 2.2, WARM[1] * 2.2, WARM[2] * 2.2, 0.0];
    lights.mask[0] = [1 << MAP_RANK, 0, 0, 0];

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
        // Undithered, which is the case that matters: the global dither switch
        // would also dither the art the posterize was chosen for, and it is
        // screen-space, so on a scrolling camera it crawls.
        posterize_dither: false,
        posterize_chroma: chroma,
        color_filter: 0,
        color_filter_strength: 1.0,
        simulate_deficiency: false,
    };

    let report = |name: &str, chroma: bool, raster: &mut Raster| -> (usize, usize) {
        let px = shot(&gpu, raster, view_proj, map, tex, &raw, &flat, &lights, &settings(chroma));
        let out = format!("{dir}/posterize_{name}.png");
        save_png(&px, &out);
        // Walk out along the horizontal radius from the light's centre. Every
        // adjacent pair is one place the frame could step.
        let (mut apart, mut together) = (0usize, 0usize);
        let y = S / 2;
        for x in (S / 2)..(S - 2) {
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
        println!("{name}: {apart} radii step APART, {together} step together — wrote {out}");
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
        "a warm light still banded into hues with chroma preserved: {} radii where some \
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

    println!("posterize chroma OK — the rings in per_channel.png are the bug, in colour");
}

/// Draw the lit map into the post chain's input and run the chain over it.
#[allow(clippy::too_many_arguments)]
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    view_proj: Mat4,
    map: floptle_render::MeshId,
    tex: TexId,
    raw: &floptle_render::InstanceRaw,
    flat: &[(floptle_render::MeshId, Option<TexId>, Light2dInstance)],
    lights: &Light2dUniform,
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

    // The engine's own order: the scene and its 2D lighting land in the post
    // input, and the terminal quantize is the last thing that touches the frame.
    let post = PostStack::new(gpu, S, S);
    let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(
        gpu,
        post.input_view(),
        &dview,
        globals,
        &[(map, Some(tex), *raw)],
        Some([0.02, 0.02, 0.04, 1.0]),
        None,
    );
    raster.light2d_pass(
        gpu,
        post.input_view(),
        &dview,
        (S, S),
        view_proj.to_cols_array_2d(),
        lights,
        flat,
    );
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
