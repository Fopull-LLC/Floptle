//! Headless probe for SDF ("true") ambient occlusion on raymarched matter: two
//! fused terrains sculpted into neighboring hills (a crease between them) plus a
//! blob resting on the ground, rendered twice — AO off and AO on. With AO on, the
//! crease between the hills and the blob's contact ring must darken; open flat
//! ground and the sky must not change.
//!
//! Run: cargo run -p floptle-render --example sdf_ao_probe -- <off.png> <on.png>

use floptle_field::{Brush, Terrain};
use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1024;
const H: u32 = 640;

fn white256() -> TextureData {
    TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 }
}

fn main() {
    let out_off = std::env::args().nth(1).unwrap_or_else(|| "sdf_ao_off.png".into());
    let out_on = std::env::args().nth(2).unwrap_or_else(|| "sdf_ao_on.png".into());
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

    // Two overlapping grassy slabs; twin hills flanking a crease near the seam.
    let mut a = Terrain::flat([96, 40, 96], [0.0; 3], [12.0, 6.0, 12.0], 0.0, [0.35, 0.6, 0.28]);
    let mut b = Terrain::flat([96, 40, 96], [0.0; 3], [12.0, 6.0, 12.0], 0.0, [0.35, 0.6, 0.28]);
    for _ in 0..30 {
        a.sculpt(Brush::Raise, [5.0, 1.0, 0.0], 3.5, 1.0);
        b.sculpt(Brush::Raise, [-5.0, 1.0, 0.0], 3.5, 1.0);
    }
    let origins = [DVec3::new(0.0, 0.0, 0.0), DVec3::new(14.0, 0.0, 0.0)];
    let volumes = [&a, &b];

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white256()]);
    let n = raymarch.set_volumes(&gpu, &[&a.baked, &b.baked]);
    assert_eq!(n, 2, "both volumes must fit the atlas");

    let target = Vec3::new(7.0, 0.5, 0.0);
    let cam_pos = DVec3::new(7.0, 10.0, 22.0);
    let fwd = (target - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let mut vol_center = [[0.0f32; 4]; 16];
    let mut vol_half = [[1.0f32, 1.0, 1.0, 0.5]; 16];
    for (i, (t, o)) in volumes.iter().zip(origins).enumerate() {
        let bc = t.baked.center;
        let hf = t.baked.half_extent;
        let cr = (o + DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam.world_position)
            .as_vec3();
        vol_center[i] = [cr.x, cr.y, cr.z, 1.0];
        vol_half[i] = [hf[0], hf[1], hf[2], 0.6];
    }
    // One blob resting on the flat ground in front of the crease — its contact
    // ring must darken with AO on.
    let mut blobs = [[0.0f32; 4]; 16];
    let bp = (DVec3::new(7.0, 0.8, 4.0) - cam.world_position).as_vec3();
    blobs[0] = [bp.x, bp.y, bp.z, 1.0];

    let mut rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.22, 0.24, 0.3, 0.0],
        bg: [0.5, 0.62, 0.78, 1.0],
        center: [0.0; 4],
        params: [0.0, 1.0, 0.3, 0.0],
        vol_center,
        vol_half,
        blobs,
        ..Default::default()
    };

    rm.ao_params = [0.0, 0.85, 1.5, 0.0];
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    save_png(&gpu, &color_tex, &out_off);

    rm.ao_params = [1.0, 0.85, 1.5, 0.0];
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    save_png(&gpu, &color_tex, &out_on);

    println!("wrote {out_off} (SDF AO off) and {out_on} (SDF AO on)");
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
