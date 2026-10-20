//! **A material shader can use its texture slots.**
//!
//! The engine binds sixteen sampled textures to every surface's fragment stage
//! for its own lighting, and the device kept wgpu's default limit of sixteen,
//! so a `.flsl` material with even one `texture` slot failed pipeline
//! validation. The device is now asked for what the adapter offers, and the
//! engine says how many slots are left rather than failing to build.

use floptle_render::{pass_prelude, FlslBlend, Gpu, Raster, FLSL_ENGINE_SAMPLERS, FLSL_ENGINE_TEXTURES};

/// A fragment shader with `n` texture slots that also reads the depth behind
/// it and the distance to the field: every built-in input that binds a texture.
fn shader(n: usize) -> String {
    let names: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
    let decl: String = names.iter().map(|t| format!("  texture {t}\n")).collect();
    let sum: String = names.iter().map(|t| format!(" + sample({t}, uv).rgb")).collect();
    format!(
        "shader slots {{\n  stage fragment\n{decl}  let gap = surfaceGap(worldPos) + fieldDistance(worldPos)\n  \
         output color = vec4(vec3(gap * 0.0){sum}, 1)\n}}\n"
    )
}

/// Build `n` slots on `gpu`; the validation error, if any.
fn build(gpu: &Gpu, n: usize) -> Option<String> {
    let compiled = floptle_shader::compile_fragment(&shader(n)).expect("compiles");
    floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}", e.message));
    assert_eq!(compiled.textures.len(), n);
    let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
    let mut raster = Raster::new(gpu);
    let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
    raster.register_flsl_shader(gpu, &chunk, n, FlslBlend::Alpha, None);
    pollster::block_on(scope.pop()).map(|e| e.to_string())
}

fn can_raster(gpu: &Gpu) -> bool {
    gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS)
}

/// Eight slots, the most FLSL allows, beside every built-in input.
#[test]
fn eight_texture_slots_build_beside_every_built_in_input() {
    let gpu = Gpu::headless(8, 8);
    let offered = gpu.adapter.limits();
    if !can_raster(&gpu)
        || offered.max_sampled_textures_per_shader_stage < FLSL_ENGINE_TEXTURES + 8
        || offered.max_samplers_per_shader_stage < FLSL_ENGINE_SAMPLERS + 8
    {
        return; // an adapter too small to offer them: the next tests cover what it says
    }
    // The adapter has them, so the device must have been asked for them.
    assert!(Raster::flsl_texture_slots(&gpu) >= 8, "the device was not asked for the adapter's textures");
    assert_eq!(build(&gpu, 8), None);
    assert!(Raster::flsl_fits(&gpu, 8).is_ok());
}

/// **The count the engine reports is the count the layouts use.** On a device
/// asked for exactly the engine's textures plus three, three slots build and
/// four are refused by name, before any pipeline is made. The samplers have
/// room to spare there, so this stands at the texture limit; the next test
/// stands at the sampler limit.
#[test]
fn the_slots_left_are_exactly_what_builds() {
    let gpu = Gpu::headless_with_stage_limits(8, 8, FLSL_ENGINE_TEXTURES + 3, 32);
    let l = gpu.device.limits();
    if !can_raster(&gpu) || l.max_sampled_textures_per_shader_stage != FLSL_ENGINE_TEXTURES + 3 || l.max_samplers_per_shader_stage < 32 {
        return; // the adapter offers fewer than this test asks for
    }
    assert_eq!(Raster::flsl_texture_slots(&gpu), 3);
    assert_eq!(build(&gpu, 3), None, "three slots must build");
    assert!(build(&gpu, 4).is_some_and(|e| e.contains("SampledTextures")), "four slots must be over the limit");
    let why = Raster::flsl_fits(&gpu, 4).unwrap_err();
    assert!(why.contains("uses 4 texture slot(s)") && why.contains("allows 3"), "{why}");
}

/// The same at the sampler limit: the engine's samplers plus two leave two
/// slots, however many textures the device would take.
#[test]
fn the_samplers_left_are_exactly_what_builds() {
    let gpu = Gpu::headless_with_stage_limits(8, 8, 32, FLSL_ENGINE_SAMPLERS + 2);
    let l = gpu.device.limits();
    if !can_raster(&gpu) || l.max_samplers_per_shader_stage != FLSL_ENGINE_SAMPLERS + 2 || l.max_sampled_textures_per_shader_stage < 32 {
        return;
    }
    assert_eq!(Raster::flsl_texture_slots(&gpu), 2);
    assert_eq!(build(&gpu, 2), None, "two slots must build");
    assert!(build(&gpu, 3).is_some_and(|e| e.contains("Samplers")), "three slots must be over the limit");
}
