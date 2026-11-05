//! Raymarched terrain reads the palette's mips by its footprint.
//!
//! The editor draws terrain as meshes, but the raymarch still draws a
//! `kind 1` terrain volume for anyone driving it directly. Its palette sample
//! had no gradients to give, so it read the full-size level at every
//! distance: ground tens of metres off sampled one texel in a dozen and
//! crawled as the camera moved. It now takes how the ray turns one pixel
//! over at the top of the fragment shader, and the footprint on the surface
//! from that, as the raster gets from `dpdx`.

use floptle_field::Terrain;
use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 256;
const H: u32 = 192;

/// Black and white at random, texel by texel: the worst case for a missing mip.
fn noise() -> TextureData {
    let mut seed = 0x9e37_79b9u32;
    let mut px = Vec::with_capacity(256 * 256 * 4);
    for _ in 0..256 * 256 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let v = if seed >> 31 == 0 { 0 } else { 255 };
        px.extend_from_slice(&[v, v, v, 255]);
    }
    TextureData { pixels: px, width: 256, height: 256 }
}

/// The ground from 40 m up, looking down: about a dozen texels a pixel.
fn shoot(gpu: &Gpu, raymarch: &mut Raymarch, ground: &Terrain, mips: bool, dx: f64) -> Vec<f32> {
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("raymarched-mips"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let cam = RenderCamera::new(
        DVec3::new(dx, 40.0, 0.0),
        Quat::from_rotation_x(-1.5),
        Projection::Perspective { fov_y: 50f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let mut vol_center = [[0.0f32; 4]; floptle_render::MAX_VOLUMES];
    let mut vol_half = [[1.0f32, 1.0, 1.0, 0.5]; floptle_render::MAX_VOLUMES];
    let (bc, hf) = (ground.baked.center, ground.baked.half_extent);
    let cr = (DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - cam.world_position).as_vec3();
    vol_center[0] = [cr.x, cr.y, cr.z, 1.0];
    vol_half[0] = [hf[0], hf[1], hf[2], 0.6];
    let mut terrain_tile = [[0.0f32; 4]; 32];
    terrain_tile[0][3] = if mips { 1.0 } else { 0.0 };
    let light = Vec3::new(0.2, 1.0, 0.1).normalize();
    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        // Dim enough that white ground does not clip: the two pictures are
        // compared in linear light.
        light_color: [0.25, 0.25, 0.25, 0.0],
        ambient: [0.05, 0.05, 0.05, 0.0],
        bg: [0.0, 0.0, 0.0, 1.0],
        vol_center,
        vol_half,
        blobs: [[0.0; 4]; 16],
        // No slot is pixel art (`w` is the nearest bitmask).
        terrain_tint: [1.0, 1.0, 1.0, 0.0],
        terrain_tile,
        ..Default::default()
    };
    raymarch.draw_into(gpu, &view, gpu.depth_view(), rm);
    floptle_render::probe::readback(gpu, &color)
        .iter()
        .map(|p| {
            let lin = |v: u8| {
                let c = v as f32 / 255.0;
                if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
            };
            (lin(p[0]) * 0.3 + lin(p[1]) * 0.59 + lin(p[2]) * 0.11) * 255.0
        })
        .collect()
}

/// Mean, spread and the crawl against `b`, over the middle of the picture.
fn measure(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    let pick: Vec<usize> = (48..144).flat_map(|y| (64..192).map(move |x| (y * W + x) as usize)).collect();
    let n = pick.len() as f32;
    let mean = pick.iter().map(|&i| a[i]).sum::<f32>() / n;
    let spread = (pick.iter().map(|&i| (a[i] - mean).powi(2)).sum::<f32>() / n).sqrt();
    let crawl = pick.iter().map(|&i| (a[i] - b[i]).abs()).sum::<f32>() / n;
    (mean, spread, crawl)
}

#[test]
fn distant_raymarched_ground_reads_its_mips_and_holds_still() {
    let gpu = Gpu::headless(W, H);
    let mut ground = Terrain::flat([128, 12, 128], [0.0; 3], [64.0, 3.0, 64.0], 0.0, [1.0, 1.0, 1.0]);
    for x in [-40.0, 0.0, 40.0] {
        for z in [-40.0, 0.0, 40.0] {
            ground.paint_texture([x, 0.0, z], 40.0, 1);
        }
    }
    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[noise()]);
    assert_eq!(raymarch.set_volumes(&gpu, &[&ground.baked]), 1);
    // A third of a pixel over.
    let one = shoot(&gpu, &mut raymarch, &ground, false, 0.0);
    let one_moved = shoot(&gpu, &mut raymarch, &ground, false, 0.05);
    let mipped = shoot(&gpu, &mut raymarch, &ground, true, 0.0);
    let mipped_moved = shoot(&gpu, &mut raymarch, &ground, true, 0.05);
    let (m1, s1, c1) = measure(&one, &one_moved);
    let (m2, s2, c2) = measure(&mipped, &mipped_moved);
    assert!(m1 > 10.0, "the fixture: the ground should be lit, mean {m1:.1}");
    let peak = one.iter().copied().fold(0.0f32, f32::max);
    assert!(peak < 250.0, "the fixture: white ground clips at {peak:.1}, so the means cannot be compared");
    // The full-size level alone reads single texels, which is still what a
    // slot that does not ask for mips draws.
    assert!(s1 > m1 * 0.3, "the fixture: one level should read single texels, spread {s1:.1} about {m1:.1}");
    assert!(c1 > m1 * 0.2, "the fixture: one level should crawl, {c1:.1} about {m1:.1}");
    assert!(s2 < s1 * 0.1, "mipped ground still reads single texels: spread {s2:.1} against {s1:.1} without mips");
    assert!(c2 < c1 * 0.1, "mipped ground crawled {c2:.2} a pixel against {c1:.2} without mips");
    assert!((m2 - m1).abs() < m1 * 0.15, "the mips should average to the same brightness: {m2:.1} against {m1:.1}");
}
