//! Validate scene/project RON files from the command line — parse each argument
//! as a scene (`.ron` under scenes/) or a `project.ron`, and report what loaded.
//! Exit code 1 if anything failed: usable in scripts and CI.
//!
//! Usage: cargo run -p floptle-scene --example validate -- solar/scenes/planetoid.ron solar/project.ron

fn main() {
    let mut failed = false;
    for arg in std::env::args().skip(1) {
        let p = std::path::Path::new(&arg);
        if arg.ends_with(".vfx.ron") {
            match floptle_scene::load_vfx_effect(p) {
                Ok(doc) => {
                    println!("OK  {arg}: effect \"{}\", {} track(s)", doc.name, doc.tracks.len())
                }
                Err(e) => {
                    println!("ERR {arg}: {e}");
                    failed = true;
                }
            }
        } else if arg.ends_with(".prefab.ron") {
            // Same flat Vec<NodeDoc> body the node clipboard uses; tolerate
            // the clipboard's tag line so a pasted clipboard validates too.
            let parsed = std::fs::read_to_string(p).map_err(|e| e.to_string()).and_then(|t| {
                let body = t.trim_start().strip_prefix("//floptle-nodes-v1").unwrap_or(&t).trim_start().to_string();
                ron::from_str::<Vec<floptle_scene::NodeDoc>>(&body).map_err(|e| e.to_string())
            });
            match parsed {
                Ok(docs) => {
                    let bad = docs
                        .iter()
                        .filter_map(|d| d.parent)
                        .find(|&i| i >= docs.len());
                    if let Some(i) = bad {
                        println!("ERR {arg}: parent index {i} out of range ({} node(s))", docs.len());
                        failed = true;
                    } else {
                        println!("OK  {arg}: prefab, {} node(s)", docs.len());
                    }
                }
                Err(e) => {
                    println!("ERR {arg}: not a prefab ({e})");
                    failed = true;
                }
            }
        } else if p.file_name().is_some_and(|f| f == "project.ron") {
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
