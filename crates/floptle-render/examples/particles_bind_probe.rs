//! The particle pass builds, and its group(1) really is the raster's.
//!
//! Billboards bind textures registered by the raster pass, so the two group(1)
//! layouts must be the same layout — not two hand-written copies that happen to
//! match, which is what they were until the surface maps grew one side from two
//! bindings to ten. A mismatch is a validation error at DRAW time, in a running
//! editor, on whichever scene happens to have an effect in it: exactly the
//! failure a probe should catch first.
//!
//! Run: cargo run -p floptle-render --example particles_bind_probe

use floptle_render::{Gpu, Raster, TexSampling, TextureData};

fn main() {
    let gpu = Gpu::headless(64, 64);
    // Building the pass validates its pipelines against the shared layout.
    let mut particles = floptle_render::particles::Particles::new(&gpu);
    let mut raster = Raster::new(&gpu);
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels: vec![255, 0, 255, 255], width: 1, height: 1 },
        TexSampling::default(),
    );
    // A combined surface set is still an ordinary texture the particle pass can
    // bind — that is the whole point of returning a `TexId`.
    let set = raster.material_set(&gpu, Some(tex), [Some(tex), None, None, None]);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("particles-bind"),
        size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    // One billboard per texture id, drawn through the raster's bind groups.
    for id in [tex, set] {
        particles.draw(
            &gpu,
            &view,
            gpu.depth_view(),
            floptle_render::particles::ParticleGlobals {
                view_proj: glam::Mat4::IDENTITY.to_cols_array_2d(),
                cam_right: [1.0, 0.0, 0.0, 0.0],
                cam_up: [0.0, 1.0, 0.0, 0.0],
                fog_color: [0.0; 4],
                fog_params: [0.0; 4],
            },
            &[floptle_render::particles::ParticleInstance {
                pos_rot: [0.0, 0.0, -3.0, 0.0],
                size: [1.0, 1.0, 0.0, 0.0],
                color: [1.0, 1.0, 1.0, 1.0],
                basis_right: [1.0, 0.0, 0.0, 0.0],
                basis_up: [0.0, 1.0, 0.0, 0.0],
            }],
            &[floptle_render::particles::ParticleBatch { texture: Some(id), range: 0..1, blend: floptle_render::particles::ParticleBlend::Alpha }],
            &raster,
        );
    }
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    println!("particle bind groups OK");
}
