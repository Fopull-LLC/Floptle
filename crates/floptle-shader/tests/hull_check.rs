#[test]
fn solar_hull_panels_compiles() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../solar/shaders/hullPanels.flsl");
    let src = std::fs::read_to_string(path).expect("read hullPanels.flsl");
    let compiled = floptle_shader::compile_fragment(&src)
        .unwrap_or_else(|e| panic!("hullPanels.flsl compile error: {e}"));
    floptle_shader::transpile::validate(floptle_shader::transpile::TEST_PRELUDE, &compiled.chunk)
        .unwrap_or_else(|e| panic!("naga rejects hullPanels.flsl: {} (line {:?})\n{}", e.message, e.chunk_line, compiled.chunk));
    eprintln!("hullPanels OK: {} uniforms, {} textures", compiled.uniforms.len(), compiled.textures.len());
}
