//! **Two tiles in one plane light as the one you can see**.
//!
//! An isometric block is drawn taller than its cell, over the cell behind it,
//! in the same plane. Depth cannot tell the two apart, so the order decides,
//! and the colour pass and the 2D light pass's G-buffer have to decide it the
//! same way: the light is applied as a correction to what the colour pass drew,
//! worked out from what the G-buffer holds. When they disagreed, the overlap was
//! corrected for the tile underneath, and a block's top came out as a band of
//! the grass behind it in the wrong colour.
//!
//! One mesh, a red tile then a blue one overlapping it, under a grey ambient
//! of 0.5. Where they overlap the frame must be the blue tile at half
//! brightness: the same hue as unlit, never red subtracted from blue.
//!
//! Run: cargo run -p floptle-render --example light2d_overlap_probe -- <outdir>

use floptle_render::probe::readback;
use floptle_render::{
    Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, MeshId, Projection, Raster,
    RenderCamera, TexId, TexSampling, TextureData, instance_of_mat, mesh,
};
use glam::{DVec3, Mat4, Quat};

const S: u32 = 128;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    // A 2×1 sheet: cell 0 red, cell 1 blue.
    let pixels: Vec<u8> = (0..8)
        .flat_map(|y| (0..16).map(move |x| (x, y)))
        .flat_map(|(x, _)| if x < 8 { [200u8, 40, 40, 255] } else { [40, 40, 200, 255] })
        .collect();
    let tex = raster.register_texture(&gpu, &TextureData { pixels, width: 16, height: 8 }, TexSampling::default());
    // Red behind, then blue in front, half a cell over it — the order a map
    // lists them, back to front.
    let cells = [(0usize, [-0.5, 0.0]), (1usize, [0.5, 0.0])];
    let m = raster.register(&gpu, &mesh::tilemap_placed(&cells, 2.0, 2.0, 2.0, 2, 1, [0.0, 0.0], &[0, 1]), None);

    let unlit = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::IDENTITY, &unlit);
    let cam = RenderCamera::new(DVec3::new(0.0, 0.0, 10.0), Quat::IDENTITY, Projection::of_camera(1.0, true, 4.0, 0.05, 300_000.0));
    let view_proj = cam.view_proj(1.0);
    let flat = [(m, Some(tex), Light2dInstance::from_raster(&raw, 1, false))];

    let shoot = |raster: &mut Raster, lights: Option<&Light2dUniform>| {
        let size = wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 };
        let make = |format: wgpu::TextureFormat, extra: wgpu::TextureUsages| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("overlap"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | extra,
                view_formats: &[],
            })
        };
        let color = make(gpu.surface_format(), wgpu::TextureUsages::COPY_SRC);
        let depth = make(Gpu::DEPTH_FORMAT, wgpu::TextureUsages::empty());
        let (view, dview) = (
            color.create_view(&wgpu::TextureViewDescriptor::default()),
            depth.create_view(&wgpu::TextureViewDescriptor::default()),
        );
        let scene: Vec<(MeshId, Option<TexId>, floptle_render::InstanceRaw)> = vec![(m, Some(tex), raw)];
        let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
        raster.draw_scene(&gpu, &view, &dview, globals, &scene, Some([0.0, 0.0, 0.0, 1.0]), None);
        if let Some(l) = lights {
            raster.light2d_pass(&gpu, &view, &dview, (S, S), view_proj.to_cols_array_2d(), l, &flat);
        }
        readback(&gpu, &color)
    };
    let mut lights = Light2dUniform {
        count: [0.0, 0.0, 0.0, 0.0],
        ambient: [0.5, 0.5, 0.5, 0.0],
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };
    lights.mask[0] = [!0, !0, 0, 0];
    let before = shoot(&mut raster, None);
    let after = shoot(&mut raster, Some(&lights));
    save_png(&after, &format!("{dir}/light2d_overlap.png"));

    // The overlap: world x 0 → screen x 64.
    let p = (S / 2 * S + S / 2) as usize;
    let (b, a) = (before[p], after[p]);
    println!("overlap unlit {:?}, lit {:?}", &b[..3], &a[..3]);
    assert!(b[2] > b[0], "the front (blue) tile must be the one drawn there: {b:?}");
    // Every channel at half its linear value: the light scaled the colour that
    // is there, and did not take the other tile's colour out of it.
    let half = |c: u8| {
        let l = (c as f32 / 255.0).powf(2.2) * 0.5;
        l.powf(1.0 / 2.2) * 255.0
    };
    for ch in 0..3 {
        assert!(
            (a[ch] as f32 - half(b[ch])).abs() < 8.0,
            "lit {a:?} against unlit {b:?}: channel {ch} should be about {:.0}. The light corrected \
             the overlap for the tile underneath",
            half(b[ch])
        );
    }
    println!("2D light over overlapping tiles OK — LOOK at light2d_overlap.png");
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
