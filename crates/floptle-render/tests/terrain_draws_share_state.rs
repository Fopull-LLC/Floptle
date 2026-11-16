//! **A run of terrain chunks binds its picture once, and the frame is the
//! same picture it was.**
//!
//! Every terrain chunk is a dynamic mesh slot of its own, and each draws
//! with the default white picture. Each slot used to own a bind group for
//! that one picture, so every chunk draw in every pass switched bind groups
//! for real: about a thousand state commands a pass on a planet, each one
//! validated and tracked on the CPU when the pass ends (card 0753).
//!
//! Here 512 slots draw through the sun map (both cascades), the depth
//! prepass, the colour pass, the glass pass and the selection mask. Each
//! pass must set group 1 a handful of times, not once a chunk, while still
//! drawing every chunk; and the pictures must be byte for byte the ones the
//! raster draws with every set issued (`Raster::set_state_elision(false)`).
//! Set `FLOPTLE_KEEP_SHOTS=<dir>` to keep the colour frames as PNGs.
//!
//! The same scene also holds the frame's bucketing to once a view: the sun
//! map, the prepass and the colour pass draw from one [`ViewPlan`], sorted
//! and uploaded once, and draw the same pictures as each pass sorting and
//! uploading for itself, even with another view drawn in between.

use floptle_core::{Material, Shading};
use floptle_render::{
    cube, instance_of, instance_of_mat, set_ext_index, EncodeStats, Globals, Gpu, InstanceRaw, MaterialParams, MeshData, MeshId, Raster, SurfaceExtras,
    TexId, TexSampling, TextureData, ViewPlan,
};
use glam::{Mat4, Vec3};

const SIZE: u32 = 128;
const CHUNKS: usize = 512;

