//! **A model whose parts wear different pictures draws the same as one
//! instance over texture arrays as it does as one instance per part.**
//!
//! Three squares side by side, each its own picture and material: two with
//! normal maps (different ones), physical shading, and the third cut out by
//! its picture's alpha. Drawn once as three meshes and three instances, each
//! with its own pictures bound, and once as one merged mesh and one instance
//! whose slots read layers of pooled arrays (`Raster::pool_texture`,
//! `array_set_for`, `push_slot_table_layered`). The two pictures must be the
//! same, byte for byte: plain, after a depth prepass (the cutout runs in
//! `fs_depth` there, on the array build), and with both tinted half
//! see-through (the blended pass).

use floptle_core::Material;
use floptle_render::{
    Raymarch, RaymarchGlobals, instance_of_mat, instance_of_slots, merge_parts, plane, Globals, Gpu, MaterialParams, MeshData, Raster,
    SurfaceExtras, TexFilter, TexId, TexSampling, TextureData,
};
use glam::Mat4;

const SIZE: u32 = 128;

fn square(x: f32) -> MeshData {
    let mut m = plane(0.3);
    for v in &mut m.vertices {
        v.pos[0] += x;
    }
    m
}

/// A 32² picture of stripes and a gradient in `tint`; with `holes`, a
/// checker of fully transparent texels.
fn picture(tint: [u8; 3], holes: bool) -> TextureData {
    let mut pixels = Vec::new();
    for y in 0..32u32 {
        for x in 0..32u32 {
            let band = if (x / 4 + y / 8) % 2 == 0 { 255 } else { 120 };
            let a = if holes && (x / 8 + y / 8) % 2 == 1 { 0 } else { 255 };
            pixels.extend_from_slice(&[
                (tint[0] as u32 * band / 255) as u8,
                (tint[1] as u32 * band / 255) as u8,
                ((tint[2] as u32 + y * 4).min(255)) as u8,
                a,
            ]);
        }
    }
    TextureData { width: 32, height: 32, pixels }
}

/// A 32² normal map of ridges along `axis`.
fn ridges(axis: usize) -> TextureData {
    let mut pixels = Vec::new();
    for y in 0..32u32 {
        for x in 0..32u32 {
            let t = if axis == 0 { x } else { y };
            let tilt = ((t as f32 * 0.8).sin() * 115.0) as i32;
            let (nx, ny) = if axis == 0 { (128 + tilt, 128) } else { (128, 128 + tilt) };
            pixels.extend_from_slice(&[nx as u8, ny as u8, 230, 255]);
        }
    }
    TextureData { width: 32, height: 32, pixels }
}

/// Lit by a key light (the field globals carry it, `Raymarch::field_bind`),
/// with a rim, both of which follow each surface's own mapped normal, so a
/// slot reading the wrong normal map shows.
fn materials() -> [Material; 3] {
    let rim = |m: Material| Material { rim: [1.0, 0.9, 0.7], rim_strength: 0.5, ..m };
    [
        rim(Material { shading: floptle_core::Shading::Physical, roughness: 0.3, normal_strength: 1.0, ..Default::default() }),
        rim(Material { shading: floptle_core::Shading::Physical, roughness: 0.7, metallic: 0.6, normal_strength: 0.8, ..Default::default() }),
        // No normal map, but read as one would be: a slot that read the wrong
        // neutral layer (white, not the flat normal) would light differently.
        rim(Material { alpha_cutoff: 0.5, shading: floptle_core::Shading::Physical, roughness: 0.4, normal_strength: 1.0, ..Default::default() }),
    ]
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Plain,
    Primed,
    Blended,
}

/// One picture, the renderer shared between pictures.
fn draw(gpu: &Gpu, raster: &mut Raster, light: &Raymarch, color: &wgpu::Texture, merged: bool, mode: Mode) -> Vec<[u8; 4]> {
    draw_with(gpu, raster, light, color, merged, mode, true)
}

