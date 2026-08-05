//! Orthographic gameplay camera probe (`floptle/0091`): a flat game puts its art
//! on one plane and its camera on that plane, and that must draw.
//!
//! The exact reported scene — an orthographic `Matter::Camera` at `z = 0` and a
//! 16x16 tilemap in the XY plane at `z = 0` — rendered twice:
//!
//! * **was** — the near plane every gameplay camera used to pass (`0.05`, in
//!   FRONT of the eye). The whole map is clipped away; the image is background.
//! * **now** — `Projection::of_camera`, which owns the orthographic depth range.
//!
//! It writes both PNGs so the difference is something you look at rather than a
//! number you trust.
//!
//! Run: cargo run -p floptle-render --example ortho_camera_probe -- <outdir>

use floptle_render::{
    instance_of_mat, Globals, Gpu, MaterialParams, Projection, Raster, RenderCamera, TexId,
    TexSampling, TextureData, mesh,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 320;
/// The user's scene, verbatim.
const COLS: u32 = 16;
const ROWS: u32 = 16;
const TILE: f32 = 1.0;
const SHEET: u32 = 2;
const ORTHO_HEIGHT: f32 = 9.5;
/// `Camera 1`'s translation in the reported scene: level with the map in Z.
const EYE: Vec3 = Vec3::new(0.0, -0.8, 0.0);

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    // A 2x2 sheet of four flat colours, so which cell landed where is legible.
    let cell = [[220u8, 90, 80], [90, 190, 120], [90, 140, 230], [230, 200, 90]];
    let n = 64u32;
    let mut pixels = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let c = cell[((y / 32) * 2 + (x / 32)) as usize];
            // A one-texel dark gutter, so a seam would be visible if there were one.
            let edge = x % 32 < 1 || y % 32 < 1;
            let s = if edge { 0.45 } else { 1.0 };
            pixels.extend_from_slice(&[
                (c[0] as f32 * s) as u8,
                (c[1] as f32 * s) as u8,
                (c[2] as f32 * s) as u8,
                255,
            ]);
        }
    }
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels, width: n, height: n },
        TexSampling::default(),
    );

    // A checkerboard of the four cells with a hole in it, so "nothing drew" and
    // "the map drew" cannot be confused with each other.
    let data: Vec<u32> = (0..COLS * ROWS)
        .map(|i| {
            let (x, y) = (i % COLS, i / COLS);
            if (5..8).contains(&x) && (5..8).contains(&y) {
                floptle_core::EMPTY_TILE
            } else {
                (x + y) % 4
            }
        })
        .collect();
    let md = mesh::tilemap(COLS, ROWS, TILE, SHEET, SHEET, [1.0 / n as f32; 2], &data);
    let map = raster.register(&gpu, &md, None);
    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    // The map sits at the world origin; the camera is the render-space origin.
    let raw = instance_of_mat(Mat4::from_translation(-EYE), &mat);

    let shots = [
        // What every gameplay camera built before 0091.
        ("was", Projection::Orthographic { height: ORTHO_HEIGHT, near: 0.05, far: 300_000.0 }),
        ("now", Projection::of_camera(1.05, true, ORTHO_HEIGHT, 0.05, 300_000.0)),
    ];
    let mut drawn = [0usize; 2];
    for (i, (name, proj)) in shots.iter().enumerate() {
        let cam = RenderCamera::new(DVec3::ZERO, Quat::IDENTITY, *proj);
        let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ortho-color"),
            size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.surface_format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let globals =
            Globals { view_proj: cam.view_proj(1.0).to_cols_array_2d(), ..Default::default() };
        // A background nothing in the sheet is near, so "map" vs "no map" is one test.
        raster.draw_scene(
            &gpu,
            &view,
            gpu.depth_view(),
            globals,
            &[(map, Some::<TexId>(tex), raw)],
            Some([0.06, 0.06, 0.09, 1.0]),
            None,
        );
        let px = readback(&gpu, &color);
        // The sheet's four cells are strongly saturated and the background is a
        // near-neutral dark blue, so "is this a tile" is a saturation test.
        // Brightness is not: an sRGB-encoded dark background is not dark enough
        // in 8-bit to threshold against.
        drawn[i] = px
            .iter()
            .filter(|p| {
                let (lo, hi) = (p[..3].iter().min().unwrap(), p[..3].iter().max().unwrap());
                *hi as i32 - *lo as i32 > 40
            })
            .count();
        let out = format!("{dir}/ortho_camera_{name}.png");
        save_png(&px, &out);
        println!("{name}: {} of {} pixels are map — wrote {out}", drawn[i], px.len());
    }

    let total = (S * S) as usize;
    assert!(
        drawn[0] * 100 < total,
        "the OLD near plane should have clipped the map away; it drew {} px",
        drawn[0]
    );
    assert!(
        drawn[1] > total / 2,
        "the map must fill the ortho frame, and only {} of {total} px drew",
        drawn[1]
    );
    println!("orthographic gameplay camera OK");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
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
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut o = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * bpp) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            o.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    o
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
