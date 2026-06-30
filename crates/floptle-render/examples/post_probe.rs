//! Headless post-processing probe — renders a few bright emissive spheres on a dark
//! background into the PostStack input, then runs bloom + vignette and reads back the
//! result. Validates the post.wgsl passes + the ping-pong chain: the spheres should
//! glow (bloom) and the corners darken (vignette).
//!
//! Run: cargo run -p floptle-render --example post_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, PostSettings,
    PostStack, Projection, Raster, RenderCamera, TexId,
};
use glam::{DVec3, Quat};

const W: u32 = 960;
const H: u32 = 540;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "post.png".into());
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

    let mut raster = Raster::new(&gpu);
    let sphere = raster.register(&gpu, &uv_sphere(0.7, 32, 48), None);
    let post = PostStack::new(&gpu, W, H);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 8.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [0.4, 0.8, 0.5, 0.0],
        light_color: [0.5, 0.5, 0.55, 0.0],
        ambient: [0.05, 0.05, 0.07, 0.0],
        ..Default::default()
    };

    // Three bright emissive spheres (so bloom has something to bleed).
    let cols = [[3.0, 1.2, 0.4], [0.4, 2.6, 3.0], [2.8, 0.5, 2.6]];
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let x = -3.0 + i as f64 * 3.0;
            let mut m = MaterialParams::flat([0.05, 0.05, 0.05]);
            m.emissive = *c;
            m.emissive_strength = 1.0;
            let t = Transform::from_translation(DVec3::new(x, 0.0, 0.0));
            (sphere, None, instance_of_mat(t.render_matrix(cam.world_position), &m))
        })
        .collect();

    // Scene renders into the post input target, then the post chain → color_view.
    raster.draw_scene(&gpu, post.input_view(), gpu.depth_view(), globals, &instances, Some([0.02, 0.02, 0.04, 1.0]));
    let settings = PostSettings {
        bloom: true,
        bloom_threshold: 0.6,
        bloom_intensity: 1.1,
        vignette: true,
        vignette_strength: 0.6,
        vignette_radius: 0.6,
    };
    post.run(&gpu, &settings, &color_view);

    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — bloom + vignette over 3 emissive spheres");
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
