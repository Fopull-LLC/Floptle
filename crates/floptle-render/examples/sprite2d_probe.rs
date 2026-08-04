//! What the 2D layer costs, measured against the way a 2D game had to do it
//! before (`floptle/0058`).
//!
//! Run: `cargo run -p floptle-render --release --example sprite2d_probe`
//!
//! The comparison is the shipping bullet-hell workload: **1400 sprites and a
//! 200-tile room**, at 1080p, drawn with one texture.
//!
//! * **before** — the room is one quad per tile and the sprites are a pool of
//!   scene nodes, so every tile and every bullet is an independent instance
//!   whose transform the game writes each frame.
//! * **after** — the room is one welded mesh (one draw, and no seams by
//!   construction) and the sprites are one batch node the engine fills.
//!
//! Be clear about what moves and what doesn't: **the sprites cost the same on
//! the GPU either way**, because 1400 quads is 1400 quads however they were
//! gathered. What changes for them is that nobody writes 1400 node transforms
//! from Lua any more, and that each one can now carry its own tint. The room is
//! where the frame time actually moves.

use floptle_render::{
    Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster, RenderCamera,
    TexSampling, instance_of, instance_of_mat, mesh,
};
use floptle_render::mesh::TextureData;
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 1920;
const H: u32 = 1080;
const WARMUP: u32 = 8;
const FRAMES: u32 = 48;

/// The shipping arena: 13 x 7 floor, a two-deep wall ring, some rubble.
const COLS: u32 = 20;
const ROWS: u32 = 10;
/// The shipping bullet cap.
const SPRITES: usize = 1400;
/// 32 px tiles at 240p — deliberately not a round number, because that is the
/// case the seams showed up in.
const TILE: f32 = 1.436_4;

