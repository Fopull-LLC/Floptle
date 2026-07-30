//! Spritesheet probe: a Material that slices its base texture into a grid must
//! draw exactly ONE cell, filling the quad — no neighbours bleeding in, no
//! shrunken copy of the whole sheet.
//!
//! The mechanism under test is `Material::effective_tiling` composed with
//! `base_texel`'s mode-1 UV transform in raster.wgsl: the transform scales UVs
//! about the 0.5 CENTRE, so a cell window's offset is its own centre minus that
//! one. Get it wrong (corner-based offset, say) and every cell but the middle of
//! the sheet lands on the wrong frame — which is exactly what this asserts
//! against, per cell, through `MaterialParams::from_material` (the same packing
//! the editor draws with).
//!
//! Four quads across, showing cells 0 (top-left), 3 (top-right), 5 (row 1) and
//! 15 (bottom-right) of a 4×4 sheet whose cells are flat, distinct colours — plus
//! a fifth CONTROL quad with the same texture and no sheet, which must show the
//! whole 4×4 grid. The control is what keeps the harness honest: a flat band on
//! the first four proves one cell fills the quad, and the control proves the
//! texture really does have 16 different cells to get wrong.
//!
//! Run: cargo run -p floptle-render --example sprite_sheet_probe -- <out.png>

use floptle_render::{
    instance_of_mat, plane, Globals, Gpu, MaterialParams, MeshData, Projection, Raster,
    RenderCamera, TexId, TexSampling, TextureData,
};
use glam::{Mat4, Quat, Vec3};

const W: u32 = 640;
const H: u32 = 200;
const COLS: u32 = 4;
const ROWS: u32 = 4;
/// The cells this probe puts on screen, left to right. `None` = the control quad
/// (no sheet at all — the whole texture, as before this feature existed).
const SHOWN: [Option<u32>; 5] = [Some(0), Some(3), Some(5), Some(15), None];
/// Quad half-extent and centre spacing, in world units.
const HALF: f32 = 0.5;
const STEP: f32 = 1.5;
/// Camera distance and vertical FOV — the screen-space sample points are derived
/// from these, so the layout can move without the asserts going stale.
const DIST: f32 = 6.0;
const FOV_Y: f32 = 0.5;

