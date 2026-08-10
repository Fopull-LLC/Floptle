//! Lit fog, as a picture rather than as a number — five frames of the same
//! scene so the knobs can be compared side by side.
//!
//! `fog_probe` proves the arithmetic; this one exists because "does it look like
//! weather" is not a thing an assertion can answer. A low sun behind a row of
//! standing stones over a valley of ground mist:
//!
//!   fog_shot_flat.png    — the fog that was there before (amount 0)
//!   fog_shot_lit.png     — the same fog, lit by the sun, nothing occluding it
//!   fog_shot_shafts.png  — shafts on: the stones cut the light between them
//!   fog_shot_back.png    — the camera turned around, sun behind: forward
//!                          scattering means this should be a DIM frame
//!   fog_shot_thick.png   — density up, shafts on, the whole valley in it
//!
//! Run: cargo run -p floptle-render --example fog_shot -- <out-dir>

use floptle_field::{Brush, BrushProfile, Terrain};
use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 960;
const H: u32 = 540;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(W, H);

    // A shallow bowl: high ground east and west, the valley floor between them
    // where the mist settles.
    let mut t = Terrain::flat([160, 56, 128], [0.0; 3], [40.0, 10.0, 32.0], -2.0, [0.32, 0.34, 0.3]);
    for x in [-26.0f32, -20.0, 24.0, 30.0] {
        for _ in 0..40 {
            t.sculpt(Brush::Raise, [x, -1.0, 0.0], 9.0, 1.0, BrushProfile::default());
        }
    }

    let mut rm = Raymarch::new(&gpu);
    rm.set_terrain_textures(&gpu, &[TextureData {
        pixels: vec![255; 64 * 64 * 4],
        width: 64,
        height: 64,
    }]);
    rm.set_volumes(&gpu, &[&t.baked]);

    // The camera sits in the valley looking west into the low sun.
    let eye = DVec3::new(4.0, 2.2, 16.0);
    let sun = Vec3::new(-0.93, 0.17, -0.05).normalize();

    let bc = t.baked.center;
    let hf = t.baked.half_extent;
    let vol_c = (DVec3::new(bc[0] as f64, bc[1] as f64, bc[2] as f64) - eye).as_vec3();
    let mut vol_center = [[0.0f32; 4]; 16];
    let mut vol_half = [[1.0f32, 1.0, 1.0, 0.5]; 16];
    vol_center[0] = [vol_c.x, vol_c.y, vol_c.z, 1.0];
    vol_half[0] = [hf[0], hf[1], hf[2], 0.6];

    // A row of standing stones between the camera and the sun. Blobs are field
    // matter, so they cast into the fog through the same march the terrain does —
    // and the gaps between them are the point of the frame.
    let mut blobs = [[0.0f32; 4]; 16];
    let mut n = 0;
    for i in 0..7 {
        let z = -6.0 + i as f32 * 4.4;
        let p = Vec3::new(-7.0, 2.2, z) - eye.as_vec3();
        blobs[n] = [p.x, p.y, p.z, 1.3];
        n += 1;
    }

    // (file, camera yaw toward the sun?, amount, shafts, density)
    let frames: [(&str, bool, f32, bool, f32); 5] = [
        ("fog_shot_flat.png", true, 0.0, false, 0.07),
        ("fog_shot_lit.png", true, 1.0, false, 0.07),
        ("fog_shot_shafts.png", true, 1.0, true, 0.07),
        ("fog_shot_back.png", false, 1.0, true, 0.07),
        ("fog_shot_thick.png", true, 1.4, true, 0.11),
    ];

    for (name, into_sun, amount, shafts, density) in frames {
        // Aimed just off the sun, so the frame is about the light BETWEEN the stones
        // rather than about a white disc.
        let look = if into_sun { Vec3::new(-0.55, 0.02, -1.0) } else { Vec3::new(0.55, 0.02, 1.0) };
        let rot = Quat::from_rotation_arc(Vec3::NEG_Z, look.normalize());
        let cam = RenderCamera::new(
            eye,
            rot,
            Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 500.0 },
        );
        let vp = cam.view_proj(W as f32 / H as f32);
        let g = RaymarchGlobals {
            view_proj: vp.to_cols_array_2d(),
            inv_view_proj: vp.inverse().to_cols_array_2d(),
            light_dir: [sun.x, sun.y, sun.z, 0.0],
            light_color: [0.5, 0.43, 0.31, 0.0],
            ambient: [0.02, 0.025, 0.045, 0.0],
            bg: [0.05, 0.06, 0.1, 1.0],
            params: [0.0, n as f32, 0.06, 1.0],
            vol_center,
            vol_half,
            blobs,
            shadow_params: [1.0, 20.0, 1.0, 120.0],
            terrain_params: [16.0, 0.0, 0.0, 1.0],
            fog_color: [0.72, 0.66, 0.6, 0.0],
            fog_params: [0.0, 90.0, 1.0, 0.0],
            // A layer 5 m deep over the valley floor, broken into drifting patches.
            vol_fog_a: [density, 3.5, 4.0, 0.45],
            vol_fog_b: [11.0, 0.0, eye.y as f32, 1.0],
            vol_fog_c: [amount, 0.5, 28.0, if shafts { 1.0 } else { 0.0 }],
            ..Default::default()
        };
        let (tex, view) = target(&gpu, name);
        rm.draw_into(&gpu, &view, gpu.depth_view(), g);
        save(&gpu, &tex, &format!("{dir}/{name}"));
        println!("wrote {name}");
    }
}

fn target(gpu: &Gpu, label: &str) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

fn save(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
    let padded =
        (W * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("fog-shot-readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fog-shot") });
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
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut flat = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            flat.extend_from_slice(&if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    let file = std::fs::File::create(path).expect("create png");
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().expect("header").write_image_data(&flat).expect("write");
}
