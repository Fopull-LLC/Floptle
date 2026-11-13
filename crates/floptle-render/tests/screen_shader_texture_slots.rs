//! **A screen shader's texture slots are bound once and read every frame.**
//!
//! A `stage post` pass that declares `texture pattern` samples the image the
//! pass list hands it; a slot handed nothing reads white. Setting the same
//! list again (what the editor does every frame) rebuilds nothing, a new
//! image in a slot rebuilds that pass's bindings once, and the sample costs
//! the pass next to nothing at 1440p.
//!
//! Set `FLOPTLE_KEEP_SHOTS=<dir>` to keep the frames as PNGs.

use floptle_render::post::{PostSettings, PostStack};
use floptle_render::{Gpu, PostPass, PostShaders, TexSampling, TextureData};

const SIZE: u32 = 64;

fn module(src: &str) -> (floptle_shader::CompiledPost, String) {
    let compiled = floptle_shader::compile_post(src).expect("the screen shader compiles");
    let prelude = format!("{}\n{}", floptle_shader::transpile::POST_PRELUDE, floptle_shader::transpile::POST_FIELD_SHIM);
    floptle_shader::validate(&prelude, &compiled.chunk).unwrap_or_else(|e| panic!("naga rejects it: {}", e.message));
    let text = format!("{prelude}\n{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
    (compiled, text)
}

fn target(gpu: &Gpu, w: u32, h: u32) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("screen-shader-out"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// A 2×2 picture, one flat colour a quarter.
fn quad(texels: [[u8; 3]; 4]) -> TextureData {
    TextureData { pixels: texels.iter().flat_map(|t| [t[0], t[1], t[2], 255]).collect(), width: 2, height: 2 }
}

fn keep(gpu: &Gpu, tex: &wgpu::Texture, name: &str) {
    if let Ok(dir) = std::env::var("FLOPTLE_KEEP_SHOTS") {
        std::fs::create_dir_all(&dir).ok();
        floptle_render::probe::save_png(gpu, tex, &format!("{dir}/{name}.png"));
    }
}

fn near(p: [u8; 4], want: [u8; 3]) -> bool {
    (0..3).all(|c| (p[c] as i32 - want[c] as i32).abs() < 40)
}

#[test]
fn a_screen_shader_reads_the_image_in_its_slot_and_rebinds_only_when_it_changes() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so no texture can be registered");
        return;
    }
    let (compiled, text) = module(
        "shader show {\n  stage post\n  texture pattern\n  uniform gain: float = 1\n  \
         output color = sample(pattern, uv).rgb * gain + sceneColor().rgb * 0\n}\n",
    );
    PostShaders::fits(&gpu, compiled.textures.len()).expect("one slot fits any device");
    let mut shaders = PostShaders::new(&gpu);
    let id = shaders.register_textured(&gpu, &text, compiled.textures.len(), None);
    let post = PostStack::new(&gpu, SIZE, SIZE);
    let out = target(&gpu, SIZE, SIZE);
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let mut raster = floptle_render::Raster::new(&gpu);
    let (red, green, blue, white) = ([255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]);
    let a = raster.register_texture(&gpu, &quad([red, green, blue, white]), TexSampling::default());
    let b = raster.register_texture(&gpu, &quad([white, blue, green, red]), TexSampling::default());

    let params = compiled.pack_params(&|_| None);
    let mut shot = |shaders: &mut PostShaders, tex: Option<floptle_render::TexId>, name: &str| {
        let textures = vec![tex.and_then(|t| raster.texture_binding(&gpu, t))];
        shaders.set_pass_list(&gpu, &[PostPass { shader: id, params: params.clone(), textures }]);
        post.run_with(&gpu, &PostSettings::default(), None, &view, Some(shaders));
        keep(&gpu, &out, name);
        floptle_render::probe::readback(&gpu, &out)
    };
    let quarters = [(16, 16), (48, 16), (16, 48), (48, 48)];
    let at = |px: &[[u8; 4]], (x, y): (usize, usize)| px[y * SIZE as usize + x];

    // Nothing in the slot: white, never whatever the last pass left bound.
    let px = shot(&mut shaders, None, "slot_empty");
    assert!(quarters.iter().all(|q| near(at(&px, *q), white)), "an empty slot is not white: {:?}", at(&px, quarters[0]));
    assert_eq!(shaders.binds_built(), 1);

    let px = shot(&mut shaders, Some(a), "slot_a");
    for (q, want) in quarters.iter().zip([red, green, blue, white]) {
        assert!(near(at(&px, *q), want), "the slot's image is not on screen at {q:?}: {:?}, wanted {want:?}", at(&px, *q));
    }
    assert_eq!(shaders.binds_built(), 2, "a new image in the slot is one rebuild");

    // The same list again, as every frame sets it: nothing is rebuilt.
    for _ in 0..5 {
        shot(&mut shaders, Some(a), "slot_a");
    }
    assert_eq!(shaders.binds_built(), 2, "an unchanged pass list rebuilt its bindings");

    let px = shot(&mut shaders, Some(b), "slot_b");
    assert!(near(at(&px, quarters[0]), white) && near(at(&px, quarters[3]), red), "the slot kept its old image");
    assert_eq!(shaders.binds_built(), 3);

    // The old entry point still runs a textured shader: its slots read white.
    shaders.set_passes(&gpu, &[(id, params.clone())]);
    post.run_with(&gpu, &PostSettings::default(), None, &view, Some(&shaders));
    let px = floptle_render::probe::readback(&gpu, &out);
    assert!(near(at(&px, quarters[0]), white), "set_passes left a slot unbound: {:?}", at(&px, quarters[0]));
}

