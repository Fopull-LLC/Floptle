//! A model's picture goes to the GPU once, however many meshes wear it.
//!
//! Models often embed one picture per file and one material per part. A
//! ragdoll of eleven parts, each its own file with the same 4096×4096 skin,
//! put eleven 64 MB copies on the GPU, and a phone's browser closed the page
//! for it. Meshes that bring the same pixels now share one texture, and a
//! picture that differs by a single pixel still gets its own.

use floptle_render::{Gpu, MeshData, Raster, TextureData, Vertex};

fn quad() -> MeshData {
    let v = |x: f32, y: f32| Vertex { pos: [x, y, 0.0], normal: [0.0, 0.0, 1.0], uv: [x, y] };
    MeshData { vertices: vec![v(0.0, 0.0), v(1.0, 0.0), v(1.0, 1.0), v(0.0, 1.0)], indices: vec![0, 1, 2, 0, 2, 3], colors: None }
}

fn picture(seed: u8) -> TextureData {
    TextureData { pixels: (0..64 * 64 * 4).map(|i| (i % 251) as u8 ^ seed).collect(), width: 64, height: 64 }
}

#[test]
fn meshes_that_bring_the_same_pixels_share_one_texture() {
    let gpu = Gpu::headless(8, 8);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass");
        return;
    }
    let mut raster = Raster::new(&gpu);
    let skin = picture(0);
    for _ in 0..11 {
        raster.register(&gpu, &quad(), Some(&skin));
    }
    assert_eq!(raster.mesh_texture_count(), 1, "eleven meshes with one picture");

    let mut one_pixel_off = picture(0);
    one_pixel_off.pixels[4 * 64 * 32 + 7] ^= 1;
    raster.register(&gpu, &quad(), Some(&one_pixel_off));
    raster.register(&gpu, &quad(), Some(&picture(9)));
    let same_pixels_other_shape = TextureData { pixels: skin.pixels.clone(), width: 128, height: 32 };
    raster.register(&gpu, &quad(), Some(&same_pixels_other_shape));
    raster.register(&gpu, &quad(), None);
    assert_eq!(raster.mesh_texture_count(), 4, "a picture that differs in a pixel or in shape is its own");
}

/// **A model whose textures ask for mipmaps gets a mip chain.** The filter was
/// applied after registration, by rebinding a sampler, and a sampler cannot add
/// levels a texture does not have: "smooth + mipmaps" sampled the single level
/// there was, and a far model's texture shimmered exactly as a crisp one did.
#[test]
fn a_mesh_texture_sampled_with_mipmaps_has_a_mip_chain() {
    use floptle_render::{TexFilter, TexSampling};
    let gpu = Gpu::headless(8, 8);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass");
        return;
    }
    let mut raster = Raster::new(&gpu);
    let skin = picture(3);
    let crisp = raster.register(&gpu, &quad(), Some(&skin));
    let smooth = raster.register_sampled(
        &gpu,
        &quad(),
        Some(&skin),
        TexSampling { filter: TexFilter::SmoothMipmaps, ..Default::default() },
    );
    assert_eq!(raster.mesh_texture_mips(crisp), Some(1));
    assert_eq!(raster.mesh_texture_mips(smooth), Some(7), "64×64 has seven levels down to 1×1");
    // The same pixels twice, one copy each way.
    assert_eq!(raster.mesh_texture_count(), 2);
    let untextured = raster.register_sampled(&gpu, &quad(), None, TexSampling { filter: TexFilter::SmoothMipmaps, ..Default::default() });
    assert_eq!(raster.mesh_texture_mips(untextured), None);
}

/// **A model's maps are shared across models too.** A kit of models pointing at
/// one texture library bound each map once per model, so twenty-five parts
/// carried twenty-five copies of the same normal map. The same pixels with the
/// same sampling now answer the same id, the pixels are one upload shared with
/// any mesh that wears them, and different sampling is a different binding of
/// that one upload.
#[test]
fn a_shared_map_is_one_texture_across_models() {
    use floptle_render::{TexFilter, TexSampling};
    let gpu = Gpu::headless(8, 8);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        return; // this device cannot build the raster pass
    }
    let mut raster = Raster::new(&gpu);
    let normal = picture(5);
    let crisp = TexSampling::default();
    let first = raster.register_texture_shared(&gpu, &normal, crisp);
    for _ in 0..24 {
        assert_eq!(raster.register_texture_shared(&gpu, &normal, crisp), first, "the same map, the same id");
    }
    assert_eq!(raster.mesh_texture_count(), 1, "twenty-five models, one upload");
    // A mesh wearing the same pixels as its own texture shares the upload.
    raster.register(&gpu, &quad(), Some(&normal));
    assert_eq!(raster.mesh_texture_count(), 1);
    // Smooth is the same upload sampled another way; mipmapped needs a chain.
    let smooth = raster.register_texture_shared(&gpu, &normal, TexSampling { filter: TexFilter::Smooth, ..crisp });
    assert_ne!(smooth, first);
    assert_eq!(raster.mesh_texture_count(), 1);
    raster.register_texture_shared(&gpu, &normal, TexSampling { filter: TexFilter::SmoothMipmaps, ..crisp });
    assert_eq!(raster.mesh_texture_count(), 2);
    // Other pixels are another texture.
    assert_ne!(raster.register_texture_shared(&gpu, &picture(6), crisp), first);
    // A released id is never handed back: the next model uploads afresh.
    raster.release_texture(&gpu, first);
    assert_ne!(raster.register_texture_shared(&gpu, &normal, crisp), first);
}
