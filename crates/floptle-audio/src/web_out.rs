//! The browser's audio output: a Web Audio scheduler of our own.
//!
//! cpal has a WebAudio backend, and this deliberately does not use it. That
//! backend keeps one cursor for where the next chunk of sound starts and
//! advances it by exactly one buffer each time a chunk finishes playing,
//! **with no clamp against the context clock**. Two consequences follow, and
//! a playtest heard both:
//!
//! * The whole cushion is one buffer — about 43 ms — and the callbacks run on
//!   the page's main thread, the same thread as the frame. One long task
//!   (importing a model, baking an occluder, compiling a shader, a
//!   collection) overruns it.
//! * Once overrun, the cursor is behind the clock **for good**. Every later
//!   chunk is scheduled in the past, which Web Audio plays immediately, so
//!   the queue never refills. What began as one dropout becomes a permanent
//!   flutter.
//!
//! Measured in a real window on 2026-09-05: a healthy lead of 85–93 ms, one
//! stall around the 500th callback, and 8–40 ms for the rest of the run. That
//! is the "flickery and odd" a player hears, and it never recovers.
//!
//! The fix is [`plan`], which is the one thing the other implementation does
//! not do: if the cursor has fallen behind, move it forward to just ahead of
//! the clock before scheduling anything. A stall then costs one gap instead
//! of every gap afterwards. Everything else here is the ordinary Web Audio
//! scheduling pattern — a timer that keeps a fixed span of sound queued ahead
//! of the clock, rather than a chain that reacts to sound having run out.
//!
//! This is not an [`AudioWorklet`], which would put the mix on the browser's
//! own audio thread and end the starvation rather than tolerate it. That
//! needs shared memory, which needs the two cross-origin isolation headers,
//! which a game on itch.io cannot set. Until that changes, the mix runs on
//! the main thread and the cushion is what protects it.
//!
//! [`AudioWorklet`]: https://developer.mozilla.org/docs/Web/API/AudioWorklet

/// Frames per scheduled chunk. A multiple of the mixer's own block, and the
/// same size cpal's backend uses, so the per-chunk cost is a known quantity.
pub(crate) const CHUNK: usize = 2048;

/// How far ahead of the clock to keep sound queued. This is the cushion a
/// main-thread stall eats into, and it is also added latency, so it is a
/// compromise rather than a maximum: long enough to sit out an asset import,
/// short enough that a gunshot still feels like one.
pub(crate) const LOOKAHEAD_SECS: f64 = 0.11;

/// Where the queue restarts after it has genuinely run dry, far enough out
/// that the chunk which restarts it is not itself late.
pub(crate) const RESTART_LEAD_SECS: f64 = 0.045;

/// How close to the clock the queue may come before it counts as having run
/// out. Only a hair, because **a thin cushion is not a dropout**: scheduling
/// 10 ms ahead is still continuous sound, and the next pump refills it to the
/// lookahead with no gap at all. This is only wide enough to cover rendering
/// and handing over the chunk we are about to schedule. Treating a thin
/// cushion as a stall would manufacture a gap every time the page was merely
/// busy — measured at roughly one a second in a real game, which is the very
/// flutter this module exists to stop.
pub(crate) const GUARD_SECS: f64 = 0.005;

/// How often to top the queue up. Several times per cushion, so a single
/// missed tick is not itself a dropout.
pub(crate) const PUMP_MS: i32 = 20;

/// What one pump should do.
#[derive(Debug, PartialEq)]
pub(crate) struct Plan {
    /// Where the next chunk starts, on the context clock.
    pub(crate) cursor: f64,
    /// How many chunks to render and schedule now.
    pub(crate) chunks: usize,
    /// The cursor was behind the clock and has been moved forward — a stall
    /// long enough to drain the queue. Worth counting; not worth panicking.
    pub(crate) resynced: bool,
}

