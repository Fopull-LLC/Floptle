//! Headless render probe — renders reference meshes plus a centered SDF blob to a
//! PNG, so the raymarched look can be inspected and iterated without a window. This
//! drives the real `Raster` + `Raymarch` passes (the same code the editor runs), so
//! what the PNG shows is what the editor shows.
//!
//! Run:
//!   cargo run -p floptle-render --example blob_probe -- <out.png> [cam_dist] [blob_scale]
//!
//! e.g. `blob_probe shots/near.png 3 1.5` for a close-up of a 1.5-unit blob.

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of, uv_sphere, Globals, Gpu, InstanceRaw, MeshId, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TexId,
};
use glam::{DVec3, Quat, Vec3};
use floptle_render::probe::{save_png};

const W: u32 = 1024;
const H: u32 = 768;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "blob.png".into());
    let cam_dist: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(6.0);
    let blob_scale: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1.5);

    let gpu = Gpu::headless(W, H);

    // Offscreen, copyable color target (the depth target comes from the gpu).
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
    let cube_id = raster.register(&gpu, &cube(0.7), None);
    let sphere_id = raster.register(&gpu, &uv_sphere(0.85, 24, 36), None);
    let raymarch = Raymarch::new(&gpu);

    // ---- the scene: reference meshes around a centered blob ----
    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, cam_dist as f64),
        Quat::IDENTITY, // looks down -Z
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let aspect = W as f32 / H as f32;
    let view_proj = cam.view_proj(aspect);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.12, 0.12, 0.16, 0.0],
        ..Default::default()
    };

    let mesh = |id: MeshId, pos: [f64; 3], color: [f32; 3]| -> (MeshId, Option<TexId>, InstanceRaw) {
        let t = Transform::from_translation(DVec3::from(pos));
        (id, None, instance_of(t.render_matrix(cam.world_position), color))
    };
    let instances = vec![
        mesh(cube_id, [-2.8, 0.0, 0.0], [0.9, 0.45, 0.35]),
        mesh(sphere_id, [2.8, 0.0, 0.0], [0.4, 0.7, 0.95]),
        mesh(cube_id, [-1.7, -0.2, -3.0], [0.5, 0.85, 0.45]),
        mesh(sphere_id, [1.7, 0.25, -3.0], [0.95, 0.85, 0.4]),
    ];

    // The blob, centered at the origin (dead-center on screen).
    let mut blobs = [[0.0f32; 4]; 16];
    let c = (DVec3::ZERO - cam.world_position).as_vec3();
    blobs[0] = [c.x, c.y, c.z, blob_scale.max(0.05)];
    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.12, 0.12, 0.16, 0.0],
        bg: [0.02, 0.02, 0.05, 1.0],
        center: [0.0; 4],
        params: [0.0, 1.0, 0.0, 0.0],
        blobs,
        ..Default::default()
    };

    // Same draw order as the editor: blob first (clears + writes depth), then the
    // meshes compose into the shared depth buffer.
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, None, None);

    save_png(&gpu, &color_tex, &out);
    println!("wrote {out}  (cam_dist={cam_dist}, blob_scale={blob_scale})");
}
