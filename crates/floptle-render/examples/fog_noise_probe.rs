//! **Fog noise, sampled at its own frequency rather than the march's.**
//!
//! The volumetric march evaluates three octaves of value noise — twenty-four
//! hashes — and it was doing so at every step. A step is a fraction of a metre
//! and the noise is tens of metres across, so consecutive samples differed by
//! less than the dither the march already applies on purpose: it was the single
//! most expensive thing in the frame, spent on a difference nobody could see.
//!
//! Two changes came out of that, and neither is safe by inspection:
//!
//! * a sample is HELD for a run of steps (`fog_noise_stride`), and
//! * octaves finer than a step are replaced by their mean (`cloud_fbm_lod`).
//!
//! Both trade exactness for speed, so both need a check that would fail if the
//! trade went too far. `fog_probe` cannot be that check — it runs with noise
//! switched OFF on purpose, so every assertion in it passed unchanged while this
//! code was being rewritten underneath it.
//!
//! **The assertions are convergence, not appearance.** A held sample is only
//! sound if the picture it produces is the picture the un-held march was
//! converging to, and the way to ask that without a golden image is to render
//! the same fog at step counts that produce DIFFERENT strides and require the
//! results to agree. A stride that lost the noise, doubled it, or thinned the
//! fog would move one of them and not the other.
//!
//! Run: cargo run -p floptle-render --example fog_noise_probe -- <out-dir>

use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat};

const S: u32 = 192;
const FAR: f32 = 40.0;
const DENSITY: f32 = 0.06;
const FOG_COLOR: [f32; 3] = [0.5, 0.55, 0.6];

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);
    let mut rm = Raymarch::new(&gpu);
    rm.set_sky_texture(
        &gpu,
        Some(&TextureData { pixels: vec![0; 4 * 4 * 4], width: 4, height: 4 }),
    );

    the_noise_actually_reaches_the_picture(&gpu, &rm, &dir);
    a_held_sample_lands_where_the_march_was_going(&gpu, &rm, &dir);
    dropping_an_octave_does_not_thin_the_fog(&gpu, &rm);

    println!("fog noise probe OK");
}

// ---------------------------------------------------------------------------
// 1. The noise is still in there.
//
// The failure this guards is the quiet one: a stride that came out as "hold the
// first sample for the whole ray" produces perfectly smooth fog that looks
// entirely reasonable on its own, costs almost nothing, and is wrong. Switching
// the noise on has to MOVE pixels.
fn the_noise_actually_reaches_the_picture(gpu: &Gpu, rm: &Raymarch, dir: &str) {
    let off = shot(gpu, rm, 0.0, 18.0, 16.0);
    let on = shot(gpu, rm, 0.9, 18.0, 16.0);
    save(&off, &format!("{dir}/fog_noise_off.png"));
    save(&on, &format!("{dir}/fog_noise_on.png"));
    let (m_off, s_off) = stats(&off);
    let (m_on, s_on) = stats(&on);
    println!("noise off: mean {m_off:.4} spread {s_off:.4}");
    println!("noise on : mean {m_on:.4} spread {s_on:.4}");
    // **Per pixel, not per frame.** A frame's own spread is dominated by the
    // lamp's glow, which is there either way — comparing the two totals would
    // pass with the noise switched off entirely. What has to be shown is that
    // turning the noise on MOVED pixels, so the comparison is pixel against
    // matching pixel.
    let moved = mean_abs_diff(&off, &on);
    println!("noise moves the picture by {moved:.4} per pixel");
    assert!(
        moved > 0.01,
        "turning the noise on must visibly change the fog — a stride that held one \
         sample for a whole ray would still look like reasonable smooth fog and \
         would not: {moved:.4} per pixel"
    );
}

/// Mean absolute difference in linear luminance, pixel against matching pixel.
fn mean_abs_diff(a: &[[u8; 4]], b: &[[u8; 4]]) -> f32 {
    let lum = |p: &[u8; 4]| (srgb(p[0]) + srgb(p[1]) + srgb(p[2])) / 3.0;
    a.iter().zip(b).map(|(x, y)| (lum(x) - lum(y)).abs()).sum::<f32>() / a.len() as f32
}

