//! Regression probe for the skinning × depth-prepass seam (`floptle/0100`).
//!
//! The depth prepass draws skinned parts through `skin_prepass_pipeline`, which
//! reads the bone palette out of the globals bind group — but it used to set its
//! globals WITHOUT publishing this frame's palette, so it primed depth from
//! wherever the character was LAST frame. This frame's triangles then depth-fail
//! against their own stale silhouette, and a moving character flickers.
//!
//! The invariant this checks needs no golden image: **a pose must render the
//! same whether or not the previous frame differed from it.** Frame 2 and frame
//! 3 draw the identical pose; frame 1 draws a different one. With the bug frame
//! 2 inherits frame 1's depth and frame 3 does not, so the two disagree.
//!
//! Run: cargo run -p floptle-render --example skin_prepass_probe -- <out-prefix>

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of_mat, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster,
    RenderCamera, SkinDraw, TexId,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 640;
const H: u32 = 360;

fn main() {
    let prefix = std::env::args().nth(1).unwrap_or_else(|| "skin_prepass".into());
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
    let body = cube(0.9);
    let nverts = body.vertices.len();
    let mesh = raster.register(&gpu, &body, None);

    // One joint driving every vertex: the simplest rig that still goes down the
    // whole GPU-skinning path. What is under test is WHEN the palette reaches
    // the GPU, not how many bones blend.
    let joints = vec![[0u16; 4]; nverts];
    let weights = vec![[1.0f32, 0.0, 0.0, 0.0]; nverts];
    let skin_base = raster.register_skin(&gpu, &joints, &weights);
    assert!(skin_base != 0, "the skin store refused the part — the probe would test the CPU path");

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 6.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.05, far: 400.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.25, 0.25, 0.3, 0.0],
        ..Default::default()
    };

    let model = Transform::from_translation(DVec3::ZERO).render_matrix(cam.world_position);
    let mp = MaterialParams::flat([0.8, 0.35, 0.3]);
    let raw = instance_of_mat(model, &mp);

    // The stale pose must sit IN FRONT of the live one at the same pixels, or a
    // stale prepass writes depth where nothing is drawn and the bug is invisible.
    // +Z is toward the camera.
    let near_pose = Mat4::from_translation(Vec3::new(0.0, 0.0, 2.5));
    let live_pose = Mat4::IDENTITY;

    let mut frame = |pose: Mat4| -> Vec<u8> {
        raster.begin_skin_frame();
        let idx = raster.push_skin_pose(skin_base, pose, &[pose]);
        let skins = vec![SkinDraw { mesh, tex: None, instance: raw, pose: idx }];
        let plain: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        // The editor's order: prepass primes the main depth, the color pass
        // LOADS it under LessEqual.
        raster.depth_prepass_with(&gpu, globals, &plain, &[], &skins, gpu.depth_texture());
        clear_color(&gpu, &color_view, [0.02, 0.02, 0.05, 1.0]);
        raster.draw_scene_with(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &plain,
            &[],
            &skins,
            None,
            None,
        );
        read_pixels(&gpu, &color_tex)
    };

    frame(near_pose); // frame 1 — a DIFFERENT pose, to leave stale palette behind
    let after_move = frame(live_pose); // frame 2 — the pose under test
    let settled = frame(live_pose); // frame 3 — same pose, stale palette now agrees

    save_png(&after_move, &format!("{prefix}_after_move.png"));
    save_png(&settled, &format!("{prefix}_settled.png"));

    let differing = after_move
        .as_chunks::<4>().0
        .iter()
        .zip(settled.as_chunks::<4>().0)
        .filter(|(a, b)| a[0].abs_diff(b[0]) > 4 || a[1].abs_diff(b[1]) > 4 || a[2].abs_diff(b[2]) > 4)
        .count();
    // Sanity: the pose is actually on screen, so "identical" means something.
    // Count the CUBE, not brightness — the cleared background is dark but far
    // from black, and a brightness threshold happily counts the whole frame.
    // The cube is warm (0.8, 0.35, 0.3), the background cold.
    let body_px = settled
        .as_chunks::<4>().0
        .iter()
        .filter(|p| p[0] as i32 > p[2] as i32 + 30)
        .count();
    println!("{body_px} cube px, {differing} px differ between the moved-into and settled frames");
    assert!(
        body_px > 2_000,
        "the skinned cube is not on screen ({body_px} px) — the probe proves nothing"
    );
    assert_eq!(
        differing, 0,
        "the same pose rendered differently depending on the previous frame: the depth prepass is \
         posing from a stale bone palette"
    );
    println!("ok — a pose renders the same whichever pose preceded it");
}

fn clear_color(gpu: &Gpu, view: &wgpu::TextureView, c: [f64; 4]) {
    let mut encoder =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color { r: c[0], g: c[1], b: c[2], a: c[3] }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    gpu.queue.submit([encoder.finish()]);
}

fn read_pixels(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
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
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    pixels
}

fn save_png(pixels: &[u8], path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(pixels).unwrap();
}
