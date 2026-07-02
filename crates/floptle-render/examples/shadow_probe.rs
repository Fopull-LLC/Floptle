//! Headless probe for SDF sun shadows — the full receive/cast matrix on one scene:
//! a tall western hill (field matter) casting east across flat ground with the sun
//! low in the west, a raster cube parked INSIDE that shadow band (meshes RECEIVE
//! field shadows), a second cube + a standing capsule out on open ground casting
//! their own shadows via collider-proxy occluders (meshes CAST), and a blob
//! (field matter) shadowing terrain. Rendered as a style matrix:
//!
//!   shadow_off.png    — everything lit (the baseline)
//!   shadow_soft.png   — dreamy-soft penumbra (low k)
//!   shadow_hard.png   — razor-hard retro edge (high k)
//!   shadow_retro.png  — hard + 2-band quantize + Bayer dither (the PS1 edge)
//!   shadow_tint.png   — soft purple-dusk shadows at strength 0.9
//!   shadow_full.png   — soft shadows + SDF AO together (the combined look)
//!
//! Run: cargo run -p floptle-render --example shadow_probe

use floptle_core::transform::Transform;
use floptle_field::{Brush, Terrain};
use floptle_render::{
    capsule, cube, instance_of, Globals, Gpu, InstanceRaw, MeshId, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TexId, TextureData,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1024;
const H: u32 = 640;

fn white256() -> TextureData {
    TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 }
}

fn main() {
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

    // One grassy slab with a tall hill on its western edge; the rest stays flat so
    // the hill's cast shadow (and the proxies') reads cleanly.
    let mut t = Terrain::flat([128, 48, 128], [0.0; 3], [16.0, 6.0, 16.0], 0.0, [0.35, 0.6, 0.28]);
    for _ in 0..45 {
        t.sculpt(Brush::Raise, [-9.0, 1.0, 0.0], 4.5, 1.0);
    }

    let mut raster = Raster::new(&gpu);
    let cube_id = raster.register(&gpu, &cube(0.7), None);
    let capsule_id = raster.register(&gpu, &capsule(0.5, 0.55, 16, 24), None);
    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white256()]);
    assert_eq!(raymarch.set_volumes(&gpu, &[&t.baked]), 1);

    // Camera: south-east, high enough to see the whole shadow band.
    let target = Vec3::new(-1.0, 0.0, 0.0);
    let cam_pos = DVec3::new(8.0, 12.0, 24.0);
    let fwd = (target - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    // The sun sits low in the west, so the hill throws east across the flat.
    let light = Vec3::new(-1.0, 0.42, 0.18).normalize();

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.22, 0.24, 0.3, 0.0],
        ..Default::default()
    };

    // Raster meshes: one cube inside the hill's shadow band (RECEIVES), one cube +
    // a standing capsule on open sunny ground (CAST via their proxies).
    let mesh = |id: MeshId, pos: [f64; 3], color: [f32; 3]| -> (MeshId, Option<TexId>, InstanceRaw) {
        let tr = Transform::from_translation(DVec3::from(pos));
        (id, None, instance_of(tr.render_matrix(cam.world_position), color))
    };
    let cube_shadowed = [-1.0, 0.7, 0.0]; // inside the hill's shadow
    let cube_sunny = [7.0, 0.7, 6.0]; // open ground
    let capsule_pos = [5.0, 1.05, -6.0]; // "character" standing on the flat
    let instances = vec![
        mesh(cube_id, cube_shadowed, [0.9, 0.45, 0.35]),
        mesh(cube_id, cube_sunny, [0.4, 0.7, 0.95]),
        mesh(capsule_id, capsule_pos, [0.95, 0.85, 0.4]),
    ];

    // Their collider proxies, exactly as the editor harvests them (box per cube,
    // capsule segment for the capsule).
    let rel = |p: [f64; 3]| (DVec3::from(p) - cam.world_position).as_vec3();
    let mut prox_a = [[0.0f32; 4]; 32];
    let mut prox_b = [[0.0f32; 4]; 32];
    let prox_rot = [[0.0f32, 0.0, 0.0, 1.0]; 32];
    let c = rel(cube_shadowed);
    prox_a[0] = [c.x, c.y, c.z, 0.0];
    prox_b[0] = [0.7, 0.7, 0.7, 2.0];
    let c = rel(cube_sunny);
    prox_a[1] = [c.x, c.y, c.z, 0.0];
    prox_b[1] = [0.7, 0.7, 0.7, 2.0];
    let c = rel(capsule_pos);
    prox_a[2] = [c.x, c.y - 0.55, c.z, 0.5];
    prox_b[2] = [c.x, c.y + 0.55, c.z, 1.0];

    // A blob (field matter) resting on the flat — shadows terrain from within the field.
    let mut blobs = [[0.0f32; 4]; 16];
    let bp = rel([1.5, 0.8, -8.0]);
    blobs[0] = [bp.x, bp.y, bp.z, 1.0];

    let bc = t.baked.center;
    let hf = t.baked.half_extent;
    let vc = (DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam.world_position).as_vec3();
    let mut vol_center = [[0.0f32; 4]; 16];
    let mut vol_half = [[1.0f32, 1.0, 1.0, 0.5]; 16];
    vol_center[0] = [vc.x, vc.y, vc.z, 1.0];
    vol_half[0] = [hf[0], hf[1], hf[2], 0.6];

    let base = RaymarchGlobals {
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
        prox_count: [3.0, 0.0, 0.0, 0.0],
        prox_a,
        prox_b,
        prox_rot,
        ..Default::default()
    };

    // (name, shadow_params [on, k, strength, dist], shadow_tint [rgb, quantize],
    //  shadow_extra [dither], ao on?)
    let frames: [(&str, [f32; 4], [f32; 4], f32, bool); 6] = [
        ("shadow_off.png", [0.0, 12.0, 1.0, 150.0], [0.0, 0.0, 0.0, 0.0], 0.0, false),
        ("shadow_soft.png", [1.0, 4.0, 1.0, 150.0], [0.0, 0.0, 0.0, 0.0], 0.0, false),
        ("shadow_hard.png", [1.0, 64.0, 1.0, 150.0], [0.0, 0.0, 0.0, 0.0], 0.0, false),
        ("shadow_retro.png", [1.0, 24.0, 1.0, 150.0], [0.0, 0.0, 0.0, 2.0], 1.0, false),
        ("shadow_tint.png", [1.0, 8.0, 0.9, 150.0], [0.45, 0.25, 0.6, 0.0], 0.0, false),
        ("shadow_full.png", [1.0, 8.0, 1.0, 150.0], [0.0, 0.0, 0.0, 0.0], 0.0, true),
    ];
    for (name, params, tint, dither, ao) in frames {
        let mut rm = base;
        rm.shadow_params = params;
        rm.shadow_tint = tint;
        rm.shadow_extra = [dither, 0.0, 0.0, 0.0];
        rm.ao_params = [if ao { 1.0 } else { 0.0 }, 0.85, 1.5, 0.0];
        // Same order + plumbing as the editor: raymarch draws (and uploads the
        // globals the field group reads), then the meshes compose in, marching the
        // same field for their received shadows.
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
        raster.draw_scene(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &instances,
            None,
            Some(raymarch.field_bind()),
        );
        save_png(&gpu, &color_tex, name);
        println!("wrote {name}");
    }
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