// ---------------------------------------------------------------------------
// 2. A held sample lands where the un-held march was going.
//
// Step counts of 8, 16 and 48 give three different strides over the same field.
// If holding a sample changed the answer rather than just the cost, they would
// disagree — and they are compared on the MEAN, because the point at issue is
// whether the fog's total density survived, not whether two different march
// cadences put their lumps in identical places.
fn a_held_sample_lands_where_the_march_was_going(gpu: &Gpu, rm: &Raymarch, dir: &str) {
    let mut means = Vec::new();
    for steps in [8.0f32, 16.0, 48.0] {
        let px = shot(gpu, rm, 0.9, 18.0, steps);
        let (m, s) = stats(&px);
        println!("{steps:>3.0} steps: mean {m:.4} spread {s:.4}");
        save(&px, &format!("{dir}/fog_noise_{steps:.0}steps.png"));
        means.push(m);
    }
    let lo = means.iter().cloned().fold(f32::MAX, f32::min);
    let hi = means.iter().cloned().fold(0.0f32, f32::max);
    let spread = (hi - lo) / hi.max(1e-4);
    println!("mean across step counts varies by {:.2}%", spread * 100.0);
    assert!(
        spread < 0.08,
        "the same fog marched at 8, 16 and 48 steps must come out the same brightness — \
         a stride or an octave drop that changed the density would move one of these: \
         {means:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. A dropped octave contributes its MEAN, not nothing.
//
// `cloud_fbm_lod` stops sampling octaves finer than a step can resolve. Dropping
// them outright would remove their average as well as their variation, and the
// fog would visibly THIN wherever the steps got long — a gradient toward the
// horizon that reads as a bug in the fog rather than as a sampling decision.
//
// A fine noise scale next to a coarse one exercises the drop: at scale 2 over a
// 40-unit ray the finest octaves are far below the step size, at scale 18 they
// are not. Both must carry the same amount of fog.
fn dropping_an_octave_does_not_thin_the_fog(gpu: &Gpu, rm: &Raymarch) {
    let coarse = stats(&shot(gpu, rm, 0.9, 18.0, 16.0)).0;
    let fine = stats(&shot(gpu, rm, 0.9, 2.0, 16.0)).0;
    println!("scale 18: mean {coarse:.4}   scale 2: mean {fine:.4}");
    let d = (coarse - fine).abs() / coarse.max(1e-4);
    assert!(
        d < 0.12,
        "fog whose noise is too fine to resolve must still be as THICK as coarse fog — \
         it is the detail that is dropped, not the density: {coarse:.4} vs {fine:.4}"
    );
}

// ---- the frame ----------------------------------------------------------------

/// One fog frame: `noise` amount, noise `scale`, march `steps`. A lamp sits in
/// the fog so the light-injection path runs too — the stride lives inside the
/// same loop.
fn shot(gpu: &Gpu, rm: &Raymarch, noise: f32, scale: f32, steps: f32) -> Vec<[u8; 4]> {
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 1.0, near: 0.1, far: 500.0 },
    );
    let vp = cam.view_proj(1.0);
    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    let mut point_count = [0.0f32; 4];
    point_pos[0] = [0.0, 0.0, -12.0, 30.0];
    point_color[0] = [3.0, 2.6, 2.0, 0.0];
    point_count[0] = 1.0;

    let globals = RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_dir: [0.0, -1.0, 0.0, 0.0],
        // The sun is off, as it is in every interior — and as it was in the
        // scene this optimisation came from.
        light_color: [0.0, 0.0, 0.0, 0.0],
        ambient: [0.02, 0.02, 0.03, 0.0],
        bg: [0.0, 0.0, 0.0, 1.0],
        sky_params: [1.0, 1.0, 0.0, 0.0],
        sky_tint: [1.0, 1.0, 1.0, 1.0],
        shadow_params: [1.0, 32.0, 1.0, 150.0],
        point_count,
        point_pos,
        point_color,
        // Dither off: it would add its own variation to a test about variation.
        fog_color: [FOG_COLOR[0], FOG_COLOR[1], FOG_COLOR[2], 0.0],
        fog_params: [0.0, FAR, 1.0, 0.0],
        // [density, layer top, falloff, noise amount] — a layer top far overhead
        // so height plays no part and the noise is the only thing modulating it.
        vol_fog_a: [DENSITY, 1000.0, 1.0, noise],
        // [noise scale, drift, camera height, volumetric on]
        vol_fog_b: [scale, 0.0, 0.0, 1.0],
        // [amount, anisotropy, steps, shafts]
        vol_fog_c: [1.0, 0.0, steps, 0.0],
        ..Default::default()
    };

    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("fog-noise"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    rm.draw_into(gpu, &view, gpu.depth_view(), globals);
    read_rgba(gpu, &tex)
}

/// Mean and standard deviation of the frame's linear luminance.
fn stats(px: &[[u8; 4]]) -> (f32, f32) {
    let v: Vec<f32> = px.iter().map(|p| (srgb(p[0]) + srgb(p[1]) + srgb(p[2])) / 3.0).collect();
    let m = v.iter().sum::<f32>() / v.len() as f32;
    let var = v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len() as f32;
    (m, var.sqrt())
}

fn srgb(b: u8) -> f32 {
    let c = b as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn save(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flatten().copied().collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("header").write_image_data(&flat).expect("write");
}

fn read_rgba(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (S * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("fog-noise-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("fog-noise-readback"),
    });
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
    let mut out = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}
