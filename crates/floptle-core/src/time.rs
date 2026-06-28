//! The engine clock and the fixed-step accumulator — the heartbeat the whole
//! frame loop hangs on (roadmap Phase 1).
//!
//! Two timesteps, deliberately separate:
//! - **Variable** (`Time::dt`): advances once per rendered frame; rendering,
//!   camera, and `on_update(dt)` scripts read it. Smooth, frame-rate dependent.
//! - **Fixed** (`FixedTimestep`): a determinism-preserving accumulator that
//!   yields a whole number of constant-`dt` ticks per frame; physics, the SDF
//!   sim, and `on_fixed_update` run on it so simulation is reproducible
//!   regardless of frame rate. (See `docs/subsystems/time.md`, ADR-0012.)
//!
//! Promoting the global scalar `t` to a per-entity rate field `r(p)` (`LocalTime`,
//! ADR-0017) lands in Phase 5 next to these; the clock is intentionally that seam.

/// Wall-clock-driven master clock. `tick(real_dt)` is called once per frame with
/// the measured elapsed seconds; everything else reads the cooked values.
#[derive(Debug, Clone, Copy)]
pub struct Time {
    /// Seconds since the previous frame (already clamped + scaled).
    pub dt: f32,
    /// Seconds since the clock started (sum of scaled `dt`s).
    pub elapsed: f64,
    /// Frames advanced since start.
    pub frame: u64,
    /// Global time scale (1.0 = real-time). Pauses/bullet-time multiply here;
    /// per-region rates (ADR-0017) layer on top later.
    pub scale: f32,
    /// Upper bound on a single frame's `real_dt` before scaling, so a stall (the
    /// debugger, a hitch) can't inject a huge step that explodes the sim.
    pub max_frame: f32,
}

impl Default for Time {
    fn default() -> Self {
        Self { dt: 0.0, elapsed: 0.0, frame: 0, scale: 1.0, max_frame: 0.25 }
    }
}

impl Time {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance the clock by one frame given the measured wall-clock delta.
    pub fn tick(&mut self, real_dt: f32) {
        let clamped = real_dt.clamp(0.0, self.max_frame);
        self.dt = clamped * self.scale;
        self.elapsed += self.dt as f64;
        self.frame += 1;
    }
}

/// Fixed-timestep accumulator: banks variable frame time and pays it out in
/// constant-size ticks so the simulation is deterministic and frame-rate
/// independent. Carries a `MAX_TICKS` ceiling so a long stall can't trigger a
/// "spiral of death" (each catch-up tick costing more than it buys).
#[derive(Debug, Clone, Copy)]
pub struct FixedTimestep {
    /// The constant simulation step, seconds (e.g. 1/60).
    pub step: f32,
    /// Unspent banked time.
    accumulator: f32,
    /// Hard cap on ticks emitted in one frame.
    max_ticks: u32,
}

impl FixedTimestep {
    /// `hz` is the simulation rate (ticks/second), e.g. `60.0`.
    pub fn new(hz: f32) -> Self {
        Self { step: 1.0 / hz, accumulator: 0.0, max_ticks: 8 }
    }

    /// Bank a frame's worth of time. Then call [`Self::tick`] in a `while` loop.
    pub fn accumulate(&mut self, frame_dt: f32) {
        self.accumulator += frame_dt;
        // clamp so a hitch doesn't queue hundreds of steps
        let ceil = self.step * self.max_ticks as f32;
        if self.accumulator > ceil {
            self.accumulator = ceil;
        }
    }

    /// Drain one fixed step if one is banked. Drive as:
    /// `while ft.tick() { world.fixed_update(ft.step); }`
    pub fn tick(&mut self) -> bool {
        if self.accumulator >= self.step {
            self.accumulator -= self.step;
            true
        } else {
            false
        }
    }

    /// Fraction `[0,1)` into the next step — for interpolating render state
    /// between two fixed simulation states (anti-stutter).
    pub fn alpha(&self) -> f32 {
        self.accumulator / self.step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_giant_frame() {
        let mut t = Time::new();
        t.tick(10.0); // a 10s stall
        assert!(t.dt <= t.max_frame);
        assert_eq!(t.frame, 1);
    }

    #[test]
    fn fixed_step_is_deterministic() {
        let mut ft = FixedTimestep::new(60.0);
        // ~3.5 steps of time -> exactly 3 ticks, remainder banked
        ft.accumulate(3.5 / 60.0);
        let mut ticks = 0;
        while ft.tick() {
            ticks += 1;
        }
        assert_eq!(ticks, 3);
        assert!(ft.alpha() > 0.0 && ft.alpha() < 1.0);
    }

    #[test]
    fn no_spiral_of_death() {
        let mut ft = FixedTimestep::new(60.0);
        ft.accumulate(100.0); // huge stall
        let mut ticks = 0;
        while ft.tick() {
            ticks += 1;
        }
        assert!(ticks <= 8, "catch-up must be capped, got {ticks}");
    }
}
