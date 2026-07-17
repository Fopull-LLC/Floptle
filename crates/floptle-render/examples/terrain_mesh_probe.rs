//! Terrain 2.0 P2 parity probe — the SAME sculpted field rendered BOTH ways, from the
//! same camera under the same light, so the render swap is evidence rather than a claim.
//!
//!   old: the raymarch sphere-traces the dense voxel field as primary visibility (w = 1)
//!   new: the field is meshed (sparse chunks + surface nets) and drawn by the RASTER
//!        pass, while the volume flips to w = 3 — still casting sun shadows, still in
//!        the AO field, no longer drawn
//!
//! Two views, because they are the two Ty photographed:
//!   `closeup` — camera on a hill flank at default detail, where trilinear-gradient
//!               normals kink on the voxel lattice ("looks very strange up close")
//!   `grazing` — low camera, low sun across open ground, where the shadow ray hugs the
//!               noisy f16 shell and stripes ("weird black line patterns")
//!
//! What it measures:
//!   * SILHOUETTE AGREEMENT — % of pixels where both paths agree on ground-vs-sky. The
//!     two paths must draw the same shape; this is the parity half.
//!   * SHADING GRAIN — mean |luminance - blur3x3(luminance)| over ground pixels. This is
//!     the lattice speckle itself, as a number. Lower is smoother; the mesh path should
//!     win outright, and that win IS the bug fix.
//!
//! Run: cargo run --release -p floptle-render --example terrain_mesh_probe -- <prefix>