/// A texture's bytes, whatever its format, rows unpadded.
fn read(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let bpp = tex.format().block_copy_size(Some(wgpu::TextureAspect::All)).expect("a copyable format");
    let (w, h) = (tex.width(), tex.height());
    let unpadded = w * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("share-state-readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    (0..h).flat_map(|row| data[(row * padded) as usize..(row * padded + unpadded) as usize].to_vec()).collect()
}

fn target(gpu: &Gpu, label: &str, format: wgpu::TextureFormat) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// The scene: 512 chunk slots in a grid across the frame, straight through
/// an identity projection, with a textured cube, a faded chunk and a pane of
/// glass among them so the runs are broken the way a real frame breaks them.
struct Scene {
    instances: Vec<(MeshId, Option<TexId>, InstanceRaw)>,
    mask: Vec<(MeshId, InstanceRaw)>,
}

fn scene(gpu: &Gpu, raster: &mut Raster) -> Scene {
    let side = (CHUNKS as f32).sqrt().ceil() as usize;
    let step = 2.0 / side as f32;
    let mut instances = Vec::new();
    let mut chunks = Vec::new();
    for i in 0..CHUNKS {
        // A chunk of its own shape: a small box, nudged a little in depth,
        // in a slot with its own buffers, as terrain meshes them.
        let half = step * 0.4;
        let mut data: MeshData = cube(half);
        for v in &mut data.vertices {
            v.pos[2] += (i % 7) as f32 * 0.01;
        }
        let id = raster.register_dynamic(gpu, data.vertices.len() as u32, data.indices.len() as u32, false);
        assert!(raster.replace_dynamic(gpu, id, &data), "slot {i} took its mesh");
        let (x, y) = ((i % side) as f32, (i / side) as f32);
        let at = Vec3::new(-1.0 + step * (x + 0.5), -1.0 + step * (y + 0.5), 0.5);
        let c = [(i % 5) as f32 / 4.0, (i % 3) as f32 / 2.0, (i % 11) as f32 / 10.0];
        let mut raw = instance_of(Mat4::from_translation(at), c);
        if i == 200 {
            // Faded: the blended bucket.
            raw.color[3] = 0.5;
        }
        chunks.push(id);
        instances.push((id, None, raw));
    }
    // A cube wearing a picture of its own, mid-list, so the run of chunks
    // either side of it binds its own group again.
    let pixels = (0..16).flat_map(|i| if i % 2 == 0 { [255, 64, 0, 255] } else { [0, 64, 255, 255] }).collect();
    let tex = raster.register_texture(gpu, &TextureData { pixels, width: 4, height: 4 }, TexSampling::default());
    let boxy = raster.register(gpu, &cube(0.2), None);
    instances.insert(CHUNKS / 2, (boxy, Some(tex), instance_of(Mat4::from_translation(Vec3::new(0.1, 0.1, 0.3)), [1.0; 3])));
    // Glass: drawn by its own pass.
    let glass = raster.push_surface_extras(SurfaceExtras::from_material(&Material {
        shading: Shading::Physical,
        transmission: 0.8,
        ..Default::default()
    }));
    let pane = raster.register(gpu, &cube(0.3), None);
    let mut raw = instance_of(Mat4::from_translation(Vec3::new(-0.3, -0.2, 0.2)), [0.6, 0.9, 0.7]);
    set_ext_index(&mut raw, glass);
    instances.push((pane, None, raw));
    // The selection: every chunk, as an outline would mask a whole terrain.
    let mask = instances.iter().filter(|(id, _, _)| chunks.contains(id)).map(|(id, _, raw)| (*id, *raw)).collect();
    Scene { instances, mask }
}

/// What each pass recorded, and the pictures: colour, mask, sun map.
struct Frame {
    sun: EncodeStats,
    prepass: EncodeStats,
    colour: EncodeStats,
    glass: EncodeStats,
    mask: EncodeStats,
    pictures: [Vec<u8>; 3],
}

fn frame(gpu: &Gpu, raster: &mut Raster, s: &Scene, colour: &wgpu::Texture, mask: &wgpu::Texture) -> Frame {
    let view = colour.create_view(&wgpu::TextureViewDescriptor::default());
    let mask_view = mask.create_view(&wgpu::TextureViewDescriptor::default());
    let mut globals = Globals {
        view_proj: Mat4::IDENTITY.to_cols_array_2d(),
        ambient: [0.8, 0.8, 0.8, 0.0],
        ..Default::default()
    };
    // Both cascades, through the same box: every chunk lies inside it.
    globals.sun_vp = Mat4::IDENTITY.to_cols_array_2d();
    globals.sun_map = [1.0, 1.0, 0.0, 1.0 / 512.0];
    globals.sun_near_vp = Mat4::IDENTITY.to_cols_array_2d();
    globals.sun_near = [1.0, 1.0, 0.0, 1.0 / 512.0];
    raster.sun_shadow_pass(gpu, globals, &s.instances, &[], &[]);
    let sun = raster.last_pass_encode();
    raster.depth_prepass_with(gpu, globals, &s.instances, &[], &[], gpu.depth_texture());
    let prepass = raster.last_pass_encode();
    raster.draw_scene_with(gpu, &view, gpu.depth_view(), globals, &s.instances, &[], &[], Some([0.05, 0.05, 0.1, 1.0]), None);
    let colour_n = raster.last_pass_encode();
    raster.draw_transmissive(gpu, &view, gpu.depth_view(), globals, &s.instances, &[], None, &[], 0);
    let glass = raster.last_pass_encode();
    raster.draw_mask(gpu, &mask_view, globals, &s.mask, &[]);
    let mask_n = raster.last_pass_encode();
    Frame {
        sun,
        prepass,
        colour: colour_n,
        glass,
        mask: mask_n,
        pictures: [read(gpu, colour), read(gpu, mask), read(gpu, raster.sun_map_texture())],
    }
}

#[test]
fn chunk_slots_bind_their_picture_once_a_run_and_draw_the_same_frame() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        // This device cannot build the raster pass (CI's OpenGL), so
        // nothing can be drawn.
        return;
    }
    let colour = target(&gpu, "share-state-colour", gpu.scene_format());
    let mask = target(&gpu, "share-state-mask", floptle_render::outline::MASK_FORMAT);
    let mut raster = Raster::new(&gpu);
    raster.set_sun_map_layout(&gpu, 512, true);
    let s = scene(&gpu, &mut raster);

    let on = frame(&gpu, &mut raster, &s, &colour, &mask);
    if let Ok(dir) = std::env::var("FLOPTLE_KEEP_SHOTS") {
        std::fs::create_dir_all(&dir).ok();
        floptle_render::probe::save_png(&gpu, &colour, &format!("{dir}/share-state-on.png"));
    }
    raster.set_state_elision(false);
    let off = frame(&gpu, &mut raster, &s, &colour, &mask);
    if let Ok(dir) = std::env::var("FLOPTLE_KEEP_SHOTS") {
        floptle_render::probe::save_png(&gpu, &colour, &format!("{dir}/share-state-off.png"));
    }

    // Every chunk drew, in every pass, both ways.
    for (pass, n, at_least) in [
        ("sun map", on.sun, 2 * CHUNKS),
        ("prepass", on.prepass, CHUNKS),
        ("colour", on.colour, CHUNKS),
        ("mask", on.mask, CHUNKS),
    ] {
        assert!(n.draws as usize >= at_least, "the {pass} pass drew {} buckets, fewer than its {at_least} chunks", n.draws);
    }
    for (pass, a, b) in [
        ("sun map", on.sun, off.sun),
        ("prepass", on.prepass, off.prepass),
        ("colour", on.colour, off.colour),
        ("glass", on.glass, off.glass),
        ("mask", on.mask, off.mask),
    ] {
        assert_eq!(a.draws, b.draws, "the {pass} pass drew a different number of buckets with every set issued");
    }
    // With every set issued, each draw binds group 1 (the counter counts).
    assert!(
        off.colour.bind_sets as usize > CHUNKS,
        "with nothing skipped the colour pass set {} bind groups for {} draws",
        off.colour.bind_sets,
        off.colour.draws
    );
    // Skipping, the chunks share one bind group, so a pass sets a handful:
    // its own groups 0 and 2, the shared chunk picture, the textured cube's
    // and the shared one again after it.
    for (pass, n) in [("sun map", on.sun), ("prepass", on.prepass), ("colour", on.colour), ("glass", on.glass)] {
        assert!(n.bind_sets <= 8, "the {pass} pass set {} bind groups for {} draws; a run of chunks should bind its picture once", n.bind_sets, n.draws);
        assert!(n.pipeline_sets <= 8, "the {pass} pass set {} pipelines for {} draws", n.pipeline_sets, n.draws);
    }
    assert!(on.mask.bind_sets <= 2, "the mask pass set {} bind groups", on.mask.bind_sets);

    // The same pictures, byte for byte, and pictures with something in them.
    let bpp = |t: &wgpu::Texture| t.format().block_copy_size(Some(wgpu::TextureAspect::All)).unwrap() as usize;
    for (what, a, b, px) in [
        ("colour", &on.pictures[0], &off.pictures[0], bpp(&colour)),
        ("selection mask", &on.pictures[1], &off.pictures[1], bpp(&mask)),
        ("sun map", &on.pictures[2], &off.pictures[2], 4),
    ] {
        let first = &a[..px];
        assert!(a.chunks(px).any(|c| c != first), "the {what} is one flat value: nothing drew");
        let differ = a.iter().zip(b).filter(|(x, y)| x != y).count();
        assert!(a == b, "the {what} differs in {differ} bytes between skipping repeated state and issuing all of it");
    }
}

