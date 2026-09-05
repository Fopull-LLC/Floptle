// THROWAWAY: headless model conversion, the editor's own path.
fn main() {
    let mut args = std::env::args().skip(1);
    let src = args.next().expect("usage: conv <model> [out.glb]");
    let src = std::path::PathBuf::from(src);
    match floptle_convert::convert(&src) {
        Ok((bytes, report)) => {
            let out = match args.next() {
                Some(o) => std::path::PathBuf::from(o),
                None => floptle_convert::output_path(&src),
            };
            std::fs::write(&out, &bytes).expect("write");
            println!("ok {} -> {} ({} bytes) — {}", src.display(), out.display(), bytes.len(), report.summary());
        }
        Err(e) => {
            eprintln!("FAILED {}: {e}", src.display());
            std::process::exit(1);
        }
    }
}