use floptle_field::{Brush, BrushProfile, ChunkField, Terrain};
use floptle_render::{
    chunk_mesh_data, instance_of_mat, Globals, Gpu, MaterialParams, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TextureData,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 900;
const H: u32 = 560;

/// The one place the two paths are told what the terrain looks like — a flat mid-grey,
/// so every difference in the output is SHADING, not albedo.
const TINT: [f32; 3] = [0.55, 0.55, 0.55];

fn white() -> TextureData {
    TextureData { pixels: vec![255, 255, 255, 255], width: 1, height: 1 }
}

/// The probe terrain: rolling hills at the engine's default detail (0.5-unit voxels),
/// the grainy case. Deterministic — no rng — so the numbers below are comparable run to
/// run and a regression shows up as a moved number.
///
/// One dab does NOT make a hill: `Brush::Raise` is a CSG union with a ball of the
/// brush's radius, so it is idempotent — repeating a dab at the same spot is what
/// accumulates height (the same loop `terrain_closeup_probe` uses). The first version of
/// this probe dabbed once and rendered a near-flat plain that the camera flew over.
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

struct View {
    name: &'static str,
    cam: DVec3,
    target: Vec3,
    light: Vec3,
}

fn main() {
    let prefix = std::env::args().nth(1).unwrap_or_else(|| "tmesh".into());
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

    // ---- Migrate the dense field and mesh it (the P1 path, exercised end to end) ----
    let t0 = std::time::Instant::now();
    let field = ChunkField::from_dense(&terrain.baked, 0.5);
    let import_ms = t0.elapsed().as_secs_f32() * 1000.0;
    let t0 = std::time::Instant::now();
    let chunks = floptle_field::mesh_field(&field, 1);
    let mesh_ms = t0.elapsed().as_secs_f32() * 1000.0;
    let tris: usize = chunks.iter().map(|(_, m)| m.tri_count()).sum();
    println!(
        "field: dense {:.1} MB -> sparse {:.1} MB ({} chunks, import {:.1} ms)\n\
         mesh : {} chunks, {} tris, {:.1} ms",
        (terrain.baked.distance.len() * 4 + terrain.baked.color.len()) as f32 / 1e6,
        field.memory_bytes() as f32 / 1e6,
        field.data_chunks(),
        import_ms,
        chunks.len(),
        tris,
        mesh_ms,
    );

    let mut raster = Raster::new(&gpu);
    // One dynamic slot per chunk, filled through the same register/replace pair the
    // editor uses — so this probe also proves the slot + color-store plumbing.
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
        assert_ne!(raster.dyn_paint_base(id), 0, "chunk got a terrain color block");
        slots.push(id);
    }
    println!("gpu  : {} dynamic slots, terrain color store {:.2} MB", slots.len(), raster.tpaint_bytes() as f32 / 1e6);

    let views = [
        View {
            name: "closeup",
            cam: DVec3::new(6.5, 4.5, 6.5),
            target: Vec3::new(0.0, 2.2, 0.0),
            light: Vec3::new(0.5, 0.7, 0.4),
        },
        View {
            name: "grazing",
            cam: DVec3::new(0.0, 2.4, 13.0),
            target: Vec3::new(0.0, 0.4, -2.0),
            light: Vec3::new(-1.0, 0.14, 0.15),
        },
    ];

    let mut worst_agree = 100.0f32;
    for v in &views {
        let fwd = (v.target - v.cam.as_vec3()).normalize();
        let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
        let cam = RenderCamera::new(
            v.cam,
            rot,
            Projection::Perspective { fov_y: 58f32.to_radians(), near: 0.02, far: 2000.0 },
        );
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let light = v.light.normalize();
        let cr = (DVec3::ZERO - v.cam).as_vec3(); // the terrain sits at the world origin

        let rg = |kind: f32| RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            ambient: [0.10, 0.10, 0.12, 0.0],
            bg: [0.5, 0.62, 0.78, 1.0],
            params: [0.0, 0.0, 0.0, 1.0], // w = 1 volume
            vol_center: {
                let mut a = [[0.0f32; 4]; 16];
                a[0] = [cr.x, cr.y, cr.z, kind];
                a
            },
            vol_half: {
                let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16];
                a[0] = [16.0, 6.0, 16.0, 0.6];
                a
            },
            terrain_tint: [TINT[0], TINT[1], TINT[2], 0.0],
            terrain_params: [16.0, 0.0, 0.0, 1.0],
            // Shadows ON (k = 12), quantize + dither OFF: any banding is the FIELD.
            shadow_params: [1.0, 12.0, 1.0, 150.0],
            shadow_tint: [0.0, 0.0, 0.0, 0.0],
            ao_params: [1.0, 0.85, 1.5, 0.0], // SDF AO on — the "AO acts weird" half
            ..Default::default()
        };

        // --- OLD: raymarch draws the terrain (kind 1) ---
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg(1.0));
        let old = readback(&gpu, &color_tex);
        save_png(&old, &format!("{prefix}_{}_old.png", v.name));

        // --- NEW: kind 3 — the raymarch paints sky only, the raster draws the chunks ---
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg(3.0));
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            ambient: [0.10, 0.10, 0.12, 0.0],
            ..Default::default()
        };
        // Camera-relative model matrix (ADR-0015); every chunk shares it, which is what
        // keeps the triplanar projection continuous across chunk boundaries.
        let model = Mat4::from_translation(cr);
        let mat = MaterialParams { color: TINT, ambient: 1.0, ..MaterialParams::flat(TINT) };
        let instances: Vec<_> = slots
            .iter()
            .map(|&id| {
                let mut m = mat;
                m.terrain_paint_base = raster.dyn_paint_base(id);
                // Terrain colors carry the palette SLOT in alpha (0 = untextured);
                // without this flag the alpha-cutout path reads slot 0 as "alpha 0"
                // and discards the whole surface. Mirrors `push_terrain_instances`.
                m.terrain_splat = true;
                (id, None, instance_of_mat(model, &m))
            })
            .collect();
        raster.draw_scene(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &instances,
            None, // load: compose over the raymarch's sky + depth
            Some(raymarch.field_bind()),
        );
        let new = readback(&gpu, &color_tex);
        save_png(&new, &format!("{prefix}_{}_new.png", v.name));

        // ---- Measure ----
        let sky = |p: [u8; 4]| p[2] > p[1] + 12 && p[2] > p[0] + 20; // the blue bg
        let mut agree = 0u32;
        for i in 0..(W * H) as usize {
            if sky(old[i]) == sky(new[i]) {
                agree += 1;
            }
        }
        // Normal visualisation: unlit, with each chunk's colour block overwritten by its
        // own encoded vertex normal. Shows the interpolated normal field itself, with no
        // lighting in the way — if THIS is smooth, a rim in the lit render is the
        // shader's doing, not the mesher's.
        if std::env::var("NORMALS").is_ok() {
            for (i, (_, cm)) in chunks.iter().enumerate() {
                let mut d = chunk_mesh_data(cm);
                d.colors = Some(
                    cm.normals
                        .iter()
                        .map(|n| {
                            [
                                ((n[0] * 0.5 + 0.5) * 255.0) as u8,
                                ((n[1] * 0.5 + 0.5) * 255.0) as u8,
                                ((n[2] * 0.5 + 0.5) * 255.0) as u8,
                                255,
                            ]
                        })
                        .collect(),
                );
                raster.replace_dynamic(&gpu, slots[i], &d);
            }
            raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg(3.0));
            let inst: Vec<_> = slots
                .iter()
                .map(|&id| {
                    let mut m = MaterialParams { unlit: true, ..MaterialParams::flat([1.0; 3]) };
                    m.terrain_paint_base = raster.dyn_paint_base(id);
                    (id, None, instance_of_mat(model, &m))
                })
                .collect();
            raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &inst, None, None);
            let nrm = readback(&gpu, &color_tex);
            save_png(&nrm, &format!("{prefix}_{}_normals.png", v.name));

            // Walk a scanline across the rim and print the normal beside the lit value,
            // so the dark band is read rather than reasoned about.
            raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg(3.0));
            let li: Vec<_> = slots
                .iter()
                .map(|&id| {
                    let mut m = mat;
                    m.terrain_paint_base = raster.dyn_paint_base(id);
                    m.terrain_splat = true;
                    (id, None, instance_of_mat(model, &m))
                })
                .collect();
            raster.draw_scene(
                &gpu,
                &color_view,
                gpu.depth_view(),
                globals,
                &li,
                None,
                Some(raymarch.field_bind()),
            );
            let lit = readback(&gpu, &color_tex);
            // Find the darkest non-sky pixel: that is the rim.
            let mut worst = (999i32, 0usize);
            for (i, px) in lit.iter().enumerate() {
                if !sky(*px) && (px[1] as i32) < worst.0 {
                    worst = (px[1] as i32, i);
                }
            }
            let (cy, cx) = (worst.1 as u32 / W, worst.1 as u32 % W);
            println!("    rim scanline y={cy} around x={cx}:");
            for x in cx.saturating_sub(4)..(cx + 5).min(W) {
                let i = (cy * W + x) as usize;
                let n = [
                    nrm[i][0] as f32 / 127.5 - 1.0,
                    nrm[i][1] as f32 / 127.5 - 1.0,
                    nrm[i][2] as f32 / 127.5 - 1.0,
                ];
                let ndl = n[0] * light.x + n[1] * light.y + n[2] * light.z;
                println!(
                    "      x={x:>3} lit {:>3}  n=({:>6.2},{:>6.2},{:>6.2}) |n|={:.2} n·l={ndl:>6.2}{}",
                    lit[i][1],
                    n[0],
                    n[1],
                    n[2],
                    (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt(),
                    if sky(nrm[i]) { "  <- SKY in normal pass" } else { "" },
                );
            }
            for (i, (_, cm)) in chunks.iter().enumerate() {
                raster.replace_dynamic(&gpu, slots[i], &chunk_mesh_data(cm));
            }
        }

        // ---- Isolate: which term makes the rim? Re-render the mesh path with the
        // field effects switched off one group at a time. Guessing at shader symptoms is
        // what burned days on the last terrain bug; each variant is one hypothesis.
        for (label, mut vrg, unlit) in [
            ("no shadow/AO", rg(3.0), false),
            ("unlit       ", rg(3.0), true),
        ] {
            vrg.shadow_params[0] = 0.0;
            vrg.ao_params[0] = 0.0;
            raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), vrg);
            let mut m2 = mat;
            m2.unlit = unlit;
            let inst: Vec<_> = slots
                .iter()
                .map(|&id| {
                    let mut m = m2;
                    m.terrain_paint_base = raster.dyn_paint_base(id);
                    m.terrain_splat = true;
                    (id, None, instance_of_mat(model, &m))
                })
                .collect();
            raster.draw_scene(
                &gpu,
                &color_view,
                gpu.depth_view(),
                globals,
                &inst,
                None,
                Some(raymarch.field_bind()),
            );
            let px = readback(&gpu, &color_tex);
            println!("    {label}: grain {:>5.2}", grain(&px, &sky));
            save_png(
                &px,
                &format!("{prefix}_{}_diag_{}.png", v.name, label.trim().replace(['/', ' '], "_")),
            );
        }

        // The old path with AO off: if this lands on the NEW path's value, then the whole
        // old-vs-new gap at this pixel is SDF AO, and the mesh path is receiving none.
        let mut oao = rg(1.0);
        oao.ao_params[0] = 0.0;
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), oao);
        let old_no_ao = readback(&gpu, &color_tex);

        // The same subtraction on the mesh path: how much does SDF AO move it at all?
        let mut nao = rg(3.0);
        nao.ao_params[0] = 0.0;
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), nao);
        raster.draw_scene(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &instances,
            None,
            Some(raymarch.field_bind()),
        );
        let new_no_ao = readback(&gpu, &color_tex);

        let mid = ((H * 3 / 4) * W + W / 2) as usize;
        println!(
            "    probe px:  old {:>3} -> {:>3} without SDF AO   |   new {:>3} -> {:>3} without SDF AO",
            old[mid][1], old_no_ao[mid][1], new[mid][1], new_no_ao[mid][1]
        );
        // How far is the meshed surface from the field the GPU marches for AO/shadows?
        // The mesh is extracted from a ChunkField RESAMPLED off the dense grid, while the
        // atlas holds the dense grid itself — they need not agree.
        // Interior only: the mesh also carries the slab's side/bottom WALLS at the import
        // box rim, where the dense field is solid (d < 0) — counting those would report a
        // displacement that is really just the box edge.
        let (mut s_dense, mut s_chunk, mut n_v) = (0.0f64, 0.0f64, 0u32);
        for (_, cm) in &chunks {
            let o = Vec3::from(cm.origin);
            for p in cm.positions.iter().step_by(7) {
                let w = Vec3::from(*p) + o;
                if w.x.abs() > 13.0 || w.z.abs() > 13.0 || w.y < -4.0 {
                    continue;
                }
                s_dense += terrain.baked_distance_at(w.to_array()) as f64;
                s_chunk += field.d(w) as f64;
                n_v += 1;
            }
        }
        println!(
            "    mesh vertex mean signed d:  vs its OWN ChunkField {:+.3}  |  vs the dense field {:+.3}  ({n_v} verts, voxel 0.5)",
            s_chunk / n_v as f64,
            s_dense / n_v as f64,
        );
        let pct = agree as f32 / (W * H) as f32 * 100.0;
        worst_agree = worst_agree.min(pct);
        let g_old = grain(&old, &sky);
        let g_new = grain(&new, &sky);
        println!(
            "{:>8}: silhouette agreement {pct:>5.1}%   shading grain  old {g_old:>5.2}  new {g_new:>5.2}  ({:.1}x smoother)",
            v.name,
            g_old / g_new.max(1e-3),
        );
    }
    println!("wrote {prefix}_*_{{old,new}}.png   (worst agreement {worst_agree:.1}%)");
}

