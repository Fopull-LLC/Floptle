//! Headless probe for INFINITE-TERRAIN expansion: start from a small slab, sculpt
//! a ridge that runs off the +X / -Z edges while calling `ensure_contains` (as the
//! editor does), then raymarch using the field's *own* center/half_extent (which
//! the box has shifted as it grew). Validates the grow + render-box-offset path.
//!
//! Run: cargo run -p floptle-render --example terrain_expand_probe -- <out.png>

use floptle_field::{Brush, Terrain};
use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1024;
const H: u32 = 640;

fn white256() -> TextureData {
    TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "terrain_expand.png".into());
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

    // Start with a SMALL slab (box half-extent 8 → reaches only ±8 on X/Z).
    let mut terrain = Terrain::flat([64, 36, 64], [0.0, 0.0, 0.0], [8.0, 6.0, 8.0], 0.0, [0.35, 0.6, 0.28]);
    let before = terrain.baked.dims;
    // Walk a ridge of dabs from the origin out past +X=20 and -Z=-20 — far beyond
    // the original ±8 box. Each dab first grows the field to contain the brush.
    let mut x = -6.0f32;
    let mut z = 6.0f32;
    for _ in 0..40 {
        let c = [x, 0.6, z];
        terrain.ensure_contains(c, 3.0 * 1.5);
        terrain.sculpt(Brush::Raise, c, 3.0, 1.0);
        x += 0.7;
        z -= 0.7;
    }
    let after = terrain.baked.dims;
    println!("dims {before:?} -> {after:?}, center {:?}, half {:?}", terrain.baked.center, terrain.baked.half_extent);

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white256()]);
    raymarch.set_volume(&gpu, &terrain.baked);

    let cam_pos = DVec3::new(6.0, 16.0, 30.0);
    let fwd = (Vec3::new(4.0, 0.0, -4.0) - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    // Box center is the field's own (shifted) center, camera-relative — the exact
    // formula the editor uses now.
    let bc = terrain.baked.center;
    let hf = terrain.baked.half_extent;
    let cr = (DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam.world_position).as_vec3();

    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.22, 0.24, 0.3, 0.0],
        bg: [0.5, 0.62, 0.78, 1.0],
        center: [0.0; 4],
        params: [0.0, 0.0, 0.0, 0.0],
        vol_center: [cr.x, cr.y, cr.z, 1.0],
        vol_half: [hf[0], hf[1], hf[2], 0.1],
        blobs: [[0.0; 4]; 16],
    };
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — expanded terrain ridge");
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
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
    let mut encoder =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
