//! Alpha-cutout probe: a plane with a transparent-background texture must show the
//! background THROUGH the transparent texels, not composite them as black.
//!
//! The bug: the opaque pass doesn't blend, so a transparent PNG's see-through pixels
//! (usually black RGB, alpha 0) were written straight to the target as solid black. The
//! fix is a hard alpha cutout in `fs` for opaque materials — the retro-correct answer.
//!
//! Run: cargo run -p floptle-render --example alpha_cutout_probe -- <out.png>

use floptle_render::{
    instance_of_mat, plane, Globals, Gpu, MaterialParams, MeshData, Projection, Raster,
    RenderCamera, TexId, TexSampling, TextureData,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 256;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "alpha_cutout.png".into());
    let gpu = Gpu::headless(S, S);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cutout-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    // A texture that is opaque RED on its LEFT half and fully TRANSPARENT (black RGB,
    // alpha 0 — a typical PNG background) on its RIGHT half.
    let mut pixels = Vec::with_capacity((64 * 64 * 4) as usize);
    for _y in 0..64 {
        for x in 0..64 {
            if x < 32 {
                pixels.extend_from_slice(&[220, 40, 40, 255]);
            } else {
                pixels.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels, width: 64, height: 64 },
        TexSampling::default(),
    );

    let data: MeshData = plane(1.5);
    let mesh = raster.register(&gpu, &data, None);
    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };

    let eye = Vec3::new(0.0, 0.0, 3.0);
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 0.8, near: 0.02, far: 100.0 },
    );
    let raw = instance_of_mat(Mat4::from_translation(-eye), &mat);
    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        ..Default::default()
    };
    // BLUE background: the transparent half must reveal THIS, not black.
    raster.draw_scene(
        &gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        &[(mesh, Some::<TexId>(tex), raw)],
        Some([0.1, 0.2, 0.8, 1.0]),
        None,
    );

    let raw = readback(&gpu, &color);
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let px: Vec<[u8; 4]> =
        raw.into_iter().map(|p| if bgra { [p[2], p[1], p[0], p[3]] } else { p }).collect();
    let at = |fx: f32, fy: f32| px[((fy * S as f32) as u32 * S + (fx * S as f32) as u32) as usize];

    let left = at(0.30, 0.5); // over the opaque red half of the plane
    let right = at(0.70, 0.5); // over the transparent half — must be BACKGROUND
    println!("left (opaque texel) {left:?}   right (transparent texel) {right:?}");

    assert!(
        left[0] > 150 && left[2] < 80,
        "the opaque half of the texture should be RED, got {left:?}"
    );
    // The transparent half reveals the blue background (sRGB-encoded, so its red channel
    // is ~89, not 0). The bug rendered it near-black; assert blue clearly dominates.
    let (r, b) = (right[0] as i32, right[2] as i32);
    assert!(
        b > 150 && b > r + 60,
        "the transparent half must reveal the BLUE background, got {right:?} — \
         a transparent-background texture is compositing as black"
    );

    save_png(&px, &out);
    println!("alpha cutout OK; wrote {out}");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
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
    let mut o = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * bpp) as usize;
            o.push([view[i], view[i + 1], view[i + 2], view[i + 3]]);
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
