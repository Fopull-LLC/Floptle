//! The runtime application: owns the `World` + the clocks and drives the frame
//! loop. Phase 1 skeleton — the loop itself (variable clock + deterministic
//! fixed-step + ECS systems) is **real**; opening a window and creating the
//! `render::Gpu` to actually draw is the marked fill-in.

use floptle_core::math::{DVec3, Quat};
use floptle_core::transform::Transform;
use floptle_core::{Entity, FixedTimestep, Time, World};

/// Demo component: how fast an entity spins (radians/sec about Y).
pub struct Spin {
    pub speed: f32,
}

/// Which procedural primitive an entity renders as. Kept render-agnostic (no GPU
/// handle) so the world stays free of renderer types; the runner maps a `Shape` to
/// a registered `MeshId`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Cube,
    Sphere,
}

impl Shape {
    /// Stable index used by the runner to look up the registered mesh handle.
    pub fn index(self) -> usize {
        self as usize
    }
}

/// A drawable: which shape, and a flat tint color (the lit base color).
pub struct Renderable {
    pub shape: Shape,
    pub color: [f32; 3],
}

pub struct App {
    pub world: World,
    pub time: Time,
    pub fixed: FixedTimestep,
    // pub gpu: Option<render::Gpu>,  // <- Phase 1 fill-in: created when the window opens
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let mut world = World::new();

        // Seed the demo scene: a spinning cube at the origin, a still sphere to one
        // side (a clean read on the directional light), and a counter-spinning cube
        // to the other — at distinct world positions so depth + parallax are visible.
        let scene: [(DVec3, Shape, f32, [f32; 3]); 3] = [
            (DVec3::new(0.0, 0.0, 0.0), Shape::Cube, 0.8, [0.95, 0.45, 0.35]),
            (DVec3::new(2.6, 0.0, 0.0), Shape::Sphere, 0.2, [0.40, 0.70, 0.95]),
            (DVec3::new(-2.6, 0.0, 0.0), Shape::Cube, -0.6, [0.55, 0.85, 0.45]),
        ];
        for (pos, shape, speed, color) in scene {
            let e = world.spawn();
            world.insert(e, Transform::from_translation(pos));
            world.insert(e, Spin { speed });
            world.insert(e, Renderable { shape, color });
        }

        Self { world, time: Time::new(), fixed: FixedTimestep::new(60.0) }
    }

    /// Advance one frame. This is exactly the shape the windowed loop will call
    /// from `redraw`: cook the clock, run variable-rate systems for this frame's
    /// `dt`, then drain the fixed-step accumulator for the deterministic systems.
    pub fn frame(&mut self, real_dt: f32) {
        self.time.tick(real_dt);
        self.update(self.time.dt);

        self.fixed.accumulate(self.time.dt);
        while self.fixed.tick() {
            self.fixed_update(self.fixed.step);
        }
    }

    /// Variable-rate systems (render-facing). The spin system here is the seed of
    /// the Phase-1 demo.
    fn update(&mut self, dt: f32) {
        let spins: Vec<(Entity, f32)> =
            self.world.query::<Spin>().map(|(e, s)| (e, s.speed)).collect();
        for (e, speed) in spins {
            if let Some(t) = self.world.get_mut::<Transform>(e) {
                t.rotation *= Quat::from_rotation_y(speed * dt);
            }
        }
    }

    /// Fixed-rate systems (physics / SDF sim) — deterministic. Bodies land in Phase 6.
    fn fixed_update(&mut self, _fixed_dt: f32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_advances_clock_and_spins() {
        let mut app = App::new();
        for _ in 0..60 {
            app.frame(1.0 / 60.0);
        }
        assert_eq!(app.time.frame, 60);
        assert!((app.time.elapsed - 1.0).abs() < 1e-3);
        // the three seeded demo entities are all live after a second of frames
        assert_eq!(app.world.len(), 3);
    }
}
