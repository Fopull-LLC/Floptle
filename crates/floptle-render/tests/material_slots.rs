//! **A model whose parts differ only in material numbers draws the same as
//! one instance as it does as one instance per part.**
//!
//! Three squares side by side, each its own material: different colours,
//! roughness, metallic, rim, glow and ambient, one of them unlit. Drawn once
//! as three meshes and three instances (how every model drew), and once as
//! one merged mesh, one instance and a slot table (`Raster::push_slot_table`,
//! `instance_of_slots`). The two pictures must be the same, byte for byte.
//! The same holds with a tint laid over the merged instance and over each of
//! the three parts, as a node's Tint does.

use floptle_core::Material;
use floptle_render::{
    instance_of_mat, instance_of_slots, merge_parts, plane, Globals, Gpu, MaterialParams, MeshData, Raster,
    SurfaceExtras,
};
use glam::Mat4;

const SIZE: u32 = 96;

fn square(x: f32) -> MeshData {
    let mut m = plane(0.3);
    for v in &mut m.vertices {
        v.pos[0] += x;
    }
    m
}

fn materials() -> [Material; 3] {
    [
        Material {
            color: [0.9, 0.2, 0.1],
            shading: floptle_core::Shading::Physical,
            roughness: 0.2,
            metallic: 0.0,
            shininess: 64.0,
            specular_strength: 1.0,
            ..Default::default()
        },
        Material {
            color: [0.1, 0.7, 0.3],
            shading: floptle_core::Shading::Physical,
            roughness: 0.9,
            metallic: 1.0,
            reflectivity: 0.5,
            rim: [0.2, 0.4, 1.0],
            rim_strength: 0.8,
            emissive: [1.0, 0.8, 0.0],
            emissive_strength: 0.4,
            ambient: 0.5,
            ..Default::default()
        },
        Material { color: [0.3, 0.3, 0.9], unlit: true, ..Default::default() },
    ]
}

/// Lay a tint over a record the way a node's Tint does: colour and alpha
/// multiply, a rim with strength replaces, the ambient multiplies.
fn tint(raw: &mut floptle_render::InstanceRaw, on: bool) {
    if !on {
        return;
    }
    let c = [0.6, 0.9, 1.0];
    for (v, k) in raw.color.iter_mut().zip(c) {
        *v *= k;
    }
    raw.rim = [1.0, 0.0, 0.5, raw.rim[3]];
    raw.params[1] = 0.6;
    raw.params[3] *= 0.7;
}

/// One picture. The renderer is shared between pictures, because building its
/// pipelines is most of what this test costs; each picture registers its own
/// meshes and materials into it.
fn draw(gpu: &Gpu, raster: &mut Raster, color: &wgpu::Texture, merged: bool, tinted: bool) -> Vec<[u8; 4]> {
    let parts = [square(-0.66), square(0.0), square(0.66)];
    let mats = materials();
    let params: Vec<MaterialParams> = mats
        .iter()
        .map(|m| {
            let mut p = MaterialParams::from_material(m);
            p.ext_index = raster.push_surface_extras(SurfaceExtras::from_material(m));
            p
        })
        .collect();
    // The fixture: the slots' extras differ, so a slot reading another's
    // extras (or the instance's) would show.
    assert!(params[0].ext_index != params[1].ext_index && params[0].ext_index != 0, "the materials share extras: {:?}", params.iter().map(|p| p.ext_index).collect::<Vec<_>>());
    let model = Mat4::from_translation(glam::Vec3::new(0.0, 0.0, 0.5));
    let mut list = Vec::new();
    if merged {
        let refs: Vec<&MeshData> = parts.iter().collect();
        let (mesh, starts) = merge_parts(&refs).expect("three parts merge");
        let id = raster.register(gpu, &mesh, None);
        let table = raster.push_slot_table(&starts, &params).expect("a table for three slots");
        let mut raw = instance_of_slots(model, table, 0, false);
        tint(&mut raw, tinted);
        list.push((id, None, raw));
    } else {
        for (part, p) in parts.iter().zip(&params) {
            let id = raster.register(gpu, part, None);
            let mut raw = instance_of_mat(model, p);
            tint(&mut raw, tinted);
            list.push((id, None, raw));
        }
    }
    let globals = Globals {
        view_proj: Mat4::IDENTITY.to_cols_array_2d(),
        light_dir: [0.4, 0.5, -0.75, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.3, 0.3, 0.3, 0.0],
        ..Default::default()
    };
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    raster.draw_scene(gpu, &view, gpu.depth_view(), globals, &list, Some([0.0, 0.0, 0.0, 1.0]), None);
    floptle_render::probe::readback(gpu, color)
}

