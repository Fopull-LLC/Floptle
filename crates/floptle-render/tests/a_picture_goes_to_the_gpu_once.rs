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
