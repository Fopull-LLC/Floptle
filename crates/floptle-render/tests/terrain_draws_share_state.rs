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

use floptle_core::{Material, Shading};
use floptle_render::{
    cube, instance_of, set_ext_index, EncodeStats, Globals, Gpu, InstanceRaw, MeshData, MeshId, Raster, SurfaceExtras,
    TexId, TexSampling, TextureData,
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
