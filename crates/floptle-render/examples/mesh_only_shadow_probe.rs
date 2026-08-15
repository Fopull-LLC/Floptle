//! Sun shadows in a scene made of NOTHING BUT MESHES — no terrain, no blobs,
//! no baked occluder volumes. A flat plane and a character standing on it, both
//! casting through their collider proxies, which is what an ordinary game scene
//! actually looks like.
//!
//! `shadow_probe` has always built a terrain first, so every frame it renders
//! has at least one volume in the field. This one has **zero volumes**, and that
//! is the whole point: it is the path a project hits the moment it drags in a
//! plane and a character instead of sculpting a hill.
//!
//! **It asserts, in both directions.** Open ground away from the caster must be
//! evenly lit — the defect this exists for painted scanlines of full shadow
//! across it — and the caster must still be casting, because a "fix" that
//! simply stops shadowing anything would sail through the first check alone.
//!
//! Writes `<dir>/mesh_only_smooth.png` (the shadow as it should read) and
//! `<dir>/mesh_only_bands.png` (the same, posterized into 4 bands, dither off).
//!
//! Run: cargo run -p floptle-render --example mesh_only_shadow_probe -- <dir>

use floptle_core::transform::Transform;
use floptle_render::{
    capsule, cube, instance_of, Globals, Gpu, InstanceRaw, MeshId, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TexId,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 900;
const H: u32 = 560;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
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

    let mut raster = Raster::new(&gpu);
    let cube_id = raster.register(&gpu, &cube(0.7), None);
    let capsule_id = raster.register(&gpu, &capsule(0.5, 0.55, 16, 24), None);
    let raymarch = Raymarch::new(&gpu);
    // NO `set_volumes` call at all. `params.w` stays 0, so the field holds
    // nothing and every shadow ray's only possible occluder is a proxy.

    // Looking down at the ground from in front of the character, the way the
    // Scene view sits when somebody notices their floor looks wrong.
    let target = Vec3::new(0.0, 0.6, 0.0);
    let cam_pos = DVec3::new(0.0, 6.0, 15.0);
    let fwd = (target - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.5, 0.9, 0.45).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.22, 0.24, 0.3, 0.0],
        ..Default::default()
    };

    // The ground: a wide, thin box, the way a Plane node with a collider reads.
    let plane_pos = [0.0f64, -0.35, 0.0];
    let plane_half = [14.0f32, 0.35, 14.0];
    // A "character": a standing capsule on the plane.
    let body_pos = [0.0f64, 1.05, 0.0];

    let rel = |p: [f64; 3]| (DVec3::from(p) - cam.world_position).as_vec3();
    let mesh = |id: MeshId, pos: [f64; 3], scale: Vec3, color: [f32; 3]| -> (MeshId, Option<TexId>, InstanceRaw) {
        let tr = Transform {
            translation: DVec3::from(pos),
            scale,
            ..Default::default()
        };
        (id, None, instance_of(tr.render_matrix(cam.world_position), color))
    };
    let instances = vec![
        // 0.7 half-extents on the unit cube, so scale = half / 0.7.
        mesh(
            cube_id,
            plane_pos,
            Vec3::new(plane_half[0] / 0.7, plane_half[1] / 0.7, plane_half[2] / 0.7),
            [0.30, 0.28, 0.33],
        ),
        mesh(capsule_id, body_pos, Vec3::ONE, [0.95, 0.85, 0.4]),
    ];

    // Both colliders, exactly as the editor harvests them: a box for the plane,
    // a capsule segment for the body. The PLANE'S OWN PROXY is the part that
    // matters — every fragment of the ground starts its shadow ray inside it.
    let mut prox_a = [[0.0f32; 4]; 32];
    let mut prox_b = [[0.0f32; 4]; 32];
    let prox_rot = [[0.0f32, 0.0, 0.0, 1.0]; 32];
    let c = rel(plane_pos);
    prox_a[0] = [c.x, c.y, c.z, 0.0];
    prox_b[0] = [plane_half[0], plane_half[1], plane_half[2], 2.0]; // w = 2 → box
    let c = rel(body_pos);
    prox_a[1] = [c.x, c.y - 0.55, c.z, 0.5];
    prox_b[1] = [c.x, c.y + 0.55, c.z, 1.0]; // w = 1 → capsule

    let base = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.22, 0.24, 0.3, 0.0],
        bg: [0.62, 0.42, 0.16, 1.0],
        params: [0.0, 0.0, 0.3, 0.0], // no blobs, and NO VOLUMES
        prox_count: [2.0, 0.0, 0.0, 0.0],
        prox_a,
        prox_b,
        prox_rot,
        ..Default::default()
    };

    // (name, quantize bands) — the reported setup is 4 bands with dither off,
    // and the defect showed up under both, so both are checked.
    for (name, bands) in [("mesh_only_smooth.png", 0.0f32), ("mesh_only_bands.png", 4.0)] {
        let mut rm = base;
        rm.shadow_params = [1.0, 8.0, 1.0, 150.0];
        rm.shadow_tint = [0.0, 0.0, 0.0, bands];
        rm.shadow_extra = [0.0, 0.0, 0.0, 0.0]; // dither off
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
        raster.draw_scene(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &instances,
            None,
            Some(raymarch.field_bind()),
        );
        let pixels = read_back(&gpu, &color_tex);
        check(&pixels, name);
        let path = std::path::Path::new(&dir).join(name);
        save_png(&pixels, &path);
        println!("wrote {}", path.display());
    }
    println!(
        "mesh-only sun shadows OK — and then LOOK at mesh_only_smooth.png: only the picture \
         says whether the penumbra reads as a shadow rather than as a smudge"
    );
}