/// **A texture read in a screen pass is nearly free.** The same pass with and
/// without one sample of a 256² image, at 2560×1440, on the GPU's own clock.
/// Compared with each other in one run, so the machine's speed cancels.
#[test]
fn sampling_a_texture_costs_a_screen_pass_next_to_nothing() {
    const W: u32 = 2560;
    const H: u32 = 1440;
    let gpu = Gpu::headless(W, H);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so no texture can be registered");
        return;
    }
    let Some(mut timer) = floptle_render::GpuTimer::new(&gpu) else {
        eprintln!("skipped — this GPU reports no timestamps");
        return;
    };
    let (plain_c, plain_text) = module(
        "shader plain {\n  stage post\n  output color = sceneColor().rgb * vec3(0.9, 1.0, 1.1)\n}\n",
    );
    let (tex_c, tex_text) = module(
        "shader textured {\n  stage post\n  texture pattern\n  \
         output color = sceneColor().rgb * vec3(0.9, 1.0, 1.1) * sample(pattern, uv * 8).rgb\n}\n",
    );
    let mut shaders = PostShaders::new(&gpu);
    let plain = shaders.register(&gpu, &plain_text, None);
    let textured = shaders.register_textured(&gpu, &tex_text, 1, None);
    let mut raster = floptle_render::Raster::new(&gpu);
    let noise: Vec<u8> = (0..256u32 * 256 * 4).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    let image = raster.register_texture(&gpu, &TextureData { pixels: noise, width: 256, height: 256 }, TexSampling::default());
    let binding = raster.texture_binding(&gpu, image);

    let post = PostStack::new(&gpu, W, H);
    let out = target(&gpu, W, H);
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let mut cost = |shaders: &mut PostShaders, pass: PostPass| -> f32 {
        shaders.set_pass_list(&gpu, &[pass]);
        let mut best = f32::MAX;
        for _ in 0..12 {
            if !timer.begin() {
                continue;
            }
            post.run_timed(&gpu, &PostSettings::default(), None, &view, Some(shaders), None, Some(&mut timer));
            timer.end(&gpu);
            for _ in 0..200 {
                gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok();
                timer.poll();
                if timer.begin() {
                    break;
                }
            }
            if let Some(s) = timer.spans().iter().find(|s| s.label == "post: screen shaders") {
                best = best.min(s.ms);
            }
        }
        best
    };
    let without = cost(&mut shaders, PostPass { shader: plain, params: plain_c.pack_params(&|_| None), textures: Vec::new() });
    let with = cost(&mut shaders, PostPass { shader: textured, params: tex_c.pack_params(&|_| None), textures: vec![binding] });
    eprintln!("screen pass at {W}x{H}: {without:.3} ms plain, {with:.3} ms with one 256² sample (+{:.3} ms)", with - without);
    if without == f32::MAX || with == f32::MAX {
        eprintln!("skipped — no timing came back");
        return;
    }
    assert!(
        with <= without * 2.0 + 0.02,
        "one texture sample more than doubled a full-screen pass: {without:.3} ms → {with:.3} ms"
    );
}
