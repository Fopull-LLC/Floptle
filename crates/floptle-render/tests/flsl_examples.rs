//! Every built-in example shader must compile against the REAL pass sources —
//! the exact modules the editor assembles (not the transpiler's TEST_PRELUDE
//! mirror). Catches raster.wgsl/field.wgsl drift before a user's first
//! double-click on an example.

use floptle_shader::examples::EXAMPLES;
use floptle_shader::ir::Stage;

#[test]
fn fragment_examples_validate_against_the_real_raster_prelude() {
    for (name, src) in EXAMPLES {
        let ir = floptle_shader::parse(src).unwrap_or_else(|e| panic!("{name}: {}", e.message));
        if ir.stage != Some(Stage::Fragment) {
            continue;
        }
        let compiled =
            floptle_shader::compile_fragment(src).unwrap_or_else(|e| panic!("{name}: {e}"));
        floptle_shader::validate(floptle_render::pass_prelude(), &compiled.chunk)
            .unwrap_or_else(|e| panic!("{name}: naga rejects against the real prelude: {}", e.message));
    }
}

#[test]
fn sdf_examples_splice_into_both_real_passes() {
    // The editor's sync_field_shapes assembly, one example per slot 0.
    for (name, src) in EXAMPLES {
        let Ok((ir, ck)) = floptle_shader::check_sdf(src) else { continue };
        let c = floptle_shader::transpile_sdf(&ir, &ck, 0)
            .unwrap_or_else(|e| panic!("{name}: {}", e.message));
        let mut field_code = c.dist_fn.clone();
        field_code.push_str(
            "fn custom_d(p: vec3<f32>) -> f32 {\n    return flsl_shape0_d(p);\n}\n",
        );
        let mut color_code = c.col_fn.clone();
        color_code.push_str(
            "fn custom_col(p: vec3<f32>) -> Matter {\n    return Matter(flsl_shape0_d(p), flsl_shape0_col(p));\n}\n",
        );
        color_code.push_str("fn nearest_shape(p: vec3<f32>) -> i32 {\n    return 0;\n}\n");
        let support = floptle_shader::stdlib::SUPPORT_WGSL;
        let rm = floptle_render::Raymarch::preview_custom_source(Some((
            &field_code,
            &color_code,
            support,
        )));
        floptle_shader::validate_module(&rm)
            .unwrap_or_else(|e| panic!("{name}: raymarch splice rejected: {}", e.message));
        let raster = floptle_render::raster_custom_source(Some((&field_code, support)));
        floptle_shader::validate_module(&raster)
            .unwrap_or_else(|e| panic!("{name}: raster splice rejected: {}", e.message));
    }
}
