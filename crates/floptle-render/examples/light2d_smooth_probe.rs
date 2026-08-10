//! **A 2D light is not part of the palette** (`floptle/0127`).
//!
//! A posterized project quantizes its *palette*: the set of values its art is
//! allowed to be. A light is not one of those values — it is a multiplier on
//! whatever value the art is. While the quantize was the last thing to touch the
//! frame, the two were the same setting, and there was no way to configure a
//! scene that was right:
//!
//! | `posterize_bands` | `posterize_dither` | the light |
//! |---|---|---|
//! | 8 | off | hard concentric rings — the reported "harsh bands" |
//! | 8 | on  | a stipple; it reads as a dither pattern, not as light |
//! | off | — | smooth, and the project loses the palette it chose posterize for |
//!
//! Every row gives something up. So this probe asserts the thing the card asks
//! for — *"a 2D light produces no step that the surface underneath it does not
//! already have"* — and it asserts it **dithered and undithered**, which is what
//! "regardless of how you configure your scene" means and what stops a
//! dither-based fix from passing. Dither was tried in the field and rejected on
//! sight: it lands in row two.
//!
//! Three things have to be true at once, and each has a control, because any two
//! of them are easy to satisfy by breaking the third:
//!
//! 1. **The light is smooth.** Measured as the *difference* between a lit and an
//!    unlit shot of the same frame, which cancels the art exactly — including
//!    the dither pattern, which is deterministic and screen-space. Whatever is
//!    left is the light and nothing else.
//! 2. **The light is still there.** The difference has to swing across several
//!    bands' worth of brightness, or "no steps" would be satisfied by a light
//!    that does nothing.
//! 3. **The art is still posterized.** A ramp with no light on it lands on no
//!    more levels than the band count. A fix that quietly stopped quantizing
//!    would pass 1 and 2 and destroy the look the project is made of.
//!
//! …and a fourth, which is what makes the probe evidence rather than assertion:
//! **the same lit frame, quantized the old way, does step.** The scene is one
//! where the bug reproduced.
//!
//! Run: cargo run -p floptle-render --example light2d_smooth_probe -- <outdir>

use floptle_render::{
    Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, PostSettings, PostStack,
    Projection, Raster, RenderCamera, TexId, TexSampling, TextureData, instance_of_mat, mesh,
};
use glam::{DVec3, Mat4, Quat};

