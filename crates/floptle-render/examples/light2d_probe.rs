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
//! Then once more at a smaller size, because one renderer serves viewports of
//! several sizes and the G-buffer they share only ever grows.
//!
//! It writes every shot as a PNG, because the numbers below say *whether*
//! something changed and only the picture says whether it looks like light.
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
    let flat = [(map, Some::<TexId>(tex), Light2dInstance::from_raster(&raw, MAP_RANK, false))];

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
        let px = shot(&gpu, &mut raster, S, view_proj, map, tex, &raw, &flat, lights.as_ref());
        mid[i] = luma(&px, S, S / 2, S / 2);
        edge[i] = luma(&px, S, 6, S / 2);
        let out = format!("{dir}/light2d_{name}.png");
        save_png(&px, S, &out);
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

    // ---- a smaller frame through a G-buffer that has already grown ----------
    //
    // One renderer serves several viewports of different sizes in a frame — the
    // Scene view, a docked Game view, camera previews, render targets — and the
    // G-buffer only grows, so the small ones draw into a corner of a buffer
    // sized for the biggest. If that corner were addressed wrongly the lighting
    // would land offset or scaled, and only in the smaller view: the exact shape
    // of "fine in the Scene view, wrong in the Game view" this renderer has paid
    // for three times.
    const SMALL: u32 = 96;
    let px = shot(&gpu, &mut raster, SMALL, view_proj, map, tex, &raw, &flat, Some(&lit));
    let small_mid = luma(&px, SMALL, SMALL / 2, SMALL / 2);
    let small_edge = luma(&px, SMALL, 2, SMALL / 2);
    let out = format!("{dir}/light2d_small.png");
    save_png(&px, SMALL, &out);
    println!("small: centre {small_mid:.3}, edge {small_edge:.3} — wrote {out}");
    // The same scene through the same camera: the same picture, fewer pixels.
    assert!(
        (small_mid - mid[1]).abs() < 0.05,
        "the light moved when the frame shrank: {small_mid:.3} against {:.3}",
        mid[1]
    );
    assert!(
        (small_edge - edge[1]).abs() < 0.05,
        "the falloff moved when the frame shrank: {small_edge:.3} against {:.3}",
        edge[1]
    );
    // ---- the reported bug: one light must not black out the scene ----------
    //
    // Reported as "I put a light in my scene with just a tileset and the tileset
    // is no longer visible". The base used to be white while no 2D light existed
    // and the 3D ambient the moment one did, so placing a FIRST light dropped a
    // whole level to 12% brightness. Adding a light has to add light.
    let mut only_light = lit;
    only_light.ambient = [1.0, 1.0, 1.0, 0.0]; // the default 2D base
    let px = shot(&gpu, &mut raster, S, view_proj, map, tex, &raw, &flat, Some(&only_light));
    let corner = luma(&px, S, 6, S / 2);
    let centre = luma(&px, S, S / 2, S / 2);
    save_png(&px, S, &format!("{dir}/light2d_default_base.png"));
    println!("default base: centre {centre:.3}, edge {corner:.3}");
    assert!(
        corner >= mid[0] - 0.02,
        "placing a light DARKENED the far corner: {corner:.3} against {:.3} with no \
         lighting at all. A first light must only ever add.",
        mid[0]
    );
    assert!(centre > corner, "…and the light itself still has to show");

    // ---- an authored alpha is the alpha that reaches the screen ------------
    //
    // `floptle/0121`. The composite used to write `albedo × light` OVER the
    // frame at the surface's own alpha — but the raster pass had already blended
    // that same sprite in, so a translucent one arrived twice and landed at an
    // effective `1 - (1-a)²`. 0.5 drew at 0.75; 0.72 drew at 0.92. In every 2D
    // project, with no light placed, invisible everywhere an author could look.
    //
    // It composites a DIFFERENCE now — `C·a·(light - 1)` — so this checks both
    // ends of that claim at four alphas:
    //
    //   * no light placed, white base  ⇒ identical to the render with 2D
    //     lighting switched off entirely;
    //   * a light on it                ⇒ the analytic `C·light` over `B` at the
    //     alpha the author actually typed.
    let alphas = [0.25f32, 0.5, 0.75, 1.0];
    // A base that is not white, so "no lights" is not the only case where the
    // pass has something to do. This one both darkens and brightens per channel,
    // which is why the delta goes out as two halves.
    let mut warm = Light2dUniform {
        count: [1.0, 0.0, 0.0, 0.0],
        ambient: [0.35, 0.35, 0.4, 0.0],
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };
    warm.pos[0] = [0.0, 0.0, 0.0, 7.0];
    warm.color[0] = [1.1, 0.8, 0.4, 0.0];
    warm.mask[0] = [1 << MAP_RANK, 0, 0, 0];
    let idle = Light2dUniform {
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        ..Default::default()
    };

    // The engine's own opaque renders give the two colours the law needs — `C`
    // as it draws it, and `C·light` as it lights it — so the check below is
    // about COMPOSITING and does not re-derive the lighting maths a second time
    // and grade the shader against the probe's opinion of it.
    let opaque = |raster: &mut Raster, lights: Option<&Light2dUniform>| {
        let m = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
        let raw = instance_of_mat(Mat4::IDENTITY, &m);
        let flat = [(map, Some::<TexId>(tex), Light2dInstance::from_raster(&raw, MAP_RANK, false))];
        let px = shot(&gpu, raster, S, view_proj, map, tex, &raw, &flat, lights);
        px[((S / 2) * S + S / 2) as usize]
    };
    let c_off = opaque(&mut raster, None);
    let c_lit = opaque(&mut raster, Some(&warm));

    for a in alphas {
        // Unlit, as every surface on the 2D path is — the gather forces it, so
        // that the difference the composite subtracts is the one the raster pass
        // actually added.
        let m = MaterialParams { unlit: true, alpha: a, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
        let raw = instance_of_mat(Mat4::IDENTITY, &m);
        let flat = [(map, Some::<TexId>(tex), Light2dInstance::from_raster(&raw, MAP_RANK, false))];

        let off = shot(&gpu, &mut raster, S, view_proj, map, tex, &raw, &flat, None);
        let idle_px = shot(&gpu, &mut raster, S, view_proj, map, tex, &raw, &flat, Some(&idle));
        let lit_px = shot(&gpu, &mut raster, S, view_proj, map, tex, &raw, &flat, Some(&warm));
        let at = |p: &Vec<[u8; 4]>| p[((S / 2) * S + S / 2) as usize];
        let (o, i, l) = (at(&off), at(&idle_px), at(&lit_px));
        save_png(&lit_px, S, &format!("{dir}/light2d_alpha_{:02}_lit.png", (a * 100.0) as u32));
        println!(
            "alpha {a:.2}: off {:?}  idle {:?}  lit {:?}  (want lit {:?})",
            &o[..3],
            &i[..3],
            &l[..3],
            &over(c_lit, a)[..3]
        );

        // 0. The compositing law this probe grades against has to be the one the
        //    frame actually obeys, or every assertion below is measuring the
        //    probe. The render with no 2D lighting at all is pure raster
        //    blending, so it is the control: if `over` cannot predict THAT, the
        //    colour space is wrong and the rest means nothing.
        for (c, &got) in o.iter().enumerate().take(3) {
            let want = over(c_off, a)[c];
            assert!(
                (got as i32 - want as i32).abs() <= 2,
                "the probe's own compositing law is wrong: raster-only channel {c} at alpha \
                 {a} is {got} where the law says {want}"
            );
        }

        // 1. No light placed and a white base changes nothing, at every alpha.
        //    This is the claim the module opens with, and it used to hold only
        //    for a = 1.
        for c in 0..3 {
            assert!(
                (i[c] as i32 - o[c] as i32).abs() <= 1,
                "alpha {a}: the idle pass moved channel {c} from {} to {} — a scene that has \
                 placed no lights must be untouched, whatever its sprites' opacity",
                o[c],
                i[c]
            );
        }

        // 2. With a light on it, the surface composites at ITS OWN alpha with
        //    its lit colour: `C·light` over `B` at `a`, not at `1-(1-a)²`.
        for (c, &got) in l.iter().enumerate().take(3) {
            let want = over(c_lit, a)[c];
            assert!(
                (got as i32 - want as i32).abs() <= 3,
                "alpha {a}: channel {c} lit to {got} where `C·light` over the background at \
                 the AUTHORED alpha is {want}. A sprite that reaches the screen at an opacity \
                 its author did not type is a readability budget nobody can spend — that is \
                 the whole of `floptle/0121`."
            );
        }
    }

    // ---- one cell of a spritesheet, lit ------------------------------------
    //
    // The G-buffer samples the albedo texture itself, so it has to sample it
    // through the SAME UV window the colour pass used. A sprite on a sheet is
    // nothing but that window, and when it was missing the deferred pass wrote
    // every cell of the sheet squashed across the one quad — so the delta
    // composite laid a stretched copy of the whole sheet over the sprite while
    // the raster pass had the right frame the entire time. That is why it read
    // as a glitch and not as a wrong animation frame.
    //
    // It has to be checked *lit*, because unlit is the one state where the pass
    // does not run.
    {
        // Four cells, four flat colours, eight pixels each so a linear sampler
        // always has somewhere to land that is not a cell boundary.
        let cells: [[u8; 4]; 4] =
            [[220, 40, 40, 255], [40, 200, 40, 255], [40, 60, 230, 255], [230, 230, 230, 255]];
        let (tw, th) = (32u32, 8u32);
        let pixels: Vec<u8> = (0..th)
            .flat_map(|_| (0..tw).flat_map(|x| cells[(x / 8) as usize]).collect::<Vec<u8>>())
            .collect();
        let sheet_tex = raster.register_texture(
            &gpu,
            &TextureData { pixels, width: tw, height: th },
            TexSampling::default(),
        );
        // One quad, half the frame across, with plain 0..1 UVs — the window has
        // to come from the material, exactly as a `Matter::Sprite` gets it.
        let qd = mesh::tilemap(1, 1, ORTHO_HEIGHT * 0.5, 1, 1, [0.0, 0.0], &[0]);
        let quad = raster.register(&gpu, &qd, None);
        // The REAL window, through the same call the editor's sprite draw makes.
        // Restating the offset convention here would grade the probe's opinion of
        // it rather than the pass.
        let sheet_mat = floptle_core::Material {
            sheet_cols: 4,
            sheet_rows: 1,
            cell: 2,
            unlit: true,
            ..Default::default()
        };
        let mp = MaterialParams::from_material_inset(
            &sheet_mat,
            [1.0 / tw as f32, 1.0 / th as f32],
        );
        let raw = instance_of_mat(Mat4::IDENTITY, &mp);
        let flat =
            [(quad, Some::<TexId>(sheet_tex), Light2dInstance::from_raster(&raw, MAP_RANK, false))];
        let px = shot(&gpu, &mut raster, S, view_proj, quad, sheet_tex, &raw, &flat, Some(&lit));
        let out = format!("{dir}/light2d_sheet_lit.png");
        save_png(&px, S, &out);
        // Four samples across the quad — one in the middle of each stripe the
        // whole sheet would have drawn.
        let row = S / 2;
        let xs = [S * 5 / 16, S * 7 / 16, S * 9 / 16, S * 11 / 16];
        let at = |x: u32| px[(row * S + x) as usize];
        println!("sheet cell 2 lit: {:?} — wrote {out}", xs.map(|x| [at(x)[0], at(x)[1], at(x)[2]]));

        // **The quad is left-right symmetric, and the sheet is not.** One cell
        // is a flat colour and the light is radial about the centre, so two
        // samples the same distance either side of it must agree. The sheet's
        // cells differ from each other, so the moment the deferred pass draws
        // the sheet instead of the cell the pairs come apart — which is the
        // measurement, rather than "is it still bluish", because the composite
        // adds a DIFFERENCE and a wrong cell can still leave blue on top.
        for (l, r) in [(xs[0], xs[3]), (xs[1], xs[2])] {
            let (a, b) = (at(l), at(r));
            for c in 0..3 {
                assert!(
                    (a[c] as i32 - b[c] as i32).abs() <= 4,
                    "the lit pass sampled the whole sheet, not cell 2: channel {c} is {} at \
                     x={l} and {} at x={r}, and those two pixels are the same distance from a \
                     radial light on a quad drawing ONE flat colour",
                    a[c],
                    b[c]
                );
            }
        }
        // …and it is the cell that was asked for, not merely a consistent one.
        for x in xs {
            let p = at(x);
            assert!(
                p[2] > p[0] && p[2] > p[1],
                "cell 2 is the blue one, and x={x} came out {:?}",
                &p[..3]
            );
        }
    }

    println!("2D lighting OK");
}

