//! A billboard fades out where it meets the scene instead of cutting a hard line.
//!
//! The particle pass samples the depth buffer it tests against and, for a
//! track with a soft-edge distance, scales the particle by how far in front of
//! the surface behind it each fragment sits. One white quad is drawn 0.1 units
//! in front of a depth plane with a 0.5-unit soft edge: it should come out at
//! about a fifth of its brightness. The same quad with no soft edge, and the
//! same quad a full unit in front of the plane, come out at full brightness —
//! so a fade that fires everywhere, or nowhere, both fail.

use floptle_render::particles::{ParticleBatch, ParticleBlend, ParticleGlobals, ParticleInstance, Particles};
use floptle_render::{Gpu, Raster};
use glam::Mat4;

const SIZE: u32 = 32;

/// Render one quad at `quad_dist` in front of the camera against a depth
/// buffer cleared to `plane_dist`, and return the red channel at the centre.
fn centre_brightness(gpu: &Gpu, particles: &mut Particles, raster: &Raster, quad_dist: f32, plane_dist: f32, soft: f32) -> u8 {
    let proj = Mat4::perspective_rh(1.0, 1.0, 0.1, 100.0);
    // The depth buffer as if a surface sat `plane_dist` straight ahead.
    let clip = proj * glam::Vec4::new(0.0, 0.0, -plane_dist, 1.0);
    let plane_depth = clip.z / clip.w;
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("soft-particles"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.scene_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: gpu.depth_view(),
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(plane_depth), store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    gpu.queue.submit([enc.finish()]);

    particles.draw(
        gpu,
        &view,
        gpu.depth_view(),
        ParticleGlobals {
            view_proj: proj.to_cols_array_2d(),
            cam_right: [1.0, 0.0, 0.0, 0.0],
            cam_up: [0.0, 1.0, 0.0, 0.0],
            fog_color: [0.0; 4],
            fog_params: [0.0; 4],
            proj_z: ParticleGlobals::proj_z(&proj, false),
        },
        &[ParticleInstance {
            pos_rot: [0.0, 0.0, -quad_dist, 0.0],
            size: [quad_dist, quad_dist, 0.0, 0.0],
            color: [1.0, 1.0, 1.0, 1.0],
            basis_right: [1.0, 0.0, 0.0, 1.0],
            basis_up: [0.0, 1.0, 0.0, 1.0],
            params: [soft, 0.0, 0.0, 0.0],
        }],
        &[ParticleBatch { texture: None, blend: ParticleBlend::Alpha, range: 0..1 }],
        raster,
    );
    let px = floptle_render::probe::readback(gpu, &color);
    px[(SIZE / 2 * SIZE + SIZE / 2) as usize][0]
}

#[test]
fn a_particle_fades_out_where_it_meets_the_scene() {
    let gpu = Gpu::headless(SIZE, SIZE);
    let mut particles = Particles::new(&gpu);
    let raster = Raster::new(&gpu);

    // 0.1 in front of the surface, soft over 0.5: a fifth of the way up, in
    // linear light — the sRGB target makes that about 120/255.
    let near = centre_brightness(&gpu, &mut particles, &raster, 3.0, 3.1, 0.5);
    // The same quad with a hard edge, and the same quad a unit clear of the
    // surface: full brightness both.
    let hard = centre_brightness(&gpu, &mut particles, &raster, 3.0, 3.1, 0.0);
    let clear = centre_brightness(&gpu, &mut particles, &raster, 3.0, 4.0, 0.5);
    // And behind the surface it is not drawn at all.
    let behind = centre_brightness(&gpu, &mut particles, &raster, 3.2, 3.1, 0.5);

    assert!(hard >= 250, "a hard-edged particle in front of the surface draws in full (got {hard})");
    assert!(clear >= 250, "a particle well clear of the surface draws in full (got {clear})");
    assert!(behind == 0, "a particle behind the surface is depth-tested away (got {behind})");
    assert!(
        (80..=160).contains(&near),
        "a particle 0.1 in front of the surface with a 0.5 soft edge draws at about a fifth (got {near})"
    );
}
