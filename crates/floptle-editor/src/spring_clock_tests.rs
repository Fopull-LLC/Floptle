//! **Spring bones measure motion over the time it was drawn** (card 0774).
//! A body's drawn pose advances by the simulation's step, which falls short
//! of the frame's whenever ticks are dropped. Springs took a node's speed as
//! its drawn movement over the frame's `dt`, so on a frame that dropped time
//! a steadily moving rider read as slowing down, and on the next as speeding
//! up again: hair shook on anything moving at a constant speed.

use floptle_core::FixedTimestep;

/// Rigged Sae riding a gravity-free body that holds a steady 5 m/s, her hair
/// a spring chain. Played under `frames` (frame times in seconds) with ticks
/// at `hz`. Returns the hair tip's position relative to Sae's node, and
/// `drawDt` as `lateUpdate` read it, frame by frame.
fn ride(tag: &str, hz: f32, frames: &[f32]) -> (Vec<[f64; 3]>, Vec<f64>) {
    let d = std::env::temp_dir().join(format!("floptle_spring_clock_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("scripts")).unwrap();
    let sae = concat!(env!("CARGO_MANIFEST_DIR"), "/../floptle-assets/tests/fixtures/Sae.mirrored.rigged.glb");
    std::fs::copy(sae, d.join("Sae.glb")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"c\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/ride.lua"),
        "function fixedUpdate(node, dt)\n\
           node.vel = vec3(5, 0, 0)\n\
         end\n\
         function lateUpdate(node, dt)\n\
           local sae = find('Sae')\n\
           local p = sae:animator():boneWorld('Hair_4')\n\
           local o = sae.worldPos\n\
           log(string.format('TIP %.6f %.6f %.6f %.9f', p.x - o.x, p.y - o.y, p.z - o.z, drawDt))\n\
         end\n",
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        "(name: \"s\", nodes: [\
         (name: \"Carrier\", scripts: [(kind: \"ride\")], matter: Empty,\
          rigidbody: Some((boxed: true, half_extents: (0.3, 0.3, 0.3), gravity: false, rotates: false, friction: 0.0))),\
         (name: \"Sae\", parent: Some(0), matter: Mesh(asset_path: \"Sae.glb\"),\
          spring_bones: Some((chains: [(name: \"Hair\", root: \"Hair_root\", stiffness: 4.0, drag: 0.3, max_angle: 170.0)], gravity: (0.0, 0.0, 0.0))))])",
    )
    .unwrap();
    let mut ed = crate::Editor::default();
    ed.open_project(d.clone());
    ed.game_tick = FixedTimestep::new(hz);
    ed.toggle_play();
    assert!(ed.playing, "{tag}: did not enter play");
    let mut tips = Vec::new();
    let mut draws = Vec::new();
    for &dt in frames {
        ed.play_step(dt, true);
        ed.drain_script_logs();
        for e in ed.console.entries.drain(..) {
            if let Some(rest) = e.msg.strip_prefix("TIP ") {
                let v: Vec<f64> = rest.split_whitespace().map(|x| x.parse().unwrap()).collect();
                tips.push([v[0], v[1], v[2]]);
                draws.push(v[3]);
            }
        }
    }
    assert!(ed.script_host.errors().is_empty(), "{tag}: {:?}", ed.script_host.errors());
    let _ = std::fs::remove_dir_all(&d);
    assert_eq!(tips.len(), frames.len(), "{tag}: a frame logged no tip");
    (tips, draws)
}

/// How far the tip wanders, frame to frame, over the second half of a run:
/// the largest step between consecutive frames once the start has settled.
fn shake(tips: &[[f64; 3]]) -> f64 {
    let half = tips.len() / 2;
    tips[half..]
        .windows(2)
        .map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2) + (w[1][2] - w[0][2]).powi(2)).sqrt())
        .fold(0.0, f64::max)
}

/// **A rider at a steady speed keeps still hair when ticks are dropped.**
/// Ticks at 480 Hz cap a frame at eight of them, 16.7 ms, so frames of 17 and
/// 23 ms in turn each drop a little, a different amount every other frame:
/// the drawn world moves 16.7 ms a frame while `dt` alternates. Measured over
/// `dt`, a body at a constant 5 m/s reads as braking and surging by turns,
/// and the chain shakes. Measured over the drawn step, it reads as the steady
/// motion it is, and the hair is as still as on even frames that drop
/// nothing. `drawDt` says the drawn step to scripts.
#[test]
fn a_rider_at_a_steady_speed_keeps_still_hair_when_ticks_drop() {
    let even = vec![1.0 / 60.0; 120];
    let uneven: Vec<f32> = (0..120).map(|i| if i % 2 == 0 { 0.017 } else { 0.023 }).collect();
    let (calm, calm_draw) = ride("even", 480.0, &even);
    let (rough, rough_draw) = ride("uneven", 480.0, &uneven);
    // The fixture: frames that drop nothing draw exactly their `dt`, and the
    // uneven frames draw the step cap's 16.7 ms whatever their length.
    for (i, d) in calm_draw.iter().enumerate().skip(2) {
        assert!((d - 1.0 / 60.0).abs() < 1e-4, "frame {i}: an even frame drew {d} s, not its own 1/60");
    }
    for (i, d) in rough_draw.iter().enumerate().skip(2) {
        assert!((d - 8.0 / 480.0).abs() < 1e-4, "frame {i}: drawDt {d} s is not the eight-tick cap the frame was drawn over");
    }
    let (s_calm, s_rough) = (shake(&calm), shake(&rough));
    assert!(s_rough < s_calm * 2.0 + 1e-3, "the hair shakes {s_rough:.5} a frame on uneven frames against {s_calm:.5} on even ones: springs measured the ride over dt, not the time drawn");
}