/// Mean luminance of a rectangle of the frame, 0..1.
fn patch(px: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += (px[i] as f32 * 0.299 + px[i + 1] as f32 * 0.587 + px[i + 2] as f32 * 0.114)
                / 255.0;
            n += 1.0;
        }
    }
    sum / n.max(1.0)
}

/// The two claims, both read from the SAME render — so a software rasteriser
/// answers them the way a real card does, and neither depends on an exact shade.
fn check(px: &[u8], name: &str) {
    // Open ground, front-left, nowhere near the character or its shadow. Read
    // row by row: the defect was horizontal bands of full shadow, so a whole-
    // region average would have quietly passed while the floor looked striped.
    let rows: Vec<f32> = (380..545).map(|y| patch(px, 60, y, 320, y + 1)).collect();
    let lo = rows.iter().cloned().fold(f32::INFINITY, f32::min);
    let hi = rows.iter().cloned().fold(0.0f32, f32::max);
    assert!(
        hi > 0.05,
        "{name}: the open floor read as black ({hi:.3}) — nothing to say about evenness",
    );
    assert!(
        (hi - lo) / hi < 0.15,
        "{name}: open ground away from the caster is not evenly lit — its darkest row is \
         {lo:.3} against a brightest of {hi:.3}. That is the scanline-shadow defect: a \
         shadow ray that leaves the field entirely was being read as fully blocked.",
    );

    // …and the caster is still casting. Without this, deleting the shadow pass
    // passes the check above perfectly.
    let shade = patch(px, 395, 272, 435, 292); // the ground just left of the body
    let open = patch(px, 60, 380, 320, 545);
    assert!(
        shade < open * 0.9,
        "{name}: the character is not casting — the ground beside it reads {shade:.3} \
         against open ground's {open:.3}",
    );
}

/// Pull the rendered frame back to the CPU as tightly-packed RGBA.
fn read_back(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let bpp = 4u32;
    let unpadded = W * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
    encoder.copy_texture_to_buffer(
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
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for row in 0..H {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    pixels
}

fn save_png(pixels: &[u8], path: &std::path::Path) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(pixels).unwrap();
}
