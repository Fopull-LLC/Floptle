//! `cargo run -p floptle-input --example merge_starter -- <project-dir>`
//!
//! Adds any starter binding a project's `input.ron` is missing, leaving
//! everything already there untouched. The same `merge_missing` the editor's
//! "add missing starter bindings" button runs — offered as a CLI so it can be
//! applied to a project without opening the editor.

fn main() {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: merge_starter <project-dir>");
        std::process::exit(2);
    };
    let root = std::path::Path::new(&dir);

    let mut map = match floptle_input::load_map(root) {
        Ok(Some(m)) => m,
        Ok(None) => floptle_input::InputMap::default(),
        Err(e) => {
            eprintln!("{}/input.ron won't parse: {e}", root.display());
            std::process::exit(1);
        }
    };

    let added = map.merge_missing(&floptle_input::InputMap::starter());
    if added == 0 {
        println!("nothing missing — {}/input.ron already covers the starter set", root.display());
        return;
    }
    match floptle_input::save_map(&map, root) {
        Ok(()) => println!("added {added} entr(y/ies) to {}/input.ron", root.display()),
        Err(e) => {
            eprintln!("could not write: {e}");
            std::process::exit(1);
        }
    }
}
