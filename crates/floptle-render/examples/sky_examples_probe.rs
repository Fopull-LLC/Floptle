//! Contact sheet for every BUILT-IN Sky-stage example shader: each one is
//! compiled through the production path, naga-validated against the real
//! raymarch splice, and rendered at three times into one grid PNG (columns =
//! shaders, rows = t seconds) — so a glance shows the look AND that it moves.
//!
//! Asserts, per shader: the frame isn't flat (the sky actually drove the
//! pixels) and the t=2 vs t=14 frames differ (the animation actually runs).
//!
//! Run: cargo run -p floptle-render --example sky_examples_probe -- <out.png>

use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 400;
const H: u32 = 250;
// 7.73 lands right at a stormNight lightning-cycle start (fract(7.73·2.2) ≈ 0)
// so the sheet actually shows a strike.
const TIMES: [f32; 3] = [2.0, 7.73, 14.0];

/// Where to point the camera per shader (sun/moon/band directions differ).
fn look_dir(name: &str) -> Vec3 {
    match name {
        "dayBreeze.flsl" => Vec3::new(0.55, 0.28, 0.25),
        "sunsetStreaks.flsl" => Vec3::new(0.9, 0.14, 0.15),
        "stormNight.flsl" => Vec3::new(0.2, 0.25, 1.0),
        "starryNight.flsl" => Vec3::new(0.8, 0.35, -0.45),
        "moonlitClouds.flsl" => Vec3::new(0.4, 0.42, 0.3),
        "auroraVeil.flsl" => Vec3::new(0.0, 0.3, 1.0),
        "retroSun.flsl" => Vec3::new(0.0, 0.04, 1.0),
        _ => Vec3::new(0.3, 0.35, 1.0),
    }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "sky_examples.png".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sky-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let skies: Vec<(&str, floptle_shader::transpile::CompiledSky)> = floptle_shader::examples::EXAMPLES
        .iter()
        .filter_map(|(name, src)| floptle_shader::compile_sky(src).ok().map(|c| (*name, c)))
        .collect();
    assert!(skies.len() >= 8, "expected the sky example pack, found {}", skies.len());

    let cols = skies.len();
    let rows = TIMES.len();
    let mut sheet = vec![[0u8; 4]; cols * W as usize * rows * H as usize];
    let sheet_w = cols * W as usize;

    let mut raymarch = Raymarch::new(&gpu);
    for (col, (name, sky)) in skies.iter().enumerate() {
        // Validate the REAL splice before swapping it in (what the editor does).
        let src = Raymarch::preview_sky_source(&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL);
        floptle_shader::validate_module(&src)
            .unwrap_or_else(|e| panic!("{name}: spliced sky module invalid: {}", e.message));
        raymarch.set_sky_shader(&gpu, Some((&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL)));

        let mut sky_uniforms = [[0.0f32; 4]; 16];
        for (i, u) in sky.uniforms.iter().enumerate().take(16) {
            sky_uniforms[i] = u.default;
        }

        // Roll-free look-at: from_rotation_arc(NEG_Z, fwd) flips/rolls for +z-ish
        // forwards (near-antiparallel arc), so build yaw·pitch explicitly.
        let fwd = look_dir(name).normalize();
        let rot = Quat::from_rotation_y((-fwd.x).atan2(-fwd.z)) * Quat::from_rotation_x(fwd.y.asin());
        let cam = RenderCamera::new(
            DVec3::new(0.0, 1.0, 0.0),
            rot,
            Projection::Perspective { fov_y: 75f32.to_radians(), near: 0.05, far: 2000.0 },
        );
        let view_proj = cam.view_proj(W as f32 / H as f32);

        let mut frames: Vec<Vec<[u8; 4]>> = Vec::new();
        for (row, t) in TIMES.iter().enumerate() {
            let rm = RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                bg: [1.0, 0.0, 1.0, 1.0], // magenta — must be overridden by the sky
                params: [*t, 0.0, 0.0, 0.0],
                sky_meta: [1.0, 0.0, 0.0, 0.0],
                sky_uniforms,
                ..Default::default()
            };
            raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
            let px = readback(&gpu, &color_tex);
            for y in 0..H as usize {
                let dst = (row * H as usize + y) * sheet_w + col * W as usize;
                let srcr = y * W as usize;
                sheet[dst..dst + W as usize].copy_from_slice(&px[srcr..srcr + W as usize]);
            }
            frames.push(px);
        }

        // Keep the sheet on disk even when an assert below trips — tuning needs eyes.
        save_png(&sheet, sheet_w as u32, (rows * H as usize) as u32, &out);

        // Not flat: the sky wrote a real image, not one color.
        let lum = |p: [u8; 4]| p[0] as f32 * 0.3 + p[1] as f32 * 0.59 + p[2] as f32 * 0.11;
        let mean = frames[0].iter().map(|p| lum(*p)).sum::<f32>() / frames[0].len() as f32;
        let var = frames[0].iter().map(|p| (lum(*p) - mean).powi(2)).sum::<f32>()
            / frames[0].len() as f32;
        assert!(var.sqrt() > 4.0, "{name}: frame is flat (σ = {:.2}) — sky not applied?", var.sqrt());
        // Animated: first and last times differ somewhere meaningful.
        let moved = frames[0]
            .iter()
            .zip(&frames[2])
            .filter(|(a, b)| (lum(**a) - lum(**b)).abs() > 8.0)
            .count();
        assert!(
            moved > (W * H / 200) as usize,
            "{name}: only {moved} pixels changed between t={} and t={} — is it animated?",
            TIMES[0],
            TIMES[2]
        );
        println!("{name}: σ {:.1}, {moved} moving px — OK", var.sqrt());
    }

    save_png(&sheet, sheet_w as u32, (rows * H as usize) as u32, &out);
    println!("sky examples OK ({} shaders × {} times); wrote {out}", cols, rows);
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded = (W * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(H) } },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(gpu.config.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
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
