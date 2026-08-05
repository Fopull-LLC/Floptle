//! Where a frame's time actually went (`floptle/0077`).
//!
//! The engine used to keep a smoothed FPS number and nothing else. No
//! attribution: not per script, not per subsystem, not per draw. So when a game
//! got slow, the author's only available move was to file an engine ticket — and
//! that is not hypothetical, it is what happened four times:
//!
//! | filed as | actually was |
//! |---|---|
//! | `0059` "a crowded scene is unplayable" | component lookup was a linear scan |
//! | `0063` "cross-script wiring is slow" | `findScript` was a linear scan |
//! | `0071` "currently unplayable" | a scatter field asked for 117,000 props |
//! | `0074` "I can see through unloaded terrain" | mesh priority ignored world distance |
//!
//! Every one of those cost a round trip through the engine to discover a number
//! the game could have read itself. Three of the four were diagnosable from a
//! COUNT alone.
//!
//! # Deliberately not a profiler
//!
//! No sampling, no flamegraph, no call stacks. A fixed set of named buckets a
//! game author already has words for, plus per-script attribution — because the
//! question is never "what is the hot function", it is "which of MY scripts is
//! doing this".
//!
//! # Two numbers per bucket, never one
//!
//! A rolling mean and the worst of the last N frames. The spike is the thing
//! anybody is ever chasing, and a mean hides it: a 40 ms hitch once a second
//! adds under a millisecond to a 60-frame average.
//!
//! # Off means off, and says so
//!
//! Collection costs nothing when nothing is looking — a profiler that is itself a
//! frame cost gets turned off, and then it does not exist. But "off" must not
//! read as "fast": [`FrameProfile::bucket`] returns `None` while disabled rather
//! than zero, so a game asserting a budget in a smoke test cannot pass by
//! accident. That is the `floptle/0082` shape applied to this task's own API.

use std::collections::HashMap;

/// The named buckets a frame is divided into.
///
/// Fixed on purpose. These are the words a game author uses about their own
/// project, and a closed set means the editor's readout and the Lua surface
/// cannot disagree about what exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bucket {
    /// Lua: every pass (`update`, `fixedUpdate`, `lateUpdate`) plus the ECS sync
    /// around them. Broken down per script by [`FrameProfile::scripts`].
    Scripts,
    /// The rigid-body sim, including collision and the character controllers.
    Physics,
    /// Terrain residency, field generation and surface-net meshing.
    Terrain,
    /// Scatter: chunk sweeps, settling raycasts, instance building.
    Scatter,
    /// Particle simulation and the batches it produces.
    Particles,
    /// The audio mixer: voice updates, spatialisation and the effect chain.
    Audio,
    /// Skeletal animation: clip sampling, blending, pose composition, skinning.
    Animation,
    /// Game UI: layout solve, hit-testing, the element draw list.
    Ui,
    /// Everything from the gather to submit: instances, the raymarch, the raster
    /// passes, post.
    Render,
}

impl Bucket {
    /// Every bucket, in the order a readout should list them — roughly the order
    /// of a frame.
    pub const ALL: [Bucket; 9] = [
        Bucket::Scripts,
        Bucket::Physics,
        Bucket::Terrain,
        Bucket::Scatter,
        Bucket::Particles,
        Bucket::Audio,
        Bucket::Animation,
        Bucket::Ui,
        Bucket::Render,
    ];

    /// The name a script passes to `perf.ms(...)`, and the label a readout shows.
    ///
    /// camelCase-free because every one is a single lowercase word: these are
    /// what a game author calls them, not what the crates are called.
    pub fn name(self) -> &'static str {
        match self {
            Bucket::Scripts => "scripts",
            Bucket::Physics => "physics",
            Bucket::Terrain => "terrain",
            Bucket::Scatter => "scatter",
            Bucket::Particles => "particles",
            Bucket::Audio => "audio",
            Bucket::Animation => "animation",
            Bucket::Ui => "ui",
            Bucket::Render => "render",
        }
    }

    /// Resolve a name from a script. `None` for anything unrecognised — the
    /// caller turns that into an error naming the whole set, rather than
    /// answering zero for a typo (`floptle/0082`).
    pub fn from_name(name: &str) -> Option<Bucket> {
        Bucket::ALL.into_iter().find(|b| b.name() == name)
    }
}

