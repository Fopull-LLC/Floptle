//! Holding a frame to a steady rate (`app.setFrameCap`), and choosing the
//! render scale from the GPU's own timings (`app.setDynamicResolution`).
//!
//! A game whose GPU cost swings with the view misses the display's period on
//! some frames and not others, and each miss shows a frame for two or three
//! refreshes while the world moves `speed × dt` per frame: at 40 m/s that is
//! metres of judder. Two fixes, both the game's to choose: a lower rate it can
//! always make, held on a steady clock; or fewer pixels whenever the GPU
//! falls behind, so it stops falling behind.

impl crate::Editor {
    /// Hold this frame until its slot under the frame cap.
    ///
    /// Slept to within a millisecond, then spun, because a sleep alone
    /// oversleeps by the scheduler's granularity and a cap that lands late
    /// half the time is the judder it was set to remove. The slots advance by
    /// the period from the last one, not from now, so the rate holds exactly;
    /// a frame that ran more than a whole period late starts the clock again
    /// rather than rushing the frames after it.
    ///
    /// Only for a game in a window: headless verbs run as fast as they can.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn pace_frame(&mut self) {
        use std::time::{Duration, Instant};
        if !self.playing || self.frame_cap <= 0.0 || self.window.is_none() {
            self.frame_next = None;
            return;
        }
        let period = Duration::from_secs_f64(1.0 / self.frame_cap as f64);
        let now = Instant::now();
        let slot = frame_slot(self.frame_next, now, period);
        if slot > now {
            let wait = slot - now;
            if wait > Duration::from_micros(1500) {
                std::thread::sleep(wait - Duration::from_millis(1));
            }
            while Instant::now() < slot {
                std::hint::spin_loop();
            }
        }
        self.frame_next = Some(slot + period);
    }

    /// A browser paces frames itself (`requestAnimationFrame`).
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn pace_frame(&mut self) {}

    /// The period a frame has, in milliseconds: the cap's, else the display's,
    /// 0 when neither is known.
    pub(crate) fn frame_period_ms(&self) -> f32 {
        if self.frame_cap > 0.0 { 1000.0 / self.frame_cap } else { self.refresh_period * 1000.0 }
    }

    /// Whether GPU timings are wanted for dynamic resolution.
    pub(crate) fn wants_gpu_for_resolution(&self) -> bool {
        self.playing && self.dyn_res.is_some()
    }

    /// Feed one frame's GPU time to dynamic resolution, and take the scale it
    /// chooses.
    pub(crate) fn feed_dynamic_resolution(&mut self, gpu_ms: f32) {
        let period = self.frame_period_ms();
        let dt = (self.frame_ms / 1000.0).clamp(0.0, 0.25);
        if let Some(d) = self.dyn_res.as_mut()
            && let Some(s) = d.update(gpu_ms, period, dt)
        {
            self.project.render_scale = s;
        }
    }
}

/// When a capped frame may start: its booked slot, unless that slot is more
/// than a period gone, in which case now. Late by less than a period, the
/// frame starts at once but keeps the slot, so the next is still booked a
/// period after it and the rate holds on average.
#[cfg(not(target_arch = "wasm32"))]
fn frame_slot(
    booked: Option<std::time::Instant>,
    now: std::time::Instant,
    period: std::time::Duration,
) -> std::time::Instant {
    match booked {
        Some(t) if now < t + period => t,
        _ => now,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::frame_slot;
    use std::time::{Duration, Instant};

    /// **A capped game draws at the cap, on a steady clock.** Frames that
    /// finish early wait for their slot; one that runs a little late keeps the
    /// schedule, so the rate holds; one that runs more than a period late
    /// starts the clock again rather than rushing the next frames through.
    #[test]
    fn capped_frames_keep_their_slots_and_a_long_stall_restarts_the_clock() {
        let period = Duration::from_micros(13_889); // 72 fps
        let t0 = Instant::now();
        // Early: wait for the booked slot.
        assert_eq!(frame_slot(Some(t0 + period), t0 + Duration::from_millis(3), period), t0 + period);
        // A touch late: the booked slot stands, so the next stays on the grid.
        let late = t0 + period + Duration::from_millis(4);
        assert_eq!(frame_slot(Some(t0 + period), late, period), t0 + period);
        // A stall of several periods: from now.
        let stalled = t0 + period * 5;
        assert_eq!(frame_slot(Some(t0 + period), stalled, period), stalled);
        // Over a run of frames that each finish early, the slots sit a period apart.
        let mut booked = None;
        let mut starts = Vec::new();
        let mut now = t0;
        for _ in 0..10 {
            let s = frame_slot(booked, now, period);
            starts.push(s);
            booked = Some(s + period);
            now = s.max(now) + Duration::from_millis(5); // the frame's own work
        }
        for w in starts.windows(2) {
            assert_eq!(w[1] - w[0], period, "the cap drifted");
        }
    }
}
