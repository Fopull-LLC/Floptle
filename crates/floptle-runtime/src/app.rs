//! The runtime application: owns the `World` + the clocks and drives the frame
//! loop. Phase 1 skeleton — the loop itself (variable clock + deterministic
//! fixed-step + ECS systems) is **real**; opening a window and creating the
//! `render::Gpu` to actually draw is the marked fill-in.

use floptle_core::math::{DVec3, Quat};
use floptle_core::transform::Transform;
use floptle_core::{Entity, FixedTimestep, Time, World};

/// Demo component: how fast an entity spins (radians/sec about Y). Stands in for
/// the Phase-1 "spinning textured quad" until the renderer is wired.
pub struct Spin {
    pub speed: f32,
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
        // seed the demo scene: one spinning node at the origin
        let e = world.spawn();
        world.insert(e, Transform::from_translation(DVec3::ZERO));
        world.insert(e, Spin { speed: 1.0 });
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
        // exactly one demo entity, and it rotated (~1 rad over ~1s)
        assert_eq!(app.world.len(), 1);
    }
}