/// How many frames the "worst recently" window covers.
///
/// One second at 60 fps. Long enough that a hitch does not scroll off before you
/// have read it, short enough that fixing one is visibly reflected.
pub const WINDOW: usize = 60;

/// The smoothing factor for the rolling mean, per frame.
///
/// The same 0.9/0.1 the FPS counter has always used, so the two move at the same
/// rate and a reader is not comparing a fast number with a slow one.
const SMOOTH: f32 = 0.9;

/// One bucket's history: a smoothed mean and a ring of recent samples.
#[derive(Clone, Debug, Default)]
struct Series {
    mean: f32,
    ring: Vec<f32>,
    next: usize,
}

impl Series {
    fn push(&mut self, ms: f32) {
        self.mean = if self.ring.is_empty() { ms } else { self.mean * SMOOTH + ms * (1.0 - SMOOTH) };
        if self.ring.len() < WINDOW {
            self.ring.push(ms);
        } else {
            self.ring[self.next] = ms;
            self.next = (self.next + 1) % WINDOW;
        }
    }

    fn worst(&self) -> f32 {
        self.ring.iter().copied().fold(0.0, f32::max)
    }
}

/// What one bucket cost: the rolling mean, and the worst frame of the last
/// [`WINDOW`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// Milliseconds, smoothed.
    pub ms: f32,
    /// Milliseconds, the worst single frame in the window. **This is the number
    /// worth looking at** — a hitch is invisible in the mean.
    pub worst_ms: f32,
}

/// The counts a frame produced.
///
/// Separate from the times because three of the four misdiagnosed tickets were
/// answerable from a count alone, and a count is free to keep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Scene nodes walked by the draw gather.
    pub nodes: usize,
    /// …rejected as off screen before any work was done for them
    /// (`floptle/0075`).
    pub culled: usize,
    /// Raster instances submitted, terrain and scatter included.
    pub instances: usize,
    /// Draw calls issued.
    pub draws: usize,
    /// Terrain chunks resident (meshed and in memory).
    pub chunks: usize,
    /// Scatter props submitted this frame. `0071` was a report of 117,000 of
    /// these and no way to see the number.
    pub props: usize,
    /// Live particles across every effect.
    pub particles: usize,
    /// Live one-shot particle effects — `spawnEffect` instances that have not
    /// finished. `particles` above is what they cost; this is how many asked
    /// (`floptle/0114`).
    pub effects: usize,
    /// …and how many a frame refused because the ceiling was already reached.
    /// Nonzero means the look is being cut, so it cannot be a number nobody can
    /// see.
    pub effects_dropped: usize,
    /// Placeable lights the shader was given, of the sixteen slots it has.
    pub lights: usize,
    /// …and how many were ranked out because more than sixteen qualified. A cap
    /// that says so is a good cap (`floptle/0116`).
    pub lights_dropped: usize,
    /// Audio voices mixing.
    pub voices: usize,
}

/// A frame's cost, per bucket and per script.
///
/// Written by the driver through [`FrameProfile::record`] / [`Self::begin`], read
/// by the editor readout and by Lua. Silent — and free — until [`Self::enable`].
#[derive(Clone, Debug, Default)]
pub struct FrameProfile {
    on: bool,
    series: HashMap<Bucket, Series>,
    /// This frame's accumulation, before `end_frame` folds it into `series`. A
    /// bucket may be timed in several pieces (physics runs per tick, scripts run
    /// three passes) and they all belong to one frame's figure.
    frame: HashMap<Bucket, f32>,
    /// Per script KIND — the file name, which is what a game author has words
    /// for. Same two-phase accumulation as the buckets.
    script_frame: HashMap<String, f32>,
    scripts: HashMap<String, Series>,
    counts: Counts,
    /// Frames folded since collection was turned on, so a reader can tell
    /// "nothing has happened yet" from "it costs nothing".
    frames: u64,
}

impl FrameProfile {
    /// Turn collection on or off. Off is the default and costs nothing.
    ///
    /// Turning it off CLEARS the history rather than freezing it: a stale mean
    /// from before a fix looks exactly like a fix that did not work.
    pub fn enable(&mut self, on: bool) {
        if self.on == on {
            return;
        }
        self.on = on;
        self.series.clear();
        self.scripts.clear();
        self.frame.clear();
        self.script_frame.clear();
        self.counts = Counts::default();
        self.frames = 0;
    }

