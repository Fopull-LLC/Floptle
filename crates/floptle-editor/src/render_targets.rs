//! Live camera render targets — the `rt:<name>` textures a game points a camera
//! at and then wears on a material or a UI image (minimaps, mirrors, security
//! monitors, scopes, split-screen).
//!
//! The rendering itself is [`Editor::render_world_into`], which is the same path
//! the editor's own viewports use. What lives here is the *decision*: which
//! targets exist, how big each one is, which ones are due to redraw this frame,
//! and what to say about the ones that cannot be served. That decision is a pure
//! function ([`plan_render_targets`]) so it is testable without a GPU — the part
//! that allocates textures is deliberately thin.
//!
//! `floptle/0078`: before this, every target was 480×270 and redrew every frame,
//! and a fifth target was dropped silently in whatever order the ECS query
//! happened to return. All three are now the game's choice, and the one thing
//! that remains a limit says so.

use std::collections::HashMap;

use floptle_core::{Entity, Matter};

/// A live render target's GPU side: the material handle plus the two views the
/// world is rendered into, and the size they were allocated at (so a camera
/// asking for a different size is noticed rather than ignored).
pub(crate) struct RenderTarget {
    pub(crate) tex: floptle_render::TexId,
    pub(crate) color: wgpu::TextureView,
    pub(crate) depth: wgpu::TextureView,
    pub(crate) w: u32,
    pub(crate) h: u32,
}

/// One camera's render-target request, exactly as the scene declares it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TargetReq {
    pub(crate) e: Entity,
    pub(crate) name: String,
    pub(crate) fov_y: f32,
    pub(crate) mask: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    /// Redraws per second; 0 means every frame.
    pub(crate) hz: f32,
    /// Orthographic rather than perspective — a target camera has the same
    /// choice the game view does. A minimap is very often the one place a
    /// perspective game wants an orthographic shot.
    pub(crate) ortho: bool,
    pub(crate) ortho_height: f32,
}

/// What to do about render targets this frame.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct TargetPlan {
    /// The targets to render, in a stable order.
    pub(crate) draw: Vec<TargetReq>,
    /// Targets that exist but are not due to redraw this frame (throttled by
    /// their `hz`). Their texture keeps its last frame, which is the point.
    pub(crate) idle: Vec<String>,
    /// Targets past [`Matter::TARGET_LIMIT`], which will not be rendered at all.
    pub(crate) dropped: Vec<String>,
    /// Names claimed by more than one camera — they would take turns writing one
    /// texture, so the picture would flicker between two viewpoints.
    pub(crate) duplicates: Vec<String>,
}

/// Decide this frame's render-target work.
///
/// `now` is the play/edit clock in seconds and `last` the clock reading at which
/// each target last redrew. Ordering is by name (then entity), NOT query order:
/// which targets survive the limit has to be the same on every run and after
/// every unrelated scene edit, or a scene "works" until a node is added
/// somewhere else entirely.
pub(crate) fn plan_render_targets(
    mut reqs: Vec<TargetReq>,
    now: f32,
    last: &HashMap<String, f32>,
) -> TargetPlan {
    reqs.sort_by(|a, b| a.name.cmp(&b.name).then(a.e.index().cmp(&b.e.index())));
    let mut plan = TargetPlan::default();
    let mut claimed: HashMap<&str, Entity> = HashMap::new();
    let mut kept = 0usize;
    for r in &reqs {
        // A second camera on the same name is not served: it would overwrite the
        // first camera's picture on alternate frames, which reads as a flickering
        // screen and not as a mistake.
        if let Some(&first) = claimed.get(r.name.as_str()) {
            if first != r.e && !plan.duplicates.contains(&r.name) {
                plan.duplicates.push(r.name.clone());
            }
            continue;
        }
        claimed.insert(r.name.as_str(), r.e);
        if kept >= Matter::TARGET_LIMIT {
            plan.dropped.push(r.name.clone());
            continue;
        }
        kept += 1;
        // Due? A target with no entry has never drawn, so it draws now — the
        // first frame of a 1 Hz minimap must not be a second of black.
        let due = match (r.hz > 0.0, last.get(&r.name)) {
            (false, _) | (_, None) => true,
            (true, Some(&t)) => now - t >= 1.0 / r.hz,
        };
        if due {
            plan.draw.push(r.clone());
        } else {
            plan.idle.push(r.name.clone());
        }
    }
    plan
}

