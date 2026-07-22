//! Headless probe for solar/shaders/hullPanels.flsl — the fix for Ty's "the
//! grid changes size as I move closer/farther" bug. It renders the REAL shader
//! file through the production path on a ROW of identical boxes receding into
//! the distance. Because the shader now samples `objectPos` (surface-locked
//! object-local space) instead of `worldPos` (camera-relative, ADR-0015), every
//! box must show the SAME panel grid, just perspective-smaller with distance —
//! the grid is locked to the hull, not swimming with the camera. A left sphere
//! and a big foreground box show the seams + weathering up close.
//!
//! Run: cargo run -p floptle-render --example hull_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of_mat, pass_prelude, uv_sphere, FlslBlend, Globals, Gpu, InstanceRaw,
    MaterialParams, MeshId, Projection, Raster, RenderCamera, TexId,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1600;
const H: u32 = 600;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "hull.png".into());
    let src_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../solar/shaders/hullPanels.flsl");
    let src = std::fs::read_to_string(src_path).expect("read hullPanels.flsl");

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
    let box_mesh = raster.register(&gpu, &cube(0.9), None);
    let sphere = raster.register(&gpu, &uv_sphere(0.9, 32, 48), None);

    // Compile hullPanels exactly like the editor does.
    let compiled = floptle_shader::compile_fragment(&src).expect("hullPanels compiles");
    floptle_shader::validate(pass_prelude(), &compiled.chunk)
        .unwrap_or_else(|e| panic!("naga: {} (chunk line {:?})", e.message, e.chunk_line));
    let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
    let id = raster.register_flsl_shader(&gpu, &chunk, 0, FlslBlend::Opaque, None);
    // A metallic gunmetal baseColor (rgba).
    let params = compiled.pack_params(
        &|name| match name {
            "baseColor" => Some([0.55, 0.58, 0.66, 1.0]),
            _ => None,
        },
        &|_| None,
    );
    let bind = raster.set_flsl_binding(&gpu, None, id, &params, &[]);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.4, 8.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.13, 0.13, 0.17, 0.0],
        ..Default::default()
    };

    let mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    let tf = |x: f64, y: f64, z: f64| {
        Transform::from_translation(DVec3::new(x, y, z)).render_matrix(cam.world_position)
    };
    // A row of IDENTICAL boxes receding in Z (each shows the same grid) + a big
    // foreground box and a sphere on the left for close-up seams.
    let flsl: Vec<floptle_render::FlslDraw> = vec![
        (box_mesh, None, bind, instance_of_mat(tf(-4.6, 0.0, 2.0), &mp)),
        (sphere, None, bind, instance_of_mat(tf(-2.2, 0.0, 0.0), &mp)),
        (box_mesh, None, bind, instance_of_mat(tf(0.4, 0.0, 0.0), &mp)),
        (box_mesh, None, bind, instance_of_mat(tf(2.4, 0.0, -3.0), &mp)),
        (box_mesh, None, bind, instance_of_mat(tf(4.2, 0.0, -7.0), &mp)),
        (box_mesh, None, bind, instance_of_mat(tf(6.0, 0.0, -12.0), &mp)),
    ];
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![];

    raster.draw_scene_with(
        &gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        &instances,
        &flsl,
        Some([0.02, 0.02, 0.05, 1.0]),
        None,
    );
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — hullPanels on a receding row: every box shows the SAME grid (surface-locked)");
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
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
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
