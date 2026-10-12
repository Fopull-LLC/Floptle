//! 2D lighting probe: normal maps.
//!
//! A floor of tiles, each with a hemisphere bump drawn in its normal map, and a
//! warm light to the left. With the normal map the half of each bump facing the
//! light must come out brighter than the half facing away, by more than the
//! light's own falloff accounts for; without one, the same floor lights exactly
//! as before, and a flat normal map changes nothing.
//!
//! Run: cargo run -p floptle-render --example light2d_normal_probe -- <outdir>

use floptle_render::{
    instance_of_mat, Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, Projection,
    Raster, RenderCamera, TexId, TexSampling, TextureData, mesh,
};
use glam::{DVec3, Mat4, Quat};

const S: u32 = 256;
const COLS: u32 = 16;
const ROWS: u32 = 16;
const ORTHO_HEIGHT: f32 = 16.0;
const MAP_RANK: u32 = 1;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    let n = 16u32;
    let grey: Vec<u8> = (0..n * n).flat_map(|_| [150u8, 150, 150, 255]).collect();
    let tex = raster.register_texture(&gpu, &TextureData { pixels: grey, width: n, height: n }, TexSampling::default());
    // A hemisphere in each tile: tilted out to the left on the left half, out
    // to the right on the right half. Texture v runs down the art.
    let mut bump = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let nx = (x as f32 + 0.5 - 8.0) / 8.0;
            let ny = (8.0 - (y as f32 + 0.5)) / 8.0;
            let r2 = nx * nx + ny * ny;
            let (nx, ny, nz) = if r2 < 0.85 { (nx, ny, (1.0 - r2).sqrt()) } else { (0.0, 0.0, 1.0) };
            let e = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
            bump.extend_from_slice(&[e(nx), e(ny), e(nz), 255]);
        }
    }
    let flat: Vec<u8> = (0..n * n).flat_map(|_| [128u8, 128, 255, 255]).collect();
    let bump_tex = raster.register_texture(&gpu, &TextureData { pixels: bump, width: n, height: n }, TexSampling::default());
    let flat_tex = raster.register_texture(&gpu, &TextureData { pixels: flat, width: n, height: n }, TexSampling::default());
    let bumpy = raster.material_set(&gpu, Some(tex), [Some(bump_tex), None, None, None]);
    let flat_set = raster.material_set(&gpu, Some(tex), [Some(flat_tex), None, None, None]);

    let data: Vec<u32> = (0..COLS * ROWS).map(|_| 0).collect();
    let md = mesh::tilemap(COLS, ROWS, 1.0, 1, 1, [0.0, 0.0], &data);
    let map = raster.register(&gpu, &md, None);
    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::IDENTITY, &mat);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.05, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);
    let mut lit = Light2dUniform {
        count: [1.0, 0.0, 0.0, 0.0],
        ambient: [0.2, 0.2, 0.25, 0.0],
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };
    lit.pos[0] = [-4.0, 0.5, 0.0, 9.0];
    lit.color[0] = [1.6, 1.3, 0.9, 0.0];
    lit.mask[0] = [1 << MAP_RANK, 0, 0, 0];
    lit.falloff[0] = [0.0, 2.0, 0.0, 9.0 * 0.3];

    // The tile spanning x -2..-1, y 0..1: a quarter in from each side.
    let px_of = |wx: f32| ((wx + 8.0) * 16.0) as u32;
    let row = ((8.0 - 0.5) * 16.0) as u32;
    let mut contrast = Vec::new();
    for (name, set) in [("plain", tex), ("flat", flat_set), ("bumpy", bumpy)] {
        let g = [(map, Some::<TexId>(set), Light2dInstance::from_raster(&raw, MAP_RANK, false))];
        let px = shot(&gpu, &mut raster, S, view_proj, map, tex, &raw, &g, Some(&lit));
        let (l, r) = (luma(&px, S, px_of(-1.75), row), luma(&px, S, px_of(-1.25), row));
        save_png(&px, S, &format!("{dir}/light2d_normal_{name}.png"));
        println!("{name}: facing the light {l:.3}, facing away {r:.3}");
        contrast.push(l - r);
    }
    assert!(
        (contrast[1] - contrast[0]).abs() < 0.01,
        "a flat normal map must light exactly as no normal map: {:.3} vs {:.3}",
        contrast[1],
        contrast[0]
    );
    assert!(
        contrast[2] > contrast[0] + 0.08,
        "the bump's lit side is not brighter than the falloff alone makes it: {:.3} vs {:.3}",
        contrast[2],
        contrast[0]
    );
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
