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
    instance_of, instance_of_mat, mesh,
};
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

    // The welded room, built once — this is the mesh a Tilemap node uploads.
    let cells: Vec<u32> = (0..COLS * ROWS).map(|i| i % 4).collect();
    let t0 = std::time::Instant::now();
    let room = mesh::tilemap(COLS, ROWS, TILE, 2, 2, [1.0 / 128.0, 1.0 / 128.0], &cells);
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let room_id = raster.register(&gpu, &room, None);

    // Straight down -Z at the plane, far enough back to hold the room.
    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 26.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.05, far: 500.0 },
    );
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
                Vec3::new(x, y, 0.0),
            );
            room_quads.push((quad, None, instance_of(m, [0.6, 0.6, 0.6])));
        }
    }
    let room_mesh = vec![(room_id, None, instance_of(Mat4::IDENTITY, [0.6, 0.6, 0.6]))];

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
                Vec3::new(a.cos() * r, a.sin() * r, 0.1),
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
}