    /// Is anything being collected?
    pub fn enabled(&self) -> bool {
        self.on
    }

    /// How many frames have been folded in since collection started. Zero means
    /// "ask again next frame", which is not the same as "it is free".
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Add `ms` to a bucket's total for the frame in progress. A no-op while
    /// disabled, which is what makes the instrumentation free.
    pub fn record(&mut self, bucket: Bucket, ms: f32) {
        if !self.on {
            return;
        }
        *self.frame.entry(bucket).or_default() += ms;
    }

    /// Add `ms` to one script kind's total, and to the `Scripts` bucket.
    ///
    /// Both, from one call, because the per-script figures summing to something
    /// other than the bucket is the kind of discrepancy that makes a reader
    /// distrust the whole readout.
    pub fn record_script(&mut self, kind: &str, ms: f32) {
        if !self.on {
            return;
        }
        *self.script_frame.entry(kind.to_owned()).or_default() += ms;
        *self.frame.entry(Bucket::Scripts).or_default() += ms;
    }

    /// Publish this frame's counts.
    pub fn set_counts(&mut self, counts: Counts) {
        if self.on {
            self.counts = counts;
        }
    }

    /// Fold the frame in progress into the history. Called once per rendered
    /// frame, after everything has reported.
    pub fn end_frame(&mut self) {
        if !self.on {
            return;
        }
        // Every bucket is pushed, including the ones that reported nothing —
        // otherwise a subsystem that went idle keeps its old mean forever and
        // reads as still costing what it used to.
        for b in Bucket::ALL {
            let ms = self.frame.remove(&b).unwrap_or(0.0);
            self.series.entry(b).or_default().push(ms);
        }
        // A script that stopped running this frame gets a zero for the same
        // reason, and a script that has been destroyed stops being listed at all.
        let names: Vec<String> = self.scripts.keys().cloned().collect();
        for name in names {
            if !self.script_frame.contains_key(&name) {
                self.scripts.entry(name).or_default().push(0.0);
            }
        }
        for (name, ms) in std::mem::take(&mut self.script_frame) {
            // A script seen for the FIRST time is back-filled with the frames it
            // was not running for. Without this its mean starts from its own first
            // sample while every bucket's started from frame one, and the two are
            // then not comparable — the per-script rows would not add up to the
            // `scripts` bucket, which is exactly the discrepancy that makes a
            // reader stop trusting a readout. It cost nothing in those frames, so
            // zero is the true value, not a filler.
            let series = self.scripts.entry(name).or_insert_with(|| {
                let mut s = Series::default();
                for _ in 0..self.frames.min(WINDOW as u64) {
                    s.push(0.0);
                }
                s
            });
            series.push(ms);
        }
        self.frames += 1;
    }

    /// What a bucket cost, or `None` while collection is off.
    ///
    /// `None` and not `Cost::default()`: a game asserting `perf.ms("scripts") <
    /// 4` must not pass because nothing was measured.
    pub fn bucket(&self, b: Bucket) -> Option<Cost> {
        if !self.on {
            return None;
        }
        let s = self.series.get(&b)?;
        Some(Cost { ms: s.mean, worst_ms: s.worst() })
    }

    /// What one script kind cost, or `None` while off / if that script has never
    /// run.
    pub fn script(&self, kind: &str) -> Option<Cost> {
        if !self.on {
            return None;
        }
        let s = self.scripts.get(kind)?;
        Some(Cost { ms: s.mean, worst_ms: s.worst() })
    }

