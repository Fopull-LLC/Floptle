//! Camera stacks — the cameras a camera draws on top of its own picture.
//!
//! A camera's `stack` names other cameras, bottom first. Each one renders only
//! the nodes on its own layers into a layer of its own, over a transparent
//! clear and an empty depth buffer, and the layers are laid over the base
//! camera's frame before post-processing, their depth with them, so the base
//! camera's depth of field and AO see the stacked things where they are. A first-person arm rig on its own
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
    /// Laid over after the base camera's depth of field and motion blur.
    pub(crate) post_after: bool,
    /// What it draws besides its nodes.
    pub(crate) draws: floptle_core::CameraDraws,
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
            let Some(Matter::Camera { fov_y, ortho, ortho_height, cull_mask, post_after, draws, .. }) = world.get::<Matter>(e)
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
                    post_after: *post_after,
                    draws: *draws,
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
    /// The camera that last drew each layer.
    placed: HashMap<(u32, u32), Vec<Placed>>,
    /// `base → name` pairs already reported missing.
    warned: std::collections::HashSet<String>,
}

/// The camera that drew a layer, as compositing it needs to know.
#[derive(Clone, Copy, Default)]
struct Placed {
    /// Where it stood, and the inverse of its camera-relative view-
    /// projection: what takes the layer's depth back into the world.
    at: floptle_core::math::DVec3,
    inv_vp: floptle_core::math::Mat4,
    /// Laid over after the base camera's depth of field and motion blur.
    after: bool,
}

/// How many layer sizes are kept. The Game view and a render target or two;
/// past that the oldest go, rather than a resized window leaking a set.
const SIZES_KEPT: usize = 4;