fn sun_globals() -> Globals {
    let mut globals = Globals {
        view_proj: Mat4::IDENTITY.to_cols_array_2d(),
        ambient: [0.8, 0.8, 0.8, 0.0],
        ..Default::default()
    };
    globals.sun_vp = Mat4::IDENTITY.to_cols_array_2d();
    globals.sun_map = [1.0, 1.0, 0.0, 1.0 / 512.0];
    globals.sun_near_vp = Mat4::IDENTITY.to_cols_array_2d();
    globals.sun_near = [1.0, 1.0, 0.0, 1.0 / 512.0];
    globals
}

/// The sun map, the prepass and the colour pass, either from one plan or
/// each sorting and uploading for itself (the entry points that take the
/// instance list). With `between`, another view (a few of the instances,
/// into a target of its own) is drawn after the sun map, as a render target
/// or a docked panel would be: it takes over the instance buffer. Returns
/// what the passes encoded and the colour frame and sun map.
fn passes(gpu: &Gpu, raster: &mut Raster, s: &Scene, colour: &wgpu::Texture, planned: bool, between: Option<&wgpu::Texture>) -> (EncodeStats, [Vec<u8>; 2]) {
    let view = colour.create_view(&wgpu::TextureViewDescriptor::default());
    let globals = sun_globals();
    let other = |raster: &mut Raster| {
        if let Some(t) = between {
            let v = t.create_view(&wgpu::TextureViewDescriptor::default());
            // Instances the window's plan holds elsewhere, so what this
            // view leaves at the start of the buffer is not the window's.
            let mut p = raster.plan_view(&s.instances[40..80], &[], &[]);
            raster.draw_scene_planned(gpu, &v, gpu.depth_view(), globals, &mut p, Some([0.0; 4]), None);
        }
    };
    raster.take_encode_stats();
    let n = if planned {
        let mut plan: ViewPlan = raster.plan_view(&s.instances, &[], &[]);
        raster.sun_shadow_pass_planned(gpu, globals, &mut plan, &[]);
        let n = raster.take_encode_stats();
        other(raster);
        let _ = raster.take_encode_stats();
        raster.depth_prepass_planned(gpu, globals, &mut plan, gpu.depth_texture());
        raster.draw_scene_planned(gpu, &view, gpu.depth_view(), globals, &mut plan, Some([0.05, 0.05, 0.1, 1.0]), None);
        let mut n2 = raster.take_encode_stats();
        n2.bucket_records += n.bucket_records;
        n2.instance_bytes += n.instance_bytes;
        n2
    } else {
        raster.sun_shadow_pass(gpu, globals, &s.instances, &[], &[]);
        let n = raster.take_encode_stats();
        other(raster);
        let _ = raster.take_encode_stats();
        raster.depth_prepass_with(gpu, globals, &s.instances, &[], &[], gpu.depth_texture());
        raster.draw_scene_with(gpu, &view, gpu.depth_view(), globals, &s.instances, &[], &[], Some([0.05, 0.05, 0.1, 1.0]), None);
        let mut n2 = raster.take_encode_stats();
        n2.bucket_records += n.bucket_records;
        n2.instance_bytes += n.instance_bytes;
        n2
    };
    (n, [read(gpu, colour), read(gpu, raster.sun_map_texture())])
}

