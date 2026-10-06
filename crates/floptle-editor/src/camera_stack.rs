//! Camera stacks — the cameras a camera draws on top of its own picture.
//!
//! A camera's `stack` names other cameras, bottom first. Each one renders only
//! the nodes on its own layers into a layer of its own, over a transparent
//! clear and an empty depth buffer, and the layers are laid over the base
//! camera's frame before post-processing. A first-person arm rig on its own
//! layer, filmed by a stacked camera, is drawn whole however close the player
//! stands to a wall: the camera filming it never saw the wall.
//!
//! Every view of the game goes through here — the docked Game panel, the
//! fullscreen Game tab, an exported build, `floptle shot`, `camera.capture` —
//! and so does a render target, so a scope's feed can carry its own reticle
//! camera.
//!
//! The stack is one level deep. A stacked camera's own stack is not drawn,
//! which is also what makes two cameras naming each other harmless.

use std::collections::HashMap;

use floptle_core::{Entity, Matter, World};
use floptle_render::{Projection, RenderCamera, StackLayer};

/// One stacked camera, resolved against the scene.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StackCam {
    pub(crate) e: Entity,
    pub(crate) fov_y: f32,
    pub(crate) ortho: bool,
    pub(crate) ortho_height: f32,
    pub(crate) mask: u32,
}

/// What a camera's stack resolves to this frame.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct StackPlan {
    /// The cameras to draw, bottom first.
    pub(crate) draw: Vec<StackCam>,
    /// Names that are not another camera in the scene. Said once each; a
    /// disabled camera is not here, because switching one off is how a game
    /// hides what it films.
    pub(crate) missing: Vec<String>,
}

/// Resolve `base`'s stack: each name to the first enabled camera node of that
/// name other than `base` itself.
pub(crate) fn resolve_stack(world: &World, base: Entity) -> StackPlan {
    let mut plan = StackPlan::default();
    let Some(Matter::Camera { stack, .. }) = world.get::<Matter>(base) else { return plan };
    for name in stack {
        let mut found = false;
        for (e, n) in world.query::<floptle_core::Name>() {
            if e == base || n.0 != *name {
                continue;
            }
            let Some(Matter::Camera { fov_y, ortho, ortho_height, cull_mask, .. }) = world.get::<Matter>(e)
            else {
                continue;
            };
            found = true;
            if !floptle_core::is_disabled(world, e) {
                plan.draw.push(StackCam {
                    e,
                    fov_y: *fov_y,
                    ortho: *ortho,
                    ortho_height: *ortho_height,
                    mask: *cull_mask,
                });
            }
            break;
        }
        if !found {
            plan.missing.push(name.clone());
        }
    }
    plan
}

/// The layers stacked cameras render into, by size, plus the pass that lays
/// them over a frame. Each user renders and composites before the next one
/// renders, so every target of one size shares the same few layers.
#[derive(Default)]
pub(crate) struct CameraStacks {
    composite: Option<floptle_render::StackComposite>,
    layers: HashMap<(u32, u32), Vec<StackLayer>>,
    /// `base → name` pairs already reported missing.
    warned: std::collections::HashSet<String>,
}

/// How many layer sizes are kept. The Game view and a render target or two;
/// past that the oldest go, rather than a resized window leaking a set.
const SIZES_KEPT: usize = 4;

impl CameraStacks {
    /// `n` layers at `size`, allocated on first use.
    fn ensure(&mut self, gpu: &floptle_render::Gpu, size: (u32, u32), n: usize) {
        if !self.layers.contains_key(&size) && self.layers.len() >= SIZES_KEPT {
            self.layers.clear();
        }
        let set = self.layers.entry(size).or_default();
        while set.len() < n {
            set.push(StackLayer::new(gpu, size.0, size.1));
        }
        if self.composite.is_none() {
            self.composite = Some(floptle_render::StackComposite::new(gpu));
        }
    }

    /// Lay the first `n` layers at `size` over `target`, bottom first.
    pub(crate) fn composite(
        &self,
        gpu: &floptle_render::Gpu,
        size: (u32, u32),
        n: usize,
        target: &wgpu::TextureView,
    ) {
        let (Some(pass), Some(set)) = (self.composite.as_ref(), self.layers.get(&size)) else { return };
        for layer in set.iter().take(n) {
            pass.composite(gpu, layer, target);
        }
    }
}

