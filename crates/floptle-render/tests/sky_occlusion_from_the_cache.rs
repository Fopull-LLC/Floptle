//! **`skyOcclusion` read from the sky cache.**
//!
//! The beach of `sky_occlusion_by_a_beach.rs`, crowded the way a game fills
//! its proxy slots (`crowd`): crew, props, a dock, a hut and boats spread
//! over the sand and the water. Nearly every sea pixel is near a proxy, so
//! the op's early outs settle little and most of the sea marches its four
//! rays.

mod beach;

use beach::*;
use floptle_core::math::DVec3;

/// What the op costs on the crowded beach today, over the plain shader,
/// beside six field reads: printed, for the record and for comparing
/// against the cache.
#[test]
fn a_crowded_beach_marches() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    if beach.gpu.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return;
    }
    let Some(mut timer) = floptle_render::GpuTimer::new(&beach.gpu) else { return };
    for view in [View::EyeLevel, View::Above] {
        let (globals, eye, _) = beach.stage_with(view, Layout::Crowd, DVec3::ZERO);
        let plain = beach.shader("1.0");
        let six = beach.shader(SIX_READS);
        let r24 = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 24)");
        let r8 = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 8)");
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, six, r24, r8, plain, six, r24, r8]);
        let base = t[0].min(t[4]);
        let over = |k: usize| t[k].min(t[k + 4]) - base;
        println!("{view:?} crowd: plain {base:.3} ms; over it, six reads {:.3}, reach 24 {:.3}, reach 8 {:.3}", over(1), over(2), over(3));
    }
}
