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
const GATHERS: [(&str, &str); 11] = [
    ("push_mesh_instances", "imported models, map meshes, skinned characters"),
    // Not geometry either, but the same failure with a different face: a node's
    // Tint multiplies over everything it drew. Applied on one path only, a
    // ghosted building or a flashing enemy is tinted while you edit it and
    // plain in the game.
    ("apply_node_tint", "the node's tint, over everything it drew"),
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
    ("prepass_and_bind", "the opaque depth prepass, run AND bound"),
    // …and RUNNING it is not BINDING it. The prepass writes its own sampleable
    // copy, and until that copy is on the shared field group nothing can read
    // it — the same bug one step later, and it has now happened twice on the
    // surface path: once bound inside the `rm_draw` arm, once guarded on
    // "was the target reallocated?" (permanently false once a frame draws two
    // views, so the window drew with the Game panel's depth buffer). The two
    // are one call now, which is why the name above covers both.
    ("wants_prepass", "the shared answer to whether this view needs a prepass"),
    // Same shape as the GI volume above: the probe TEXTURE is shared, and the
    // lanes that say where each probe's room is are camera-relative, so they
    // have to be stamped per view. Stamped on one path only, a docked Game
    // panel would reflect the sky indoors while the Scene view reflected the
    // room — which is the exact failure this file exists for, in the exact
    // feature that was added to fix it.
    ("probe_uniforms", "where each reflection probe's room is, from this eye"),
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
/// Is this name actually *called* in `hay`?
///
/// Comments are stripped first. A name that appears only in a `//` line is
/// somebody explaining why the call is not there — and reading that as the call
/// itself makes this test pass on exactly the code it exists to catch.
fn calls(hay: &str, name: &str) -> bool {
    let code: String = hay
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    code.match_indices(name).any(|(i, _)| !code[..i].trim_end().ends_with("fn"))
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

/// **Everything a scene render binds is something start-up sets up.**
///
/// `render_world_into` binds six pieces of the device as one `if let` tuple. A
/// missing one used to mean the whole render silently did nothing: no panic, no
/// message, a valid and entirely black frame. `floptle shot` shipped its first
/// picture that way — 960x540 of black, exit 0 — because it built its device
/// from a copy of start-up that stopped one line too early.
///
/// So the rule is that anything the draw binds is created in
/// `Editor::init_gpu_side`, which is the ONE function both the window and the
/// headless verb go through. This reads the bind list out of the source rather
/// than restating it, so adding a seventh thing to the tuple and forgetting the
/// setup fails here instead of in somebody's black PNG.
#[test]
fn every_field_a_scene_render_binds_is_set_up_in_one_place() {
    let off = offscreen();
    // The six-way bind, as written: `self.<field>.as_ref()` / `.as_mut()` inside
    // the tuple that guards the draw.
    // Anchored on the LAST member of the tuple and walked back to the `) = (`
    // that opens it: anchoring on the first member instead finds an earlier,
    // unrelated `self.raster.as_mut()` and reads a window that is not the bind.
    let last = off.find("self.tri_layer.as_mut(),").expect(
        "the scene draw no longer binds `self.tri_layer.as_mut()` — if the bind was \
         reshaped, move this check with it rather than deleting it",
    );
    let open = off[..last].rfind(") = (").expect("the bind tuple has no opening");
    let tuple_end = off[last..].find(") {").expect("the bind tuple is unterminated") + last;
    let tuple = &off[open..tuple_end];

    let mut bound: Vec<&str> = Vec::new();
    for line in tuple.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("self.") else { continue };
        let Some(field) = rest.split(['.', ',']).next() else { continue };
        if !field.is_empty() && !bound.contains(&field) {
            bound.push(field);
        }
    }
    assert!(
        bound.len() >= 6,
        "only found {bound:?} in the scene draw's bind tuple, which is fewer than the six          it has always taken — this test has stopped reading what it thinks it is reading"
    );

    let setup = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"),
    )
    .expect("read main.rs");
    let start = setup.find("fn init_gpu_side").expect(
        "`init_gpu_side` is gone — it is the one place both the window and the headless          verbs set the device up, and without it they can drift again",
    );
    let end = setup[start..].find("\n    }\n").map(|i| i + start).unwrap_or(setup.len());
    let init = &setup[start..end];

    let missing: Vec<&str> = bound
        .iter()
        .filter(|f| **f != "gpu" && **f != "raster") // set by `attach_gpu` itself
        .filter(|f| !init.contains(&format!("self.{f} = Some(")))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "the scene draw binds {missing:?}, and `init_gpu_side` never creates {}. A device \
         missing any one of them draws NOTHING — silently, into a valid black frame.",
        if missing.len() == 1 { "it" } else { "them" }
    );
}