impl crate::Editor {
    /// Render `base`'s stack into layers at `size`, projected at `aspect` —
    /// the base camera's own, so a stacked camera frames exactly the picture
    /// it is laid over. Returns how many layers are ready for
    /// [`CameraStacks::composite`].
    pub(crate) fn render_camera_stack(
        &mut self,
        base: Entity,
        size: (u32, u32),
        aspect: f32,
        elapsed: f32,
    ) -> usize {
        let plan = resolve_stack(&self.world, base);
        if !plan.missing.is_empty() {
            let base_name =
                self.world.get::<floptle_core::Name>(base).map(|n| n.0.clone()).unwrap_or_default();
            for name in &plan.missing {
                if self.camera_stacks.warned.insert(format!("{base_name}\u{1f}{name}")) {
                    log::warn!(
                        "camera \"{base_name}\" stacks \"{name}\", and there is no other camera of \
                         that name in this scene — nothing is drawn for it. Check the spelling, or \
                         take it out of the stack."
                    );
                }
            }
        }
        if plan.draw.is_empty() {
            return 0;
        }
        let size = (size.0.max(1), size.1.max(1));
        let Some(gpu) = self.gpu.as_ref() else { return 0 };
        self.camera_stacks.ensure(gpu, size, plan.draw.len());
        for (i, sc) in plan.draw.iter().enumerate() {
            let (color, depth) = {
                let l = &self.camera_stacks.layers[&size][i];
                (l.color_view().clone(), l.depth_view().clone())
            };
            let wt = floptle_core::world_transform(&self.world, sc.e);
            let cam = RenderCamera::new(
                wt.translation,
                wt.rotation,
                Projection::of_camera(sc.fov_y, sc.ortho, sc.ortho_height, 0.05, 300000.0),
            );
            self.render_world_into(
                &color,
                &depth,
                &cam,
                aspect,
                elapsed,
                sc.mask,
                None,
                size,
                crate::offscreen::OffscreenOpts { overlay: true, ..Default::default() },
            );
        }
        plan.draw.len()
    }