/// Collect the scene's render-target cameras.
pub(crate) fn target_requests(world: &floptle_core::World) -> Vec<TargetReq> {
    world
        .query::<Matter>()
        .filter_map(|(e, m)| match m {
            Matter::Camera {
                fov_y,
                target,
                cull_mask,
                target_w,
                target_h,
                target_hz,
                ortho,
                ortho_height,
                ..
            } if !target.is_empty() => {
                let (w, h) = Matter::clamp_target_size(*target_w, *target_h);
                Some(TargetReq {
                    e,
                    name: target.clone(),
                    fov_y: *fov_y,
                    mask: *cull_mask,
                    w,
                    h,
                    hz: target_hz.max(0.0),
                    ortho: *ortho,
                    ortho_height: *ortho_height,
                })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Entity` has no public constructor, so tests spawn real ones — indices
    /// are handed out in order, which is what the ordering test needs.
    fn ents(n: usize) -> Vec<Entity> {
        let mut w = floptle_core::World::default();
        (0..n).map(|_| w.spawn()).collect()
    }

    fn req(name: &str, e: Entity, hz: f32) -> TargetReq {
        TargetReq {
            e,
            name: name.into(),
            fov_y: 1.0,
            mask: u32::MAX,
            w: 256,
            h: 256,
            hz,
            ortho: false,
            ortho_height: floptle_core::Matter::ORTHO_HEIGHT,
        }
    }

    #[test]
    fn a_target_with_no_rate_draws_every_frame() {
        let last = HashMap::from([("minimap".to_string(), 9.999)]);
        let e = ents(1);
        let plan = plan_render_targets(vec![req("minimap", e[0], 0.0)], 10.0, &last);
        assert_eq!(plan.draw.len(), 1, "hz = 0 means every frame");
        assert!(plan.idle.is_empty());
    }

    #[test]
    fn a_ten_hz_target_skips_the_frames_between() {
        let e = ents(1);
        let reqs = vec![req("minimap", e[0], 10.0)];
        // Drew at t = 10.0; at 60 fps the next five frames must not redraw.
        let last = HashMap::from([("minimap".to_string(), 10.0)]);
        for i in 1..=5 {
            let now = 10.0 + i as f32 / 60.0;
            let plan = plan_render_targets(reqs.clone(), now, &last);
            assert!(plan.draw.is_empty(), "10 Hz redrew at +{i}/60 s");
            assert_eq!(plan.idle, vec!["minimap".to_string()]);
        }
        let plan = plan_render_targets(reqs.clone(), 10.1, &last);
        assert_eq!(plan.draw.len(), 1, "a tenth of a second later it is due");
    }

    #[test]
    fn a_target_that_has_never_drawn_is_due_however_slow_it_is() {
        // A 1 Hz security monitor whose first frame waited a second would show a
        // second of black, and "it doesn't work" is what gets reported.
        let e = ents(1);
        let plan = plan_render_targets(vec![req("cctv", e[0], 1.0)], 0.0, &HashMap::new());
        assert_eq!(plan.draw.len(), 1);
    }

    #[test]
    fn which_targets_survive_the_limit_does_not_depend_on_query_order() {
        let names: Vec<String> =
            (0..Matter::TARGET_LIMIT + 3).map(|i| format!("cam{i:02}")).collect();
        let es = ents(names.len());
        let build = |order: Vec<usize>| -> TargetPlan {
            let reqs = order.iter().map(|&i| req(&names[i], es[i], 0.0)).collect::<Vec<_>>();
            plan_render_targets(reqs, 0.0, &HashMap::new())
        };
        let forward = build((0..names.len()).collect());
        let backward = build((0..names.len()).rev().collect());
        assert_eq!(forward, backward, "the surviving set must not be query order");
        assert_eq!(forward.draw.len(), Matter::TARGET_LIMIT);
        assert_eq!(forward.dropped.len(), 3, "the extras are named, not silently gone");
        assert_eq!(forward.dropped[0], "cam08");
    }

    #[test]
    fn two_cameras_on_one_name_is_reported_and_only_one_draws() {
        let es = ents(2);
        let plan = plan_render_targets(
            vec![req("mirror", es[0], 0.0), req("mirror", es[1], 0.0)],
            0.0,
            &HashMap::new(),
        );
        assert_eq!(plan.draw.len(), 1, "one texture, one writer");
        assert_eq!(plan.duplicates, vec!["mirror".to_string()]);
    }

    #[test]
    fn a_size_outside_what_the_engine_allocates_is_clamped_not_accepted() {
        let (w, h) = Matter::clamp_target_size(0, 99_999);
        assert_eq!(w, Matter::TARGET_MIN, "a zero-wide texture is not creatable");
        assert_eq!(h, Matter::TARGET_MAX);
    }
}