    /// Every script that has run since collection started, **most expensive
    /// first** — which is the order the question is asked in.
    pub fn scripts(&self) -> Vec<(String, Cost)> {
        if !self.on {
            return Vec::new();
        }
        let mut v: Vec<(String, Cost)> = self
            .scripts
            .iter()
            .map(|(k, s)| (k.clone(), Cost { ms: s.mean, worst_ms: s.worst() }))
            .collect();
        v.sort_by(|a, b| b.1.ms.total_cmp(&a.1.ms).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// This frame's counts. Always available in shape; zeroed while off, which is
    /// safe here in a way it is not for the times — a count of zero nodes is
    /// obviously not a measurement.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The sum of the buckets' means, which is what the frame costs where the
    /// engine can see it. `None` while off.
    ///
    /// Not the same as the frame time: vsync, the OS and the GPU finishing are
    /// all outside every bucket. Presented as "accounted for" rather than
    /// "total" for exactly that reason — a readout claiming to add up to the
    /// frame time and not doing so is worse than one that never claimed it.
    pub fn accounted_ms(&self) -> Option<f32> {
        if !self.on {
            return None;
        }
        Some(Bucket::ALL.iter().filter_map(|b| self.bucket(*b)).map(|c| c.ms).sum())
    }
}

/// A scope timer: `let _t = Span::new();` … `p.record(bucket, t.ms())`.
///
/// Deliberately not a Drop guard holding a `&mut FrameProfile` — the profile
/// lives on the editor beside the things being measured, and a guard borrowing it
/// for the length of a subsystem would fight every other borrow in the frame.
pub struct Span(std::time::Instant);

impl Span {
    pub fn new() -> Self {
        Span(std::time::Instant::now())
    }

    /// Milliseconds since the span started.
    pub fn ms(&self) -> f32 {
        self.0.elapsed().as_secs_f32() * 1000.0
    }
}

impl Default for Span {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// While off, times read as ABSENT rather than zero — so a smoke test
    /// asserting a budget cannot pass because nothing was measured.
    ///
    /// This is the task's own API held to `floptle/0082`: the failure mode being
    /// designed out is a number that means "no data" and looks like "free".
    #[test]
    fn a_disabled_profile_answers_nothing_rather_than_zero() {
        let mut p = FrameProfile::default();
        assert!(!p.enabled());
        p.record(Bucket::Scripts, 12.0);
        p.record_script("player", 12.0);
        p.end_frame();
        assert_eq!(p.bucket(Bucket::Scripts), None, "a budget check must not pass on no data");
        assert_eq!(p.script("player"), None);
        assert_eq!(p.accounted_ms(), None);
        assert!(p.scripts().is_empty());
        assert_eq!(p.frames(), 0);
    }

    /// The mean smooths and the worst does not — the whole reason there are two
    /// numbers. A single 40 ms hitch in a second of 2 ms frames barely moves the
    /// average and is exactly what somebody is looking for.
    #[test]
    fn a_hitch_shows_in_the_worst_and_hides_in_the_mean() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..40 {
            p.record(Bucket::Physics, 2.0);
            p.end_frame();
        }
        p.record(Bucket::Physics, 40.0);
        p.end_frame();
        let c = p.bucket(Bucket::Physics).expect("enabled");
        assert!(c.ms < 8.0, "the mean should barely move: {}", c.ms);
        assert!((c.worst_ms - 40.0).abs() < 1e-3, "the worst must be the hitch: {}", c.worst_ms);
    }

    /// Several reports in one frame add up to that frame's figure, because
    /// physics runs per tick and scripts run three passes.
    #[test]
    fn pieces_of_one_frame_add_up() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record(Bucket::Physics, 1.0);
        p.record(Bucket::Physics, 2.0);
        p.record(Bucket::Physics, 3.0);
        p.end_frame();
        let c = p.bucket(Bucket::Physics).expect("enabled");
        assert!((c.worst_ms - 6.0).abs() < 1e-3, "three ticks of one frame: {}", c.worst_ms);
    }

