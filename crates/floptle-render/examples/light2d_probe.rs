//! 2D lighting probe (`floptle/0113`, step 2): a dark room with a torch in it.
//!
//! A flat tilemap under an orthographic camera, drawn three times:
//!
//! * **unlit** — the raster pass alone, exactly what shipped before this. The
//!   reference every other shot is compared against.
//! * **lit** — one warm 2D light near the middle. The map must be brighter where
//!   the light is and darker at the edges, and the falloff must actually reach
//!   zero rather than trailing off across the whole frame.
//! * **masked** — the same light, restricted to a sorting layer the map is NOT
//!   on. The map must come out at ambient: this is the "a torch passes over the
//!   background without lighting it" case, and it is the one thing a 2D artist
//!   asks for that nothing else in the engine does.
//!
//! It writes all three PNGs, because the numbers below say *whether* something
//! changed and only the picture says whether it looks like light.
//!
//! Run: cargo run -p floptle-render --example light2d_probe -- <outdir>

use floptle_render::{
    instance_of_mat, Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, Projection,
    Raster, RenderCamera, TexId, TexSampling, TextureData, mesh,
};
use glam::{DVec3, Mat4, Quat};

const S: u32 = 256;
const COLS: u32 = 16;
const ROWS: u32 = 16;
const TILE: f32 = 1.0;
const SHEET: u32 = 1;
const ORTHO_HEIGHT: f32 = 16.0;
/// The rank the map's sorting layer resolves to.
const MAP_RANK: u32 = 1;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    // One flat mid-grey cell. Deliberately featureless: every difference between
    // the shots below is then the lighting and nothing else.
    let n = 8u32;
    let pixels: Vec<u8> = (0..n * n).flat_map(|_| [150u8, 150, 150, 255]).collect();
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels, width: n, height: n },
        TexSampling::default(),
    );

    let data: Vec<u32> = (0..COLS * ROWS).map(|_| 0).collect();
    let md = mesh::tilemap(COLS, ROWS, TILE, SHEET, SHEET, [0.0, 0.0], &data);
    let map = raster.register(&gpu, &md, None);
    // Unlit, as every 2D layer is: a flat layer lit by the scene's sun goes dark
    // at night, so 2D lighting has to be the thing that lights it.
    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::IDENTITY, &mat);
    let flat = [(map, Some::<TexId>(tex), Light2dInstance::from_raster(&raw, MAP_RANK))];

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.05, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);

    // A dim ambient so "unlit by any 2D light" is visibly dark rather than black
    // — the same choice the engine makes, and the reason a scene with no lights
    // in it does not look broken.
    let ambient = [0.25, 0.25, 0.3, 0.0];
    let mut lit = Light2dUniform {
        count: [1.0, 0.0, 0.0, 0.0],
        ambient,
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };
    // Camera-relative, as every light reaches the shader — and the map's model
    // matrix is the identity, so the map is at the render-space origin too.
    lit.pos[0] = [0.0, 0.0, 0.0, 7.0];
    lit.color[0] = [1.6, 1.2, 0.7, 0.0];
    lit.mask[0] = [1 << MAP_RANK, 0, 0, 0];

    // The same light pointed at a layer the map is not on.
    let mut masked = lit;
    masked.mask[0] = [1 << (MAP_RANK + 1), 0, 0, 0];

    let shots: [(&str, Option<Light2dUniform>); 3] =
        [("unlit", None), ("lit", Some(lit)), ("masked", Some(masked))];
    let mut mid = [0f32; 3];
    let mut edge = [0f32; 3];

    for (i, (name, lights)) in shots.iter().enumerate() {
        let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("light2d-color"),
            size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.surface_format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let globals =
            Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
        raster.draw_scene(
            &gpu,
            &view,
            gpu.depth_view(),
            globals,
            &[(map, Some::<TexId>(tex), raw)],
            Some([0.02, 0.02, 0.04, 1.0]),
            None,
        );
        if let Some(l) = lights {
            raster.light2d_pass(
                &gpu,
                &view,
                gpu.depth_view(),
                (S, S),
                view_proj.to_cols_array_2d(),
                l,
                &flat,
            );
        }
        let px = readback(&gpu, &color);
        mid[i] = luma(&px, S / 2, S / 2);
        edge[i] = luma(&px, 6, S / 2);
        let out = format!("{dir}/light2d_{name}.png");
        save_png(&px, &out);
        println!("{name}: centre {:.3}, edge {:.3} — wrote {out}", mid[i], edge[i]);
    }

    // The map is unlit-material, so without the pass it is the texture's own
    // flat grey everywhere: centre and edge agree.
    assert!(
        (mid[0] - edge[0]).abs() < 0.02,
        "the unlit reference must be flat, and it is {:.3} vs {:.3}",
        mid[0],
        edge[0]
    );
    // A light brightens the middle above AMBIENT — which is what the masked shot
    // measures. Comparing against the unlit reference instead would conflate two
    // different things: ambient legitimately darkens a scene that had none, so
    // "lit is brighter than unlit" can be false while the light works perfectly.
    assert!(
        mid[1] > mid[2] + 0.15,
        "the torch did not brighten the centre: {:.3} against ambient {:.3}",
        mid[1],
        mid[2]
    );
    assert!(
        mid[1] > edge[1] + 0.15,
        "the falloff is not falling off: centre {:.3}, edge {:.3}",
        mid[1],
        edge[1]
    );
    // …and it must reach ZERO by the edge, not merely be dimmer there. Past the
    // range the pixel is pure ambient, which is what `masked` also is.
    assert!(
        (edge[1] - edge[2]).abs() < 0.02,
        "the light still reaches the frame edge: {:.3} vs the masked {:.3}",
        edge[1],
        edge[2]
    );
    // The masked shot is the whole point: a light that does not reach this
    // layer leaves it at ambient, uniformly.
    assert!(
        (mid[2] - edge[2]).abs() < 0.02,
        "a light masked off this layer still lit it: centre {:.3}, edge {:.3}",
        mid[2],
        edge[2]
    );
    assert!(
        mid[2] < mid[1] - 0.15,
        "masking changed nothing: {:.3} vs the lit {:.3}",
        mid[2],
        mid[1]
    );
    println!("2D lighting OK");
}

/// Perceptual-ish brightness of one pixel, 0..1.
fn luma(px: &[[u8; 4]], x: u32, y: u32) -> f32 {
    let p = px[(y * S + x) as usize];
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
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
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut o = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * bpp) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            o.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    o
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
