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

/// What the cache holds for the cell round `p` against the cone at that
/// cell's centre, for the lobe `n` points along: the difference times
/// four (under 0.5), or 0.6 where the cell's centre is in solid matter,
/// 0.8 where the cell is not filled, 1 outside every level.
const FILL_CHECK: &str = r#"
fn sky_fill_check(p: vec3<f32>, n: vec3<f32>, reach: f32) -> f32 {
    for (var l = 0u; l < SKYC_LEVELS; l = l + 1u) {
        let cell = skyc_f(SKYC_LEVEL + l * SKYC_LEVEL_WORDS + 3u);
        let u = floor((p - skyc_corner(l)) / cell);
        if (any(u < vec3<f32>(0.0)) || any(u > vec3<f32>(f32(SKYC_DIMS) - 1.0))) {
            continue;
        }
        let s = (vec3<u32>(u) + skyc_mod(l)) % vec3<u32>(SKYC_DIMS);
        let c = skyc_cell(l, s);
        let flags = c.y >> 24u;
        if ((flags & 0xFDu) != skyc_tag(l, s)) {
            return 0.8;
        }
        if ((flags & 2u) == 0u) {
            return 0.6;
        }
        let centre = skyc_corner(l) + (u + vec3<f32>(0.5)) * cell;
        let a = select(select(2u, 1u, abs(n.y) > abs(n.z)), 0u, abs(n.x) > max(abs(n.y), abs(n.z)));
        let idx = a * 2u + select(0u, 1u, n[a] < 0.0);
        let word = select(c.y, c.x, idx < 4u);
        let got = f32((word >> (8u * (idx % 4u))) & 255u) / 255.0;
        let want = sky_cone(centre, n, reach, skyc_static(skyc_word(SKYC_SETTLED)));
        return min(abs(got - want) * 4.0, 0.49);
    }
    return 1.0;
}
"#;

/// After the cache settles, every cell round the sea holds the cone the op
/// marches, at the cell's centre and along each axis, to within its 8-bit
/// rounding: the fill pass, the scrolled storage and the tags all agree
/// with where the cell is.
#[test]
fn the_fill_holds_the_cone_at_every_cell_centre() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    // The second step moves the camera 30 units forward, so most of what
    // it sees is read from cells a scroll brought in, stored where cells
    // from behind the camera were.
    for (view, shift) in [(View::EyeLevel, 0.0), (View::EyeLevel, 30.0), (View::Above, 0.0)] {
        let (globals, eye, rm) = beach.stage_with(view, Layout::Empty, DVec3::new(shift, 0.0, 0.0));
        beach.settle(rm, eye);
        for n in ["vec3(0, 1, 0)", "vec3(1, 0, 0)", "vec3(0, 0, -1)"] {
            let call = format!("skyOcclusion(worldPos, {n}, 24)");
            let bind = beach.shader_with(&call, Some(("sky_occlusion(", "sky_fill_check(")), FILL_CHECK);
            beach.draw(globals, eye, bind);
            let (mut sea, mut held, mut solid, mut unfilled, mut outside, mut worst) = (0usize, 0usize, 0usize, 0usize, 0usize, 0.0f32);
            for v in beach.read().into_iter().flatten() {
                sea += 1;
                if v < 0.5 {
                    held += 1;
                    worst = worst.max(v / 4.0);
                } else if v < 0.7 {
                    solid += 1;
                } else if v < 0.9 {
                    unfilled += 1;
                } else {
                    outside += 1;
                }
            }
            println!("{view:?} +{shift} n {n}: {sea} sea px; {held} held, {solid} in solid, {unfilled} unfilled, {outside} outside; worst {worst:.4}");
            assert_eq!(unfilled, 0, "{view:?} n {n}: {unfilled} of {sea} pixels read an unfilled cell");
            assert!(held > sea * 9 / 10, "{view:?} n {n}: only {held} of {sea} pixels read a filled cell");
            assert!(worst <= 0.006, "{view:?} n {n}: a cell differs from the cone at its centre by {worst:.4}");
        }
    }
}
