//! **Ground seen low across keeps its detail with anisotropic filtering.**
//!
//! A striped floor runs from under the camera to the horizon. With plain
//! trilinear filtering (anisotropy 1) the far stripes melt into a flat grey,
//! because the sampler drops to the mip of the footprint's longest side; at 8
//! they stay apart. Changing the level remakes the samplers and the binds
//! that hold them once; asking for the level already set makes nothing.
//! Set `FLOPTLE_KEEP_SHOTS=<dir>` to keep both frames as PNGs.

use floptle_render::{
    instance_of_mat, Globals, Gpu, MaterialParams, MeshData, Projection, Raster, RenderCamera, TexFilter, TexSampling,
    TexWrap, TextureData, Vertex,
};
use glam::{DVec3, Quat};

const W: u32 = 256;
const H: u32 = 160;

/// Black and white stripes across x, four texels each, on a 256² picture.
fn stripes() -> TextureData {
    let mut pixels = Vec::with_capacity(256 * 256 * 4);
    for _y in 0..256 {
        for x in 0..256u32 {
            let v = if (x / 4) % 2 == 0 { 255 } else { 0 };
            pixels.extend_from_slice(&[v, v, v, 255]);
        }
    }
    TextureData { pixels, width: 256, height: 256 }
}

/// A floor 400 m across at y = 0, its stripes repeating every 4 m.
fn floor() -> MeshData {
    let (s, r) = (200.0, 100.0);
    MeshData {
        vertices: [[-s, -s, 0.0, 0.0], [s, -s, r, 0.0], [-s, s, 0.0, r], [s, s, r, r]]
            .iter()
            .map(|v| Vertex { pos: [v[0], 0.0, v[1]], normal: [0.0, 1.0, 0.0], uv: [v[2], v[3]] })
            .collect(),
        indices: vec![0, 2, 1, 1, 2, 3],
        colors: None,
    }
}

/// How much the far stripes still differ from each other: the spread of
/// brightness along one row of pixels near the horizon.
fn far_contrast(px: &[[u8; 4]], row: u32) -> f32 {
    let line: Vec<f32> = (W / 8..W * 7 / 8).map(|x| px[(row * W + x) as usize][0] as f32).collect();
    let mean = line.iter().sum::<f32>() / line.len() as f32;
    (line.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / line.len() as f32).sqrt()
}

#[test]
fn far_ground_keeps_its_stripes_at_8_and_loses_them_at_1() {
    let gpu = Gpu::headless(W, H);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::ANISOTROPIC_FILTERING) {
        eprintln!("skipped — this device does not filter anisotropically");
        return;
    }
    let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aniso-out"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let mut raster = Raster::new(&gpu);
    assert_eq!(raster.anisotropy(), 1, "a renderer starts with plain trilinear filtering");
    let mesh = raster.register(&gpu, &floor(), None);
    let smooth = TexSampling { filter: TexFilter::SmoothMipmaps, wrap: TexWrap::Repeat, ..Default::default() };
    let tex = raster.register_texture(&gpu, &stripes(), smooth);

    // 1.6 m above the floor, looking along it just below the horizon.
    let cam = RenderCamera::new(
        DVec3::new(0.0, 1.6, 0.0),
        Quat::from_rotation_x(-0.06),
        Projection::Perspective { fov_y: 50f32.to_radians(), near: 0.1, far: 1000.0 },
    );
    let globals = Globals { view_proj: cam.view_proj(W as f32 / H as f32).to_cols_array_2d(), ..Default::default() };
    let mut m = MaterialParams::flat([1.0; 3]);
    m.unlit = true;
    let at = floptle_core::transform::Transform::IDENTITY.render_matrix(cam.world_position);
    let shot = |raster: &mut Raster, name: &str| {
        raster.draw_scene(&gpu, &view, gpu.depth_view(), globals, &[(mesh, Some(tex), instance_of_mat(at, &m))], Some([0.3, 0.0, 0.0, 1.0]), None);
        if let Ok(dir) = std::env::var("FLOPTLE_KEEP_SHOTS") {
            std::fs::create_dir_all(&dir).ok();
            floptle_render::probe::save_png(&gpu, &out, &format!("{dir}/{name}.png"));
        }
        floptle_render::probe::readback(&gpu, &out)
    };

    let plain = shot(&mut raster, "aniso_1");
    let bind = raster.material_bind(tex).cloned().expect("the stripes' bind");
    assert!(raster.set_anisotropy(&gpu, 8), "a new level is a change");
    let now = raster.material_bind(tex).cloned().unwrap();
    assert!(now != bind, "the picture's bind still holds the old sampler");
    assert!(!raster.set_anisotropy(&gpu, 8), "the level already set changed something");
    assert!(raster.material_bind(tex).cloned().unwrap() == now, "asking again remade the bind");
    let sharp = shot(&mut raster, "aniso_8");

    // The floor is grey to white; the sky behind it is red. Far ground is the
    // band from a few rows under the horizon to a third of the way down.
    let grey = |p: [u8; 4]| (p[0] as i32 - p[1] as i32).abs() < 30;
    let horizon = (0..H).find(|&r| grey(plain[(r * W + W / 2) as usize])).expect("the floor is in the picture");
    let (mut at1, mut at8) = (0.0, 0.0);
    for row in horizon + 6..(horizon + (H - horizon) / 3).min(H) {
        at1 += far_contrast(&plain, row);
        at8 += far_contrast(&sharp, row);
    }
    eprintln!("far-ground contrast: anisotropy 1 = {at1:.1}, 8 = {at8:.1}");
    assert!(at8 > at1 * 1.5, "anisotropy 8 kept no more of the far stripes than 1 did: {at8:.1} vs {at1:.1}");

    // Levels between the ones a sampler takes are held to the one below.
    assert!(raster.set_anisotropy(&gpu, 5));
    assert_eq!(raster.anisotropy(), 4);
    assert_eq!((floptle_render::anisotropy_level(0), floptle_render::anisotropy_level(99)), (1, 16));
}