/// Mean |L - blur3x3(L)| over non-sky pixels: the high-frequency shading residual. The
/// voxel-lattice normal kink and the shadow-shell stripes both live in exactly this band,
/// which is why one number tracks both complaints.
fn grain(px: &[[u8; 4]], sky: &dyn Fn([u8; 4]) -> bool) -> f32 {
    let lum = |p: [u8; 4]| p[0] as f32 * 0.299 + p[1] as f32 * 0.587 + p[2] as f32 * 0.114;
    let (mut sum, mut n) = (0.0f64, 0u32);
    for y in 1..(H - 1) {
        for x in 1..(W - 1) {
            let i = (y * W + x) as usize;
            if sky(px[i]) {
                continue;
            }
            // Skip pixels whose 3×3 touches sky — a silhouette edge is a real gradient,
            // not grain, and would swamp the signal.
            let mut blur = 0.0;
            let mut ok = true;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let j = ((y as i32 + dy) as u32 * W + (x as i32 + dx) as u32) as usize;
                    if sky(px[j]) {
                        ok = false;
                    }
                    blur += lum(px[j]);
                }
            }
            if !ok {
                continue;
            }
            sum += (lum(px[i]) - blur / 9.0).abs() as f64;
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { (sum / n as f64) as f32 }
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (W * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
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