    /// Render `base`'s stack and lay it straight over `target` — for a view
    /// whose own picture is already finished.
    pub(crate) fn draw_camera_stack_over(
        &mut self,
        base: Entity,
        target: &wgpu::TextureView,
        size: (u32, u32),
        aspect: f32,
        elapsed: f32,
    ) {
        let n = self.render_camera_stack(base, size, aspect, elapsed);
        if n > 0
            && let Some(gpu) = self.gpu.as_ref()
        {
            self.camera_stacks.composite(gpu, (size.0.max(1), size.1.max(1)), n, target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(world: &mut World, name: &str, stack: &[&str]) -> Entity {
        let e = world.spawn();
        world.insert(e, floptle_core::Name(name.into()));
        world.insert(
            e,
            Matter::Camera {
                fov_y: 1.0,
                active: false,
                target: String::new(),
                cull_mask: u32::MAX,
                target_w: Matter::TARGET_W,
                target_h: Matter::TARGET_H,
                target_hz: 0.0,
                ortho: false,
                ortho_height: Matter::ORTHO_HEIGHT,
                stack: stack.iter().map(|s| s.to_string()).collect(),
            },
        );
        e
    }

    #[test]
    fn a_stack_resolves_in_the_order_it_was_written() {
        let mut w = World::default();
        let main = camera(&mut w, "Main", &["Hud", "Arms"]);
        let arms = camera(&mut w, "Arms", &[]);
        let hud = camera(&mut w, "Hud", &[]);
        let plan = resolve_stack(&w, main);
        let order: Vec<Entity> = plan.draw.iter().map(|c| c.e).collect();
        assert_eq!(order, vec![hud, arms], "bottom first, as listed — not scene order");
        assert!(plan.missing.is_empty());
    }

    #[test]
    fn a_name_that_is_not_another_camera_is_reported() {
        let mut w = World::default();
        let main = camera(&mut w, "Main", &["Arms", "Main", "Crate"]);
        let prop = w.spawn();
        w.insert(prop, floptle_core::Name("Crate".into()));
        w.insert(prop, Matter::Empty);
        let plan = resolve_stack(&w, main);
        assert!(plan.draw.is_empty());
        assert_eq!(
            plan.missing,
            vec!["Arms".to_string(), "Main".to_string(), "Crate".to_string()],
            "absent, itself, and a node that is not a camera are all missing"
        );
    }

    #[test]
    fn a_disabled_camera_is_skipped_without_a_warning() {
        let mut w = World::default();
        let main = camera(&mut w, "Main", &["Arms"]);
        let arms = camera(&mut w, "Arms", &[]);
        w.insert(arms, floptle_core::Disabled);
        let plan = resolve_stack(&w, main);
        assert!(plan.draw.is_empty(), "switching the camera off hides what it films");
        assert!(plan.missing.is_empty(), "and that is a choice, not a mistake");
    }

    /// A wall between the camera and an arm. The camera beneath does not
    /// render the arm's layer; the stacked one renders nothing else.
    #[cfg(feature = "editor-ui")]
    fn wall_and_arm(ed: &mut crate::Editor, stack: &[&str]) -> Entity {
        use floptle_core::math::{DVec3, Vec3};
        ed.project.layers = vec!["Default".to_string(), "Arms".to_string()];
        let shape = |ed: &mut crate::Editor, name: &str, at: DVec3, scale: Vec3, color: [f32; 3]| {
            let e = ed.world.spawn();
            ed.world.insert(e, floptle_core::Name(name.into()));
            ed.world.insert(
                e,
                floptle_core::Transform { translation: at, scale, ..Default::default() },
            );
            ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color });
            e
        };
        shape(ed, "Wall", DVec3::new(0.0, 0.0, -2.0), Vec3::new(6.0, 6.0, 0.5), [1.0, 0.0, 0.0]);
        let arm = shape(ed, "Arm", DVec3::new(0.0, 0.0, -4.0), Vec3::ONE, [0.0, 1.0, 0.0]);
        ed.world.insert(arm, floptle_core::Layer("Arms".into()));
        let main = camera(&mut ed.world, "Main", stack);
        let arms = camera(&mut ed.world, "Arms", &[]);
        for (e, mask) in [(main, 1u32), (arms, 1 << 1)] {
            ed.world.insert(e, floptle_core::Transform::default());
            if let Some(Matter::Camera { cull_mask, .. }) = ed.world.get_mut::<Matter>(e) {
                *cull_mask = mask;
            }
        }
        main
    }

    /// The centre pixel of one shot from `main`, stack included.
    #[cfg(feature = "editor-ui")]
    fn centre(ed: &mut crate::Editor, main: Entity) -> [u8; 4] {
        let (w, h) = (64u32, 36u32);
        let wt = floptle_core::world_transform(&ed.world, main);
        let cam = RenderCamera::new(wt.translation, wt.rotation, Projection::of_camera(1.0, false, 1.0, 0.05, 300000.0));
        let px = crate::shot::render_frame_pixels(ed, &cam, Some(main), w, h, 1, false).expect("no device");
        let i = ((h / 2 * w + w / 2) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    // The picture tests photograph through `floptle shot`, an editor verb.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_stacked_camera_draws_its_layer_through_a_wall() {
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let main = wall_and_arm(&mut ed, &[]);
        let [r, g, _, _] = centre(&mut ed, main);
        assert!(r > g + 40, "without a stack the wall hides the arm, got r{r} g{g}");

        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let main = wall_and_arm(&mut ed, &["Arms"]);
        let [r, g, _, _] = centre(&mut ed, main);
        assert!(g > r + 40, "the stacked arm is drawn over the wall in front of it, got r{r} g{g}");
    }

    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_stacked_camera_leaves_what_it_did_not_draw() {
        // The arm covers the middle only; the corner is the wall the camera
        // beneath drew, untouched by a layer cleared to transparent.
        let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
        let main = wall_and_arm(&mut ed, &["Arms"]);
        let (w, h) = (64u32, 36u32);
        let wt = floptle_core::world_transform(&ed.world, main);
        let cam = RenderCamera::new(wt.translation, wt.rotation, Projection::of_camera(1.0, false, 1.0, 0.05, 300000.0));
        let px = crate::shot::render_frame_pixels(&mut ed, &cam, Some(main), w, h, 1, false).expect("no device");
        let i = ((2 * w + 2) * 4) as usize;
        let (r, g) = (px[i], px[i + 1]);
        assert!(r > g + 40, "the wall's corner came out r{r} g{g}");
    }

    #[test]
    fn a_stacked_camera_draws_with_its_own_lens_and_layers() {
        let mut w = World::default();
        let main = camera(&mut w, "Main", &["Arms"]);
        let arms = camera(&mut w, "Arms", &[]);
        if let Some(Matter::Camera { fov_y, cull_mask, .. }) = w.get_mut::<Matter>(arms) {
            *fov_y = 0.9;
            *cull_mask = 1 << 3;
        }
        let plan = resolve_stack(&w, main);
        assert_eq!(plan.draw[0].fov_y, 0.9, "a viewmodel FOV of its own");
        assert_eq!(plan.draw[0].mask, 1 << 3);
    }
}
