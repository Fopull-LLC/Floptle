//! CLI over `floptle_field::procgen` — roll and generate a whole randomized
//! solar system (the editor's "🪐 New Solar System" runs the same code).
//!
//! Usage: cargo run --release -p floptle-field --example gen_solar [-- <out_dir> [seed] [scene]]
//! Default out_dir `solar/terrain`, seed RANDOM (from the clock — pass one to
//! reproduce a system), scene "system". Writes `<out_dir>/<scene>.<id>.cfield`
//! + `<out_dir>/<scene>.palette` + `solar/scenes/<scene>.ron`.

use floptle_field::procgen::{generate_system, SystemSpec};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = args.get(1).map(String::as_str).unwrap_or("solar/terrain");
    let seed: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
            .unwrap_or(11)
    });
    let scene = args.get(3).map(String::as_str).unwrap_or("system");

    let spec = SystemSpec::random(seed, None);
    println!(
        "seed {seed}: star \"{}\" (lum {:.0}), {} bodies — generating…",
        spec.star.name,
        spec.star.luminosity,
        spec.bodies.len()
    );
    let out = std::path::Path::new(out_dir);
    let scenes = std::path::Path::new("solar/scenes");
    let tex = out
        .parent()
        .map(|p| p.join("textures/terrain"))
        .unwrap_or_else(|| "solar/textures/terrain".into());
    let done = generate_system(&spec, scene, out, scenes, &tex, &mut |line| println!("  {line}"))
        .expect("generate system");
    println!("{}", done.summary);
    println!("Open scene \"{scene}\" in the solar project and press Play.  (seed {seed})");
}