/// Draw the map once at `s`×`s`, with the 2D lighting pass over it when there
/// are lights, and read the result back.
#[allow(clippy::too_many_arguments)]
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    s: u32,
    view_proj: glam::Mat4,
    map: floptle_render::MeshId,
    tex: TexId,
    raw: &floptle_render::InstanceRaw,
    flat: &[(floptle_render::MeshId, Option<TexId>, Light2dInstance)],
    lights: Option<&Light2dUniform>,
) -> Vec<[u8; 4]> {
    let size = wgpu::Extent3d { width: s, height: s, depth_or_array_layers: 1 };
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
    let color = make("light2d-color", gpu.surface_format(), wgpu::TextureUsages::COPY_SRC);
    // Its own depth, because every attachment in a pass must be the same size.
    let depth = make("light2d-probe-depth", Gpu::DEPTH_FORMAT, wgpu::TextureUsages::empty());
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let dview = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(
        gpu,
        &view,
        &dview,
        globals,
        &[(map, Some::<TexId>(tex), *raw)],
        Some([0.02, 0.02, 0.04, 1.0]),
        None,
    );
    if let Some(l) = lights {
        raster.light2d_pass(gpu, &view, &dview, (s, s), view_proj.to_cols_array_2d(), l, flat);
    }
    readback(gpu, &color, s)
}

