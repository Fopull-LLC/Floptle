//! The engine clock and the fixed-step accumulator — the heartbeat the whole
//! frame loop hangs on (roadmap Phase 1).
//!
//! Two timesteps, separate:
//! - **Variable** (`Time::dt`): advances once per rendered frame; rendering,
//!   camera, and `on_update(dt)` scripts read it. Smooth, frame-rate dependent.
//! - **Fixed** (`FixedTimestep`): a determinism-preserving accumulator that
//!   yields a whole number of constant-`dt` ticks per frame; physics, the SDF
//!   sim, and `on_fixed_update` run on it so simulation is reproducible
//!   regardless of frame rate.

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
    /// per-region rates layer on top.
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

impl Default for FixedTimestep {
    /// The engine's default gameplay tick rate: 60 Hz (`docs/multiplayer.md` §3 —
    /// parry-tight input granularity; per-project configurable later).
    fn default() -> Self {
        Self::new(60.0)
    }
}

impl FixedTimestep {
    /// `hz` is the simulation rate (ticks/second), e.g. `60.0`.
    pub fn new(hz: f32) -> Self {
        Self { step: 1.0 / hz, accumulator: 0.0, max_ticks: 8 }
    }

    /// Reset banked time (e.g. on Play start, so the first frame doesn't inherit
    /// a stale accumulator).
    pub fn reset(&mut self) {
        self.accumulator = 0.0;
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

// --- Falling behind -------------------------------------------------------------

/// What a frame does when the fixed tick costs more than the time it simulates.
///
/// With a 60 Hz tick that costs 30 ms, no amount of catching up can hold real
/// time: every catch-up tick makes the frame longer, which banks more ticks for
/// the next frame. [`FixedTimestep`]'s clamp bounds that queue but not the loop,
/// so the game settles at the worst rate it can reach. Freeflier's main level
/// did exactly this: 2 fps, and the loading screen never lifted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverloadPolicy {
    /// Run at most one tick per rendered frame while overloaded. The game plays
    /// in slow motion at a frame rate that can still be looked at and debugged.
    #[default]
    SlowMotion,
    /// Keep draining every banked tick, as before. For a session whose clock is
    /// shared with other machines, where running slow would desync it.
    CatchUp,
}

/// Watches what each tick really costs and says when the game can't keep up.
///
/// Fed once per frame with the ticks it ran and the wall time they took. The
/// per-tick cost is smoothed, so one hitch (a level streaming in, a shader
/// compiling) is not an overload but a sustained cost above the slice is. It
/// takes a clearly lower cost to leave the state than to enter it, so a game
/// sitting on the line doesn't flip in and out every few frames.
#[derive(Clone, Debug, Default)]
pub struct TickLoad {
    /// Smoothed milliseconds per tick.
    avg_ms: f32,
    overloaded: bool,
    /// Wall seconds the current overload has lasted.
    for_s: f32,
    /// Frames fed since construction or the last reset.
    frames: u32,
    /// Consecutive frames whose own per-tick cost was over the slice.
    over_run: u32,
}

/// What changed on one [`TickLoad::record`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadChange {
    Entered,
    Left,
}

impl TickLoad {
    /// Smoothing weight of the newest frame.
    const WEIGHT: f32 = 0.2;
    /// Leave the state only once a tick is back under this share of its slice.
    const LEAVE_BELOW: f32 = 0.8;
    /// Frames needed before a verdict: the first few after Play are the level
    /// loading, not the game.
    const SETTLE_FRAMES: u32 = 5;
    /// Consecutive over-slice frames it takes to enter the state. One hitch is
    /// one frame; a game that can't keep up is every frame.
    const ENTER_RUN: u32 = 6;

    /// Fold in one frame: `ticks` ticks took `spent_ms` of wall time, each
    /// simulating `step_s` seconds; the frame itself was `frame_s` of wall time.
    pub fn record(&mut self, ticks: u32, spent_ms: f32, step_s: f32, frame_s: f32) -> Option<LoadChange> {
        if ticks == 0 {
            return None;
        }
        let per = spent_ms / ticks as f32;
        self.frames = self.frames.saturating_add(1);
        self.avg_ms = if self.frames == 1 { per } else { self.avg_ms + (per - self.avg_ms) * Self::WEIGHT };
        let slice = step_s * 1000.0;
        self.over_run = if per > slice { self.over_run.saturating_add(1) } else { 0 };
        if self.overloaded {
            self.for_s += frame_s;
            if self.avg_ms < slice * Self::LEAVE_BELOW {
                self.overloaded = false;
                self.for_s = 0.0;
                return Some(LoadChange::Left);
            }
        } else if self.frames >= Self::SETTLE_FRAMES
            && self.over_run >= Self::ENTER_RUN
            && self.avg_ms > slice
        {
            self.overloaded = true;
            self.for_s = 0.0;
            return Some(LoadChange::Entered);
        }
        None
    }