fn main() {
    let gpu = Gpu::headless(W, H);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sprite2d-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    let quad = raster.register(&gpu, &mesh::plane(0.5), None);
    // A 2x2 sheet of flat colours, each cell filled edge to edge — so a seam
    // between two tiles, or a cell bleeding into its neighbour, is obvious.
    let sheet = raster.register_texture(&gpu, &checker_sheet(), TexSampling::default());

    // The welded room, built once — this is the mesh a Tilemap node uploads.
    // A DIAGONAL pattern, not `i % 4`: with a grid width that divides by four,
    // every row would come out identical and the picture would show vertical
    // bands with no horizontal edges in it at all — which is exactly the half of
    // the seam guarantee you would then not be looking at.
    let cells: Vec<u32> =
        (0..COLS * ROWS).map(|i| ((i / COLS) + (i % COLS)) % 4).collect();
    let t0 = std::time::Instant::now();
    let room = mesh::tilemap(COLS, ROWS, TILE, 2, 2, [1.0 / 128.0, 1.0 / 128.0], &cells);
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let room_id = raster.register(&gpu, &room, None);

    // Straight down -Z at the plane, far enough back to hold the room.
    const CAM_Z: f32 = 26.0;
    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, CAM_Z as f64),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.05, far: 500.0 },
    );
    // `view_proj` is CAMERA-RELATIVE (ADR-0015): it has no translation, so every
    // model matrix must already have the camera's world position taken out of
    // it. Feeding it absolute positions puts the whole scene behind the camera
    // and renders an empty frame — which still benchmarks beautifully, and is
    // why this probe also writes a picture.
    let rel = |p: Vec3| Vec3::new(p.x, p.y, p.z - CAM_Z);
    let globals = Globals {
        view_proj: cam.view_proj(W as f32 / H as f32).to_cols_array_2d(),
        light_dir: [0.3, 0.8, 0.5, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.5, 0.5, 0.55, 0.0],
        ..Default::default()
    };

    // ---- the room, two ways -------------------------------------------------
    let mut room_quads: Vec<(MeshId, Option<floptle_render::TexId>, InstanceRaw)> = Vec::new();
    let (hw, hh) = (COLS as f32 * TILE * 0.5, ROWS as f32 * TILE * 0.5);
    for row in 0..ROWS {
        for col in 0..COLS {
            // One transform per tile — the shape that opens the seams, because
            // each edge is computed through its own matrix.
            let x = col as f32 * TILE - hw + TILE * 0.5;
            let y = hh - row as f32 * TILE - TILE * 0.5;
            let m = Mat4::from_scale_rotation_translation(
                Vec3::splat(TILE),
                Quat::IDENTITY,
                rel(Vec3::new(x, y, 0.0)),
            );
            room_quads.push((quad, None, instance_of(m, [0.6, 0.6, 0.6])));
        }
    }
    let room_at = Mat4::from_translation(rel(Vec3::ZERO));
    let room_mesh = vec![(room_id, None, instance_of(room_at, [0.6, 0.6, 0.6]))];

    // ---- the sprites --------------------------------------------------------
    // Identical instance data both ways; what differs is who built it. Placed
    // on a spiral so they overlap the way a bullet pattern does.
    let build_sprites = |tinted: bool| -> Vec<(MeshId, Option<floptle_render::TexId>, InstanceRaw)> {
        let mut out = Vec::with_capacity(SPRITES);
        for i in 0..SPRITES {
            let a = i as f32 * 0.137;
            let r = 1.0 + (i as f32) * 0.008;
            let m = Mat4::from_scale_rotation_translation(
                Vec3::splat(0.35),
                Quat::from_rotation_z(a),
                rel(Vec3::new(a.cos() * r, a.sin() * r, 0.1)),
            );
            let mut mp = MaterialParams::flat([1.0, 0.9, 0.4]);
            if tinted {
                // What the pooled-quad version could not do: this one is
                // flashing, and it is the only one that changes colour.
                let hit = (i % 97) == 0;
                mp.color = if hit { [1.0, 0.2, 0.2] } else { [1.0, 0.9, 0.4] };
            }
            out.push((quad, None, instance_of_mat(m, &mp)));
        }
        out
    };
    let sprites_pooled = build_sprites(false);
    let sprites_batch = build_sprites(true);

    let mut before = room_quads.clone();
    before.extend_from_slice(&sprites_pooled);
    let mut after = room_mesh.clone();
    after.extend_from_slice(&sprites_batch);

    let mut time = |label: &str, set: &[(MeshId, Option<floptle_render::TexId>, InstanceRaw)]| {
        let mut draw = || {
            raster.draw_scene(&gpu, &view, gpu.depth_view(), globals, set, None, None);
        };
        for _ in 0..WARMUP {
            draw();
        }
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let t = std::time::Instant::now();
        for _ in 0..FRAMES {
            draw();
            gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;
        println!("  {label:<44} {ms:6.2} ms/frame   ({} instances)", set.len());
        ms
    };

    println!("2D probe — {SPRITES} sprites + a {COLS}x{ROWS} room, {W}x{H}\n");

    println!("the room on its own:");
    let a = time("one quad per tile (before)", &room_quads);
    let b = time("one welded mesh (after)", &room_mesh);
    println!("  {:>44} {:.2}x\n", "room speedup:", a / b.max(1e-6));

    println!("the sprites on its own:");
    time("pooled scene quads (before)", &sprites_pooled);
    time("one sprite batch, tinted (after)", &sprites_batch);
    println!("  {:>44} same instance count, so the GPU cost is the\n  {:>44} same — the win here is that nothing writes 1400\n  {:>44} node transforms from Lua, and each sprite can\n  {:>44} finally carry its own tint.\n", "", "", "", "");

    println!("together:");
    let a = time("before", &before);
    let b = time("after", &after);
    println!("  {:>44} {:.2}x", "overall speedup:", a / b.max(1e-6));
    println!(
        "\nbuilding the welded room mesh took {build_ms:.3} ms — paid when the grid\nchanges, not per frame. {} vertices, {} indices.",
        room.vertices.len(),
        room.indices.len()
    );

    // A PICTURE, because a benchmark cannot tell you the tiles line up or that
    // the tint reached the right sprite. `sprite2d_probe -- out.png` draws the
    // welded room textured with the sheet, a row of sprites cycling its cells,
    // and three tinted copies of one cell.
    if let Some(out) = std::env::args().nth(1) {
        let mut shot: Vec<(MeshId, Option<floptle_render::TexId>, InstanceRaw)> = Vec::new();
        let mut room_mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        room_mat.unlit = true;
        room_mat.alpha = 1.0;
        shot.push((room_id, Some(sheet), instance_of_mat(room_at, &room_mat)));

        // One sprite per cell, along the bottom, each showing a different cell
        // of the same sheet through the same instance lane a batch uses.
        for cell in 0..4u32 {
            let m = floptle_core::Material {
                sheet_cols: 2,
                sheet_rows: 2,
                cell,
                unlit: true,
                ..Default::default()
            };
            let mut mp = MaterialParams::from_material_inset(&m, [1.0 / 128.0, 1.0 / 128.0]);
            mp.unlit = true;
            let x = -6.0 + cell as f32 * 3.0;
            let t = Mat4::from_scale_rotation_translation(
                Vec3::splat(2.2),
                Quat::IDENTITY,
                rel(Vec3::new(x, -9.0, 1.0)),
            );
            shot.push((quad, Some(sheet), instance_of_mat(t, &mp)));
        }
        // …and the same cell three times with three tints: the thing a shared
        // Material could never do.
        for (i, tint) in [[1.0, 1.0, 1.0], [1.0, 0.25, 0.25], [0.3, 0.5, 1.0]].iter().enumerate() {
            let m = floptle_core::Material {
                sheet_cols: 2,
                sheet_rows: 2,
                cell: 0,
                unlit: true,
                ..Default::default()
            };
            let mut mp = MaterialParams::from_material_inset(&m, [1.0 / 128.0, 1.0 / 128.0]);
            mp.unlit = true;
            mp.color = *tint;
            let t = Mat4::from_scale_rotation_translation(
                Vec3::splat(2.2),
                Quat::IDENTITY,
                rel(Vec3::new(6.0 + i as f32 * 2.6, 9.0, 1.0)),
            );
            shot.push((quad, Some(sheet), instance_of_mat(t, &mp)));
        }
        // CLEAR for the picture. `None` means LoadOp::Load for colour AND
        // depth, so a shot drawn after the timing loops would be depth-rejected
        // against 48 frames of stale depth and composited onto whatever was
        // already there — which is a blank image that still benchmarks fine.
        raster.draw_scene(
            &gpu,
            &view,
            gpu.depth_view(),
            globals,
            &shot,
            Some([0.06, 0.07, 0.09, 1.0]),
            None,
        );
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        save_png(&gpu, &color, &out);
        println!("wrote {out}");
    }
}

/// A 2x2 sheet: four flat colours, each filling its cell right to the edge.
///
/// Flat and edge-to-edge on purpose — a gap between two tiles shows as the
/// clear colour, and a cell reaching into its neighbour shows as the wrong
/// colour along one side. Both would be invisible on textured art.
fn checker_sheet() -> TextureData {
    const N: u32 = 128;
    let cells = [[220u8, 90, 70], [90, 190, 110], [80, 130, 230], [235, 200, 90]];
    let mut pixels = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let c = cells[((y / (N / 2)) * 2 + (x / (N / 2))) as usize];
            pixels.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    TextureData { pixels, width: N, height: N }
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
    let bpp = 4u32;
    let unpadded = W * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
    encoder.copy_texture_to_buffer(
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
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for row in 0..H {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
