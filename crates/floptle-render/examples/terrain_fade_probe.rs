//! What a chunk arriving looks like (`floptle/0067`) — the dissolve-in, rendered.
//!
//! The complaint was *"it's obvious when it pops in"*. Sizing the LOD rings to
//! the body cut how MANY chunks arrive; it cannot change the fact that each one
//! still switches from absent to fully lit between two frames. This is the other
//! half: the streamer ramps a chunk's instance alpha over its first third of a
//! second and the raster pass discards a matching fraction of its pixels, so the
//! chunk resolves instead of appearing.
//!
//! A dissolve rather than a blend, because terrain is opaque and stays in the
//! opaque pass — see the note in `raster.wgsl`'s `fs`.
//!
//! The number here is GROUND COVERAGE: what fraction of the pixels the fully
//! opaque render covers are still drawn at each alpha. If the dither is doing
//! its job, coverage tracks alpha; if it were doing nothing, every panel would
//! read 100%; if it were discarding everything, 0%.
//!
//! It renders through the depth prepass, in the editor's order, because that is
//! the half a dissolve can get wrong invisibly: prime depth for a pixel the
//! color pass then discards and the hole occludes what is behind it. `fs_depth`
//! and `fs` run the identical test on identical inputs — the position is
//! `@invariant` and the alpha is per-instance — so they agree by construction,
//! and this probe is what notices if that ever stops being true.
//!
//! Run: cargo run --release -p floptle-render --example terrain_fade_probe

