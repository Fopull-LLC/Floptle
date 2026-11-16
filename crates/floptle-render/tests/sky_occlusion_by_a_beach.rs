//! **`skyOcclusion` over shallow water by a beach.**
//!
//! A sea near a shore sits inside the terrain's volume with its floor a few
//! metres down, and collider proxies (boats, rocks, a dock) stand around it.
//! The cone's rays all go up, but a test that does not know which way they
//! point sees the floor within reach and the proxies within its sphere, and
//! marches four rays a pixel for water that is plainly open.
//!
//! The fixture is a generated terrain the way a game makes one (a chunk field
//! sampled into its coarse shadow stand-in), with thirty proxies along the
//! shore, seen from eye level and from eighteen metres up.

mod beach;

use beach::*;

/// The op with its early outs reads what the full march reads, on both
/// views at reaches of 8, 24 and 32 (skyVisibility's). Where a march is
/// started past the stretch the point read proved clear, its samples fall
/// in other places than the full march's, and its penumbra can read a
/// little differently: never more than 0.15, and on no more than one pixel
/// in ten thousand by more than 0.02.
#[test]
fn early_outs_read_what_the_full_march_reads() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    for view in [View::EyeLevel, View::Above] {
        for reach in ["8", "24", "32"] {
            let call = format!("skyOcclusion(worldPos, vec3(0, 1, 0), {reach})");
            let (sea, dark, worst, over) = compare(&mut beach, view, &call);
            // The fixture: some of the sea is under cover at every reach.
            assert!(dark > sea / 100, "{view:?} reach {reach}: only {dark} of {sea} pixels are under any cover");
            assert!(
                worst <= 0.15 && over <= sea / 10_000,
                "{view:?} reach {reach}: the early outs differ from the full march by up to {worst:.3}, and by over 0.02 on {over} of {sea} pixels"
            );
        }
    }
}

/// The same over fine knolls just under a terrain box's top (`knolls`), at
/// short reaches and with the normal tilted as well as up: where the reads
/// above the point land just over the box, they must not call open a half
/// a knoll reaches into.
#[test]
fn early_outs_read_what_the_full_march_reads_just_over_a_box() {
    let Some(mut beach) = Beach::new(knolls()) else { return };
    for n in ["vec3(0, 1, 0)", "normalize(vec3(0.35, 1, 0.2))"] {
        for reach in ["2", "3", "4", "8", "16"] {
            let call = format!("skyOcclusion(worldPos, {n}, {reach})");
            let (sea, dark, worst, over) = compare(&mut beach, View::Knolls, &call);
            assert!(dark > sea / 100, "reach {reach}, n {n}: only {dark} of {sea} pixels are under any cover");
            assert!(
                worst <= 0.15 && over <= sea / 10_000,
                "reach {reach}, n {n}: the early outs differ from the full march by up to {worst:.3}, and by over 0.02 on {over} of {sea} pixels"
            );
        }
    }
}

/// On both views, `skyOcclusion` at a reach of 24 costs no more than nine
/// times what six `fieldDistance` reads cost on the same frame (each over
/// the plain shader). A ratio, not a time, so a slower or busier GPU moves
/// both sides. Measured on an RTX 4060 shared with other work: 4 to 7
/// times; with the reads above the point never saying open, 12 to 20; before
/// any of this, 23 and 17.
#[test]
fn sky_occlusion_by_a_beach_costs_a_few_field_reads() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    // A software rasteriser's clock says nothing about a GPU's.
    if beach.gpu.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return;
    }
    let Some(mut timer) = floptle_render::GpuTimer::new(&beach.gpu) else { return };
    for view in [View::EyeLevel, View::Above] {
        let (globals, eye) = beach.stage(view);
        let plain = beach.shader("1.0");
        let six = beach.shader(SIX_READS);
        let sky = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 24)");
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, six, sky, plain, six, sky]);
        assert!(t.iter().all(|t| *t < f32::MAX), "no timing came back");
        let base = t[0].min(t[3]);
        let (reads, op) = (t[1].min(t[4]) - base, t[2].min(t[5]) - base);
        println!("{view:?}: plain {base:.3} ms; over it, six reads {reads:.3} ms, skyOcclusion reach 24 {op:.3} ms");
        assert!(
            op <= 9.0 * reads.max(0.01),
            "{view:?}: skyOcclusion at a reach of 24 costs {op:.3} ms a {W}x{H} frame over the plain shader, {:.1} times six field reads ({reads:.3} ms)",
            op / reads.max(0.01)
        );
    }
}