#[test]
fn three_parts_and_their_merge_draw_the_same_pixels() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("material-slots"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut raster = Raster::new(&gpu);
    for tinted in [false, true] {
        let parts = draw(&gpu, &mut raster, &color, false, tinted);
        let merged = draw(&gpu, &mut raster, &color, true, tinted);
        // The fixture: the three squares are there and differ from each other.
        let lit = parts.iter().filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8).count();
        assert!(lit > (SIZE * SIZE / 8) as usize, "the squares did not draw: {lit} lit pixels");
        let row = (SIZE / 2) as usize * SIZE as usize;
        let at = |x: f32| parts[row + ((x * 0.5 + 0.5) * SIZE as f32) as usize];
        assert!(at(-0.66) != at(0.0) && at(0.0) != at(0.66), "the fixture's three materials look alike");
        let differ = parts.iter().zip(&merged).filter(|(a, b)| a != b).count();
        assert_eq!(differ, 0, "tinted {tinted}: the merged model differs from its parts in {differ} pixels");
    }
}

/// **A slot table is shared whole, and a malformed one is refused.** Two
/// merged models with the same slots share one table; a different slot is a
/// different table. More slots than an instance can address, starts that do
/// not climb, or a count that does not match the slots get no table. The
/// parts merge into one mesh whose starts are where each part begins.
#[test]
fn slot_tables_are_shared_whole_and_bad_ones_refused() {
    let gpu = Gpu::headless(16, 16);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass");
        return;
    }
    let mut raster = Raster::new(&gpu);
    let p = |c: f32| MaterialParams::flat([c, 0.5, 0.5]);
    let a = raster.push_slot_table(&[4, 8], &[p(0.1), p(0.2), p(0.3)]).expect("a table");
    let b = raster.push_slot_table(&[4, 8], &[p(0.1), p(0.2), p(0.3)]).expect("a table");
    let c = raster.push_slot_table(&[4, 8], &[p(0.1), p(0.2), p(0.4)]).expect("a table");
    assert_eq!(a, b, "the same slots made two tables");
    assert_ne!(a, c, "different slots shared a table");
    assert!(raster.push_slot_table(&[4], &[p(0.1), p(0.2), p(0.3)]).is_none(), "a start too few");
    assert!(raster.push_slot_table(&[8, 4], &[p(0.1), p(0.2), p(0.3)]).is_none(), "starts that fall");
    let nine: Vec<MaterialParams> = (0..9).map(|i| p(i as f32 / 10.0)).collect();
    let starts: Vec<u32> = (1..9).map(|i| i * 4).collect();
    assert!(raster.push_slot_table(&starts, &nine).is_none(), "nine slots past MAX_SLOTS");

    let parts = [square(-0.5), square(0.0), square(0.5)];
    let refs: Vec<&MeshData> = parts.iter().collect();
    let (mesh, starts) = merge_parts(&refs).expect("three parts merge");
    assert_eq!(starts, vec![4, 8], "each square has four corners");
    assert_eq!(mesh.vertices.len(), 12);
    let moved: Vec<u32> = parts[1].indices.iter().map(|i| i + 4).collect();
    assert_eq!(mesh.indices[6..12], moved[..], "the second part's indices moved past the first");
    assert!(merge_parts(&refs[..1]).is_none(), "one part is nothing to merge");
}
