//! One grid, two sheets (`floptle/0092`): a tilemap layer whose squares come
//! from more than one image draws them all, in one grid, at one set of
//! coordinates.
//!
//! The mesh builder is unchanged — it is handed one page's squares at a time,
//! renumbered into that page's own cell space, with everything else holed out.
//! What this probe checks is the part a unit test cannot: that the pages
//! composite into one picture rather than one overdrawing the other, and that
//! the seam between a square from sheet A and its neighbour from sheet B is a
//! seam like any other (which it must be, because both edges are still the same
//! expression in the same builder).
//!
//! Run: cargo run -p floptle-render --example tilemap_pages_probe -- <outdir>

use floptle_render::{
    instance_of_mat, mesh, Globals, Gpu, MaterialParams, Projection, Raster, RenderCamera,
    TexId, TexSampling, TextureData,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 320;
const COLS: u32 = 8;
const ROWS: u32 = 8;
const TILE: f32 = 1.0;
/// Page 0 is cut 2x2; page 1 is cut 1x1. Deliberately DIFFERENT, because a
/// shared cut would hide a page using the wrong sheet's grid.
const P0: (u32, u32) = (2, 2);
const P1: (u32, u32) = (1, 1);

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    // Page 0: four flat colours in a 2x2. Page 1: one flat colour, unmistakably
    // not on the first sheet.
    let quad = [[220u8, 90, 80], [90, 190, 120], [90, 140, 230], [230, 200, 90]];
    let tex0 = flat_sheet(&gpu, &mut raster, 64, |x, y| quad[((y / 32) * 2 + (x / 32)) as usize]);
    let tex1 = flat_sheet(&gpu, &mut raster, 64, |_, _| [235, 100, 235]);

    // A checkerboard of the two SHEETS: even squares from page 0, odd from
    // page 1, so "did both pages draw" and "did they land in the right cells"
    // are the same question.
    let data: Vec<u32> = (0..COLS * ROWS)
        .map(|i| {
            let (x, y) = (i % COLS, i / COLS);
            if (x + y) % 2 == 0 {
                floptle_core::tile_cell_of(0, (x + y) % 4)
            } else {
                floptle_core::tile_cell_of(1, 0)
            }
        })
        .collect();

    let pages = [(0u32, P0, tex0), (1, P1, tex1)];
    let mut draws = Vec::new();
    for (page, (pc, pr), tex) in pages {
        let Some(squares) = page_squares(&data, page, pc * pr) else { continue };
        let md = mesh::tilemap(COLS, ROWS, TILE, pc, pr, [1.0 / 64.0; 2], &squares);
        let mid = raster.register(&gpu, &md, None);
        draws.push((mid, Some::<TexId>(tex)));
    }
    assert_eq!(draws.len(), 2, "both sheets must contribute a mesh");

    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::of_camera(1.05, true, COLS as f32 * TILE, 0.05, 300_000.0),
    );
    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::from_translation(Vec3::ZERO), &mat);
    let instances: Vec<_> = draws.iter().map(|&(m, t)| (m, t, raw)).collect();

    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pages-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    raster.draw_scene(
        &gpu,
        &view,
        gpu.depth_view(),
        Globals { view_proj: cam.view_proj(1.0).to_cols_array_2d(), ..Default::default() },
        &instances,
        Some([0.06, 0.06, 0.09, 1.0]),
        None,
    );

    let px = readback(&gpu, &color);
    // Sample the centre of a square known to be page 1 and one known to be
    // page 0. The grid is 8x8 filling the frame, so square (x, y) centres at
    // ((x + 0.5) / 8, (y + 0.5) / 8) of the image.
    let at = |cx: u32, cy: u32| {
        let fx = ((cx as f32 + 0.5) / COLS as f32 * S as f32) as u32;
        let fy = ((cy as f32 + 0.5) / ROWS as f32 * S as f32) as u32;
        px[(fy.min(S - 1) * S + fx.min(S - 1)) as usize]
    };
    // (1, 0): x + y is odd → page 1 → magenta.
    let odd = at(1, 0);
    // (0, 0): x + y is even → page 0 → one of the four quad colours, none of
    // which is magenta (all have a green channel above their blue, or vice
    // versa, but never both channels high with green low).
    let even = at(0, 0);
    println!("page-1 square {odd:?}   page-0 square {even:?}");
    let magenta = |p: [u8; 4]| p[0] > 150 && p[2] > 150 && p[1] < 140;
    assert!(magenta(odd), "the second sheet must draw its own squares, got {odd:?}");
    assert!(!magenta(even), "and must not draw over the first sheet's, got {even:?}");
    assert!(
        even.iter().take(3).any(|&c| c > 60),
        "the first sheet's squares must still be there, got {even:?}"
    );

    let out = format!("{dir}/tilemap_pages.png");
    save_png(&px, &out);
    println!("two sheets, one grid — wrote {out}");
}

/// The split the editor does per page, repeated here because a probe cannot
/// reach into the editor binary. Kept to the same five lines it is there.
fn page_squares(data: &[u32], page: u32, page_cells: u32) -> Option<Vec<u32>> {
    let mut any = false;
    let out: Vec<u32> = data
        .iter()
        .map(|&packed| {
            if packed == floptle_core::EMPTY_TILE {
                return floptle_core::EMPTY_TILE;
            }
            let cell = floptle_core::tile_index(packed);
            let local = floptle_core::tile_in_page(cell);
            if floptle_core::tile_page(cell) != page || local >= page_cells {
                return floptle_core::EMPTY_TILE;
            }
            any = true;
            floptle_core::tile_pack(local, floptle_core::tile_xform(packed))
        })
        .collect();
    any.then_some(out)
}

fn flat_sheet(
    gpu: &Gpu,
    raster: &mut Raster,
    n: u32,
    color: impl Fn(u32, u32) -> [u8; 3],
) -> TexId {
    let mut pixels = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let c = color(x, y);
            pixels.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    raster.register_texture(
        gpu,
        &TextureData { pixels, width: n, height: n },
        TexSampling::default(),
    )
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
