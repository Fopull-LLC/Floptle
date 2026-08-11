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
//! Then it happened a **fourth** time, and in the one way this test could not
//! see: an arm that builds its instance INLINE rather than calling a helper.
//! Water was gathered only by the Scene view — an ocean you could edit and could
//! not play — and the offscreen `Primitive` arm had quietly stopped applying
//! vertex paint, so a painted cube was painted on screen and plain in the Game
//! view. Neither showed up here, because there was no shared call to miss.
//!
//! So the rule this file now enforces is the stronger one: **an arm that builds
//! geometry does it in a function both gathers call.** That is what makes the
//! check possible at all, and it is why `water_draw` and `primitive_draw` exist
//! as functions rather than as two copies of the same twenty lines.
//!
//! A **fifth** time, and this one was not a gather at all: the offscreen path
//! ran no opaque depth prepass. Nothing was missing from the picture, so none of
//! the checks above could see it — instead four separate features (contact
//! shadows, shoreline foam, screen-space reflections, lamp shadows) each read an
//! empty depth texture, took their "nothing to report" branch, and drew nothing.
//! A docked Game panel showed a different game from the same game fullscreen.
//! Hence the last two entries below: a pass counts as a gather here.
//!
//! If this fails after a refactor that genuinely moved the gather somewhere
//! better, move the check with it — don't delete it.

const SRC: &str = include_str!("../src/render_frame.rs");

/// The calls that put world geometry into a frame. Each must appear on both
/// paths; a call that exists on only one is a kind of object some views cannot
/// see.
const GATHERS: [(&str, &str); 9] = [
    ("push_mesh_instances", "imported models, map meshes, skinned characters"),
    ("tilemap_draws", "tilemaps — the 2D level itself"),
    ("sprite_draws", "sprite batches"),
    ("primitive_draw", "primitives, with their vertex paint"),
    ("water_draw", "water volumes — seas and pools"),
    // Not geometry, but the same failure: the palette quantize has to run in
    // the same place relative to the 2D light composite on both paths, or a
    // Game view posterizes its lighting while the Scene view does not
    // (`floptle/0127`).
    ("quantize_palette", "the palette quantize, before the 2D light"),
    // Also not geometry: where the baked GI volume IS. The probe texture is
    // shared, but the four uniform lanes that locate it are camera-relative, so
    // they have to be stamped per view. Stamped on one path only, the Game view
    // would render with no bounce at all while the Scene view looked right —
    // which is precisely the shape of failure this file exists for.
    (".gi().apply(", "the baked GI volume's camera-relative position"),
    // Nor is this geometry, and it is the one that had drifted longest: the
    // OPAQUE DEPTH PREPASS. Contact shadows, `surfaceGap` (shoreline foam, soft
    // particles), screen-space reflections and lamp shadows all read it, and
    // every one of them silently does nothing without it — no error, no warning,
    // just a picture missing four features. It ran on the surface path only, so
    // a docked Game panel showed a visibly different game from the same game
    // fullscreen.
    ("depth_prepass_with", "the opaque depth prepass"),
    // …and RUNNING it is not BINDING it. The prepass writes its own sampleable
    // copy, and until that copy is on the shared field group nothing can read
    // it — which is the same bug one step later, and it has already happened
    // once on the surface path.
    ("bind_frame_targets", "binding the prepass so shaders can read it"),
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

/// Everything before the offscreen gather: the main (Scene view) path, plus the
/// helpers both paths share.
fn main_path() -> &'static str {
    let at = SRC.find("fn render_world_into").unwrap();
    &SRC[..at]
}

/// Does `hay` **call** `name`, as opposed to merely declaring it?
///
/// The shared helpers are defined above the offscreen gather, so a plain
/// `contains` would count `fn water_draw(` on the main path and report a call
/// that is not there. Only occurrences that are not the `fn` item count.
fn calls(hay: &str, name: &str) -> bool {
    hay.match_indices(name).any(|(i, _)| !hay[..i].trim_end().ends_with("fn"))
}

#[test]
fn every_gather_on_the_main_path_also_runs_for_offscreen_views() {
    let off = offscreen();
    let missing: Vec<&str> = GATHERS
        .iter()
        .filter(|(call, _)| !calls(off, call))
        .map(|(_, what)| *what)
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
        assert!(
            calls(main_path(), call),
            "{call} ({what}) is not called on the main path — either this test is checking \
             a call that no longer exists anywhere (a pass that means nothing), or the \
             SCENE view is the one now missing it, which is the same bug pointing the \
             other way"
        );
    }
}