    /// Is the tick costing more than the time it simulates?
    pub fn overloaded(&self) -> bool {
        self.overloaded
    }

    /// The smoothed cost of one tick, in milliseconds.
    pub fn tick_ms(&self) -> f32 {
        self.avg_ms
    }

    /// Wall seconds the current overload has lasted; zero when not overloaded.
    pub fn overloaded_for(&self) -> f32 {
        self.for_s
    }

    /// Forget everything, for a new Play session.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

impl FixedTimestep {
    /// Drop whatever is banked beyond the part of a step already in progress,
    /// keeping `alpha`. What [`OverloadPolicy::SlowMotion`] does after the one
    /// tick it allows.
    pub fn drop_backlog(&mut self) {
        if self.accumulator >= self.step {
            self.accumulator %= self.step;
        }
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;

    const STEP: f32 = 1.0 / 60.0;

    /// A tick that costs double its slice is an overload once it has lasted a
    /// few frames, and stops being one only once it is well under the slice.
    #[test]
    fn a_sustained_cost_over_the_slice_is_an_overload_and_one_hitch_is_not() {
        let mut l = TickLoad::default();
        // Settled and cheap.
        for _ in 0..10 {
            assert_eq!(l.record(1, 5.0, STEP, STEP), None);
        }
        // One 200 ms hitch moves the average but not past the slice for good.
        let hitch = l.record(1, 200.0, STEP, 0.2);
        let after: Vec<_> = (0..10).filter_map(|_| l.record(1, 5.0, STEP, STEP)).collect();
        assert_eq!((hitch, after.as_slice()), (None, &[][..]), "a single hitch read as an overload");

        let mut entered = 0;
        for _ in 0..20 {
            if l.record(8, 8.0 * 31.0, STEP, 0.25) == Some(LoadChange::Entered) {
                entered += 1;
            }
        }
        assert_eq!(entered, 1, "entered once, not every frame");
        assert!(l.overloaded() && l.tick_ms() > 25.0, "tick {}", l.tick_ms());
        assert!(l.overloaded_for() > 1.0);

        // Just under the slice is not enough to leave: the line needs margin.
        for _ in 0..30 {
            l.record(1, 16.0, STEP, 0.03);
        }
        assert!(l.overloaded(), "left the state sitting on the line");
        let mut left = 0;
        for _ in 0..30 {
            if l.record(1, 6.0, STEP, STEP) == Some(LoadChange::Left) {
                left += 1;
            }
        }
        assert_eq!(left, 1);
        assert!(!l.overloaded() && l.overloaded_for() == 0.0);
    }

    #[test]
    fn dropping_the_backlog_keeps_the_step_in_progress() {
        let mut t = FixedTimestep::new(60.0);
        t.accumulate(STEP * 5.5);
        assert!(t.tick());
        t.drop_backlog();
        assert!(!t.tick(), "a banked tick survived");
        assert!((t.alpha() - 0.5).abs() < 1e-3, "alpha {}", t.alpha());
    }
}

// --- The wall clock ------------------------------------------------------------
//
// `std::time::Instant::now()` compiles for `wasm32-unknown-unknown` and panics
// when called ("time not implemented on this platform"), so every subsystem
// that times itself — the profiler, the mesher, physics, the runner — would
// take the browser build down on its first frame. `web-time` is the drop-in:
// the same API over `performance.now()` in a page, and a re-export of `std`'s
// own types everywhere else, so on the desktop this is exactly what it was.
//
// The browser CI gate (`tools/web/clippy.toml`) refuses `std::time::Instant`
// in the engine half outright; reach for these instead.

/// A monotonic timestamp: `std::time::Instant` on every native target, and
/// `performance.now()` in a browser.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
/// A monotonic timestamp: `std::time::Instant` on every native target, and
/// `performance.now()` in a browser.
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;

/// The wall clock: `std::time::SystemTime` natively, `Date.now()` in a browser.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::SystemTime;
/// The wall clock: `std::time::SystemTime` natively, `Date.now()` in a browser.
#[cfg(target_arch = "wasm32")]
pub use web_time::SystemTime;

/// The epoch [`SystemTime`] counts from — the one that matches it.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::UNIX_EPOCH;
/// The epoch [`SystemTime`] counts from — the one that matches it.
#[cfg(target_arch = "wasm32")]
pub use web_time::UNIX_EPOCH;

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
