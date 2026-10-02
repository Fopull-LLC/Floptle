//! Smoke is lit by the sun and dark on a planet's night side; heat haze bends
//! the scene behind it.
//!
//! One white quad straight ahead of the camera. Lit, with the key light
//! overhead: over ground that faces the light (the day side) it shows the
//! light, over ground that faces away (the night side) only the ambient. The
//! same quad unlit is the authored white either way. And a Distortion quad
//! over a picture that is red on the left and green on the right shows green
//! a little left of centre when it bends, and red when it does not.

use floptle_render::particles::{ParticleBatch, ParticleBlend, ParticleGlobals, ParticleInstance, Particles};
use floptle_render::{Gpu, Raster};
use glam::Mat4;

const SIZE: u32 = 32;

fn target(gpu: &Gpu, label: &str) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.scene_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

fn clear(gpu: &Gpu, view: &wgpu::TextureView) {
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: gpu.depth_view(),
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    gpu.queue.submit([enc.finish()]);
}

fn globals(proj: Mat4) -> ParticleGlobals {
    ParticleGlobals {
        view_proj: proj.to_cols_array_2d(),
        cam_right: [1.0, 0.0, 0.0, 0.0],
        cam_up: [0.0, 1.0, 0.0, 0.0],
        proj_z: ParticleGlobals::proj_z(&proj, false),
        // The light straight overhead, white; a dim ambient.
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.08, 0.08, 0.08, 0.0],
        ..Default::default()
    }
}

/// A 2-unit white quad 3 units ahead, `lit` with the ground's `up`.
fn quad(lit: f32, up: [f32; 3]) -> ParticleInstance {
    ParticleInstance {
        pos_rot: [0.0, 0.0, -3.0, 0.0],
        size: [2.0, 2.0, 0.0, 0.0],
        color: [1.0, 1.0, 1.0, 1.0],
        basis_right: [1.0, 0.0, 0.0, 1.0],
        basis_up: [0.0, 1.0, 0.0, 1.0],
        light: [up[0], up[1], up[2], lit],
        ..Default::default()
    }
}

fn centre(gpu: &Gpu, particles: &mut Particles, raster: &Raster, inst: ParticleInstance) -> u8 {
    let proj = Mat4::perspective_rh(1.0, 1.0, 0.1, 100.0);
    let (tex, view) = target(gpu, "lit-particles");
    clear(gpu, &view);
    particles.draw(
        gpu,
        &view,
        gpu.depth_view(),
        globals(proj),
        &[inst],
        &[ParticleBatch { texture: None, blend: ParticleBlend::Alpha, range: 0..1 }],
        raster,
        None,
    );
    let px = floptle_render::probe::readback(gpu, &tex);
    px[(SIZE / 2 * SIZE + SIZE / 2) as usize][0]
}

fn device() -> Option<(Gpu, Particles, Raster)> {
    let gpu = Gpu::headless(SIZE, SIZE);
    let particles = Particles::new(&gpu);
    let downlevel = gpu.adapter.get_downlevel_capabilities().flags;
    if !downlevel.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return None;
    }
    let raster = Raster::new(&gpu);
    Some((gpu, particles, raster))
}

#[test]
fn lit_smoke_is_bright_on_the_day_side_and_dark_on_the_night_side() {
    let Some((gpu, mut particles, raster)) = device() else { return };
    let day = centre(&gpu, &mut particles, &raster, quad(1.0, [0.0, 1.0, 0.0]));
    let night = centre(&gpu, &mut particles, &raster, quad(1.0, [0.0, -1.0, 0.0]));
    let unlit = centre(&gpu, &mut particles, &raster, quad(0.0, [0.0, -1.0, 0.0]));
    assert!(unlit >= 250, "an unlit particle keeps its authored white (got {unlit})");
    assert!(day > night + 60, "the day side ({day}) must read clearly brighter than the night side ({night})");
    assert!(night < 110, "the night side shows only the dim ambient (got {night})");
    assert!(day < 250, "lit smoke is shaded, not the authored white (got {day})");
}

#[test]
fn heat_haze_bends_the_scene_behind_it() {
    let Some((gpu, mut particles, raster)) = device() else { return };
    // The scene behind: red on the left half, green on the right.
    let scene = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("haze-scene"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let pixels: Vec<u8> = (0..SIZE * SIZE)
        .flat_map(|i| if i % SIZE < SIZE / 2 { [255, 0, 0, 255] } else { [0, 255, 0, 255] })
        .collect();
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &scene, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &pixels,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIZE * 4), rows_per_image: Some(SIZE) },
        wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
    );
    let scene_view = scene.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor::default());
    let proj = Mat4::perspective_rh(1.0, 1.0, 0.1, 100.0);
    // Left of centre, at a quarter of the width: red behind it.
    let mut probe = |strength: f32| {
        let (tex, view) = target(&gpu, "haze");
        clear(&gpu, &view);
        let mut q = quad(0.0, [0.0; 3]);
        q.size = [6.0, 6.0, 0.0, 0.0];
        // A white texel pushes by the full strength right and down.
        q.extra = [0.0, 0.0, 0.0, strength];
        particles.draw(
            &gpu,
            &view,
            gpu.depth_view(),
            globals(proj),
            &[q],
            &[ParticleBatch { texture: None, blend: ParticleBlend::Distortion, range: 0..1 }],
            &raster,
            Some((&scene_view, &sampler)),
        );
        let px = floptle_render::probe::readback(&gpu, &tex);
        px[(SIZE / 2 * SIZE + SIZE / 4) as usize]
    };
    let still = probe(0.0);
    let bent = probe(0.4);
    assert!(still[0] > 200 && still[1] < 50, "unbent, the haze shows the red behind it: {still:?}");
    assert!(bent[1] > 200 && bent[0] < 50, "bent, it shows the green from further right: {bent:?}");
}
