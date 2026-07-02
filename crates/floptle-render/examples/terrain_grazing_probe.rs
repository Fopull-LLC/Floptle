//! Headless GRAZING-ANGLE terrain probe — the case that exposed transparent gaps
//! along hill/ravine silhouettes. The camera sits low and looks roughly horizontally
//! across tall hills and a deep ravine, so the terrain edges are seen at a grazing
//! angle. The background is vivid MAGENTA: any transparent gap punched through the
//! terrain shows up as magenta pixels *inside* the terrain body, easy to spot.
//!
//! Run: cargo run -p floptle-render --example terrain_grazing_probe -- <out.png>

use floptle_field::{Brush, Terrain};
use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 900;
const H: u32 = 420;

fn flat256() -> TextureData {
    let n = 256u32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let on = ((x / 16) + (y / 16)) % 2 == 0;
            let c = if on { [150, 150, 158, 255] } else { [110, 112, 120, 255] };
            px.extend_from_slice(&c);
        }
    }
    TextureData { pixels: px, width: n, height: n }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "terrain_grazing.png".into());
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

    // Tall hills + a deep narrow ravine — lots of grazing silhouette edges.
    let mut terrain = Terrain::flat([112, 48, 112], [0.0, 0.0, 0.0], [16.0, 7.0, 16.0], 0.0, [0.35, 0.6, 0.28]);
    for _ in 0..40 {
        terrain.sculpt(Brush::Raise, [-6.0, 1.0, -2.0], 4.5, 1.0);
        terrain.sculpt(Brush::Raise, [5.0, 2.0, -7.0], 3.0, 1.0);
        terrain.sculpt(Brush::Raise, [8.0, 1.0, 4.0], 3.5, 1.0);
        terrain.sculpt(Brush::Lower, [0.5, 0.0, 1.0], 2.2, 1.0); // deep ravine
        terrain.sculpt(Brush::Lower, [2.0, 0.0, -3.0], 1.8, 1.0);
    }

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[flat256()]);
    raymarch.set_volume(&gpu, &terrain.baked);

    // Low camera, looking nearly horizontally across the relief (grazing).
    let cam_pos = DVec3::new(1.5, 2.6, 18.0);
    let target = Vec3::new(0.0, 1.8, 0.0);
    let fwd = (target - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 52f32.to_radians(), near: 0.05, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let cr = (DVec3::ZERO - cam.world_position).as_vec3();

    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.25, 0.27, 0.33, 0.0],
        bg: [1.0, 0.0, 1.0, 1.0], // MAGENTA — any terrain gap shows as magenta
        center: [0.0; 4],
        params: [0.0, 0.0, 0.0, 0.0],
        vol_center: { let mut a = [[0.0f32; 4]; 16]; a[0] = [cr.x, cr.y, cr.z, 1.0]; a },
        vol_half: { let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16]; a[0] = [16.0, 7.0, 16.0, 0.1]; a },
        blobs: [[0.0; 4]; 16],
        ..Default::default() // neutral terrain material
    };
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — grazing hills + ravine (magenta bg = gap detector)");
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
