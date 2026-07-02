// scratch: verify UVMappedR6 imports with skeleton + clips
fn main() {
    let path = std::path::Path::new("/mnt/disks/3tb/GithubRepositories/Floptle/assets/models/_test/UVMappedR6.glb");
    let names = floptle_assets::probe_animations(path);
    println!("probe: {names:?}");
    let rigged = floptle_assets::import_rigged(path).expect("import ok").expect("has anims");
    println!("skeleton nodes: {}", rigged.skeleton.len());
    for n in rigged.skeleton.nodes.iter() {
        println!("  node {:?} parent {:?}", n.name, n.parent);
    }
    println!("parts: {}", rigged.parts.len());
    for p in rigged.parts.iter() {
        println!("  part node={} verts={} skinned={}", p.node, p.mesh.vertices.len(), p.skin.is_some());
    }
    for c in &rigged.clips {
        println!("clip {:?} dur={:.2} channels={}", c.name, c.duration, c.channels.len());
    }
    println!("size {:.2} min {:?} max {:?}", rigged.size, rigged.min, rigged.max);
}
