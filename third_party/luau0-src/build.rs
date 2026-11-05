// FLOPTLE PATCH: the published crate has no build script, so cargo fingerprints
// it by its Rust sources alone, and an edit to the Luau C++ under `luau/` never
// rebuilt anything. mlua-sys compiles that C++ in ITS build script, which reruns
// when this crate changes. Watching `luau/` here makes a C++ edit count.
fn main() {
    println!("cargo:rerun-if-changed=luau");
}