impl CameraStacks {
    /// `n` layers at `size`, allocated on first use.
    fn ensure(&mut self, gpu: &floptle_render::Gpu, size: (u32, u32), n: usize) {
        if !self.layers.contains_key(&size) && self.layers.len() >= SIZES_KEPT {
            self.layers.clear();
            self.placed.clear();
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
    ///
    /// With `depth` (the frame's depth, and the base camera and aspect it was
    /// drawn with), each layer's depth goes into it where the layer drew
    /// something solid, so depth-based post sees the stacked things at their
    /// own distance. Layers laid over after depth of field are left for
    /// [`Self::composite_after`].
    pub(crate) fn composite(
        &self,
        gpu: &floptle_render::Gpu,
        size: (u32, u32),
        n: usize,
        target: &wgpu::TextureView,
        depth: Option<(&wgpu::TextureView, &RenderCamera, f32)>,
    ) {
        let (Some(pass), Some(set)) = (self.composite.as_ref(), self.layers.get(&size)) else { return };
        let placed = self.placed.get(&size);
        for (i, layer) in set.iter().take(n).enumerate() {
            let placed = placed.and_then(|p| p.get(i));
            if placed.is_some_and(|p| p.after) {
                continue;
            }
            pass.composite(gpu, layer, target);
            if let (Some((depth, base, aspect)), Some(p)) = (depth, placed) {
                pass.composite_depth(gpu, layer, depth, layer_to_base(base, aspect, p.at, p.inv_vp));
            }
        }
    }
}

/// The matrix from a layer's clip space to the base camera's: back into the
/// world from the layer camera (`at`, `inv_vp`), across to the base camera's
/// position, and through the base camera's view-projection.
fn layer_to_base(
    base: &RenderCamera,
    aspect: f32,
    at: floptle_core::math::DVec3,
    inv_vp: floptle_core::math::Mat4,
) -> floptle_core::math::Mat4 {
    base.view_proj(aspect) * floptle_core::math::Mat4::from_translation((at - base.world_position).as_vec3()) * inv_vp
}

impl CameraStacks {
    /// Whether any of the first `n` layers at `size` is laid over after depth
    /// of field, so post has something to hand [`Self::composite_after`].
    pub(crate) fn any_after(&self, size: (u32, u32), n: usize) -> bool {
        self.placed.get(&size).is_some_and(|p| p.iter().take(n).any(|p| p.after))
    }

    /// Lay the layers [`Self::composite`] left out, bottom first: the ones
    /// laid over after the base camera's depth of field and motion blur. Their
    /// depth is not written, so nothing after reads them as the world.
    pub(crate) fn composite_after(
        &self,
        gpu: &floptle_render::Gpu,
        size: (u32, u32),
        n: usize,
        target: &wgpu::TextureView,
    ) {
        let (Some(pass), Some(set), Some(placed)) =
            (self.composite.as_ref(), self.layers.get(&size), self.placed.get(&size))
        else {
            return;
        };
        for (layer, p) in set.iter().zip(placed).take(n) {
            if p.after {
                pass.composite(gpu, layer, target);
            }
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
            let placed = self.camera_stacks.placed.entry(size).or_default();
            placed.resize(placed.len().max(i + 1), Default::default());
            placed[i] = Placed { at: cam.world_position, inv_vp: cam.view_proj(aspect).inverse(), after: sc.post_after };
            self.render_world_into(
                &color,
                &depth,
                &cam,
                aspect,
                elapsed,
                sc.mask,
                None,
                size,
                crate::offscreen::OffscreenOpts { overlay: true, draws: sc.draws, ..Default::default() },
            );
        }
        plan.draw.len()
    }

    /// Render `base`'s stack and lay it straight over `target` — for a view
    /// whose own picture is already drawn. `depth` is that picture's depth and
    /// the camera it was drawn with, for the post still to come. Returns the
    /// layers' size and count, for [`CameraStacks::composite_after`].
    pub(crate) fn draw_camera_stack_over(
        &mut self,
        base: Entity,
        target: &wgpu::TextureView,
        size: (u32, u32),
        aspect: f32,
        elapsed: f32,
        depth: Option<(&wgpu::TextureView, &RenderCamera)>,
    ) -> ((u32, u32), usize) {
        let size = (size.0.max(1), size.1.max(1));
        let n = self.render_camera_stack(base, size, aspect, elapsed);
        if n > 0
            && let Some(gpu) = self.gpu.as_ref()
        {
            let depth = depth.map(|(d, cam)| (d, cam, aspect));
            self.camera_stacks.composite(gpu, size, n, target, depth);
        }
        (size, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A stacked camera's depth is converted into the base camera's.** A
    /// cockpit camera at the eye with its own field of view: a point 0.6 m
    /// ahead in its picture is 0.6 m ahead in the base camera's depth. Taken
    /// as it was, it would read as whatever distance the layer's own
    /// projection puts at that depth.
    #[test]
    fn a_stack_layer_s_depth_is_converted_to_the_base_camera_s() {
        use floptle_core::math::{DVec3, Quat, Vec4};
        let eye = DVec3::new(10.0, 2.0, -3.0);
        let rot = Quat::from_rotation_y(0.4);
        let base = RenderCamera::new(eye, rot, Projection::of_camera(1.0, false, 10.0, 0.1, 5000.0));
        let stack = RenderCamera::new(eye, rot, Projection::of_camera(0.6, false, 10.0, 0.05, 300000.0));
        let aspect = 1.5;
        let ahead = rot * floptle_core::math::Vec3::NEG_Z * 0.6;
        let clip = |c: &RenderCamera| {
            let p = c.view_proj(aspect) * Vec4::new(ahead.x, ahead.y, ahead.z, 1.0);
            p / p.w
        };
        let in_layer = clip(&stack);
        let m = layer_to_base(&base, aspect, stack.world_position, stack.view_proj(aspect).inverse());
        let got = m * Vec4::new(in_layer.x, in_layer.y, in_layer.z, 1.0);
        let want = clip(&base).z;
        assert!((got.z / got.w - want).abs() < 1e-4, "0.6 m ahead came out at depth {} in the base camera, not {want}", got.z / got.w);
        assert!((in_layer.z - want).abs() > 1e-3, "the two projections agree, so this proves nothing");
    }

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
                draws: floptle_core::CameraDraws::default(),
                post_after: false,
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

    /// A cockpit 0.6 m away on a stacked camera, the base camera focused on a
    /// wall 40 m off: how many pixels across the cockpit's edge are a mix of
    /// the two. Depth of field on, unless `dof` is false.
    #[cfg(feature = "editor-ui")]
    fn cockpit_edge_blend(dof: bool, post_after: bool) -> Option<usize> {
        use floptle_core::math::{DVec3, Vec3};
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
        // At the picture's own resolution: retro would run post at 240 rows
        // and shrink the blur to a pixel on the way down to this one.
        ed.project.retro = false;
        ed.project.layers = vec!["Default".to_string(), "Cockpit".to_string()];
        let shape = |ed: &mut crate::Editor, at: DVec3, scale: Vec3, color: [f32; 3]| {
            let e = ed.world.spawn();
            ed.world.insert(e, floptle_core::Transform { translation: at, scale, ..Default::default() });
            ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color });
            e
        };
        shape(&mut ed, DVec3::new(0.0, 0.0, -40.0), Vec3::new(200.0, 200.0, 1.0), [1.0, 0.0, 0.0]);
        // The left half of the view, its edge down the middle.
        let panel = shape(&mut ed, DVec3::new(-0.5, 0.0, -0.6), Vec3::new(1.0, 2.0, 0.05), [0.0, 1.0, 0.0]);
        ed.world.insert(panel, floptle_core::Layer("Cockpit".into()));
        let main = camera(&mut ed.world, "Main", &["Seat"]);
        let seat = camera(&mut ed.world, "Seat", &[]);
        for (e, mask) in [(main, 1u32), (seat, 1 << 1)] {
            ed.world.insert(e, floptle_core::Transform::default());
            if let Some(Matter::Camera { cull_mask, post_after: pa, .. }) = ed.world.get_mut::<Matter>(e) {
                *cull_mask = mask;
                *pa = post_after;
            }
        }
        // The scene's own chain, if it has one, and this one otherwise: the
        // first PostProcess node is the scene's.
        if !ed.world.query::<Matter>().any(|(_, m)| matches!(m, Matter::PostProcess { .. })) {
            let pp = ed.world.spawn();
            ed.world.insert(pp, Matter::default_post_process());
        }
        for (_, m) in ed.world.query_mut::<Matter>() {
            if let Matter::PostProcess { enabled, ao, dof_focus, dof_range, dof_max_blur, .. } = m {
                *enabled = true;
                *ao = floptle_core::AoMode::Off;
                *dof_focus = 40.0;
                *dof_range = 5.0;
                *dof_max_blur = if dof { 8.0 } else { 0.0 };
            }
        }
        let (w, h) = (64u32, 36u32);
        let cam = RenderCamera::new(DVec3::ZERO, Default::default(), Projection::of_camera(1.0, false, 1.0, 0.05, 300000.0));
        let px = crate::shot::render_frame_pixels(&mut ed, &cam, Some(main), w, h, 1, false).expect("no device");
        let row = (h / 2 * w * 4) as usize;
        Some((0..w as usize).filter(|x| px[row + x * 4] > 40 && px[row + x * 4 + 1] > 40).count())
    }

    /// **A stacked cockpit can be left sharp by depth of field.** Laid over
    /// with the picture, it is blurred by its own distance, 0.6 m against a
    /// focus 40 m off, like anything that near. `post = "after"` lays it over
    /// after depth of field, and its edge is as sharp as with none.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn a_stacked_camera_laid_over_after_depth_of_field_stays_sharp() {
        let Some(sharp) = cockpit_edge_blend(false, false) else { return };
        let blurred = cockpit_edge_blend(true, false).unwrap();
        let after = cockpit_edge_blend(true, true).unwrap();
        assert!(sharp <= 2, "with no depth of field the edge should be hard: {sharp} pixels mixed");
        assert!(blurred >= sharp + 3, "depth of field did not reach the stacked layer: {blurred} pixels mixed");
        assert!(after <= sharp, "laid over after depth of field, the edge still blurred: {after} pixels mixed");
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
