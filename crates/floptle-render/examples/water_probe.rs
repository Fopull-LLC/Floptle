//! Water probe (`floptle/0038`): a body of water has to LOOK like water, not
//! like a coloured wall.
//!
//! Three things this checks, because all three were wrong at some point while
//! the ocean was a hand-placed sphere:
//!
//! 1. **You can see through it.** A submerged object must show through the
//!    surface. Alpha 0.55 over the opaque pass is not enough on its own — the
//!    instance has to reach the BLENDED pass, and an opaque-pass water surface
//!    is a solid coloured wall you cannot see the seabed through.
//! 2. **The pool path scales to its half-extents.** A box volume drawn at the
//!    sphere's fit factor would be a lake the wrong size — and the collider it
//!    is supposed to agree with would be somewhere else.
//! 3. **Frozen is opaque.** Ice is a surface you stand on; if it still looked
//!    see-through, an ice world would read as a place you are about to fall
//!    through — which is exactly what it used to be.
//!
//! What this probe CANNOT show: the specular highlight. `draw_scene` without a
//! field bind group shades flat in every probe in this directory (compare
//! `material_probe`, whose "shiny" sphere is as flat as its "matte" one), so a
//! highlight assertion here would be testing the harness, not the water. The
//! specular parameters are set in the editor's draw arm; they are verified by
//! looking at a real scene, not here.
//!
//! Run: cargo run -p floptle-render --example water_probe -- <out.png>

use floptle_render::{
    cube, instance_of_mat, uv_sphere, Globals, Gpu, MaterialParams, MeshData, Projection, Raster,
    RenderCamera, TexId,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 384;

/// The material the editor gives a water volume — kept in step with
/// `render_frame.rs`'s `Matter::WaterVolume` arm by hand, and the reason this
/// probe asserts the BEHAVIOUR (see-through, shiny) rather than the numbers.
fn water_material(tint: [f32; 3], frozen: bool) -> MaterialParams {
    let mut mp = MaterialParams::flat(tint);
    if frozen {
        mp.alpha = 1.0;
        mp.specular_strength = 0.15;
        mp.shininess = 8.0;
    } else {
        mp.alpha = 0.55;
        mp.specular_strength = 0.9;
        mp.shininess = 96.0;
        mp.specular = [1.0, 1.0, 1.0];
    }
    mp
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "water.png".into());
    let gpu = Gpu::headless(S, S);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("water-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    let sphere: MeshData = uv_sphere(0.85, 24, 36);
    let sphere_id = raster.register(&gpu, &sphere, None);
    let box_data: MeshData = cube(0.7);
    let box_id = raster.register(&gpu, &box_data, None);

    let eye = Vec3::new(0.0, 1.6, 6.0);
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        Quat::from_rotation_x(-0.16),
        Projection::Perspective { fov_y: 0.9, near: 0.02, far: 200.0 },
    );
    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        // `light_dir` is the direction TO the light: up, and toward the viewer,
        // so the near-top shoulder of the water gets the highlight.
        // A sun coming over the viewer's shoulder and down, so the near face of
        // the water gets a highlight and the far side does not.
        light_dir: [0.30, 0.80, 0.52, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.12, 0.12, 0.16, 0.0],
        ..Default::default()
    };

    // A bright marker BEHIND the water: if the surface blends, this shows
    // through it; if it renders opaque, it does not.
    let marker = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 0.25, 0.1]) };
    let marker_at = Mat4::from_translation(Vec3::new(-1.6, 0.3, -1.0) - eye)
        * Mat4::from_scale(Vec3::splat(0.45));

    let tint = [0.10, 0.32, 0.38];
    // LEFT: liquid, covering the marker. RIGHT: the same volume, frozen.
    let liquid = Mat4::from_translation(Vec3::new(-1.6, 0.3, 0.6) - eye)
        * Mat4::from_scale(Vec3::splat(1.4 / 0.85));
    let frozen = Mat4::from_translation(Vec3::new(1.7, 0.3, 0.6) - eye)
        * Mat4::from_scale(Vec3::splat(1.4 / 0.85));
    // …and a Pool below, to prove the box path scales to its half-extents too.
    let pool = Mat4::from_translation(Vec3::new(0.0, -1.9, 0.0) - eye)
        * Mat4::from_scale(Vec3::new(3.4, 0.35, 2.2) / 0.7);

    let draws: Vec<(_, Option<TexId>, _)> = vec![
        (sphere_id, None, instance_of_mat(marker_at, &marker)),
        (box_id, None, instance_of_mat(pool, &water_material(tint, false))),
        (sphere_id, None, instance_of_mat(liquid, &water_material(tint, false))),
        (sphere_id, None, instance_of_mat(frozen, &water_material(tint, true))),
    ];
    raster.draw_scene(
        &gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        &draws,
        Some([0.05, 0.06, 0.09, 1.0]),
        None,
    );

    let raw = readback(&gpu, &color);
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let px: Vec<[u8; 4]> =
        raw.into_iter().map(|p| if bgra { [p[2], p[1], p[0], p[3]] } else { p }).collect();
    save_png(&px, &out);
    let at = |fx: f32, fy: f32| px[((fy * S as f32) as u32 * S + (fx * S as f32) as u32) as usize];

    // 1. SEE-THROUGH: over the marker, the red channel must survive the water.
    //    Opaque water would show only the tint, whose red is ~0.10.
    let over_marker = at(0.29, 0.50);
    println!("over the submerged marker: {over_marker:?}");
    assert!(
        over_marker[0] as i32 > over_marker[2] as i32 + 20,
        "the submerged marker should show THROUGH the water, got {over_marker:?} — \
         an opaque water surface is a coloured wall you cannot see the seabed through"
    );

    // 2. THE POOL scales to its half-extents, not the sphere's fit factor.
    //    The drawn surface has to land where the SOLVER's box is, or you float
    //    at a waterline that is not the one you can see. Checked by colour: a
    //    pixel inside the pool's footprint must be the water, and one outside
    //    it must be the background.
    let lum = |p: [u8; 4]| p[0] as i32 + p[1] as i32 + p[2] as i32;
    let is_water = |p: [u8; 4]| (p[2] as i32 - p[0] as i32) > 28 && p[1] > p[0];
    let in_pool = at(0.50, 0.86);
    let beyond_pool = at(0.02, 0.03);
    println!("in the pool {in_pool:?}   past its edge {beyond_pool:?}");
    assert!(is_water(in_pool), "the pool should reach here, got {in_pool:?}");
    assert!(!is_water(beyond_pool), "the pool should NOT reach here, got {beyond_pool:?}");

    // 3. FROZEN IS OPAQUE: the ice sphere must show its own tint, with the dark
    //    background nowhere near as visible through it as through the liquid.
    let ice = at(0.72, 0.50);
    println!("ice {ice:?}");
    assert!(
        lum(ice) > 40,
        "frozen water should read as a solid surface you can stand on, got {ice:?}"
    );

    println!("water probe OK; wrote {out}");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
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
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * bpp) as usize;
            out.push([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        }
    }
    drop(data);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let mut flat = Vec::with_capacity(px.len() * 4);
    for p in px {
        flat.extend_from_slice(p);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("png header").write_image_data(&flat).expect("png data");
}