const S: u32 = 256;
const ORTHO_HEIGHT: f32 = 16.0;
const MAP_RANK: u32 = 1;
const BANDS: u32 = 8;
/// The colour from the original report: a mild warm white, the least exotic
/// colour a lamp can be.
const WARM: [f32; 3] = [1.0, 0.86, 0.62];
/// A channel has to move by more than this between adjacent pixels to count as a
/// step, so 8-bit rounding is not read as one. A band at 8 levels is ~36 counts,
/// so this is nowhere near able to hide one.
const STEP: i32 = 3;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    // Flat mid-grey art. Every value that varies across the frame below therefore
    // came from the light or from the quantizer, and there is nothing else it
    // could have come from.
    let flat_tex = solid(&gpu, &mut raster, 150);
    // …and a ramp, for the control that the palette is still being quantized.
    let ramp_tex = ramp(&gpu, &mut raster);

    let map = raster.register(
        &gpu,
        &mesh::tilemap(16, 16, 1.0, 1, 1, [0.0, 0.0], &vec![0u32; 16 * 16]),
        None,
    );
    // One quad filling the view, so the 256-wide ramp lands about 1:1 on the
    // frame. Repeated small it would be minified into a blur, and then "few
    // distinct values" would be a fact about the sampler rather than about the
    // quantizer — which is the shape of control that passes while broken.
    let sheet = raster.register(&gpu, &mesh::plane(ORTHO_HEIGHT * 0.5), None);
    let mat = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let raw = instance_of_mat(Mat4::IDENTITY, &mat);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.0, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);

    // Ambient exactly white is the identity, and `Light2dUniform::reach` reads it
    // as "no light reaches anything" — so the unlit reference skips the composite
    // altogether and holds the quantized art alone. The lit shot then differs
    // from it by the light term and by nothing else.
    let dark = Light2dUniform {
        ambient: [1.0, 1.0, 1.0, 0.0],
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };
    let mut lit = dark;
    lit.count = [1.0, 0.0, 0.0, 0.0];
    lit.pos[0] = [0.0, 0.0, 0.0, 8.0];
    lit.color[0] = [WARM[0] * 2.2, WARM[1] * 2.2, WARM[2] * 2.2, 0.0];
    lit.mask[0] = [1 << MAP_RANK, 0, 0, 0];

    let settings = |dither: bool| PostSettings {
        bloom: false,
        bloom_threshold: 1.0,
        bloom_intensity: 0.7,
        vignette: false,
        vignette_strength: 0.0,
        vignette_radius: 1.0,
        ssao: false,
        ssao_strength: 0.0,
        ssao_radius: 0.5,
        posterize_bands: BANDS,
        posterize_dither: dither,
        posterize_chroma: true,
        color_filter: 0,
        color_filter_strength: 1.0,
        simulate_deficiency: false,
        ..Default::default()
    };

    // ---- 3. the art is still posterized ------------------------------------
    //
    // A 256-step ramp through an 8-band quantizer has to come out on 8 levels.
    // Undithered, because dither's whole job is to trade levels for a stipple.
    let art = shot(
        &gpu, &mut raster, view_proj, sheet, ramp_tex, &raw, &dark, &settings(false), MAP_RANK,
    );
    save_png(&art, &format!("{dir}/light2d_smooth_art.png"));
    let levels = distinct_levels(&row(&art, S / 2));
    assert!(
        (2..=BANDS as usize + 1).contains(&levels),
        "a 256-value ramp came out on {levels} EXACT values against a band count of {BANDS}. \
         The palette pass is not quantizing the art — which is the half of this that a \
         project turned posterize ON for."
    );

    // ---- 1 + 2 + 4: the light, both ways -----------------------------------
    for dither in [false, true] {
        let tag = if dither { "dithered" } else { "undithered" };
        let s = settings(dither);
        let off =
            shot(&gpu, &mut raster, view_proj, map, flat_tex, &raw, &dark, &s, MAP_RANK);
        let on = shot(&gpu, &mut raster, view_proj, map, flat_tex, &raw, &lit, &s, MAP_RANK);
        save_png(&on, &format!("{dir}/light2d_smooth_{tag}.png"));
        // …and the same frame through the quantizer that used to run last. This
        // is the picture the report was about, and it is here so a reader can see
        // what the assertion below is claiming, rather than take the number.
        let old: Vec<[u8; 4]> =
            on.iter().map(|p| [q8(p[0]), q8(p[1]), q8(p[2]), p[3]]).collect();
        save_png(&old, &format!("{dir}/light2d_smooth_{tag}_old.png"));

        // Out along the horizontal radius from the light's centre. The art is the
        // same in both shots — same tint, same dither, same pixels — so the
        // difference is the light.
        let (a, b) = (row(&off, S / 2), row(&on, S / 2));
        let light: Vec<[i32; 3]> = (0..a.len())
            .map(|i| std::array::from_fn(|c| b[i][c] as i32 - a[i][c] as i32))
            .collect();
        let half = &light[(S / 2) as usize..light.len() - 2];

        // 2. it is a light and not a rounding error.
        let swing = (0..3)
            .map(|c| {
                let v: Vec<i32> = half.iter().map(|p| p[c]).collect();
                v.iter().max().unwrap() - v.iter().min().unwrap()
            })
            .max()
            .unwrap();
        assert!(
            swing > 100,
            "{tag}: the light only swings {swing} counts across its whole radius, so \
             'it does not step' means nothing here. Raise the intensity or the range."
        );

        // 4. the control: quantizing the LIT frame — the order this used to run
        //    in — steps. If it did not, this scene never had the bug and a pass
        //    below would be proving nothing.
        let old_way: usize = steps(&half.iter().map(|p| p.map(quantize_u8)).collect::<Vec<_>>());
        assert!(
            old_way > 0,
            "{tag}: quantizing the lit frame the OLD way produced no steps either, so this \
             scene does not reproduce the bug and the assertion below is vacuous."
        );

        // 1. …and the fix: the light itself adds none.
        let rings = steps(half);
        assert_eq!(
            rings, 0, "{tag}: the light stepped {rings} times along its radius (the old order \
             stepped {old_way} times here). A light is a multiplier on the palette, not a \
             value in it — if this fires, something is quantizing the frame AFTER the \
             composite again."
        );
        println!("{tag}: swing {swing} counts, {old_way} steps the old way, {rings} now");
    }

    println!("2D light smooth OK — quantised art, unquantised light, either dither setting");
}

