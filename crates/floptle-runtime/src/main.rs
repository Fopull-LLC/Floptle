//! # Floptle Runtime
//!
//! The headless-of-editor game player. An exported game is this runtime plus a
//! packed project. Also the basis for a future dedicated `server` build.
//!
//! **Phase 1.** Opens a window, creates the GPU, and drives the real core loop
//! (clock + deterministic fixed-step + ECS systems) on every redraw — currently
//! rendering a clear pass with FPS in the title. Pass `--headless` to run the loop
//! without a window (CI / no-display): it advances ~2 simulated seconds and exits.

mod app;
mod camera;
mod runner;

use app::App;

fn main() {
    env_logger::init();
    println!("{} runtime v{}", floptle_core::ENGINE_NAME, floptle_core::ENGINE_VERSION);

    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--import") {
        // `--import <path>`: import a glTF model headlessly and print its stats —
        // a no-GUI way to validate a model (geometry, normals, scale) before it
        // lands in a scene.
        match args.get(i + 1) {
            Some(path) => import_check(path),
            None => eprintln!("usage: floptle-runtime --import <model.glb>"),
        }
    } else if let Some(i) = args.iter().position(|a| a == "--bake") {
        // `--bake <path>`: import + bake a model to SDF (shell mode) headlessly and
        // report the volume + bake time — validates the mesh→SDF path with no GPU.
        match args.get(i + 1) {
            Some(path) => bake_check(path),
            None => eprintln!("usage: floptle-runtime --bake <model.glb>"),
        }
    } else if args.iter().any(|a| a == "--headless") {
        headless_selfcheck();
    } else {
        runner::run();
    }
}

/// Import a model and bake it to an SDF volume (no GPU), reporting size + time.
fn bake_check(path: &str) {
    let m = match floptle_assets::gltf_import::import(std::path::Path::new(path)) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("  import failed: {e}");
            std::process::exit(1);
        }
    };
    let part_pos: Vec<Vec<[f32; 3]>> =
        m.parts.iter().map(|p| p.mesh.vertices.iter().map(|v| v.pos).collect()).collect();
    let part_uv: Vec<Vec<[f32; 2]>> =
        m.parts.iter().map(|p| p.mesh.vertices.iter().map(|v| v.uv).collect()).collect();
    let parts: Vec<floptle_field::BakePart> = m
        .parts
        .iter()
        .enumerate()
        .map(|(i, p)| floptle_field::BakePart {
            positions: &part_pos[i],
            indices: &p.mesh.indices,
            uvs: &part_uv[i],
            texture: p.texture.map(|ti| {
                let t = &m.textures[ti];
                floptle_field::TexRef { pixels: &t.pixels, width: t.width, height: t.height }
            }),
            tint: p.base_color,
        })
        .collect();
    let res = 192u32;
    let thickness = (m.size / res as f32) * 1.2;
    let t0 = std::time::Instant::now();
    let baked =
        floptle_field::bake_model(&parts, res, 2.0, floptle_field::BakeMode::Shell { thickness });
    let dt = t0.elapsed().as_secs_f32();
    let inside = baked.distance.iter().filter(|&&d| d < 0.0).count();
    println!(
        "  baked '{}' → {res}³ SDF in {dt:.2}s ({} voxels, {} are matter, shell {thickness:.2})",
        m.name,
        baked.distance.len(),
        inside
    );
}

/// Import a glTF model with no GPU and report what came back.
fn import_check(path: &str) {
    match floptle_assets::gltf_import::import(std::path::Path::new(path)) {
        Ok(m) => {
            let verts: usize = m.parts.iter().map(|p| p.mesh.vertices.len()).sum();
            let tris: usize = m.parts.iter().map(|p| p.mesh.indices.len()).sum::<usize>() / 3;
            let textured = m.parts.iter().filter(|p| p.texture.is_some()).count();
            println!(
                "  imported '{}' — {} parts ({} textured), {} textures, {} vertices, {} triangles, size {:.3}",
                m.name, m.parts.len(), textured, m.textures.len(), verts, tris, m.size
            );
        }
        Err(e) => {
            eprintln!("  import failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Drive the real core loop for ~2 simulated seconds at 60 fps with no GPU, so the
/// engine is verifiable where there's no display.
fn headless_selfcheck() {
    let mut app = App::new();
    let dt = 1.0 / 60.0;
    for _ in 0..120 {
        app.frame(dt);
    }
    println!(
        "  headless core loop ok — {} frames, {:.2}s sim time, {} live entit{}",
        app.time.frame,
        app.time.elapsed,
        app.world.len(),
        if app.world.len() == 1 { "y" } else { "ies" }
    );
}