    /// Per-script times are attributed BY NAME and add up to the scripts bucket.
    ///
    /// The bucket total disagreeing with the rows under it is the discrepancy
    /// that makes a reader stop trusting a readout, so one call does both.
    #[test]
    fn scripts_are_attributed_by_name_and_sum_to_their_bucket() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record_script("planet_walker", 3.0);
        p.record_script("vessel_controller", 5.0);
        p.record_script("pulsate", 0.5);
        p.end_frame();
        let bucket = p.bucket(Bucket::Scripts).expect("enabled");
        assert!((bucket.worst_ms - 8.5).abs() < 1e-3, "the rows must sum to the bucket: {bucket:?}");
        // Most expensive first: that is the order the question is asked in.
        let rows = p.scripts();
        assert_eq!(rows[0].0, "vessel_controller");
        assert_eq!(rows[1].0, "planet_walker");
        assert_eq!(rows[2].0, "pulsate");
        assert!((p.script("pulsate").unwrap().worst_ms - 0.5).abs() < 1e-3);
    }

    /// A subsystem that goes idle drops to zero instead of keeping its old mean.
    ///
    /// Otherwise fixing something looks like not fixing it: the number that made
    /// you look would sit there unchanged forever.
    #[test]
    fn an_idle_bucket_decays_instead_of_remembering() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..30 {
            p.record(Bucket::Scatter, 20.0);
            p.record_script("forest", 20.0);
            p.end_frame();
        }
        assert!(p.bucket(Bucket::Scatter).unwrap().ms > 10.0);
        for _ in 0..200 {
            p.end_frame();
        }
        let c = p.bucket(Bucket::Scatter).unwrap();
        assert!(c.ms < 0.1, "an idle bucket still reads {} ms", c.ms);
        assert!(c.worst_ms < 0.1, "…and its window has rolled past the spike");
        assert!(
            p.script("forest").unwrap().ms < 0.1,
            "a script that stopped running still reads busy"
        );
    }

    /// Turning collection off and on again starts clean.
    #[test]
    fn re_enabling_does_not_resurrect_an_old_mean() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..30 {
            p.record(Bucket::Render, 30.0);
            p.end_frame();
        }
        p.enable(false);
        p.enable(true);
        assert_eq!(p.bucket(Bucket::Render), None, "no samples yet, so no answer yet");
        assert_eq!(p.frames(), 0);
        p.record(Bucket::Render, 1.0);
        p.end_frame();
        assert!(p.bucket(Bucket::Render).unwrap().worst_ms < 2.0, "the old 30 ms came back");
    }

    /// Every bucket has a name, the names are unique, and they round-trip — so
    /// the editor's labels, the docs and `perf.ms("…")` cannot drift apart.
    #[test]
    fn every_bucket_name_round_trips_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in Bucket::ALL {
            assert!(seen.insert(b.name()), "two buckets called {}", b.name());
            assert_eq!(Bucket::from_name(b.name()), Some(b));
        }
        assert_eq!(Bucket::ALL.len(), seen.len());
        // A typo resolves to nothing, so the caller can name the whole set
        // instead of quietly measuring something else.
        assert_eq!(Bucket::from_name("scripting"), None);
        assert_eq!(Bucket::from_name("Scripts"), None);
    }

    /// The per-script rows add up to the `scripts` bucket **even for a script
    /// that started running late**.
    ///
    /// The means are exponentially smoothed, so two series that began on
    /// different frames are not comparable — a script discovered on frame 30
    /// would report its full cost while the bucket that has been averaging since
    /// frame 1 reported a fraction of it, and the rows would visibly not add up to
    /// the total above them. That discrepancy is what makes a reader stop trusting
    /// a readout, so a new script is back-filled with the frames it was idle for.
    #[test]
    fn a_script_that_appears_late_still_sums_into_its_bucket() {
        let mut p = FrameProfile::default();
        p.enable(true);
        for _ in 0..30 {
            p.record_script("always", 1.0);
            p.end_frame();
        }
        // A script spawned thirty frames in — a prefab, a scene load, a pickup.
        for _ in 0..30 {
            p.record_script("always", 1.0);
            p.record_script("latecomer", 4.0);
            p.end_frame();
        }
        let bucket = p.bucket(Bucket::Scripts).expect("on").ms;
        let rows: f32 = p.scripts().iter().map(|(_, c)| c.ms).sum();
        assert!(
            (bucket - rows).abs() < bucket * 0.02,
            "the rows must add up to the bucket: bucket {bucket}, rows {rows}"
        );
    }

    /// The accounted total is the buckets added up, and it is not claimed to be
    /// the frame time.
    #[test]
    fn the_accounted_total_is_the_sum_of_the_buckets() {
        let mut p = FrameProfile::default();
        p.enable(true);
        p.record(Bucket::Scripts, 2.0);
        p.record(Bucket::Render, 4.0);
        p.end_frame();
        let sum: f32 = Bucket::ALL.iter().filter_map(|b| p.bucket(*b)).map(|c| c.ms).sum();
        assert!((p.accounted_ms().unwrap() - sum).abs() < 1e-4);
        assert!((p.accounted_ms().unwrap() - 6.0).abs() < 1e-3);
    }
}
