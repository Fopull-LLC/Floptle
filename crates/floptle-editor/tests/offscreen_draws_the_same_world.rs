//! Every view draws the same world.
//!
//! The editor gathers draws twice: once for the main surface (the Scene view)
//! and once in `render_world_into`, which every OTHER view comes through — the
//! docked or split Game view, camera previews, and render targets.
//!
//! Those two have now drifted three times, and each time the symptom was
//! identical and awful to diagnose: the thing is right there in the Scene view,
//! and the Game view is empty. Map meshes went first (a level that drew as empty
//! air). Then tilemaps and sprite batches, for two releases — which means a 2D
//! game was invisible in the one view that *is* the game, and the Scene view
//! insisted everything was fine.
//!
//! `render_world_into`'s match is exhaustive now, so a NEW kind of matter cannot
//! be dropped silently. This covers the other half: an existing kind being drawn
//! in one gather and not the other. It is a source-level check because there is
//! no way to ask a GPU-less test what a view drew.
//!
//! If this fails after a refactor that genuinely moved the gather somewhere
//! better, move the check with it — don't delete it.

const SRC: &str = include_str!("../src/render_frame.rs");

/// The calls that put world geometry into a frame. Each must appear on both
/// paths; a call that exists on only one is a kind of object some views cannot
/// see.
const GATHERS: [(&str, &str); 3] = [
    ("push_mesh_instances", "imported models, map meshes, skinned characters"),
    ("tilemap_draws", "tilemaps — the 2D level itself"),
    ("sprite_draws", "sprite batches"),
];

/// The body of `render_world_into`, from its signature to the end of the file.
///
/// Deliberately crude: it is the last of the two gathers in the file, so
/// everything after its signature is the offscreen path. A precise brace
/// matcher would be more code and no more correct for this question.
fn offscreen() -> &'static str {
    let at = SRC.find("fn render_world_into").expect(
        "render_world_into is gone — if the offscreen gather moved, move this test with it",
    );
    &SRC[at..]
}

#[test]
fn every_gather_on_the_main_path_also_runs_for_offscreen_views() {
    let off = offscreen();
    let missing: Vec<&str> = GATHERS
        .iter()
        .filter(|(call, _)| !off.contains(*call))
        .map(|(call, what)| *what)
        .collect();
    assert!(
        missing.is_empty(),
        "render_world_into never gathers: {}.\n\n\
         Every view except the Scene view is rendered through it — the docked or split \
         Game view, camera previews, render targets. Anything it does not gather is \
         invisible in all of them while looking perfect in the Scene view, which is \
         about the most expensive way this engine can fail.",
        missing.join("; ")
    );
}

/// …and the check above has to be able to fail.
///
/// If `render_world_into` were renamed or the file split, `offscreen()` would
/// return something that trivially contains every call and the test would pass
/// forever while guarding nothing.
#[test]
fn the_two_gathers_are_actually_two() {
    let off = offscreen();
    assert!(
        off.len() < SRC.len() / 2,
        "the offscreen gather is {} of {} bytes — that is not a function, it is most of \
         the file, so this test is reading the main gather too and would pass no matter what",
        off.len(),
        SRC.len()
    );
    for (call, what) in GATHERS {
        let before = &SRC[..SRC.find("fn render_world_into").unwrap()];
        assert!(
            before.contains(call),
            "{call} ({what}) is not on the main path either — this test is checking a \
             call that no longer exists anywhere, which is a pass that means nothing"
        );
    }
}
