//! Verifies `Raster::update_texture` — the in-place texture rewrite the texture-paint brush
//! uses to stamp per dab. Register a white texture, `update_texture` a red-left/blue-right
//! image into it, bind it to a plane, render, and assert the plane shows the UPDATED pixels
//! (not the original white).
//!
//! Run: cargo run -p floptle-render --example tex_update_probe -- <out.png>

use floptle_render::{
    instance_of_mat, plane, Globals, Gpu, MaterialParams, MeshData, Projection, Raster,
    RenderCamera, TexId, TexSampling, TextureData,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 256;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "tex_update.png".into());
    let gpu = Gpu::headless(S, S);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("c"),
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
    // Register WHITE 64², then UPDATE it in place to red|blue.
    let white = TextureData { pixels: vec![255; 64 * 64 * 4], width: 64, height: 64 };
    let tex = raster.register_texture(&gpu, &white, TexSampling::default());
    let mut painted = Vec::with_capacity(64 * 64 * 4);
    for _y in 0..64 {
        for x in 0..64 {
            painted.extend_from_slice(if x < 32 { &[220, 40, 40, 255] } else { &[40, 60, 220, 255] });
        }
    }
    raster.update_texture(&gpu, tex, &TextureData { pixels: painted, width: 64, height: 64 });

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
    let globals = Globals { view_proj: cam.view_proj(1.0).to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(
        &gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        &[(mesh, Some::<TexId>(tex), raw)],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );

    let px = readback(&gpu, &color);
    save_png(&px, &out);
    let at = |fx: f32, fy: f32| px[((fy * S as f32) as u32 * S + (fx * S as f32) as u32) as usize];
    let left = at(0.3, 0.5);
    let right = at(0.7, 0.5);
    println!("left {left:?} right {right:?}");
    assert!(left[0] > 150 && left[2] < 90, "left should be RED (updated), got {left:?}");
    assert!(right[2] > 150 && right[0] < 90, "right should be BLUE (updated), got {right:?}");
    println!("update_texture OK; wrote {out}");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded = (S * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(S) } },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(gpu.surface_format(), wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
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
    let file = std::fs::File::create(path).expect("create");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