/// Decide what to schedule, given where the queue ended and what time it is.
///
/// `cursor` is `None` before anything has been scheduled. `step` is one
/// chunk's duration in seconds.
///
/// **The clamp is the whole point of this module.** A cursor left in the past
/// schedules sound that Web Audio plays the instant it is asked to, which
/// silently converts the queue into a treadmill running with no slack at all.
pub(crate) fn plan(cursor: Option<f64>, now: f64, step: f64) -> Plan {
    let restart = now + RESTART_LEAD_SECS;
    let (mut cursor, resynced) = match cursor {
        // The queue ran out: what was missed is gone, and scheduling it now
        // would ask the browser to play it late, which it does immediately —
        // leaving no cushion and doing it all again next time. Restart ahead
        // of the clock instead, and count the gap.
        Some(c) if c < now + GUARD_SECS => (restart, true),
        // Still ahead of the clock, however thinly: carry straight on, which
        // is continuous sound, and the fill below rebuilds the cushion.
        Some(c) => (c, false),
        // Nothing scheduled yet — starting here is not a resync.
        None => (restart, false),
    };
    let target = now + LOOKAHEAD_SECS;
    let mut chunks = 0;
    while cursor < target {
        chunks += 1;
        cursor += step;
    }
    Plan { cursor, chunks, resynced }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use imp::{WebStream, start};

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::cell::RefCell;
    use std::rc::Rc;

    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::Closure;

    use super::{CHUNK, PUMP_MS, plan};

    /// The mix, as the pump holds it: one call fills one chunk, interleaved.
    type Render = Box<dyn FnMut(&mut [f32])>;

    /// A running output. Dropping it stops the timer and closes the context,
    /// which is what silences a game that has been shut down.
    pub(crate) struct WebStream {
        ctx: web_sys::AudioContext,
        interval: i32,
        sample_rate: u32,
        /// Kept alive for exactly as long as the timer that calls it.
        _pump: Closure<dyn FnMut()>,
    }

    impl WebStream {
        /// The context's own rate. Asked rather than chosen: forcing a rate
        /// makes the browser resample every sample we ever mix.
        pub(crate) fn sample_rate(&self) -> u32 {
            self.sample_rate
        }
    }

    impl Drop for WebStream {
        fn drop(&mut self) {
            if let Some(w) = web_sys::window() {
                w.clear_interval_with_handle(self.interval);
            }
            let _ = self.ctx.close();
        }
    }

    /// Everything the pump owns. One `RefCell` rather than several: the pump
    /// is the only borrower, and it never re-enters itself.
    struct Pump {
        ctx: web_sys::AudioContext,
        /// Round-robin `AudioBuffer`s. A buffer is only rewritten once every
        /// `pool.len()` chunks, which is longer than any of them stays
        /// queued — writing one that is still waiting to play would corrupt
        /// sound already promised to the speaker.
        pool: Vec<web_sys::AudioBuffer>,
        next_buffer: usize,
        channels: usize,
        step: f64,
        cursor: Option<f64>,
        interleaved: Vec<f32>,
        channel: Vec<f32>,
        render: Render,
        resyncs: u64,
    }

    /// Open the page's audio output and start pumping it.
    ///
    /// `make` is handed the context's real sample rate and returns the
    /// renderer, because the mixer has to be built for the rate the browser
    /// turns out to be running at.
    pub(crate) fn start<M, F>(channels: usize, make: M) -> Result<WebStream, String>
    where
        M: FnOnce(u32) -> F,
        F: FnMut(&mut [f32]) + 'static,
    {
        let window = web_sys::window().ok_or("no window to open audio on")?;
        let ctx = web_sys::AudioContext::new().map_err(|_| "the browser refused an AudioContext".to_string())?;
        let rate = ctx.sample_rate();
        if !(rate.is_finite() && rate > 0.0) {
            return Err(format!("the audio context reported a nonsense sample rate ({rate})"));
        }
        let sample_rate = rate as u32;
        let step = CHUNK as f64 / rate as f64;

        // Enough buffers that one is never rewritten while it is still
        // queued: the queue holds at most `LOOKAHEAD / step` chunks, and this
        // is comfortably more.
        let pool_len = (super::LOOKAHEAD_SECS / step).ceil() as usize + 3;
        let mut pool = Vec::with_capacity(pool_len);
        for _ in 0..pool_len {
            pool.push(
                ctx.create_buffer(channels as u32, CHUNK as u32, rate)
                    .map_err(|_| "the browser refused an audio buffer".to_string())?,
            );
        }

        let pump = Rc::new(RefCell::new(Pump {
            ctx: ctx.clone(),
            pool,
            next_buffer: 0,
            channels,
            step,
            cursor: None,
            interleaved: vec![0.0; CHUNK * channels],
            channel: vec![0.0; CHUNK],
            render: Box::new(make(sample_rate)),
            resyncs: 0,
        }));

        let tick = pump.clone();
        let closure = Closure::wrap(Box::new(move || {
            tick.borrow_mut().pump();
        }) as Box<dyn FnMut()>);
        let interval = window
            .set_interval_with_callback_and_timeout_and_arguments_0(
                closure.as_ref().unchecked_ref(),
                PUMP_MS,
            )
            .map_err(|_| "the browser refused an audio timer".to_string())?;

        // A page that has had its click can start immediately; one that has
        // not stays suspended, and the clock does not advance until it does,
        // so the pump simply has nothing to do until then.
        let _ = ctx.resume();
        // The queue deliberately starts EMPTY. Opening the audio happens
        // partway through booting a game, and the rest of that boot — parsing
        // scenes, importing models, baking occluders — is one long task
        // holding the main thread. Filling the queue here would mean the
        // first timer tick arrives seconds late to a queue that drained, and
        // reporting that as a stall would be a warning on every launch. The
        // first tick after boot finds nothing scheduled, which is not a
        // dropout, and starts the cushion there.

        Ok(WebStream { ctx, interval, sample_rate, _pump: closure })
    }

    impl Pump {
        fn pump(&mut self) {
            let now = self.ctx.current_time();
            let p = plan(self.cursor, now, self.step);
            if p.resynced {
                self.resyncs += 1;
                // Every one of these is an audible gap, and the cause is
                // always the same: the page did not get back to the audio in
                // time. Say so on the first, then rarely, because a stuttering
                // machine must not also flood the console.
                if self.resyncs == 1 || self.resyncs.is_multiple_of(32) {
                    log::warn!(
                        "audio: the page stalled and the sound queue ran dry ({} time(s)) — \
                         resynchronised",
                        self.resyncs
                    );
                }
            }
            let mut cursor = match p.chunks {
                0 => return,
                _ => p.cursor - p.chunks as f64 * self.step,
            };
            for _ in 0..p.chunks {
                self.render_one();
                if !self.schedule(cursor) {
                    return;
                }
                cursor += self.step;
            }
            self.cursor = Some(cursor);
        }

        /// Mix one chunk into the interleaved scratch.
        fn render_one(&mut self) {
            self.interleaved.fill(0.0);
            (self.render)(&mut self.interleaved);
        }

        /// Copy the scratch into a pooled buffer and schedule it at `at`.
        /// `false` if the browser refused, which ends this pump.
        fn schedule(&mut self, at: f64) -> bool {
            let buffer = &self.pool[self.next_buffer];
            self.next_buffer = (self.next_buffer + 1) % self.pool.len();
            for ch in 0..self.channels {
                for (i, s) in self.channel.iter_mut().enumerate() {
                    *s = self.interleaved[i * self.channels + ch];
                }
                // wasm-bindgen hands out a copy of a channel rather than a
                // reference to it, so the write has to go the other way.
                if buffer.copy_to_channel(&self.channel, ch as i32).is_err() {
                    log::warn!("audio: the browser refused an audio buffer write");
                    return false;
                }
            }
            let Ok(source) = self.ctx.create_buffer_source() else {
                log::warn!("audio: the browser refused an audio source");
                return false;
            };
            source.set_buffer(Some(buffer));
            if source.connect_with_audio_node(&self.ctx.destination()).is_err()
                || source.start_with_when(at).is_err()
            {
                log::warn!("audio: the browser refused to play a chunk of sound");
                return false;
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One chunk at 48 kHz.
    const STEP: f64 = CHUNK as f64 / 48_000.0;

    /// Nothing scheduled yet: start ahead of the clock, and that is not a
    /// resync — there was nothing to fall behind.
    #[test]
    fn the_first_pump_fills_the_queue_without_calling_it_a_stall() {
        let p = plan(None, 10.0, STEP);
        assert!(!p.resynced);
        assert!(p.chunks >= 2, "a cushion, not one chunk: {p:?}");
        assert!(p.cursor >= 10.0 + LOOKAHEAD_SECS, "filled to the lookahead: {p:?}");
        assert!(
            p.cursor - p.chunks as f64 * STEP >= 10.0 + RESTART_LEAD_SECS - 1e-9,
            "and the first chunk is not itself late: {p:?}"
        );
    }

    /// The queue is still ahead of the clock: top it up, no resync, and never
    /// schedule more than the cushion.
    #[test]
    fn a_healthy_queue_is_only_topped_up() {
        let now = 10.0;
        let p = plan(Some(now + LOOKAHEAD_SECS), now, STEP);
        assert_eq!(p.chunks, 0, "already full: {p:?}");
        assert!(!p.resynced);
        let p = plan(Some(now + 0.05), now, STEP);
        assert!(!p.resynced, "0.05 s ahead is ahead: {p:?}");
        assert_eq!(p.chunks, 2, "two chunks reach the lookahead: {p:?}");
    }

    /// **A thin cushion is not a dropout.** The page was busy, the queue is
    /// down to 13 ms, and that 13 ms of sound is still going to play on time.
    /// Restarting the queue here would throw away the join and put a hole in
    /// continuous audio — one per busy moment, about once a second in a real
    /// game, which is the flutter rather than the cure. Carry on from where
    /// the queue ended and rebuild the cushion behind it.
    #[test]
    fn a_thin_cushion_is_carried_on_from_rather_than_broken() {
        let now = 10.0;
        let p = plan(Some(now + 0.013), now, STEP);
        assert!(!p.resynced, "13 ms ahead is still ahead: {p:?}");
        let first = p.cursor - p.chunks as f64 * STEP;
        assert!(
            (first - (now + 0.013)).abs() < 1e-9,
            "the next chunk butts against the last one, so there is no hole: {first}"
        );
        assert!(p.cursor >= now + LOOKAHEAD_SECS, "and the cushion is back: {p:?}");
    }

    /// **The bug this module exists for.** The page stalled, the clock ran
    /// past the queue, and the cursor is now in the past. Scheduling there
    /// plays immediately and leaves no cushion at all, which is what made a
    /// single dropout permanent. It must jump forward instead.
    #[test]
    fn a_cursor_left_behind_by_a_stall_is_moved_back_in_front_of_the_clock() {
        // 300 ms of main-thread stall: the queue ended at 10.0, it is now 10.3.
        let p = plan(Some(10.0), 10.3, STEP);
        assert!(p.resynced, "the stall is reported: {p:?}");
        let first = p.cursor - p.chunks as f64 * STEP;
        assert!(first >= 10.3 + RESTART_LEAD_SECS - 1e-9, "the next chunk is in the FUTURE: {p:?}");
        assert!(p.chunks >= 1 && p.cursor >= 10.3 + LOOKAHEAD_SECS, "and the cushion is rebuilt: {p:?}");

        // Exactly out of sound counts as out of sound: a chunk scheduled at
        // the clock itself cannot be rendered and handed over in zero time.
        assert!(plan(Some(10.3), 10.3, STEP).resynced);
        // The pump after a stall is healthy again: no second gap.
        let after = plan(Some(p.cursor), 10.32, STEP);
        assert!(!after.resynced, "recovered: {after:?}");
    }
}