#[test]
fn the_passes_share_one_plan_a_view_and_draw_the_same_frame() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        // CI's OpenGL cannot build the raster pass.
        return;
    }
    let colour = target(&gpu, "plan-colour", gpu.scene_format());
    let elsewhere = target(&gpu, "plan-other-view", gpu.scene_format());
    let mut raster = Raster::new(&gpu);
    raster.set_sun_map_layout(&gpu, 512, true);
    let s = scene(&gpu, &mut raster);
    let record = std::mem::size_of::<InstanceRaw>() as u64;
    let n = s.instances.len() as u64;

    let (old, old_px) = passes(&gpu, &mut raster, &s, &colour, false, None);
    let (new, new_px) = passes(&gpu, &mut raster, &s, &colour, true, None);
    let (far, near) = (raster.sun_map_draws().0 as u64, raster.sun_near_map_draws().0 as u64);
    let (_, between_px) = passes(&gpu, &mut raster, &s, &colour, true, Some(&elsewhere));

    // Sorted once: every record of the frame once, not once a pass.
    assert!(new.bucket_records as u64 <= n, "one plan sorted {} records for {n} instances", new.bucket_records);
    // Uploaded once, plus only what each sun cascade kept.
    let most = (n + far + near) * record;
    assert!(new.instance_bytes <= most, "one plan uploaded {} bytes; the frame, once, and the sun's casters come to {most}", new.instance_bytes);
    // The counters can see the difference: each pass for itself sorted and
    // uploaded the frame again.
    assert!(old.bucket_records as u64 >= 3 * (n - 1), "each pass for itself sorted only {} records", old.bucket_records);
    assert!(old.instance_bytes > most, "each pass for itself uploaded only {} bytes", old.instance_bytes);

    for (what, i) in [("colour", 0), ("sun map", 1)] {
        let first = &new_px[i][..4];
        assert!(new_px[i].chunks(4).any(|c| c != first), "the {what} is one flat value: nothing drew");
        let differ = old_px[i].iter().zip(&new_px[i]).filter(|(a, b)| a != b).count();
        assert!(old_px[i] == new_px[i], "the {what} differs in {differ} bytes between one plan and each pass for itself");
        let differ = between_px[i].iter().zip(&new_px[i]).filter(|(a, b)| a != b).count();
        assert!(
            between_px[i] == new_px[i],
            "the {what} differs in {differ} bytes when another view drew between the sun map and the prepass"
        );
    }
}

