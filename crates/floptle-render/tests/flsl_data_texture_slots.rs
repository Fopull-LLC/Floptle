//! **A texture slot declared `data` reads its bytes as stored.**
//!
//! A flow map, a normal map or a mask holds numbers, not colours. Read
//! through the sRGB view a picture takes, a stored 128 comes back as about
//! 0.216; a slot declared `texture flow = "..." data` reads it as about
//! 0.502. Both a material shader and a `stage post` screen shader are
//! checked: each paints red where its sample is near 0.502 and green where
//! it is near 0.216, so the colour on screen says which view was bound.

use floptle_render::post::{PostSettings, PostStack};
use floptle_render::{
    instance_of_mat, pass_prelude, FlslBlend, Globals, Gpu, MaterialParams, MeshData, PostPass, PostShaders, Raster,
    TexSampling, TextureData, Vertex,
};
use glam::Mat4;

const SIZE: u32 = 32;

/// Red for a sample near 0.502, green for one near 0.216, black otherwise.
const VERDICT: &str = "vec3(step(0.45, v) * step(v, 0.55), step(0.18, v) * step(v, 0.25), 0)";

fn grey() -> TextureData {
    TextureData { pixels: [128, 128, 128, 255].repeat(4), width: 2, height: 2 }
}

fn target(gpu: &Gpu) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("data-slot-out"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn verdict(px: &[[u8; 4]]) -> &'static str {
    let p = px[(SIZE as usize / 2) * SIZE as usize + SIZE as usize / 2];
    match (p[0] > 150, p[1] > 150) {
        (true, false) => "data",
        (false, true) => "srgb",
        _ => "neither",
    }
}

#[test]
fn a_data_slot_reads_128_as_half_and_a_picture_slot_as_srgb() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so no texture can be registered");
        return;
    }
    let out = target(&gpu);
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let mut raster = Raster::new(&gpu);
    let tex = raster.register_texture(&gpu, &grey(), TexSampling::default());

    // ---- a material shader on a quad that fills the frame ----
    let quad = MeshData {
        vertices: [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]]
            .iter()
            .map(|p| Vertex { pos: [p[0], p[1], 0.5], normal: [0.0, 0.0, 1.0], uv: [0.5, 0.5] })
            .collect(),
        indices: vec![0, 1, 2, 1, 3, 2],
        colors: None,
    };
    let mesh = raster.register(&gpu, &quad, None);
    let mut material = |data: bool| {
        let decl = if data { "texture flow data" } else { "texture flow" };
        let src = format!("shader s {{\n  stage fragment\n  {decl}\n  let v = sample(flow, vec2(0.5, 0.5)).r\n  output color = {VERDICT}\n}}\n");
        let compiled = floptle_shader::compile_fragment(&src).expect("compiles");
        assert_eq!(compiled.texture_data, [data]);
        floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}", e.message));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let id = raster.register_flsl_shader(&gpu, &chunk, 1, FlslBlend::Opaque, None);
        let params = compiled.pack_params(&|_| None, &|_| None);
        let bind = raster.set_flsl_binding_data(&gpu, None, id, &params, &[Some(tex)], &compiled.texture_data);
        let globals = Globals { view_proj: Mat4::IDENTITY.to_cols_array_2d(), ambient: [1.0, 1.0, 1.0, 0.0], ..Default::default() };
        let mut m = MaterialParams::flat([1.0; 3]);
        m.unlit = true;
        let draws = [(mesh, None, bind, instance_of_mat(Mat4::IDENTITY, &m))];
        raster.draw_scene_with(&gpu, &view, gpu.depth_view(), globals, &[], &draws, &[], Some([0.0, 0.0, 0.0, 1.0]), None);
        verdict(&floptle_render::probe::readback(&gpu, &out))
    };
    assert_eq!(material(false), "srgb", "a picture slot read 128 as something other than about 0.216");
    assert_eq!(material(true), "data", "a `data` slot read 128 as something other than about 0.502");

    // ---- a screen shader ----
    let post = PostStack::new(&gpu, SIZE, SIZE);
    let mut shaders = PostShaders::new(&gpu);
    let mut screen = |keyword: &str| {
        let src = format!(
            "shader p {{\n  stage post\n  texture flow{keyword}\n  let v = sample(flow, uv).r\n  \
             output color = {VERDICT} + sceneColor().rgb * 0\n}}\n"
        );
        let compiled = floptle_shader::compile_post(&src).expect("the screen shader compiles");
        let prelude = format!("{}\n{}", floptle_shader::transpile::POST_PRELUDE, floptle_shader::transpile::POST_FIELD_SHIM);
        let text = format!("{prelude}\n{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let id = shaders.register_textured(&gpu, &text, 1, None);
        let textures = vec![raster.texture_binding_data(&gpu, tex, compiled.texture_data[0])];
        shaders.set_pass_list(&gpu, &[PostPass { shader: id, params: compiled.pack_params(&|_| None), textures }]);
        post.run_with(&gpu, &PostSettings::default(), None, &view, Some(&shaders));
        verdict(&floptle_render::probe::readback(&gpu, &out))
    };
    assert_eq!(screen(""), "srgb");
    assert_eq!(screen(" data"), "data", "a screen shader's `data` slot was read through sRGB");
    assert_eq!(screen(" linear"), "data", "`linear` is the same as `data`");
}
