//! **A cutout's opacity multiplies its picture's alpha before the cut**, as
//! glTF's MASK has it: a texel at alpha 0.8 under a factor of 0.5 is 0.4, and
//! under a cutoff of 0.5 it is a hole. Dividing the cutoff by the factor is
//! the same test, and keeps the surface in the solid pass.

use floptle_core::{BlendMode, Material};
use floptle_render::SurfaceExtras;

#[test]
fn a_cutout_s_opacity_scales_its_cutoff() {
    let cut = |alpha, alpha_cutoff, blend| {
        SurfaceExtras::from_material(&Material { alpha, alpha_cutoff, blend, ..Default::default() }).alpha_cutoff
    };
    assert_eq!(cut(1.0, 0.5, BlendMode::Cutout), 0.5);
    assert!((cut(0.5, 0.3, BlendMode::Cutout) - 0.6).abs() < 1e-6, "0.3 / 0.5");
    assert_eq!(cut(0.25, 0.5, BlendMode::Cutout), 1.0, "held to 1: almost nothing survives");
    assert_eq!(cut(0.5, 0.3, BlendMode::Opaque), 0.0, "opaque cuts nothing");
    assert_eq!(cut(0.5, 0.3, BlendMode::Blend), 0.3, "a blend is not cut by its opacity");
}
