//! Post-shader probe: an authored `stage post` pass really runs over the
//! finished frame, and the built-in ink outline draws where an outline belongs.
//!
//! This is a CONTROL-based measurement, not a screenshot to squint at. The same
//! scene is rendered twice — once through the plain chain, once with
//! `inkOutline.flsl` in it — and the difference is compared against three
//! regions worked out from the control image itself:
//!
//!   * **the silhouette** — every pixel next to a colour boundary in the
//!     control. The outline MUST darken these; that is the whole feature.
//!   * **the sphere's interior** — curved, and the classic false positive: a
//!     naive depth-DIFFERENCE detector inks a sphere solid near its rim, because
//!     depth changes fastest exactly where the surface turns away.
//!   * **a steeply raked wall** — flat, but receding hard across the screen, so
//!     its depth changes faster per pixel than the sphere's silhouette step
//!     does. It must stay clean. This is what the second-derivative ("bend, not
//!     difference") measure in the shader buys, and nothing else here proves it.
//!
//! Run: cargo run -p floptle-render --example post_shader_probe -- <out-dir>

use floptle_render::{
    instance_of_mat, plane, uv_sphere, Globals, Gpu, MaterialParams, PostSettings, PostShaders,
    PostStack, Projection, Raster, RenderCamera, SsaoFrame,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 256;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);

    // The shader under test is the SHIPPED example, compiled through the
    // production path — so this probe fails if the file people are told to use
    // stops working, not merely if some probe-local copy of it does.
    let src = floptle_shader::examples::EXAMPLES
        .iter()
        .find(|(n, _)| *n == "inkOutline.flsl")
        .expect("the ink outline example is shipped")
        .1;
    let compiled = floptle_shader::compile_post(src).expect("inkOutline compiles");
    let prelude = format!(
        "{}\n{}",
        floptle_shader::transpile::POST_PRELUDE,
        floptle_shader::transpile::POST_FIELD_SHIM
    );
    floptle_shader::validate(&prelude, &compiled.chunk)
        .unwrap_or_else(|e| panic!("naga rejects the ink outline: {}", e.message));
    let module = format!("{prelude}\n{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);

    let mut shaders = PostShaders::new(&gpu);
    let id = shaders.register(&gpu, &module, None);

    let mut raster = Raster::new(&gpu);
    let post = PostStack::new(&gpu, S, S);
    let sphere = raster.register(&gpu, &uv_sphere(1.0, 32, 48), None);
    let wall = raster.register(&gpu, &plane(40.0), None);

    let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("post-shader-out"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());

    // Unlit flat colours, so the CONTROL image's only colour boundaries are the
    // real silhouettes — no shading gradient to confuse the region finder with.
    let flat = |c: [f32; 3]| {
        let mut m = MaterialParams::flat(c);
        m.unlit = true;
        m
    };
    let cam = RenderCamera::new(
        Vec3::ZERO.as_dvec3(),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 0.9, near: 0.05, far: 200.0 },
    );
    let proj = cam.proj_matrix(1.0);
    let globals =
        Globals { view_proj: cam.view_proj(1.0).to_cols_array_2d(), ..Default::default() };

    // The wall is turned 50° about Y: at 12 units out and 80 across it fills the
    // frame and its depth runs from a few units to dozens, left to right.
    let wall_xf = Mat4::from_rotation_translation(
        Quat::from_rotation_y(0.87),
        Vec3::new(0.0, 0.0, -12.0),
    );
    let draws = [
        (wall, None, instance_of_mat(wall_xf, &flat([0.30, 0.32, 0.36]))),
        (
            sphere,
            None,
            instance_of_mat(Mat4::from_translation(Vec3::new(0.0, 0.0, -4.0)), &flat([0.80, 0.78, 0.72])),
        ),
    ];

    let settings = PostSettings::default();
    let mut render = |shaders: Option<&PostShaders>| -> Vec<[u8; 3]> {
        raster.draw_scene(
            &gpu,
            post.input_view(),
            gpu.depth_view(),
            globals,
            &draws,
            Some([0.02, 0.02, 0.03, 1.0]),
            None,
        );
        let ssao = SsaoFrame {
            depth: gpu.depth_view(),
            proj: proj.to_cols_array_2d(),
            inv_proj: proj.inverse().to_cols_array_2d(),
        };
        post.run_with(&gpu, &settings, Some(&ssao), &out_view, shaders);
        read_rgb(&gpu, &out)
    };

    let control = render(None);
    shaders.set_passes(&gpu, &[(id, compiled.pack_params(&|_| None))]);
    let inked = render(Some(&shaders));

    write_png(&format!("{dir}/post_shader_control.png"), &control);
    write_png(&format!("{dir}/post_shader_inked.png"), &inked);

    // ---- regions, all derived from the control ------------------------------
    let at = |v: &[[u8; 3]], x: usize, y: usize| v[y * S as usize + x];
    let luma = |c: [u8; 3]| 0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32;
    let n = (S * S) as usize;

    // A colour boundary in the control: one of the four neighbours is a
    // different surface. With flat unlit colours that is exactly a silhouette.
    let mut boundary = vec![false; n];
    for y in 1..S as usize - 1 {
        for x in 1..S as usize - 1 {
            let c = at(&control, x, y);
            let diff = |o: [u8; 3]| {
                (0..3).any(|k| (c[k] as i32 - o[k] as i32).abs() > 30)
            };
            if diff(at(&control, x + 1, y))
                || diff(at(&control, x - 1, y))
                || diff(at(&control, x, y + 1))
                || diff(at(&control, x, y - 1))
            {
                boundary[y * S as usize + x] = true;
            }
        }
    }
    let near = |r: i32, x: usize, y: usize| -> bool {
        for dy in -r..=r {
            for dx in -r..=r {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx >= 0
                    && ny >= 0
                    && (nx as u32) < S
                    && (ny as u32) < S
                    && boundary[ny as usize * S as usize + nx as usize]
                {
                    return true;
                }
            }
        }
        false
    };
    let darkened =
        |x: usize, y: usize| luma(at(&control, x, y)) - luma(at(&inked, x, y)) > 25.0;

    // The sphere is the light colour; the wall the dark one. Telling them apart
    // by colour rather than by re-deriving the projection keeps the regions
    // honest about what was actually drawn.
    let is_sphere = |x: usize, y: usize| at(&control, x, y)[0] > 150;

    // COVERAGE, not a pixel-for-pixel match: the question is whether every part
    // of the silhouette got a line, and a 1-pixel line laid along a boundary is
    // only ever a fraction of the pixels adjacent to it. Asking "did each
    // boundary pixel get inked" would fail a correct thin line for being thin.
    let mut edge_n = 0usize;
    let mut edge_covered = 0usize;
    let (mut ball_n, mut ball_dark) = (0usize, 0usize);
    let (mut wall_n, mut wall_dark) = (0usize, 0usize);
    for y in 4..S as usize - 4 {
        for x in 4..S as usize - 4 {
            if boundary[y * S as usize + x] {
                edge_n += 1;
                let hit = (-2i32..=2).any(|dy| {
                    (-2i32..=2).any(|dx| {
                        darkened((x as i32 + dx) as usize, (y as i32 + dy) as usize)
                    })
                });
                edge_covered += usize::from(hit);
            } else if !near(5, x, y) {
                if is_sphere(x, y) {
                    ball_n += 1;
                    ball_dark += usize::from(darkened(x, y));
                } else {
                    wall_n += 1;
                    wall_dark += usize::from(darkened(x, y));
                }
            }
        }
    }
    let pct = |a: usize, b: usize| if b == 0 { 0.0 } else { a as f32 / b as f32 };
    println!(
        "silhouette {}/{} covered ({:.0}%)  sphere interior {}/{} ({:.1}%)  raked wall {}/{} ({:.1}%)",
        edge_covered,
        edge_n,
        pct(edge_covered, edge_n) * 100.0,
        ball_dark,
        ball_n,
        pct(ball_dark, ball_n) * 100.0,
        wall_dark,
        wall_n,
        pct(wall_dark, wall_n) * 100.0
    );

    assert!(edge_n > 400, "the scene must actually have silhouettes to find, got {edge_n} px");
    assert!(ball_n > 1000 && wall_n > 1000, "both flat regions must be sampled");
    assert!(
        pct(edge_covered, edge_n) > 0.9,
        "the ink outline must draw ON the silhouettes — only {:.0}% of {edge_n} boundary pixels \
         have a line within two pixels. If this is near zero the authored pass never ran at all: \
         check that `set_passes` was called and that the chain is not taking its \
         nothing-is-on shortcut.",
        pct(edge_covered, edge_n) * 100.0
    );
    assert!(
        pct(ball_dark, ball_n) < 0.03,
        "the ink must NOT fill in a curved surface — {:.1}% of the sphere's interior darkened. \
         A sphere's depth changes fastest just inside its own rim, so a detector that thresholds \
         a depth DIFFERENCE paints a fat black band there and the object reads as burnt.",
        pct(ball_dark, ball_n) * 100.0
    );
    assert!(
        pct(wall_dark, wall_n) < 0.03,
        "the ink must NOT draw on a flat surface raked away from the camera — {:.1}% of the \
         tilted wall darkened. This wall's depth changes FASTER per pixel than the sphere's \
         silhouette step does, so anything measuring a plain difference inks the whole floor of \
         every scene. The shader measures the BEND (`dl + dr - 2*d0`), which is zero on a plane \
         at any angle.",
        pct(wall_dark, wall_n) * 100.0
    );

    // ---- two passes, in order ----------------------------------------------
    //
    // Stacking is the point of a LIST, and the cheapest way to be sure the
    // second pass really reads the first one's output (rather than the scene
    // again) is a pass whose signature is unmistakable: scan lines put a hard
    // row-to-row ripple into a flat wall that no single-pass frame has.
    let crt = floptle_shader::examples::EXAMPLES
        .iter()
        .find(|(n, _)| *n == "crtScanlines.flsl")
        .expect("the scanlines example is shipped")
        .1;
    let crt = floptle_shader::compile_post(crt).expect("crtScanlines compiles");
    let crt_id = shaders.register(
        &gpu,
        &format!("{prelude}\n{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, crt.chunk),
        None,
    );
    shaders.set_passes(
        &gpu,
        &[(id, compiled.pack_params(&|_| None)), (crt_id, crt.pack_params(&|_| None))],
    );
    let both = render(Some(&shaders));
    write_png(&format!("{dir}/post_shader_stacked.png"), &both);

    // Mean absolute row-to-row change down one column of wall, which is what a
    // scan line is and what neither the control nor the outline has.
    let ripple = |v: &Vec<[u8; 3]>| -> f32 {
        let x = 12usize;
        let mut sum = 0.0;
        for y in 21..S as usize - 20 {
            sum += (luma(at(v, x, y)) - luma(at(v, x, y - 1))).abs();
        }
        sum / (S as f32 - 41.0)
    };
    println!(
        "row ripple on the wall: control {:.2}  outline {:.2}  outline+scanlines {:.2}",
        ripple(&control),
        ripple(&inked),
        ripple(&both)
    );
    assert!(
        ripple(&both) > ripple(&inked) + 2.0,
        "a second pass must run over the FIRST one's output — the scan lines put no ripple \
         into the frame ({:.2} against the outline-only {:.2}). Equal numbers mean the list \
         ran one pass, or ran both against the same source and threw one away.",
        ripple(&both),
        ripple(&inked)
    );

    println!("post shader probe OK");
}

fn read_rgb(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 3]> {
    let bpp = 4u32;
    let padded =
        (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("post-shader-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("post-shader-readback") });
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
    let mut out = Vec::with_capacity((S * S) as usize);
    for y in 0..S as usize {
        for x in 0..S as usize {
            let i = y * padded as usize + x * bpp as usize;
            out.push(if bgra {
                [view[i + 2], view[i + 1], view[i]]
            } else {
                [view[i], view[i + 1], view[i + 2]]
            });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn write_png(path: &str, rgb: &[[u8; 3]]) {
    let flat: Vec<u8> = rgb.iter().flat_map(|c| c.iter().copied()).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("png header").write_image_data(&flat).expect("png data");
}