/// A chunk's mesh: a small box nudged in depth, a terrain colour for every
/// vertex (so a colour read one vertex off shows), `grow` boxes of it.
fn chunk_mesh(i: usize, half: f32, grow: usize, tint: u8) -> MeshData {
    let mut data: MeshData = cube(half);
    let one = data.clone();
    for k in 1..grow {
        let base = data.vertices.len() as u32;
        data.vertices.extend(one.vertices.iter().map(|v| {
            let mut v = *v;
            v.pos[0] += k as f32 * half * 0.1;
            v
        }));
        data.indices.extend(one.indices.iter().map(|&x| x + base));
    }
    for v in &mut data.vertices {
        v.pos[2] += (i % 7) as f32 * 0.01;
    }
    let n = data.vertices.len();
    data.colors = Some((0..n).map(|v| [(v * 37 % 256) as u8, (255 - v * 11 % 256) as u8, tint.wrapping_add((i * 13) as u8), 255]).collect());
    data
}

/// `n` coloured chunk slots of `grow` boxes each (each with its own block
/// in the terrain colour store, read by vertex) and a textured cube among
/// them, as `scene` lays them out.
fn coloured_scene(gpu: &Gpu, raster: &mut Raster, n: usize, grow: usize) -> (Scene, Vec<MeshId>, f32) {
    let side = (n as f32).sqrt().ceil() as usize;
    let step = 2.0 / side as f32;
    let mut instances = Vec::new();
    let mut chunks = Vec::new();
    for i in 0..n {
        let data = chunk_mesh(i, step * 0.4, grow, 0);
        let id = raster.register_dynamic(gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
        assert!(raster.replace_dynamic(gpu, id, &data), "slot {i} took its mesh");
        let (x, y) = ((i % side) as f32, (i / side) as f32);
        let at = Vec3::new(-1.0 + step * (x + 0.5), -1.0 + step * (y + 0.5), 0.5);
        let mut mp = MaterialParams::flat([1.0; 3]);
        mp.terrain_paint_base = raster.dyn_paint_base(id);
        chunks.push(id);
        instances.push((id, None, instance_of_mat(Mat4::from_translation(at), &mp)));
    }
    let pixels = (0..16).flat_map(|i| if i % 2 == 0 { [255, 64, 0, 255] } else { [0, 64, 255, 255] }).collect();
    let tex = raster.register_texture(gpu, &TextureData { pixels, width: 4, height: 4 }, TexSampling::default());
    let boxy = raster.register(gpu, &cube(0.2), None);
    instances.insert(n / 2, (boxy, Some(tex), instance_of(Mat4::from_translation(Vec3::new(0.1, 0.1, 0.3)), [1.0; 3])));
    (Scene { instances, mask: Vec::new() }, chunks, step * 0.4)
}

/// Each pass's encode counts and the colour frame and sun map, from one plan.
fn planned_frame(gpu: &Gpu, raster: &mut Raster, s: &Scene, colour: &wgpu::Texture) -> ([EncodeStats; 3], [Vec<u8>; 2]) {
    let view = colour.create_view(&wgpu::TextureViewDescriptor::default());
    let globals = sun_globals();
    let mut plan = raster.plan_view(&s.instances, &[], &[]);
    raster.sun_shadow_pass_planned(gpu, globals, &mut plan, &[]);
    let sun = raster.last_pass_encode();
    raster.depth_prepass_planned(gpu, globals, &mut plan, gpu.depth_texture());
    let prepass = raster.last_pass_encode();
    raster.draw_scene_planned(gpu, &view, gpu.depth_view(), globals, &mut plan, Some([0.05, 0.05, 0.1, 1.0]), None);
    let col = raster.last_pass_encode();
    ([sun, prepass, col], [read(gpu, colour), read(gpu, raster.sun_map_texture())])
}

fn same(what: &str, a: &[u8], b: &[u8]) {
    let first = &a[..4];
    assert!(a.chunks(4).any(|c| c != first), "the {what} is one flat value: nothing drew");
    let differ = a.iter().zip(b).filter(|(x, y)| x != y).count();
    assert!(a == b, "the {what} differs in {differ} bytes between the shared arena and buffers of their own");
}

#[test]
fn chunk_slots_share_their_buffers_and_draw_the_same_frame() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        // CI's OpenGL cannot build the raster pass.
        return;
    }
    let colour = target(&gpu, "arena-colour", gpu.scene_format());
    let raster_with = |arena: bool| {
        let mut r = Raster::new(&gpu);
        r.set_sun_map_layout(&gpu, 512, true);
        r.set_geometry_arena(arena);
        let (s, chunks, half) = coloured_scene(&gpu, &mut r, CHUNKS, 1);
        (r, s, chunks, half)
    };
    let (mut on, mut s_on, chunks_on, half) = raster_with(true);
    let (mut off, mut s_off, chunks_off, _) = raster_with(false);
    let pages = on.geometry_arena_stats().pages;

    let (n_on, px_on) = planned_frame(&gpu, &mut on, &s_on, &colour);
    let (n_off, px_off) = planned_frame(&gpu, &mut off, &s_off, &colour);
    if let Ok(dir) = std::env::var("FLOPTLE_KEEP_SHOTS") {
        std::fs::create_dir_all(&dir).ok();
        floptle_render::probe::save_png(&gpu, &colour, &format!("{dir}/arena-off.png"));
    }
    for (pass, a, b) in [("sun map", n_on[0], n_off[0]), ("prepass", n_on[1], n_off[1]), ("colour", n_on[2], n_off[2])] {
        assert_eq!(a.draws, b.draws, "the {pass} pass drew a different number of buckets from the arena");
        assert!(a.draws as usize >= CHUNKS, "the {pass} pass drew {} buckets for {CHUNKS} chunks", a.draws);
        // Buffers of their own: every chunk binds its own vertex and index
        // buffer (the counter can see them).
        assert!(b.vertex_sets as usize > CHUNKS && b.index_sets as usize > CHUNKS, "the {pass} pass with buffers of their own set {} vertex and {} index buffers", b.vertex_sets, b.index_sets);
        // The arena: the instance buffer, a page, the cube's own and the
        // page again after it, a few times at most.
        let most = 2 * pages as u32 + 8;
        assert!(a.vertex_sets <= most, "the {pass} pass set {} vertex buffers for {} draws from the arena", a.vertex_sets, a.draws);
        assert!(a.index_sets <= most, "the {pass} pass set {} index buffers for {} draws from the arena", a.index_sets, a.draws);
    }
    assert!(pages >= 1, "the chunks went into no arena page");
    same("colour", &px_on[0], &px_off[0]);
    same("sun map", &px_on[1], &px_off[1]);
    let undug = px_on[0].clone();

    // A dig remeshes one chunk at the same size: only its own range is
    // written, nothing else in the page, and the picture follows it.
    let k = 100;
    let dug = chunk_mesh(k, half * 0.7, 1, 90);
    let (bytes, before) = (on.geometry_arena_stats().bytes, on.geometry_arena_stats().written);
    assert!(on.replace_dynamic(&gpu, chunks_on[k], &dug));
    let after = on.geometry_arena_stats().written;
    let own = (dug.vertices.len() * std::mem::size_of::<floptle_render::Vertex>() + dug.indices.len() * 4) as u64;
    assert_eq!(after - before, own, "remeshing one chunk wrote {} bytes of an arena of {bytes}; its mesh is {own}", after - before);
    assert!(off.replace_dynamic(&gpu, chunks_off[k], &dug));
    // And one grows past its slot, so its range moves (and its colour block
    // with it).
    let big = chunk_mesh(k + 1, half, 60, 150);
    assert!(on.replace_dynamic(&gpu, chunks_on[k + 1], &big));
    assert!(off.replace_dynamic(&gpu, chunks_off[k + 1], &big));
    // A resized slot has a new colour block: its instance says so, as the
    // terrain's does when it is gathered again each frame.
    for (raster, scene, id) in [(&on, &mut s_on, chunks_on[k + 1]), (&off, &mut s_off, chunks_off[k + 1])] {
        let at = scene.instances.iter_mut().find(|(m, _, _)| *m == id).expect("the chunk's instance");
        at.2.normal_mat[0][3] = raster.dyn_paint_base(id) as f32;
    }
    let (_, px_on) = planned_frame(&gpu, &mut on, &s_on, &colour);
    if let Ok(dir) = std::env::var("FLOPTLE_KEEP_SHOTS") {
        floptle_render::probe::save_png(&gpu, &colour, &format!("{dir}/arena-dug.png"));
    }
    let (_, px_off) = planned_frame(&gpu, &mut off, &s_off, &colour);
    assert!(px_on[0] != undug, "the dig changed nothing in the picture");
    same("colour after a dig", &px_on[0], &px_off[0]);
    same("sun map after a dig", &px_on[1], &px_off[1]);

    // A planet unloading lets go of every chunk: the pages go with them.
    for id in chunks_on {
        on.free_dynamic(id);
    }
    let st = on.geometry_arena_stats();
    let (pages, bytes) = (st.pages, st.bytes);
    assert_eq!((pages, bytes), (0, 0), "with every chunk freed the arena still holds {pages} pages, {bytes} bytes");
}

