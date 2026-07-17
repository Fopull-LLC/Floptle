//! Repro probe for the terrain shadow-acne stripes: "weird black line patterns ...
//! from certain angles and/or up close".
//!
//! `light_vis` lifts the shadow ray off the surface by `max(0.03, eps*1.6)` scaled up
//! to 4× when the sun grazes. This probe sweeps the SUN ELEVATION from steep to nearly
//! flat with the camera close to the ground — the condition where the ray hugs the
//! noisy f16 terrain shell and stripes.
//!
//! It reports the darkest patch found on ground that should be FULLY LIT. Acne shows up
//! as dark banding on open, unoccluded terrain, so a lit-flat sample that comes back
//! dark is the bug, measured rather than eyeballed.
//!
//! Run: cargo run -p floptle-render --example terrain_acne_probe -- <out-prefix>

use floptle_field::{Brush, BrushProfile, Terrain};
use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 640;
const H: u32 = 360;

fn white256() -> TextureData {
    TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 }
}

fn main() {
    let prefix = std::env::args().nth(1).unwrap_or_else(|| "acne".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("acne-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    // Gently rolling ground — NOT flat. A perfectly flat field hides acne; real
    // sculpted terrain has exactly the low-amplitude noise that trips it.
    let mut t = Terrain::flat([128, 48, 128], [0.0; 3], [16.0, 6.0, 16.0], 0.0, [0.5, 0.55, 0.45]);
    // Low, broad swells spread over the far half — the near ground stays open so the
    // camera sees a wide, unoccluded, nearly-flat expanse. Grazing sun over open
    // gentle ground is exactly where the shell stripes.
    for i in 0..10 {
        let a = i as f32 * 2.399;
        t.sculpt(
            Brush::Raise,
            [a.cos() * 9.0, 0.15, -4.0 + a.sin() * 5.0],
            4.0,
            0.35,
            BrushProfile::default(),
        );
    }

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white256()]);
    assert_eq!(raymarch.set_volumes(&gpu, &[&t.baked]), 1);

    // Camera LOW and close, looking across the ground — the "up close / certain
    // angles" half of the report.
    let cam_pos = DVec3::new(0.0, 2.2, 13.0);
    let target = Vec3::new(0.0, 0.0, -1.0);
    let fwd = (target - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.05, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    // The volume's CAMERA-RELATIVE box (ADR-0015) — the caller supplies these; only
    // vol_atlas/vol_dims are renderer-patched.
    let cr = (DVec3::ZERO - cam_pos).as_vec3();

    // (bands, dither) — the retro posterize knobs on the Lighting node. `sun_shadow`
    // bands the PENUMBRA, so band boundaries are iso-contours of visibility: curved
    // lines across curved ground, with bayer4 stippling between adjacent bands.
    println!("penumbra quantize sweep (sun elev 0.15, camera low + close):");
    for &(bands, dither) in &[(0.0f32, 0.0f32), (4.0, 0.0), (4.0, 1.0), (2.0, 1.0)] {
        let light = Vec3::new(-1.0, 0.15, 0.15).normalize();
        let rg = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            ambient: [0.04, 0.04, 0.05, 0.0],
            bg: [0.5, 0.62, 0.78, 1.0],
            params: [0.0, 0.0, 0.0, 0.0],
            vol_center: { let mut a = [[0.0f32; 4]; 16]; a[0] = [cr.x, cr.y, cr.z, 1.0]; a },
            vol_half: { let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16]; a[0] = [16.0, 6.0, 16.0, 0.1]; a },
            terrain_tint: [1.0, 1.0, 1.0, 0.0],
            terrain_params: [16.0, 0.0, 0.0, 1.0],
            shadow_params: [1.0, 12.0, 1.0, 150.0],
            shadow_tint: [0.0, 0.0, 0.0, bands],
            shadow_extra: [dither, 0.0, 0.0, 0.0],
            ao_params: [0.0, 0.85, 1.5, 0.0],
            ..Default::default()
        };
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg);
        let px = readback(&gpu, &color_tex);
        let (mut lo, mut hi) = (255u8, 0u8);
        for y in (H * 40 / 100)..(H * 60 / 100) {
            for x in (W * 10 / 100)..(W * 90 / 100) {
                let l = px[(y * W + x) as usize][1];
                lo = lo.min(l);
                hi = hi.max(l);
            }
        }
        println!("  bands {bands} dither {dither}: hill band  min {lo:>3}  max {hi:>3}  spread {}", hi - lo);
        save_png(&px, &format!("{prefix}_q{}_d{}.png", bands as u32, dither as u32));
    }

    println!("sun elevation sweep (camera low + close):");
    let mut worst = (f32::INFINITY, 0.0f32);
    for &elev in &[0.9f32, 0.6, 0.4, 0.25, 0.15, 0.08, 0.04] {
        let light = Vec3::new(-1.0, elev, 0.15).normalize();
        let rg = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            // Low ambient so acne shows nearly undiluted.
            ambient: [0.04, 0.04, 0.05, 0.0],
            bg: [0.5, 0.62, 0.78, 1.0],
            params: [0.0, 0.0, 0.0, 0.0],
            vol_center: { let mut a = [[0.0f32; 4]; 16]; a[0] = [cr.x, cr.y, cr.z, 1.0]; a },
            vol_half: { let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16]; a[0] = [16.0, 6.0, 16.0, 0.1]; a },
            terrain_tint: [1.0, 1.0, 1.0, 0.0],
            terrain_params: [16.0, 0.0, 0.0, 1.0],
            // shadows ON, k = 12, strength 1, 150u march. Quantize and dither OFF —
            // so any banding here is the FIELD, not the retro posterize.
            shadow_params: [1.0, 12.0, 1.0, 150.0],
            shadow_tint: [0.0, 0.0, 0.0, 0.0],
            shadow_extra: [0.0; 4],
            ao_params: [0.0, 0.85, 1.5, 0.0], // AO off: isolate the shadow march
            ..Default::default()
        };
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg);
        let px = readback(&gpu, &color_tex);

        // Sample a horizontal band of GROUND in front of the camera that no hill
        // occludes. With the sun in the west and open ground, this should be smoothly
        // lit; acne turns it into stripes.
        let (mut lo, mut hi, mut sum, mut n) = (255u8, 0u8, 0u32, 0u32);
        for y in (H * 66 / 100)..(H * 80 / 100) {
            for x in (W * 10 / 100)..(W * 90 / 100) {
                let l = px[(y * W + x) as usize][1]; // green channel ≈ luminance here
                lo = lo.min(l);
                hi = hi.max(l);
                sum += l as u32;
                n += 1;
            }
        }
        let mean = sum as f32 / n as f32;
        // Contrast across a patch that should be uniformly lit = the acne signal.
        let spread = (hi - lo) as f32;
        println!(
            "  elev {elev:>4}: ground band  min {lo:>3}  max {hi:>3}  mean {mean:>5.1}  spread {spread:>5.1}"
        );
        if spread < worst.0 {
            worst = (spread, elev);
        }
        save_png(&px, &format!("{prefix}_elev{:02}.png", (elev * 100.0) as u32));
    }
    println!("wrote {prefix}_elev*.png");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = (W * bpp).div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
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
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * bpp) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