use floptle_field::{Brush, BrushProfile, ChunkField, Terrain};
use floptle_render::{
    Globals, Gpu, MaterialParams, Projection, Raster, Raymarch, RaymarchGlobals, RenderCamera,
    TextureData, chunk_mesh_data, instance_of_mat,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 640;
const H: u32 = 400;
const TINT: [f32; 3] = [0.55, 0.55, 0.55];

/// The steps rendered. 1.0 is the reference every other panel is measured
/// against, so it goes first.
const ALPHAS: [f32; 6] = [1.0, 0.0, 0.15, 0.35, 0.6, 0.85];

fn white() -> TextureData {
    TextureData { pixels: vec![255, 255, 255, 255], width: 1, height: 1 }
}

/// Rolling hills — the same deterministic field `terrain_mesh_probe` uses, so
/// the two probes are looking at the same ground.
fn probe_terrain() -> Terrain {
    let mut t = Terrain::flat([96, 40, 96], [0.0; 3], [16.0, 6.0, 16.0], 0.0, [0.5, 0.5, 0.5]);
    for _ in 0..50 {
        t.sculpt(Brush::Raise, [0.0, 1.0, 0.0], 6.0, 1.0, BrushProfile::default());
        for i in 0..5 {
            let a = i as f32 * 2.399; // golden-angle scatter
            let r = 2.5 + (i % 3) as f32 * 1.2;
            t.sculpt(
                Brush::Raise,
                [a.cos() * 7.5, 0.4, a.sin() * 7.5],
                r,
                1.0,
                BrushProfile::default(),
            );
        }
    }
    t
}

fn main() {
    let prefix = std::env::args().nth(1).unwrap_or_else(|| "tfade".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let terrain = probe_terrain();
    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white()]);
    assert_eq!(raymarch.set_volumes(&gpu, &[&terrain.baked]), 1);

    let field = ChunkField::from_dense(&terrain.baked, 0.5);
    let chunks = floptle_field::mesh_field(&field, 1);
    let mut raster = Raster::new(&gpu);
    let mut slots = Vec::new();
    for (_, cm) in &chunks {
        let data = chunk_mesh_data(cm);
        let id = raster.register_dynamic(
            &gpu,
            data.vertices.len() as u32,
            data.indices.len() as u32,
            true,
        );
        assert!(raster.replace_dynamic(&gpu, id, &data), "chunk upload");
        slots.push(id);
    }

    let cam_pos = DVec3::new(6.5, 4.5, 6.5);
    let fwd = (Vec3::new(0.0, 2.2, 0.0) - cam_pos.as_vec3()).normalize();
    let cam = RenderCamera::new(
        cam_pos,
        Quat::from_rotation_arc(Vec3::NEG_Z, fwd),
        Projection::Perspective { fov_y: 58f32.to_radians(), near: 0.02, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.5, 0.7, 0.4).normalize();
    let cr = (DVec3::ZERO - cam_pos).as_vec3();

    let rg = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.10, 0.10, 0.12, 0.0],
        bg: [0.5, 0.62, 0.78, 1.0],
        params: [0.0, 0.0, 0.0, 1.0],
        vol_center: {
            let mut a = [[0.0f32; 4]; 16];
            a[0] = [cr.x, cr.y, cr.z, 3.0]; // w = 3: shadow + AO, the raster draws it
            a
        },
        vol_half: {
            let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16];
            a[0] = [16.0, 6.0, 16.0, 0.6];
            a
        },
        terrain_tint: [TINT[0], TINT[1], TINT[2], 0.0],
        terrain_params: [16.0, 0.0, 0.0, 1.0],
        shadow_params: [1.0, 12.0, 1.0, 150.0],
        shadow_tint: [0.0, 0.0, 0.0, 0.0],
        ao_params: [1.0, 0.85, 1.5, 0.0],
        ..Default::default()
    };
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.10, 0.10, 0.12, 0.0],
        ..Default::default()
    };
    let model = Mat4::from_translation(cr);
    let sky = |p: [u8; 4]| p[2] > p[1] + 12 && p[2] > p[0] + 20;

    println!("\na chunk arriving, at each step of its dissolve\n");
    println!("  alpha   ground pixels   of full");
    let mut full = 0u32;
    let mut previous = 0f32;
    for (k, &alpha) in ALPHAS.iter().enumerate() {
        let instances: Vec<_> = slots
            .iter()
            .map(|&id| {
                let mut m = MaterialParams { color: TINT, ambient: 1.0, ..MaterialParams::flat(TINT) };
                m.terrain_paint_base = raster.dyn_paint_base(id);
                m.terrain_splat = true;
                m.alpha = alpha;
                (id, None, instance_of_mat(model, &m))
            })
            .collect();
        // The editor's order: prime depth, let the raymarch paint the sky capped
        // by it, then shade. A prepass that did not dither would cap the raymarch
        // over the WHOLE hill and the dissolved-out pixels would come back empty.
        raster.depth_prepass(&gpu, globals, &instances, gpu.depth_texture());
        // Bind it every time. Guarding this on "was the target reallocated?" is
        // what let the editor draw one view with another view's depth buffer.
        raymarch.set_depth_prime(&gpu, raster.prepass_view());
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg);
        raster.draw_scene(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &instances,
            None,
            Some(raymarch.field_bind()),
        );
        let px = readback(&gpu, &color_tex);
        save_png(&px, &format!("{prefix}_{:03}.png", (alpha * 100.0) as u32));

        let ground = px.iter().filter(|p| !sky(**p)).count() as u32;
        if k == 0 {
            full = ground.max(1);
            println!("  {alpha:>5.2}   {ground:>13}   {:>6.1}%  (reference)", 100.0);
            previous = 0.0;
            continue;
        }
        let frac = ground as f32 / full as f32;
        println!("  {alpha:>5.2}   {ground:>13}   {:>6.1}%", frac * 100.0);
        // The dissolve must be MONOTONIC — a threshold that is not ordered in
        // alpha makes pixels flicker back off as a chunk fades in, which is a
        // worse artifact than the pop.
        assert!(
            frac >= previous - 0.02,
            "coverage went BACKWARDS at alpha {alpha}: {:.1}% after {:.1}%",
            frac * 100.0,
            previous * 100.0
        );
        previous = frac;
    }

    println!(
        "\nCoverage should climb with alpha and reach the reference at 1.0. The\n\
         PNGs are the real check: `{prefix}_035.png` should read as the hill\n\
         DISSOLVING — speckled, but the silhouette and the shading already\n\
         correct — rather than as a hill with holes punched in a regular grid.\n"
    );
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded =
        (W * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.config.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * bpp) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