/// **Pages the ground has mostly left give their memory back.** A page only
/// grows, so a chunk shrinking (ground drawn coarse once the player has
/// walked on) only gave its range back to the page; with a coarse chunk
/// still on every page, no page ever emptied and the GPU held the walk's
/// peak until the planet unloaded. Here 64 big chunk slots fill eight
/// pages, then all but one a page shrink: every page stays alive, under a
/// quarter full. `maintain_geometry` moves the survivors out, a bounded
/// amount a call, and lets the emptied pages go; the pictures are the
/// ones the slots draw with buffers of their own, and the memory a budget
/// reads (`mesh_gpu_held`) is what the pages hold.
#[test]
fn emptied_pages_give_their_memory_back() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        // CI's OpenGL cannot build the raster pass.
        return;
    }
    const N: usize = 64;
    let colour = target(&gpu, "compact-colour", gpu.scene_format());
    let raster_with = |arena: bool| {
        let mut r = Raster::new(&gpu);
        r.set_sun_map_layout(&gpu, 512, true);
        r.set_geometry_arena(arena);
        // 8,192 vertices a slot, 65,536 a page: eight slots a page.
        r.set_geometry_page_limit(1 << 16, 1 << 17);
        let (s, chunks, half) = coloured_scene(&gpu, &mut r, N, 150);
        (r, s, chunks, half)
    };
    let (mut on, mut s_on, chunks_on, half) = raster_with(true);
    let (mut off, mut s_off, chunks_off, _) = raster_with(false);
    let peak = on.geometry_arena_stats();
    assert!(peak.pages >= 8, "the fixture: 64 big slots should fill eight pages ({} pages)", peak.pages);

    // All but every eighth chunk shrink to one box: one big survivor a page.
    for (raster, scene, chunks) in [(&mut on, &mut s_on, &chunks_on), (&mut off, &mut s_off, &chunks_off)] {
        for (i, &id) in chunks.iter().enumerate().filter(|(i, _)| i % 8 != 0) {
            assert!(raster.replace_dynamic(&gpu, id, &chunk_mesh(i, half, 1, 40)));
            let at = scene.instances.iter_mut().find(|(m, _, _)| *m == id).expect("the chunk's instance");
            at.2.normal_mat[0][3] = raster.dyn_paint_base(id) as f32;
        }
    }
    let shrunk = on.geometry_arena_stats();
    assert_eq!(shrunk.pages, peak.pages, "the fixture: a survivor on every page keeps every page");
    assert!(shrunk.live * 4 < shrunk.bytes, "the fixture: the pages should be under a quarter full ({} of {} bytes)", shrunk.live, shrunk.bytes);

    // A frame at a time, each moving a bounded amount.
    let mut steps = 0;
    loop {
        let moved = on.maintain_geometry(&gpu);
        if moved == 0 {
            break;
        }
        // The budget, and at most the one slot it was on when it ran out.
        assert!(moved <= (4 << 20) + 8192 * 36, "one call moved {moved} bytes");
        steps += 1;
        assert!(steps < 64, "the arena never settled");
    }
    let after = on.geometry_arena_stats();
    assert!(
        after.bytes * 2 < shrunk.bytes,
        "with a survivor on every page the arena still holds {} bytes in {} pages, of {} in {} ({} live)",
        after.bytes,
        after.pages,
        shrunk.bytes,
        shrunk.pages,
        after.live
    );
    assert!(after.moved > 0 && after.pages < shrunk.pages);
    // What a memory budget reads is what the pages hold.
    assert_eq!(on.mesh_gpu_held(chunks_on.iter().copied()), after.bytes, "the chunks' held memory is not the pages'");

    let (_, px_on) = planned_frame(&gpu, &mut on, &s_on, &colour);
    let (_, px_off) = planned_frame(&gpu, &mut off, &s_off, &colour);
    same("colour after moving the survivors", &px_on[0], &px_off[0]);
    same("sun map after moving the survivors", &px_on[1], &px_off[1]);
    // And they still remesh where they are now.
    for (raster, chunks) in [(&mut on, &chunks_on), (&mut off, &chunks_off)] {
        assert!(raster.replace_dynamic(&gpu, chunks[8], &chunk_mesh(8, half * 0.8, 150, 200)));
    }
    let (_, px_on) = planned_frame(&gpu, &mut on, &s_on, &colour);
    let (_, px_off) = planned_frame(&gpu, &mut off, &s_off, &colour);
    same("colour after remeshing a moved chunk", &px_on[0], &px_off[0]);
    // A mesher index past the mesh's own vertices would draw a neighbour's
    // vertices out of the page: the slot refuses the mesh.
    let mut bad = chunk_mesh(16, half, 150, 0);
    bad.indices[5] = bad.vertices.len() as u32;
    assert!(!on.replace_dynamic(&gpu, chunks_on[16], &bad), "a mesh indexing past its own vertices went into the arena");
}