/// The flat colour of cell `(cx, cy)` — spread so a one-cell slip is unmistakable
/// both to an assert and to an eye looking at the PNG.
fn cell_color(cx: u32, cy: u32) -> [u8; 3] {
    [(30 + cx * 70) as u8, (30 + cy * 70) as u8, (220 - (cx + cy) * 25) as u8]
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "sprite_sheet.png".into());
    let gpu = Gpu::headless(W, H);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sheet-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sheet-depth"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    // A 4×4 sheet, 16 px per cell, each cell one flat colour.
    let cell_px = 16u32;
    let (tw, th) = (COLS * cell_px, ROWS * cell_px);
    let mut pixels = Vec::with_capacity((tw * th * 4) as usize);
    for y in 0..th {
        for x in 0..tw {
            let c = cell_color(x / cell_px, y / cell_px);
            pixels.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels, width: tw, height: th },
        TexSampling::default(),
    );

    let data: MeshData = plane(HALF);
    let mesh = raster.register(&gpu, &data, None);

    let eye = Vec3::new(0.0, 0.0, DIST);
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: FOV_Y, near: 0.02, far: 100.0 },
    );
    let quad_x = |i: usize| (i as f32 - (SHOWN.len() - 1) as f32 * 0.5) * STEP;
    let mut instances = Vec::new();
    for (i, cell) in SHOWN.iter().enumerate() {
        // The production path: an artist-facing Material with a sheet grid, packed
        // by the renderer's own converter. `None` leaves the grid at 0×0 — not a
        // sheet, so the whole image draws.
        let (cols, rows, cell) = match cell {
            Some(c) => (COLS, ROWS, *c),
            None => (0, 0, 0),
        };
        let mat = floptle_core::Material {
            unlit: true,
            sheet_cols: cols,
            sheet_rows: rows,
            cell,
            ..floptle_core::Material::default()
        };
        let x = quad_x(i);
        instances.push((
            mesh,
            Some::<TexId>(tex),
            instance_of_mat(
                Mat4::from_translation(Vec3::new(x, 0.0, 0.0) - eye),
                &MaterialParams::from_material(&mat),
            ),
        ));
    }
    let globals = Globals {
        view_proj: cam.view_proj(W as f32 / H as f32).to_cols_array_2d(),
        ..Default::default()
    };
    raster.draw_scene(
        &gpu,
        &color_view,
        &depth_view,
        globals,
        &instances,
        Some([0.05, 0.05, 0.06, 1.0]),
        None,
    );

    let raw = readback(&gpu, &color);
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let px: Vec<[u8; 4]> =
        raw.into_iter().map(|p| if bgra { [p[2], p[1], p[0], p[3]] } else { p }).collect();
    let at = |fx: f32, fy: f32| px[((fy * H as f32) as u32 * W + (fx * W as f32) as u32) as usize];

    // World x → screen fraction, from the camera the probe actually used.
    let view_w = 2.0 * DIST * (FOV_Y * 0.5).tan() * (W as f32 / H as f32);
    let sx = |x: f32| 0.5 + x / view_w;
    let mut bad = Vec::new();
    for (i, cell) in SHOWN.iter().enumerate() {
        let (cx, cy) = (sx(quad_x(i)), 0.5);
        // Unlit, white-tinted, non-sRGB target: the cell's texel reaches the
        // framebuffer byte-for-byte, so a mismatch here is a UV-window bug and
        // nothing else.
        let got = at(cx, cy);
        match cell {
            Some(cell) => {
                let want = cell_color(cell % COLS, cell / COLS);
                let d: i32 = (0..3).map(|k| (got[k] as i32 - want[k] as i32).abs()).max().unwrap();
                println!("cell {cell:>2}: want {want:?}  got {:?}  Δ{d}", &got[..3]);
                // A quad showing ONE cell is flat, so its corners must match its
                // centre — this is what a half-cell offset or a bleeding window
                // would break even when the centre pixel looks right.
                let quarter = HALF * 0.6;
                for (ox, oy) in [(-quarter, 0.0), (quarter, 0.0), (0.0, -quarter), (0.0, quarter)] {
                    let e = at(sx(quad_x(i) + ox), 0.5 + oy / (2.0 * DIST * (FOV_Y * 0.5).tan()));
                    let de: i32 =
                        (0..3).map(|k| (e[k] as i32 - want[k] as i32).abs()).max().unwrap();
                    if de > 8 {
                        println!("  edge sample {:?} differs from the cell colour", &e[..3]);
                        bad.push(*cell);
                    }
                }
                if d > 8 {
                    bad.push(*cell);
                }
            }
            None => {
                // The control: no sheet ⇒ the whole 4×4 grid, so the quad's own
                // quarters must NOT all be one colour. If they are, the "sheet"
                // texture had nothing to slice and every assert above was vacuous.
                let l = at(sx(quad_x(i) - HALF * 0.6), 0.5);
                let r = at(sx(quad_x(i) + HALF * 0.6), 0.5);
                println!("control (no sheet): left {:?} right {:?}", &l[..3], &r[..3]);
                assert!(
                    l != r,
                    "the control quad is flat — the probe's texture isn't a real sheet, \
                     so nothing above was actually tested (see {out})"
                );
            }
        }
    }

    save_png(&px, &out);
    assert!(
        bad.is_empty(),
        "cells {bad:?} drew the wrong frame — the sheet's UV window is off (see {out})"
    );
    println!("spritesheet cells OK; wrote {out}");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (W * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
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
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * bpp) as usize;
            out.push([view[i], view[i + 1], view[i + 2], view[i + 3]]);
        }
    }
    out
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let mut flat = Vec::with_capacity(px.len() * 4);
    for p in px {
        flat.extend_from_slice(p);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("png header").write_image_data(&flat).expect("png data");
}