/// The clear colour every shot is drawn over — the `B` in `C over B`. Linear,
/// because a wgpu clear value always is.
const BG: [f32; 3] = [0.02, 0.02, 0.04];

/// `c` composited over the background at alpha `a`, in the frame's own colour
/// space.
///
/// Blending happens in LINEAR space even on an sRGB target (the hardware decodes
/// the destination, blends, re-encodes), so the mix has to be done there too —
/// doing it on the stored bytes would predict a different number and quietly
/// grade the renderer against the wrong law.
fn over(c: [u8; 4], a: f32) -> [u8; 4] {
    let mut out = [255u8; 4];
    for i in 0..3 {
        let lin = a * srgb_to_linear(c[i] as f32 / 255.0) + (1.0 - a) * BG[i];
        out[i] = (linear_to_srgb(lin) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    out
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// Perceptual-ish brightness of one pixel, 0..1.
fn luma(px: &[[u8; 4]], s: u32, x: u32, y: u32) -> f32 {
    let p = px[(y * s + x) as usize];
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture, s: u32) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (s * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * s) as u64,
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
                rows_per_image: Some(s),
            },
        },
        wgpu::Extent3d { width: s, height: s, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut o = Vec::with_capacity((s * s) as usize);
    for y in 0..s {
        let row = (y * padded) as usize;
        for x in 0..s {
            let i = row + (x * bpp) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            o.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    o
}

fn save_png(px: &[[u8; 4]], s: u32, path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), s, s);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
