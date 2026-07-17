//! Validate scene/project RON files from the command line — parse each argument
//! as a scene (`.ron` under scenes/) or a `project.ron`, and report what loaded.
//! Exit code 1 if anything failed: usable in scripts and CI.
//!
//! Usage: cargo run -p floptle-scene --example validate -- solar/scenes/planetoid.ron solar/project.ron

fn main() {
    let mut failed = false;
    for arg in std::env::args().skip(1) {
        let p = std::path::Path::new(&arg);
        if p.file_name().is_some_and(|f| f == "project.ron") {
            match floptle_scene::try_load_project(p) {
                Ok(Some(cfg)) => println!(
                    "OK  {arg}: project \"{}\", entry {:?}, {} layer(s)",
                    cfg.title.as_deref().unwrap_or("(untitled)"),
                    cfg.entry_scene.as_deref().unwrap_or("(default)"),
                    cfg.layers.len(),
                ),
                Ok(None) => {
                    println!("ERR {arg}: missing");
                    failed = true;
                }
                Err(e) => {
                    println!("ERR {arg}: {e}");
                    failed = true;
                }
            }
        } else {
            match floptle_scene::load(p) {
                Ok(doc) => {
                    println!("OK  {arg}: scene \"{}\", {} node(s)", doc.name, doc.nodes.len())
                }
                Err(e) => {
                    println!("ERR {arg}: {e}");
                    failed = true;
                }
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