fn draw_with(gpu: &Gpu, raster: &mut Raster, light: &Raymarch, color: &wgpu::Texture, merged: bool, mode: Mode, maps: bool) -> Vec<[u8; 4]> {
    let parts = [square(-0.66), square(0.0), square(0.66)];
    let s = TexSampling { filter: TexFilter::SmoothMipmaps, ..Default::default() };
    let bases = [picture([230, 60, 40], false), picture([40, 200, 90], false), picture([60, 80, 240], true)];
    let normals = if maps { [Some(ridges(0)), Some(ridges(1)), None] } else { [None, None, None] };
    let mut sets: Vec<TexId> = Vec::new();
    for (b, n) in bases.iter().zip(&normals) {
        let tb = raster.register_texture_shared(gpu, b, s);
        let tn = n.as_ref().map(|n| raster.register_texture_shared(gpu, n, s));
        if merged {
            raster.pool_texture(gpu, tb, b, s).expect("the base pools");
            if let (Some(tn), Some(n)) = (tn, n) {
                raster.pool_texture(gpu, tn, n, s).expect("the normal map pools");
            }
        }
        sets.push(raster.material_set(gpu, Some(tb), [tn, None, None, None, None]));
    }
    let params: Vec<MaterialParams> = materials()
        .iter()
        .map(|m| {
            let mut p = MaterialParams::from_material(m);
            p.ext_index = raster.push_surface_extras(SurfaceExtras::from_material(m));
            p
        })
        .collect();
    let model = Mat4::from_translation(glam::Vec3::new(0.0, 0.0, 0.5));
    let see_through = |raw: &mut floptle_render::InstanceRaw| {
        if mode == Mode::Blended {
            raw.color[3] *= 0.5;
        }
    };
    let mut list = Vec::new();
    if merged {
        let refs: Vec<&MeshData> = parts.iter().collect();
        let (mesh, starts) = merge_parts(&refs).expect("three parts merge");
        let id = raster.register(gpu, &mesh, None);
        let slots: Vec<Option<TexId>> = sets.iter().map(|&t| Some(t)).collect();
        let (set, layers) = raster.array_set_for(gpu, &slots).expect("one array set for the three slots");
        let table = raster.push_slot_table_layered(&starts, &params, Some(&layers)).expect("a table");
        let mut raw = instance_of_slots(model, table, 0, false);
        see_through(&mut raw);
        list.push((id, Some(set), raw));
    } else {
        for ((part, p), &set) in parts.iter().zip(&params).zip(&sets) {
            let id = raster.register(gpu, part, None);
            let mut raw = instance_of_mat(model, p);
            see_through(&mut raw);
            list.push((id, Some(set), raw));
        }
    }
    let globals = Globals {
        view_proj: Mat4::IDENTITY.to_cols_array_2d(),
        light_dir: [0.4, 0.5, 0.75, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.3, 0.3, 0.3, 0.0],
        ..Default::default()
    };
    if mode == Mode::Primed {
        raster.depth_prepass(gpu, globals, &list, gpu.depth_texture());
    }
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    raster.draw_scene(gpu, &view, gpu.depth_view(), globals, &list, Some([0.0, 0.0, 0.0, 1.0]), Some(light.field_bind()));
    floptle_render::probe::readback(gpu, color)
}

#[test]
fn three_textured_parts_and_their_array_merge_draw_the_same_pixels() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("texture-array-slots"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut raster = Raster::new(&gpu);
    // The key light, in front of the squares (they face +Z) and off to one
    // side, so a tilted normal is lit differently from a flat one.
    let light = Raymarch::new(&gpu);
    let k = glam::Vec3::new(0.5, 0.4, 0.75).normalize();
    light.upload_globals(
        &gpu,
        RaymarchGlobals {
            light_dir: [k.x, k.y, k.z, 0.0],
            light_color: [0.8, 0.8, 0.8, 0.0],
            ambient: [0.15, 0.15, 0.15, 0.0],
            ..Default::default()
        },
    );
    // The fixture: the normal maps change the picture, or a slot reading the
    // wrong map could not show.
    let flat = draw_with(&gpu, &mut raster, &light, &color, false, Mode::Plain, false);
    let mapped = draw(&gpu, &mut raster, &light, &color, false, Mode::Plain);
    let bumps = flat.iter().zip(&mapped).filter(|(a, b)| a != b).count();
    assert!(bumps > 200, "the normal maps changed only {bumps} pixels");
    for mode in [Mode::Plain, Mode::Primed, Mode::Blended] {
        let parts = draw(&gpu, &mut raster, &light, &color, false, mode);
        let merged = draw(&gpu, &mut raster, &light, &color, true, mode);
        // The fixture: the three squares drew, each its own, and the cut-out
        // one has holes in it.
        let row = (SIZE / 2 + 3) as usize * SIZE as usize;
        let at = |x: f32| parts[row + ((x * 0.5 + 0.5) * SIZE as f32) as usize];
        assert!(at(-0.66) != at(0.0) && at(0.0) != at(0.66), "{mode:?}: the fixture's three squares look alike");
        // Across the third square (centred at x = 0.66, a 0.6-wide span).
        let x0 = ((0.66 - 0.25) * 0.5 + 0.5) * SIZE as f32;
        let third: Vec<[u8; 4]> = (0..(0.25 * SIZE as f32) as usize).map(|i| parts[row + x0 as usize + i]).collect();
        assert!(third.iter().any(|p| p[0] < 4 && p[1] < 4 && p[2] < 4), "{mode:?}: the cut-out square has no holes");
        let differ = parts.iter().zip(&merged).filter(|(a, b)| a != b).count();
        assert_eq!(differ, 0, "{mode:?}: the array-merged model differs from its parts in {differ} pixels");
    }
}

/// **The array pipelines can be built off the frame, and a draw that has to
/// wait for them says so.** A renderer that was never told builds them inside
/// its first array draw, and `take_array_compile_ms` reports that wait; one
/// told to prewarm builds them on a worker, and once they are ready its first
/// array draw waits for nothing.
#[test]
fn the_array_pipelines_build_off_the_frame() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("texture-array-prewarm"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let light = Raymarch::new(&gpu);
    let mut cold = Raster::new(&gpu);
    assert!(!cold.array_pipelines_ready());
    draw(&gpu, &mut cold, &light, &color, true, Mode::Plain);
    assert!(cold.take_array_compile_ms() > 0.0, "the first array draw built the pipelines and said nothing");
    assert_eq!(cold.take_array_compile_ms(), 0.0, "the wait was reported twice");

    let mut warm = Raster::new(&gpu);
    warm.prewarm_arrays(&gpu);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !warm.array_pipelines_ready() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(warm.array_pipelines_ready(), "the worker never finished the array pipelines");
    draw(&gpu, &mut warm, &light, &color, true, Mode::Plain);
    assert_eq!(warm.take_array_compile_ms(), 0.0, "a prewarmed renderer's first array draw still waited");
}
