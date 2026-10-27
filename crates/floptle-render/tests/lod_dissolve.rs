//! **Two LOD bands dissolve into each other, inside the leaf.**
//!
//! A scattered tree crossing from one band to the next draws twice: the band
//! arriving keeps some pixels, the band leaving keeps the rest. Two things
//! made that read as pixel noise round every leaf (card 0555):
//!
//! - the cutout was skipped while a band faded, and its share multiplied into
//!   the picture's alpha, so the soft texels outside a leaf's edge were kept at
//!   random, as dots;
//! - a 4×4 grid decided which pixels each band kept, a screen door over every
//!   tree mid-fade.
//!
//! One card is drawn here, its left half below the cutoff and its right half
//! solid, once as each band. Below the cutoff neither band draws anything;
//! across the solid half the two bands between them draw every pixel once,
//! each about its share.

use floptle_core::{BlendMode, Material};
use floptle_render::{
    instance_of, plane, set_ext_index, Globals, Gpu, Raster, SurfaceExtras, TexFilter, TexSampling, TexWrap,
    TextureData,
};
use glam::Mat4;

const SIZE: u32 = 64;

/// Which pixels a band at `share` draws, `invert` for the band leaving.
fn band(gpu: &Gpu, raster: &mut Raster, color: &wgpu::Texture, share: f32, invert: bool) -> Vec<bool> {
    // Eight texels across: the left four at alpha 0.3, under a 0.5 cutoff;
    // the right four solid.
    let pixels = (0..8 * 8).flat_map(|i| [255, 255, 255, if i % 8 < 4 { 77 } else { 255 }]).collect();
    let tex = raster.register_texture(
        gpu,
        &TextureData { pixels, width: 8, height: 8 },
        TexSampling { filter: TexFilter::Pixelated, wrap: TexWrap::Clamp },
    );
    let mesh = raster.register(gpu, &plane(1.0), None);
    let ext = raster.push_surface_extras(SurfaceExtras::from_material(&Material {
        blend: BlendMode::Cutout,
        alpha_cutoff: 0.5,
        ..Default::default()
    }));
    let ext = raster.dithered_extras(ext, invert);
    // The card fills the frame, straight through an identity projection.
    let mut raw = instance_of(Mat4::from_translation(glam::Vec3::new(0.0, 0.0, 0.5)), [1.0; 3]);
    raw.color[3] = share;
    set_ext_index(&mut raw, ext);
    let globals = Globals {
        view_proj: Mat4::IDENTITY.to_cols_array_2d(),
        light_dir: [0.0, 0.0, -1.0, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.5, 0.5, 0.5, 0.0],
        ..Default::default()
    };
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    raster.draw_scene(gpu, &view, gpu.depth_view(), globals, &[(mesh, Some(tex), raw)], Some([0.0, 0.0, 0.0, 1.0]), None);
    floptle_render::probe::readback(gpu, color).iter().map(|p| p[0] > 0 || p[1] > 0 || p[2] > 0).collect()
}

#[test]
fn two_bands_dissolve_inside_the_cutout_and_draw_each_pixel_once() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lod-dissolve"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut raster = Raster::new(&gpu);
    for share in [0.3f32, 0.7] {
        let arriving = band(&gpu, &mut raster, &color, share, false);
        let leaving = band(&gpu, &mut raster, &color, 1.0 - share, true);
        // Clear of the texel boundary in the middle, where the two halves meet.
        let column = |i: usize| (i % SIZE as usize) as u32;
        let below = |i: &usize| column(*i) < SIZE / 2 - 4;
        let solid = |i: &usize| column(*i) >= SIZE / 2 + 4;
        let all = 0..arriving.len();

        let dots = all.clone().filter(below).filter(|&i| arriving[i] || leaving[i]).count();
        assert_eq!(dots, 0, "at {share}: {dots} pixels drawn where the picture is under its cutoff");

        let n = all.clone().filter(solid).count();
        let twice = all.clone().filter(solid).filter(|&i| arriving[i] && leaving[i]).count();
        let gaps = all.clone().filter(solid).filter(|&i| !arriving[i] && !leaving[i]).count();
        assert!(twice + gaps <= 1, "at {share}: {twice} pixels drawn by both bands and {gaps} by neither, of {n}");
        let kept = all.filter(solid).filter(|&i| arriving[i]).count() as f32 / n as f32;
        assert!((kept - share).abs() < 0.1, "the band arriving at {share} kept {kept:.2} of its pixels");
    }
}