/// **A page started to take moved slots is never left empty and in the
/// way.** Compaction used to start a page whenever the other pages lacked
/// room for ALL of an emptying page's live ranges, then moved only a
/// call's worth, first into the pages that had room: the new page could
/// take nothing. A page that never held a slot was never let go, and,
/// grown past a first page and empty, it read as the emptiest page of all:
/// it was chosen every call, held nothing to move, and the sparse page
/// behind it was never compacted again. Here the moves of each call (one
/// slot, by the budget) fit where room already is while the whole page's
/// would not; compaction must still run to the end, and no page may be
/// left empty.
#[test]
fn compaction_never_strands_an_empty_page() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        // CI's OpenGL cannot build the raster pass.
        return;
    }
    let mut r = Raster::new(&gpu);
    // 8,192 vertices a slot, 131,072 a page: sixteen slots a page.
    r.set_geometry_page_limit(1 << 17, 1 << 18);
    let (_, chunks, half) = coloured_scene(&gpu, &mut r, 32, 150);
    assert!(r.geometry_arena_stats().pages >= 2, "the fixture: two full pages");
    // The first page: two big slots stay, fourteen shrink to one box (under
    // a quarter full, and more live than the second page has free).
    for (i, &id) in chunks.iter().enumerate().take(16).skip(2) {
        assert!(r.replace_dynamic(&gpu, id, &chunk_mesh(i, half, 1, 0)));
    }
    // The second page: one big slot gone, room for exactly one more.
    r.free_dynamic(chunks[31]);
    // One slot a call.
    r.set_geometry_move_budget(1);
    let start = r.geometry_arena_stats();
    let mut calls = 0;
    while r.maintain_geometry(&gpu) > 0 {
        calls += 1;
        assert!(calls < 100, "compaction never settled");
    }
    // One more call, past the end, as every later frame makes.
    r.maintain_geometry(&gpu);
    let end = r.geometry_arena_stats();
    // All sixteen slots of the first page moved, one a call, and it went.
    assert!(
        calls >= 16 && end.bytes < start.bytes,
        "compaction stopped after {calls} moves with {} bytes held in {} pages for {} live (started at {} in {})",
        end.bytes,
        end.pages,
        end.live,
        start.bytes,
        start.pages
    );
}
