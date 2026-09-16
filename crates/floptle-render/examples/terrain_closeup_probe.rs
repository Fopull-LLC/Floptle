//! Headless CLOSE-UP terrain probe — the camera sits right on a sculpted hill at the
//! engine's default detail (0.5-world-unit voxels), where the SDF-normal grain is
//! worst. Used to verify the smoothness fix (voxel-scaled tetrahedron normal +
//! quintic-smoothstep sampling). A flat-lit slope should read SMOOTH, not speckled.
//!
//! Run: cargo run -p floptle-render --example terrain_closeup_probe -- <out.png>

use floptle_field::{Brush, Terrain};
use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};
use floptle_render::probe::{save_png};

const W: u32 = 1000;
const H: u32 = 640;

fn flat256() -> TextureData {
    TextureData { pixels: vec![160, 160, 168, 255], width: 1, height: 1 }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "terrain_closeup.png".into());
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

    // DEFAULT-DETAIL voxels (64 -> 0.5 world units), the grainy case.
    let mut terrain = Terrain::flat([64, 24, 64], [0.0, 0.0, 0.0], [16.0, 6.0, 16.0], 0.0, [0.4, 0.6, 0.32]);
    for _ in 0..50 {
        terrain.sculpt(Brush::Raise, [0.0, 1.0, 0.0], 6.0, 1.0, floptle_field::BrushProfile::default());
        terrain.sculpt(Brush::Raise, [4.0, 0.5, -2.0], 3.0, 1.0, floptle_field::BrushProfile::default());
    }

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[flat256()]);
    raymarch.set_volume(&gpu, &terrain.baked);

    // Close camera on the flank of the central hill, looking across its smooth slope.
    let cam_pos = DVec3::new(6.5, 4.5, 6.5);
    let target = Vec3::new(0.0, 2.2, 0.0);
    let fwd = (target - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 58f32.to_radians(), near: 0.02, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.5, 0.7, 0.4).normalize();
    let cr = (DVec3::ZERO - cam.world_position).as_vec3();

    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.30, 0.32, 0.36, 0.0],
        bg: [0.55, 0.66, 0.8, 1.0],
        center: [0.0; 4],
        params: [0.0, 0.0, 0.0, 0.0],
        vol_center: { let mut a = [[0.0f32; 4]; 16]; a[0] = [cr.x, cr.y, cr.z, 1.0]; a },
        vol_half: { let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16]; a[0] = [16.0, 6.0, 16.0, 0.1]; a },
        blobs: [[0.0; 4]; 16],
        ..Default::default()
    };
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — close-up of a 0.5-voxel hill (grain test)");
}
