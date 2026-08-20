//! **Every shader has to be expressible on OpenGL, and this asks without a GPU.**
//!
//! The engine runs on Vulkan wherever anybody develops it, so OpenGL's extra
//! rules are invisible here — and GitHub's runners have no Vulkan driver, so
//! they fall back to it. That gap burned a release tag: the raster pipeline
//! could not be built at all on the GL backend, because naga's GLSL output has
//! to pair each image with exactly one sampler (GLSL's `sampler2D` is the
//! combination of the two), and the terrain palette was sampled by a filtering
//! sampler and a nearest one. Nothing local could see it, and the only thing
//! that could was a release gate.
//!
//! This runs naga's GLSL backend over the same sources the pipelines are built
//! from. **No device, no adapter, no window** — the check is about what can be
//! written, not what can be run, so it works on a machine that could never
//! select the backend it is checking.

use std::path::{Path, PathBuf};

/// The shader directory, from the crate root rather than the working directory.
fn shader_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The modules as the pipelines actually assemble them.
///
/// `field.wgsl` is not a module: it is spliced into two of these, and asking
/// naga to compile it alone would be checking something the engine never
/// builds. That is why this is a list rather than a directory walk — and why
/// `every_shader_file_is_accounted_for` exists to stop the list going stale.
fn modules() -> Vec<(&'static str, String)> {
    let one = |name: &str| {
        std::fs::read_to_string(shader_dir().join(name))
            .unwrap_or_else(|e| panic!("read {name}: {e}"))
    };
    vec![
        ("raster.wgsl + field.wgsl", floptle_render::raster::pass_prelude().to_string()),
        ("raymarch.wgsl + field.wgsl", floptle_render::raymarch::prelude().to_string()),
        ("grid.wgsl", one("grid.wgsl")),
        ("light2d.wgsl", one("light2d.wgsl")),
        ("outline.wgsl", one("outline.wgsl")),
        ("palette.wgsl", one("palette.wgsl")),
        ("particles.wgsl", one("particles.wgsl")),
        ("post.wgsl", one("post.wgsl")),
        ("retro.wgsl", one("retro.wgsl")),
        ("ssao.wgsl", one("ssao.wgsl")),
        ("ui.wgsl", one("ui.wgsl")),
    ]
}

/// Write one module's every entry point as GLSL, returning the first refusal.
fn glsl_refusal(source: &str) -> Option<String> {
    let module = match naga::front::wgsl::parse_str(source) {
        Ok(m) => m,
        Err(e) => return Some(format!("this is not valid WGSL at all: {e}")),
    };
    let info = match naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    {
        Ok(i) => i,
        Err(e) => return Some(format!("failed WGSL validation: {e}")),
    };

    for ep in &module.entry_points {
        // **Resolve `override` declarations first**, which is what wgpu does
        // before handing a module to any backend. Without this the writer
        // refuses with "overrides should not be present at this stage" — a
        // complaint about the caller, not about the shader, and one that would
        // have read here as a shader OpenGL cannot run.
        let (module, info) = match naga::back::pipeline_constants::process_overrides(
            &module,
            &info,
            Some((ep.stage, &ep.name)),
            &Default::default(),
        ) {
            Ok(pair) => pair,
            Err(e) => return Some(format!("its override constants do not resolve: {e}")),
        };
        let options = naga::back::glsl::Options {
            // GLSL 4.10 core: the floor wgpu's GL backend targets on desktop.
            version: naga::back::glsl::Version::Desktop(410),
            ..Default::default()
        };
        let pipeline = naga::back::glsl::PipelineOptions {
            shader_stage: ep.stage,
            entry_point: ep.name.clone(),
            multiview: None,
        };
        let mut out = String::new();
        let writer = naga::back::glsl::Writer::new(
            &mut out,
            &module,
            &info,
            &options,
            &pipeline,
            naga::proc::BoundsCheckPolicies::default(),
        );
        let refused = match writer {
            Ok(mut w) => w.write().err().map(|e| e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        // A stage GL cannot express AT ALL is not a finding — the engine's
        // compute passes never run there. Only refusals about the shader.
        if let Some(why) = refused
            && !why.contains("not supported")
        {
            return Some(format!("{} `{}`: {why}", stage_name(ep.stage), ep.name));
        }
    }
    None
}

fn stage_name(s: naga::ShaderStage) -> &'static str {
    match s {
        naga::ShaderStage::Vertex => "vertex",
        naga::ShaderStage::Fragment => "fragment",
        naga::ShaderStage::Compute => "compute",
        // Mesh and ray-tracing stages: named generically because the engine
        // has none, and a stage it never writes needs no vocabulary here.
        _ => "another stage",
    }
}

#[test]
fn every_shader_can_be_written_as_glsl() {
    let mut refused: Vec<String> = Vec::new();
    for (name, source) in modules() {
        if let Some(why) = glsl_refusal(&source) {
            refused.push(format!("  {name}: {why}"));
        }
    }
    assert!(
        refused.is_empty(),
        "these shaders cannot be built on the OpenGL backend:\n{}\n\n\
         Every machine that develops this engine picks Vulkan, so this is invisible \
         locally — CI has no Vulkan driver and falls back to OpenGL, where it is a hard \
         failure to create the pipeline at all. The usual cause is one image sampled by \
         two samplers: GLSL combines image and sampler into one object, so give the image \
         a second binding of the same view instead of the sampler a second partner.",
        refused.join("\n")
    );
}

/// **The list above cannot quietly stop covering a shader.**
///
/// A directory walk would have been simpler and wrong — `field.wgsl` is spliced
/// into two modules rather than built on its own — so the list is written by
/// hand and this holds it against what is actually there.
#[test]
fn every_shader_file_is_accounted_for() {
    let listed = modules();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(shader_dir()).expect("read the shader directory").flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "wgsl") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // `field.wgsl` is a fragment, checked through the two modules it is
        // spliced into rather than on its own.
        if name == "field.wgsl" {
            continue;
        }
        if !listed.iter().any(|(listed, _)| listed.contains(&name)) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "these shaders exist and nothing checks them against OpenGL: {missing:?}"
    );
}
