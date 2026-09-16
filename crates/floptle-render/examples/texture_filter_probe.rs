//! Headless per-texture-filter probe — the same fine checker texture on three
//! spheres, sampled Pixelated / Smooth / Smooth+Mipmaps. Validates the per-texture
//! sampler path (group-1 sampler) and the CPU mip-chain upload, and shows the visible
//! difference (crisp-but-aliased vs bilinear vs shimmer-free minification).
//!
//! Run: cargo run -p floptle-render --example texture_filter_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection,
    Raster, RenderCamera, TexFilter, TexId, TexSampling, TexWrap, TextureData,
};
use glam::{DVec3, Quat, Vec3};
use floptle_render::probe::{save_png};

const W: u32 = 1200;
const H: u32 = 420;

/// A fine checker (small cells) so the far side of the sphere minifies heavily —
/// where Pixelated aliases into noise and mipmaps smooth it out.
fn checker() -> TextureData {
    let n = 512u32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            let c = if on { [235, 235, 240, 255] } else { [40, 45, 60, 255] };
            px.extend_from_slice(&c);
        }
    }
    TextureData { pixels: px, width: n, height: n }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "texture_filter.png".into());
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
    let sphere = raster.register(&gpu, &uv_sphere(0.95, 48, 64), None);

    // The same image registered with three different samplings.
    let tex = checker();
    let pixelated =
        raster.register_texture(&gpu, &tex, TexSampling { filter: TexFilter::Pixelated, wrap: TexWrap::Repeat });
    let smooth =
        raster.register_texture(&gpu, &tex, TexSampling { filter: TexFilter::Smooth, wrap: TexWrap::Repeat });
    let mipped = raster.register_texture(
        &gpu,
        &tex,
        TexSampling { filter: TexFilter::SmoothMipmaps, wrap: TexWrap::Repeat },
    );

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 6.5),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.3, 0.7, 0.7).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.5, 0.5, 0.55, 0.0],
        ..Default::default()
    };

    let mat = MaterialParams::flat([1.0, 1.0, 1.0]);
    let row: [(TexId, f64); 3] = [(pixelated, -2.6), (smooth, 0.0), (mipped, 2.6)];
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = row
        .iter()
        .map(|&(tex, x)| {
            let t = Transform::from_translation(DVec3::new(x, 0.0, 0.0));
            (sphere, Some(tex), instance_of_mat(t.render_matrix(cam.world_position), &mat))
        })
        .collect();

    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, Some([0.06, 0.07, 0.1, 1.0]), None);
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — Pixelated | Smooth | Smooth+Mipmaps (left→right)");
}
