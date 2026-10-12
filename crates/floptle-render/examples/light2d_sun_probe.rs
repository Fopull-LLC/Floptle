//! 2D lighting probe: suns and spot lamps.
//!
//! * **sun** — a light travelling to the right across the whole floor, with a
//!   square that blocks light in the middle. The floor left of the square is
//!   lit, the floor to its right is in its shadow, and the same scene with a
//!   square that does not block is lit on both sides.
//! * **spot** — a lamp above the floor aimed straight down with a narrow cone:
//!   the floor under it is lit, the floor beside it, inside its range but
//!   outside the cone, is not.
//!
//! Run: cargo run -p floptle-render --example light2d_sun_probe -- <outdir>

use floptle_render::{
    instance_of_mat, Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, Projection,
    Raster, RenderCamera, TexId, TexSampling, TextureData, mesh,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;
const ORTHO_HEIGHT: f32 = 16.0;
const RANK: u32 = 1;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    let n = 8u32;
    let grey: Vec<u8> = (0..n * n).flat_map(|_| [150u8, 150, 150, 255]).collect();
    let tex = raster.register_texture(&gpu, &TextureData { pixels: grey, width: n, height: n }, TexSampling::default());

    let floor_md = mesh::tilemap(16, 16, 1.0, 1, 1, [0.0, 0.0], &vec![0; 256]);
    let floor = raster.register(&gpu, &floor_md, None);
    let block_md = mesh::tilemap(2, 2, 1.0, 1, 1, [0.0, 0.0], &[0; 4]);
    let block = raster.register(&gpu, &block_md, None);
    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::IDENTITY, &mat);
    let block_raw = instance_of_mat(Mat4::from_translation(Vec3::new(0.0, 0.0, 0.1)), &mat);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.05, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);
    let base = Light2dUniform {
        count: [1.0, 0.0, 0.0, 0.0],
        ambient: [0.2, 0.2, 0.25, 0.0],
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };
    let px_of = |wx: f32, wy: f32| (((wx + 8.0) * 16.0) as u32, ((8.0 - wy) * 16.0) as u32);

    // ---- sun ----
    let mut sun = base;
    sun.pos[0] = [0.0, 0.0, 0.0, -1.0];
    sun.color[0] = [1.2, 1.1, 0.9, 0.0];
    sun.mask[0] = [1 << RANK, 0, 0, 0];
    sun.falloff[0] = [0.0, 2.0, 1.0, 0.0];
    sun.aim[0] = [1.0, 0.0, -1.0, -1.0];
    let mut results = Vec::new();
    for casts in [false, true] {
        let flat = [
            (floor, Some::<TexId>(tex), Light2dInstance::from_raster(&raw, RANK, false)),
            (block, Some::<TexId>(tex), Light2dInstance::from_raster(&block_raw, RANK, casts)),
        ];
        let px = shot(&gpu, &mut raster, S, view_proj, floor, tex, &raw, &flat, Some(&sun));
        let (lx, ly) = px_of(-2.5, 0.5);
        let (rx, ry) = px_of(2.5, 0.5);
        let (l, r) = (luma(&px, S, lx, ly), luma(&px, S, rx, ry));
        save_png(&px, S, &format!("{dir}/light2d_sun_{}.png", if casts { "blocked" } else { "open" }));
        println!("sun, square {}: left {l:.3}, right {r:.3}", if casts { "blocks" } else { "does not block" });
        results.push((l, r));
    }
    let ((ol, or), (bl, br)) = (results[0], results[1]);
    assert!((ol - or).abs() < 0.01, "a sun lights evenly with nothing in the way: {ol:.3} vs {or:.3}");
    assert!((bl - ol).abs() < 0.01, "the side the sun comes from stays lit: {bl:.3} vs {ol:.3}");
    assert!(br < or - 0.15, "the square must shadow the floor beyond it: {br:.3} vs open {or:.3}");

    // ---- spot ----
    let mut spot = base;
    spot.pos[0] = [0.0, 4.0, 0.0, 9.0];
    spot.color[0] = [1.4, 1.2, 0.9, 0.0];
    spot.mask[0] = [1 << RANK, 0, 0, 0];
    spot.aim[0] = [0.0, -1.0, (25f32).to_radians().cos(), (15f32).to_radians().cos()];
    let flat = [(floor, Some::<TexId>(tex), Light2dInstance::from_raster(&raw, RANK, false))];
    let px = shot(&gpu, &mut raster, S, view_proj, floor, tex, &raw, &flat, Some(&spot));
    save_png(&px, S, &format!("{dir}/light2d_spot.png"));
    let (ux, uy) = px_of(0.0, 1.0);
    let (sx, sy) = px_of(3.5, 4.0);
    let (under, side) = (luma(&px, S, ux, uy), luma(&px, S, sx, sy));
    let mut none = base;
    none.count = [0.0; 4];
    let dark = shot(&gpu, &mut raster, S, view_proj, floor, tex, &raw, &flat, Some(&none));
    let ambient = luma(&dark, S, sx, sy);
    println!("spot: under {under:.3}, beside {side:.3}, ambient {ambient:.3}");
    assert!(under > ambient + 0.15, "the floor under the lamp is lit: {under:.3} vs ambient {ambient:.3}");
    assert!((side - ambient).abs() < 0.01, "outside the cone is ambient, inside the range or not: {side:.3} vs {ambient:.3}");
}

#[allow(clippy::too_many_arguments)]
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    s: u32,
    view_proj: glam::Mat4,
    map: floptle_render::MeshId,
    tex: TexId,
    raw: &floptle_render::InstanceRaw,
    flat: &[(floptle_render::MeshId, Option<TexId>, Light2dInstance)],
    lights: Option<&Light2dUniform>,
) -> Vec<[u8; 4]> {
    let size = wgpu::Extent3d { width: s, height: s, depth_or_array_layers: 1 };
    let make = |label: &str, format: wgpu::TextureFormat, extra: wgpu::TextureUsages| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | extra,
            view_formats: &[],
        })
    };
    let color = make("light2d-color", gpu.surface_format(), wgpu::TextureUsages::COPY_SRC);
    // Its own depth, because every attachment in a pass must be the same size.
    let depth = make("light2d-probe-depth", Gpu::DEPTH_FORMAT, wgpu::TextureUsages::empty());
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let dview = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(
        gpu,
        &view,
        &dview,
        globals,
        &[(map, Some::<TexId>(tex), *raw)],
        Some([0.02, 0.02, 0.04, 1.0]),
        None,
    );
    if let Some(l) = lights {
        raster.light2d_pass(gpu, &view, &dview, (s, s), view_proj.to_cols_array_2d(), l, flat);
    }
    readback(gpu, &color, s)
}

fn luma(px: &[[u8; 4]], s: u32, x: u32, y: u32) -> f32 {
    let p = px[(y * s + x) as usize];
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture, s: u32) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (s * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * s) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(s),
            },
        },
        wgpu::Extent3d { width: s, height: s, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut o = Vec::with_capacity((s * s) as usize);
    for y in 0..s {
        let row = (y * padded) as usize;
        for x in 0..s {
            let i = row + (x * bpp) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            o.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    o
}

fn save_png(px: &[[u8; 4]], s: u32, path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), s, s);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