/// Adjacent pairs that move by more than [`STEP`] in any channel.
fn steps(v: &[[i32; 3]]) -> usize {
    v.windows(2).filter(|w| (0..3).any(|c| (w[0][c] - w[1][c]).abs() > STEP)).count()
}

/// The 8-bit stand-in for the terminal quantizer this card removed: the stored
/// byte is already ~gamma, so stepping it is the same shape of operation.
fn quantize_u8(v: i32) -> i32 {
    let scale = (BANDS - 1) as f32;
    let x = (v.clamp(0, 255) as f32 / 255.0 * scale).round() / scale;
    (x * 255.0).round() as i32
}

fn q8(v: u8) -> u8 {
    quantize_u8(v as i32) as u8
}

/// EXACT distinct values, not "values more than a threshold apart": a fuzzy
/// dedup collapses a *smooth* ramp too, by chaining through its own tolerance,
/// and would report a broken quantizer as a working one.
fn distinct_levels(v: &[[u8; 4]]) -> usize {
    let mut seen: Vec<u8> = v.iter().map(|p| p[0]).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

fn row(px: &[[u8; 4]], y: u32) -> Vec<[u8; 4]> {
    (0..S).map(|x| px[(y * S + x) as usize]).collect()
}

fn solid(gpu: &Gpu, raster: &mut Raster, v: u8) -> TexId {
    let n = 8u32;
    let pixels: Vec<u8> = (0..n * n).flat_map(|_| [v, v, v, 255]).collect();
    raster.register_texture(gpu, &TextureData { pixels, width: n, height: n }, TexSampling::default())
}

/// A 256-wide grey ramp — one tile of art holding every value there is.
fn ramp(gpu: &Gpu, raster: &mut Raster) -> TexId {
    let w = 256u32;
    let pixels: Vec<u8> = (0..w).flat_map(|x| [x as u8, x as u8, x as u8, 255]).collect();
    raster.register_texture(gpu, &TextureData { pixels, width: w, height: 1 }, TexSampling::default())
}

/// Draw the map, quantize the palette, composite the light, run the post chain —
/// the engine's own order, which is the thing under test.
#[allow(clippy::too_many_arguments)]
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    view_proj: Mat4,
    map: floptle_render::MeshId,
    tex: TexId,
    raw: &floptle_render::InstanceRaw,
    lights: &Light2dUniform,
    settings: &PostSettings,
    rank: u32,
) -> Vec<[u8; 4]> {
    let size = wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 };
    let make = |label: &str, format: wgpu::TextureFormat, extra: wgpu::TextureUsages| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | extra,
            view_formats: &[],
        })
    };
    let out_tex = make("light2d-smooth-out", gpu.surface_format(), wgpu::TextureUsages::COPY_SRC);
    let depth = make("light2d-smooth-depth", Gpu::DEPTH_FORMAT, wgpu::TextureUsages::empty());
    let out_view = out_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let dview = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let post = PostStack::new(gpu, S, S);
    let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(
        gpu,
        post.input_view(),
        &dview,
        globals,
        &[(map, Some(tex), *raw)],
        Some([0.02, 0.02, 0.04, 1.0]),
        None,
    );
    if let Some(q) = settings.palette() {
        raster.quantize_palette(gpu, post.input_view(), (S, S), q);
    }
    let flat = [(map, Some(tex), Light2dInstance::from_raster(raw, rank, false))];
    raster.light2d_pass(
        gpu,
        post.input_view(),
        &dview,
        (S, S),
        view_proj.to_cols_array_2d(),
        lights,
        &flat,
    );
    post.run(gpu, settings, None, &out_view);
    readback(gpu, &out_tex)
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (S * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
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
        let r = (y * padded) as usize;
        for x in 0..S {
            let i = r + (x * 4) as usize;
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
