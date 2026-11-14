//! **A screen shader sees the sky the frame captured.**
//!
//! `skyRadiance` in a `stage post` pass reads the environment map the chain
//! is handed (`SsaoFrame::sky`), the same one a surface's reflection reads:
//! a sky of one colour comes back as that colour, and a frame with no sky
//! captured reads black, as a surface's reflection does.

use floptle_render::post::{PostSettings, PostStack};
use floptle_render::{Gpu, PostPass, PostShaders, SsaoFrame};

const SIZE: u32 = 16;

fn target(gpu: &Gpu) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("screen-sky-out"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// An 8 × 4 sky of one colour, as an equirectangular map is laid out.
fn sky(gpu: &Gpu, rgb: [u8; 3]) -> wgpu::TextureView {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("screen-sky"),
        size: wgpu::Extent3d { width: 8, height: 4, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        tex.as_image_copy(),
        &[rgb[0], rgb[1], rgb[2], 255].repeat(32),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(32), rows_per_image: Some(4) },
        wgpu::Extent3d { width: 8, height: 4, depth_or_array_layers: 1 },
    );
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

#[test]
fn a_screen_shader_reads_the_sky_it_is_handed_and_black_without_one() {
    let gpu = Gpu::headless(SIZE, SIZE);
    let module = |src: &str| {
        let compiled = floptle_shader::compile_post(src).expect("the screen shader compiles");
        let prelude = format!("{}\n{}", floptle_shader::transpile::POST_PRELUDE, floptle_shader::transpile::POST_FIELD_SHIM);
        (compiled.pack_params(&|_| None), format!("{prelude}\n{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk))
    };
    let mut shaders = PostShaders::new(&gpu);
    let (sky_params, sky_text) = module("shader s {\n  stage post\n  output color = skyRadiance(sceneRay()) + sceneColor().rgb * 0\n}\n");
    let (ref_params, ref_text) = module("shader r {\n  stage post\n  output color = vec3(0.2, 0.6, 0.9) + sceneColor().rgb * 0\n}\n");
    let sky_pass = shaders.register(&gpu, &sky_text, None);
    let ref_pass = shaders.register(&gpu, &ref_text, None);
    let post = PostStack::new(&gpu, SIZE, SIZE);
    let out = target(&gpu);
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let blue = sky(&gpu, [51, 153, 230]);
    let identity = glam::Mat4::IDENTITY.to_cols_array_2d();

    let mut run = |pass, params: &[u8], sky: Option<&wgpu::TextureView>| {
        let frame = SsaoFrame {
            depth: gpu.depth_view(),
            proj: identity,
            inv_proj: identity,
            fog: [0.0; 4],
            inv_view: identity,
            eye: [0.0; 3],
            water_depth: 0.0,
            sky,
        };
        shaders.set_pass_list(&gpu, &[PostPass { shader: pass, params: params.to_vec(), textures: Vec::new() }]);
        post.run_with(&gpu, &PostSettings::default(), Some(&frame), &view, Some(&shaders));
        let px = floptle_render::probe::readback(&gpu, &out);
        px[(SIZE as usize / 2) * SIZE as usize + SIZE as usize / 2]
    };
    let want = run(ref_pass, &ref_params, None);
    let got = run(sky_pass, &sky_params, Some(&blue));
    assert!(
        (0..3).all(|c| (got[c] as i32 - want[c] as i32).abs() <= 6),
        "the screen shader's sky is {got:?}, the sky handed to it draws {want:?}"
    );
    let none = run(sky_pass, &sky_params, None);
    assert!(none[..3].iter().all(|&c| c < 8), "with no sky captured, skyRadiance read {none:?}");
}
