//! Headless retro + post-processing probe — the SSAO clutter scene rendered at a
//! low retro internal resolution, with SSAO + vignette, through both pipeline
//! orders. "old" upscales the scene first and runs post at window res (the AO is
//! smooth and blurry over hard chunky pixels — the mismatch this probe guards
//! against); "new" is the engine's real order: the whole post chain runs at the
//! retro internal resolution and the nearest-neighbor upscale comes last, so every
//! effect lands on the same pixel grid as the scene.
//!
//! Run: cargo run -p floptle-render --example retro_post_probe -- <old.png> <new.png>

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId,
    PostSettings, PostStack, Projection, Raster, RenderCamera, Retro, SsaoFrame, TexId,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 960;
const H: u32 = 540;
/// Retro internal rows — 135 at 16:9 gives 240×135, so each retro pixel is a
/// visible 4×4 block in the output.
const RETRO_H: u32 = 135;

fn main() {
    let out_old = std::env::args().nth(1).unwrap_or_else(|| "retro_post_old.png".into());
    let out_new = std::env::args().nth(2).unwrap_or_else(|| "retro_post_new.png".into());
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
    let box_mesh = raster.register(&gpu, &cube(1.0), None);
    let sphere = raster.register(&gpu, &uv_sphere(0.7, 32, 48), None);
    let retro = Retro::new(&gpu, RETRO_H);
    let (rw, rh) = retro.resolution();

    let cam = RenderCamera::new(
        DVec3::new(0.0, 2.2, 8.0),
        Quat::from_rotation_x(-0.18),
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let aspect = W as f32 / H as f32;
    let view_proj = cam.view_proj(aspect);
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [0.4, 0.8, 0.5, 0.0],
        light_color: [0.9, 0.88, 0.85, 0.0],
        ambient: [0.35, 0.35, 0.4, 0.0],
        ..Default::default()
    };

    // Same clutter as ssao_probe: floor, back wall, boxes/spheres in contact.
    let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
    let mut place = |mesh: MeshId, pos: [f64; 3], scale: [f32; 3], color: [f32; 3]| {
        let m = MaterialParams::flat(color);
        let mut t = Transform::from_translation(DVec3::from_array(pos));
        t.scale = Vec3::from_array(scale);
        instances.push((mesh, None, instance_of_mat(t.render_matrix(cam.world_position), &m)));
    };
    place(box_mesh, [0.0, -0.25, 0.0], [24.0, 0.5, 24.0], [0.75, 0.75, 0.72]); // floor
    place(box_mesh, [0.0, 3.0, -4.0], [24.0, 6.5, 0.5], [0.7, 0.72, 0.75]); // back wall
    place(box_mesh, [-2.2, 0.75, -1.5], [1.5, 1.5, 1.5], [0.8, 0.5, 0.4]);
    place(box_mesh, [0.2, 0.5, -2.8], [1.0, 1.0, 1.0], [0.45, 0.7, 0.5]);
    place(sphere, [2.2, 0.7, -1.0], [1.0, 1.0, 1.0], [0.5, 0.55, 0.8]);
    place(box_mesh, [3.9, 0.5, -2.6], [1.0, 1.0, 1.0], [0.75, 0.7, 0.45]);
    place(box_mesh, [5.05, 0.5, -2.6], [1.0, 1.0, 1.0], [0.75, 0.7, 0.45]);

    let proj = cam.proj_matrix(aspect);
    let ssao_frame = SsaoFrame {
        depth: retro.depth_view(),
        proj: proj.to_cols_array_2d(),
        inv_proj: proj.inverse().to_cols_array_2d(),
    };
    let settings = PostSettings {
        bloom: false,
        bloom_threshold: 1.0,
        bloom_intensity: 0.7,
        vignette: true,
        vignette_strength: 0.45,
        vignette_radius: 0.7,
        ssao: true,
        ssao_strength: 1.0,
        ssao_radius: 0.9,
        posterize_bands: 6,
        posterize_dither: true,
    };
    let clear = Some([0.55, 0.7, 0.9, 1.0]);

    // OLD order (the mismatch): scene at retro res, nearest-upscale to window res,
    // THEN post at window res — pixelated scene, smooth AO/vignette on top.
    let post_full = PostStack::new(&gpu, W, H);
    raster.draw_scene(&gpu, retro.color_view(), retro.depth_view(), globals, &instances, clear, None);
    retro.blit_to(&gpu, post_full.input_view());
    post_full.run(&gpu, &settings, Some(&ssao_frame), &color_view);
    save_png(&gpu, &color_tex, &out_old);

    // NEW order (the engine's): scene into a retro-sized pixel-perfect post
    // input, the whole chain at retro res into the retro color target, upscale
    // LAST. Pixel-perfect = per-retro-pixel AO with a tightened blur.
    let mut post_retro = PostStack::new(&gpu, rw, rh);
    post_retro.configure(&gpu, rw, rh, true);
    raster.draw_scene(&gpu, post_retro.input_view(), retro.depth_view(), globals, &instances, clear, None);
    post_retro.run(&gpu, &settings, Some(&ssao_frame), retro.color_view());
    retro.blit_to(&gpu, &color_view);
    save_png(&gpu, &color_tex, &out_new);

    println!("wrote {out_old} (post after upscale) and {out_new} (post before upscale)");
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
