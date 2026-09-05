//! `floptle run` — **does the project actually work?**
//!
//! Runs a project's scripts and physics for a bounded stretch of simulated
//! time, with no window and no GPU, and reports what happened.
//!
//! Until this existed, nothing could answer that question without a person.
//! `--play` opens a window, so a project's scripts could not be run by CI at
//! all; `floptle check` proves the files load, which is a different and much
//! weaker claim than "the game runs without raising".
//!
//! ## It is the editor's own play loop
//!
//! `Editor::play_step` — the whole of Play mode: the scene-transition queue,
//! script hot-reload, the frame pass, the fixed-rate tick pass, physics, the
//! script host's command queues — turns out to touch neither the GPU nor egui.
//! So this verb does not reimplement playing a game; it opens the project the
//! way the editor opens it, presses Play, and calls the same function the frame
//! loop calls, with a fixed `dt`.
//!
//! ## Time is fixed, on purpose
//!
//! `--seconds`/`--frames` are converted to a whole number of steps at a fixed
//! `dt`, never read off the wall clock. A verb whose answer changes between two
//! runs of the same project is not worth having: the point is to be able to say
//! "this raised" and be believed. It does mean a bug that only appears at a
//! particular frame rate will not appear here, which is the honest cost and is
//! said out loud rather than discovered.
//!
//! ## What a headless run does not have
//!
//! **No rendering, so nothing that depends on a drawn frame happens.** Models
//! are not registered (`import_model` bails without a GPU), so anything reading
//! back a *rendered* mesh sees nothing. Physics is unaffected — a mesh
//! collider's triangles are read from the file, not from the GPU registry — and
//! so are scripts, input actions, terrain and the tick pass.
//!
//! **No input.** Every key reads as up and every action as inactive, so a
//! project whose `start` waits for a press will sit still and report nothing
//! wrong. That is correct and is also why "no errors" from this verb means
//! "nothing raised", not "the game is good".
//!
//! **`perf.counts()` is honest about the same gap.** `scripts` and `physics`
//! are real numbers here — they cost the same whether or not anything ever
//! draws them. `draws`, `instances`, `lights`, `nodes` and the rest of the
//! render-gather counts stay `0`, for the same reason nothing above draws:
//! there is no gather to have counted them. That is a real "not measured
//! here", not the bug `floptle/0167` was — a project asserting a script or
//! physics budget in CI gets a real answer from `run`; one asserting on draw
//! calls or light counts wants `floptle shot` or `--play` instead.
//!
//! ## `--timing`: the one thing here that IS a wall clock
//!
//! The paragraph above says the span never comes off the clock, and it still
//! does not — `--timing` changes nothing about how far the run goes or what it
//! answers. It only reports what the steps COST: a distribution of real
//! milliseconds per step, p50/p95/p99/max.
//!
//! **A distribution, not a mean**, and for the reason `present_stats` prints
//! one: a mean cannot tell a steady frame from one that is fine four times out
//! of five and stalls on the fifth, and a collector pause is exactly that
//! shape. It is why this exists at all — the ADR-0028 VM comparison needs
//! frame p95 on a real game, and `--seconds` reports simulated time, which is
//! the same number on both VMs by construction.
//!
//! What is inside the measurement is the engine's frame: world streaming and
//! `play_step`. The runner's own bookkeeping — draining the log, folding the
//! profiler — is outside it, so the number is the game's cost and not this
//! file's. What is NOT inside it is anything a window would have done: no
//! render, no present, no vsync. A step here is the CPU half of a frame.

use std::path::Path;

use crate::console::ConsoleState;

/// The fixed step. The engine's gameplay tick is 60 Hz and a script's `update`
/// runs once per frame, so one step per tick is the cadence that makes
/// Where in a run the `--alloc` window sits.
///
/// Not the first frames: opening a project builds the world, and a frame that
/// is building is not the frame anybody is asking about. Not the last, either,
/// so the collector is running normally again before the run ends.
struct AllocWindow {
    start: u32,
    end: u32,
    at: std::cell::Cell<usize>,
}

impl AllocWindow {
    /// The shortest span with room for a warm-up, a window and a tail.
    ///
    /// Below this there is nowhere to put a window that measures a settled
    /// frame, and a window of one or two frames measures noise. Refusing is
    /// the point: a number that quietly described the opening frames of a run
    /// would be worse than no number.
    const MIN_SPAN: u32 = 12;

    fn plan(asked: u32) -> Option<Self> {
        if asked < Self::MIN_SPAN {
            return None;
        }
        let start = asked / 4;
        let end = start + asked / 2;
        debug_assert!(start >= 1 && end > start && end < asked);
        Some(Self { start, end, at: std::cell::Cell::new(0) })
    }

    fn frames(&self) -> u32 {
        self.end - self.start
    }
}

/// `--frames` and `--seconds` mean the same thing at the same rate.
const DT: f32 = 1.0 / 60.0;

/// How long to run for.
pub(crate) enum Span {
    Frames(u32),
    Seconds(f32),
}

impl Span {
    fn steps(&self) -> u32 {
        match self {
            Span::Frames(n) => *n,
            // Rounded, and never zero: `--seconds 0.001` asking for no
            // simulation at all would be a confusing way to spell `check`.
            Span::Seconds(t) => ((t / DT).round() as i64).clamp(1, u32::MAX as i64) as u32,
        }
    }
}

/// What the steps cost, in real milliseconds.
///
/// Built from every sample rather than kept as a running mean, because the
/// percentiles are the point: `floptle/0176` describes 2415 frames out of ~5100
/// over 8 ms, which a mean of the same run reports as comfortable.
///
/// ## Why a paused step is not a sample
///
/// A step is not a frame. A session held at the start of Play while the terrain
/// worker builds the ground steps happily with `dt = 0` — that is the same
/// stepped-but-not-simulated gap `floptle/0157` was, and `summary_line` already
/// says it out loud. Those steps are cheap and they are not gameplay, so
/// counting them here would answer "what does a frame of this game cost" with a
/// distribution a third of which is the loading screen.
///
/// It is not only an understatement, it is one whose SIZE varies: the hold ends
/// when the terrain does, so two runs of one project — let alone two builds of
/// the engine — pause for different numbers of steps and put their p95 at
/// different points of the real workload. A comparison drawn between two such
/// runs is measuring the terrain worker.
///
/// So the paused ones are counted and excluded, and the count is reported. Not
/// dropped silently: a caller who sees 400 of 900 steps excluded has learned
/// something true about the run.
struct Timing {
    /// One entry per step that ADVANCED the session clock, in the order they ran.
    samples: Vec<f32>,
    /// Steps that were taken without the clock moving. Reported, never averaged in.
    paused: u32,
}

impl Timing {
    fn new(capacity: u32) -> Self {
        Timing { samples: Vec::with_capacity(capacity as usize), paused: 0 }
    }

    /// Record one step. `advanced` is whether the session clock moved.
    fn push(&mut self, ms: f32, advanced: bool) {
        if advanced {
            self.samples.push(ms);
        } else {
            self.paused += 1;
        }
    }

    /// The sorted samples. Sorting a copy, so `samples` keeps the order the
    /// steps ran in for anything that later wants to see a trend.
    fn sorted(&self) -> Vec<f32> {
        let mut v = self.samples.clone();
        // `total_cmp`, not `partial_cmp().unwrap()`: a NaN sample would panic
        // there, and a timing probe that takes the process down is worse than
        // one that reports a strange number.
        v.sort_by(f32::total_cmp);
        v
    }

    fn mean(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().sum::<f32>() / self.samples.len() as f32
    }
}

/// The `p`th percentile of an ascending slice, by nearest rank.
///
/// Nearest rank rather than interpolation: every value it can return is a
/// sample that actually happened, which is what makes "p95 was 6.4 ms" a
/// statement about a frame rather than about arithmetic. Empty is 0.0 — a run
/// with no steps has no p95, and there is nowhere here to say so; the caller
/// prints the sample count beside it.
fn pct(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    // ceil(p × n), clamped into the slice: p95 of 100 samples is the 95th, p95
    // of 3 samples is the 3rd, and p0 is the 1st rather than an index of -1.
    let rank = (p * sorted.len() as f32).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// The verb's flags, as one value. Each is described on its command-line
/// flag in `cli.rs`; the short of it:
///
/// * `steam` (`--steam`) is the explicit opt-in that lets this verb talk to a
///   real Steam client (Spacewar 480 if the project sets no app id) — off by
///   default, so an ordinary run (CI included) never tries to reach one.
/// * `timing` (`--timing`) collects a real-milliseconds sample per step and
///   reports the distribution. It does not change what the run does or how far
///   it goes — see the module docs.
/// * `alloc` (`--alloc`) measures Lua-heap allocation per frame, in total and
///   per script, with the collector stopped across a mid-run window.
/// * `seed` (`--seed`) pins the game's randomness so two runs are the same run.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Options {
    pub(crate) json: bool,
    pub(crate) steam: bool,
    pub(crate) timing: bool,
    pub(crate) alloc: bool,
    pub(crate) seed: Option<u32>,
}

/// Run `root` for `span`. Returns the process exit code.
pub(crate) fn run(root: &Path, scene: Option<&str>, span: Span, opts: Options) -> i32 {
    let Options { json, steam, timing, alloc, seed } = opts;
    if !root.join("project.ron").is_file() {
        eprintln!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }

    let mut ed = crate::Editor {
        // Nothing draws, so gizmos and overlays would only cost work.
        show_gizmos: false,
        // The Console is the report. Mirroring to stderr as well would print
        // every line twice under --json, once as noise beside the document.
        console: ConsoleState { mirror_to_stderr: false, ..Default::default() },
        ..Default::default()
    };
    ed.open_project(root.to_path_buf());
    // `--seed`: pin `math.random` and the no-seed `rng()` form before the first
    // script runs, so two runs of a game that re-randomises its cast per run
    // are the same game — the only condition under which their `--timing` or
    // `--alloc` figures can be compared. The Lua state is built once per
    // editor and survives Play, so setting it here reaches the whole run.
    if let Some(seed) = seed {
        ed.script_host.set_seed(seed);
    }
    // Resolved from the project's OWN project.ron, not whatever `ed` cached
    // while opening — `open_project` doesn't hand the config back, and this
    // is a small file, cheap to read again.
    let cfg = floptle_scene::load_project(&root.join("project.ron"));
    if let Some(app_id) = crate::steam_boot::resolve_app_id(cfg.steam, steam)
        && let Some(platform) = crate::steam_boot::boot(app_id, false)
    {
        ed.script_host.set_platform(platform);
    }
    if let Some(s) = scene {
        let Some(path) = crate::inspect::resolve_scene(root, s) else {
            eprintln!("no scene called {s} under {}", root.join("scenes").display());
            return 1;
        };
        ed.open_scene_file(&path.to_string_lossy());
    }

    // **Take the open phase out, do not count it.** Everything the open said —
    // a scene that arrived with bad wiring, a package that failed to load —
    // belongs in the report: it happened, and it happened before a script ran.
    // But pressing Play CLEARS the Console (`toggle_play`, so a session shows
    // only its own output), which means remembering "the first N entries were
    // the open" both loses them and mislabels the first N of the run as though
    // they were them. Moving them somewhere Play cannot reach is exact.
    let opened: Vec<crate::console::ConsoleEntry> = std::mem::take(&mut ed.console.entries);

    ed.toggle_play();
    if !ed.playing {
        eprintln!("the project did not enter play mode");
        return 1;
    }
    let asked = span.steps();
    // **Counted, not assumed.** The report used to publish the number that was
    // asked for, which is an echo of the command line rather than an
    // observation — and the loop can end early, so the two are not the same
    // number. A field a caller reads to know how far the run got has to have
    // been measured by the thing that got there.
    let mut steps = 0u32;
    // **The clock, not the step count.** A step is not a promise that anything
    // moved: a paused session — which is what the Play-start terrain hold makes
    // one until the ground exists — steps happily with `dt = 0`. Reporting
    // `steps × DT` therefore published a span the run had NOT simulated, and it
    // was the confident kind of wrong: 3600 steps, "60.00s of simulated time",
    // and a world where `time` never left zero (`floptle/0157`). `play_t` is the
    // clock the scripts themselves read, so it cannot disagree with them.
    let t0 = ed.play_t;
    // Allocated up front, before the first step, so the loop never grows a Vec
    // inside the region it is timing.
    let mut clock = timing.then(|| Timing::new(asked));
    // `--alloc`: how much Lua heap a frame makes. Measured across a window in
    // the middle of the run — after the opening frames, which allocate the
    // world rather than a steady frame — with the collector STOPPED, because
    // it cannot be measured with the collector running (see
    // `ScriptHost::gc_stop`).
    let window = alloc.then(|| AllocWindow::plan(asked)).flatten();
    if alloc && window.is_none() {
        eprintln!(
            "--alloc needs at least {} steps to measure a settled frame; this run is {asked}, \
             so no allocation figure is reported",
            AllocWindow::MIN_SPAN
        );
    }
    let mut allocated: Option<f64> = None;
    // …and which scripts made it: bytes per frame per script kind, from the
    // same window, sampled around each hook call while the collector is off.
    let mut by_script: Vec<(String, f64)> = Vec::new();
    for step in 0..asked {
        if let Some(w) = &window {
            if step == w.start {
                ed.script_host.gc_collect();
                ed.script_host.gc_stop();
                ed.script_host.track_alloc(true);
            } else if step == w.end {
                let grew = ed
                    .script_host
                    .lua_used_memory()
                    .saturating_sub(w.at.get());
                let frames = w.frames() as f64;
                allocated = Some(grew as f64 / frames);
                by_script = ed
                    .script_host
                    .alloc_by_script()
                    .into_iter()
                    .map(|(kind, bytes)| (kind, bytes as f64 / frames))
                    .collect();
                ed.script_host.track_alloc(false);
                ed.script_host.gc_restart();
                ed.script_host.gc_collect();
            }
            // Sampled AFTER the collect+stop above, on the same step.
            if step == w.start {
                w.at.set(ed.script_host.lua_used_memory());
            }
        }
        // The clock BEFORE the step, so the step can be asked afterwards whether
        // it was a frame of the game or a frame of the loading hold.
        let was = ed.play_t;
        let began = floptle_core::time::Instant::now();
        // The streaming half of a frame, which this loop is otherwise missing.
        // Without it the Play-start terrain hold never lifts, and a held session
        // is a PAUSED one: no fixed tick, so no rails, no physics, and a `dt` of
        // zero handed to every script. The run still counted its steps and still
        // reported its full span of simulated time — it had simply simulated
        // none of it, which is the one failure a verb built to be believed must
        // not have. See `pump_world_streaming`.
        ed.pump_world_streaming();
        ed.play_step(DT, true);
        // **The engine's frame, and nothing after it.** Everything below —
        // draining the log, folding the profiler — is this file's bookkeeping,
        // and charging the game for it would make the number depend on how
        // chatty the project's `print`s are.
        if let Some(c) = clock.as_mut() {
            c.push(began.elapsed().as_secs_f32() * 1000.0, ed.play_t > was);
        }
        steps += 1;
        // The same drain the editor's frame does, for the same reason it does it
        // per frame rather than at the end: the host holds every line until
        // somebody asks, and a long run of a script that logs each step would
        // otherwise grow that buffer without limit. The Console it drains INTO
        // merges consecutive repeats into a count and caps its history, so
        // draining early is what keeps a ten-thousand-step run cheap.
        //
        // It is not what makes the log arrive — the drain after Stop below would
        // collect it all anyway. What was missing before was any drain at all,
        // which is why a run of a project raising on every step reported
        // "nothing raised".
        ed.drain_script_logs();
        // Fold this step into the profiler's history, the way `Editor::render`
        // folds one windowed frame (`floptle/0167`). Without this, `perf.ms`
        // and friends read every bucket as "no frame has completed yet" for
        // the whole run — enabled, and silently wrong in the same shape the
        // whole `perf` API exists to refuse: a value that reads as zero and
        // means "never measured". `record`/`record_script` already run during
        // `play_step` (scripts, physics), so this is the one missing piece,
        // not new instrumentation. Counts that come from a GPU gather (draws,
        // instances, lights…) stay at their true value here: `run` has no
        // renderer at all, so zero is what they honestly are, not a bug.
        ed.script_host.profile().borrow_mut().end_frame();
        // A script that asked to quit has said the run is over, and stepping a
        // stopped session further would report on a world nobody is in.
        if !ed.playing {
            break;
        }
    }
    // Stop the way the editor stops, so teardown runs and anything it reports
    // (a queue that outlived its session, a host that failed to reset) is in
    // the report rather than lost with the process.
    if ed.playing {
        ed.toggle_play();
    }
    // …and once more, for anything the teardown itself said.
    ed.drain_script_logs();

    let simulated = (ed.play_t - t0).max(0.0);
    report(
        &opened,
        &ed.console,
        steps,
        asked,
        simulated,
        Measured { clock: clock.as_ref(), allocated, by_script: &by_script, seed },
        json,
    )
}

/// The one line a run ends with.
///
/// Its own function because it is the sentence a caller believes without
/// checking anything else, and it has to be readable by a test that asserts
/// what it says. `simulated` is MEASURED off the session clock; `steps × DT` is
/// only what the loop was asked to step.
fn summary_line(steps: u32, asked: u32, simulated: f32, errors: usize, warnings: usize) -> String {
    let stepped = steps as f32 * DT;
    let mut ran = format!("ran {steps} step(s), {simulated:.2}s of simulated time");
    if steps < asked {
        // The session ended before the span did. Said out loud, because a run
        // that stopped early and a run that finished report the same way
        // otherwise, and only one of them answered the question that was asked.
        ran.push_str(&format!(" — the session ended after {steps} of {asked}"));
    }
    // Stepped but not simulated: the session was paused for some of it. Named
    // rather than smoothed over, because a run that advanced nothing looks
    // exactly like a game whose world was never built, and whoever is reading
    // this is about to go looking for the wrong thing.
    if stepped - simulated > 1e-3 {
        ran.push_str(&format!(
            " — PAUSED for {:.2}s of that, which was stepped but not simulated",
            stepped - simulated
        ));
    }
    match (errors, warnings) {
        (0, 0) => format!("{ran} — nothing raised"),
        (0, w) => format!("{ran} — {w} warning(s)"),
        (e, 0) => format!("{ran} — {e} error(s)"),
        (e, w) => format!("{ran} — {e} error(s), {w} warning(s)"),
    }
}

/// The line `--timing` adds.
///
/// Its own function for the same reason `summary_line` is one: it is a sentence
/// a reader believes without checking anything else, so a test has to be able
/// to read it.
fn timing_line(c: &Timing) -> String {
    let sorted = c.sorted();
    // Nothing simulated. Saying so is the whole answer — printing four zeros
    // would be a frame cost of nothing, which is the "reads as zero, means
    // never measured" shape this file already refuses elsewhere.
    if sorted.is_empty() {
        return format!(
            "step cost: nothing to time — all {} step(s) were stepped without advancing the clock",
            c.paused
        );
    }
    let mut line = format!(
        "step cost over {} simulating step(s): p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, \
         max {:.2} ms (mean {:.2} ms)",
        sorted.len(),
        pct(&sorted, 0.50),
        pct(&sorted, 0.95),
        pct(&sorted, 0.99),
        pct(&sorted, 1.0),
        c.mean(),
    );
    if c.paused > 0 {
        line.push_str(&format!(
            " — {} paused step(s) not counted, which is the loading hold and not a frame",
            c.paused
        ));
    }
    line
}

fn level_str(l: floptle_script::LogLevel) -> &'static str {
    match l {
        floptle_script::LogLevel::Error => "error",
        floptle_script::LogLevel::Warn => "warning",
        floptle_script::LogLevel::Debug => "print",
    }
}

/// Print what happened. Returns the exit code: 1 if anything raised, in either
/// phase — a scene that could not be wired is as much a failure as a script
/// that threw, and a caller checking one exit code has to hear about both.
/// What a run MEASURED, as opposed to what it did — each half present only
/// when it was asked for, so "not measured" and "measured as zero" stay
/// different things all the way to the report.
struct Measured<'a> {
    clock: Option<&'a Timing>,
    /// Bytes of Lua heap a frame allocates (`--alloc`).
    allocated: Option<f64>,
    /// …and how much of that each script kind allocated inside its hook calls,
    /// bytes per frame, largest first. Empty when `--alloc` was not asked for
    /// or nothing was attributed.
    by_script: &'a [(String, f64)],
    /// `--seed`, when given — echoed so a report says which run it describes.
    seed: Option<u32>,
}

/// How many scripts the text report names under `--alloc`. The JSON carries
/// them all; a person wants the ones that matter and a count of the rest.
const ALLOC_LINES: usize = 8;

fn report(
    opened: &[crate::console::ConsoleEntry],
    console: &ConsoleState,
    steps: u32,
    asked: u32,
    simulated: f32,
    measured: Measured<'_>,
    json: bool,
) -> i32 {
    let Measured { clock, allocated, by_script, seed } = measured;
    use floptle_script::LogLevel;
    let all = || opened.iter().map(|e| ("open", e)).chain(console.entries.iter().map(|e| ("play", e)));
    let errors = all().filter(|(_, e)| e.level == LogLevel::Error).count();
    let warnings = all().filter(|(_, e)| e.level == LogLevel::Warn).count();

    if json {
        let lines: Vec<serde_json::Value> = all()
            .map(|(phase, e)| {
                let mut o = serde_json::json!({
                    "level": level_str(e.level),
                    "message": e.msg,
                    // Repeats are merged at ingest, so a per-frame line is one
                    // entry with a count rather than a flood.
                    "count": e.count,
                    // Which side of Play it happened on. A scene that arrived
                    // broken and a script that raised on frame 40 are different
                    // problems and a caller should not have to guess.
                    "phase": phase,
                });
                if let Some((file, line)) = &e.source {
                    o["source"] = serde_json::json!({ "file": file, "line": line });
                }
                o
            })
            .collect();
        let mut doc = serde_json::json!({
            "ok": errors == 0,
            // What actually ran, and what was asked for. They differ when the
            // session ended early, and a caller reading only the first number
            // would think it got the run it requested.
            "steps": steps,
            "requested": asked,
            // MEASURED off the session clock, not `steps × DT`: a paused
            // session steps without advancing, and this field is what a caller
            // reads to know whether anything happened (`floptle/0157`).
            "seconds": simulated,
            // What the loop stepped. The two differ exactly when the session
            // was paused for some of the run, and a caller comparing them can
            // see that without parsing a sentence.
            "stepped": steps as f32 * DT,
            "errors": errors,
            "warnings": warnings,
            "log": lines,
        });
        // Present only under `--timing`, and absent rather than zeroed when it
        // was not asked for: a reader who finds `p95_ms: 0` in a document has
        // been told a frame took no time, which is the "reads as zero, means
        // never measured" shape the whole `perf` API exists to refuse
        // (`floptle/0167`).
        if let Some(c) = clock {
            let sorted = c.sorted();
            doc["timing"] = serde_json::json!({
                // How many steps this distribution is OF — the simulating ones.
                // `steps` above is every step the loop took, and the two differ
                // by exactly `paused`, so a caller can see the split without
                // parsing a sentence.
                "samples": sorted.len(),
                "paused": c.paused,
                "mean_ms": c.mean(),
                "p50_ms": pct(&sorted, 0.50),
                "p95_ms": pct(&sorted, 0.95),
                "p99_ms": pct(&sorted, 0.99),
                "max_ms": pct(&sorted, 1.0),
            });
        }
        // Absent, not zero, when it was not asked for — the rule the timing
        // block follows: a `bytes_per_frame: 0` reads as "allocates nothing",
        // which is a claim, and the wrong one.
        if let Some(a) = allocated {
            let attributed: f64 = by_script.iter().map(|(_, b)| b).sum();
            doc["alloc"] = serde_json::json!({
                "bytes_per_frame": a,
                "kb_per_frame": a / 1024.0,
                // Per script kind, largest first — what the total is made of.
                // What no hook call accounts for (the scene mirror, UI, the
                // host's own bookkeeping) is the difference, given as a number
                // rather than left to be worked out.
                "by_script": by_script
                    .iter()
                    .map(|(kind, b)| serde_json::json!({
                        "script": kind,
                        "bytes_per_frame": b,
                        "kb_per_frame": b / 1024.0,
                    }))
                    .collect::<Vec<_>>(),
                "unattributed_bytes_per_frame": (a - attributed).max(0.0),
            });
        }
        // Present only when a seed was given: absent means "this run drew its
        // randomness from the clock", which a comparison must know.
        if let Some(seed) = seed {
            doc["seed"] = serde_json::json!(seed);
        }
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return i32::from(errors > 0);
    }

    // The same collapsing `check` does, for the same reason and through the same
    // function. Opening a real project reports one hidden panel once per child:
    // fifty-two lines saying one thing, in front of the one line that matters.
    // The Console already merges repeats that are ADJACENT (`ConsoleState::push`);
    // this catches the ones separated by other output.
    let mut groups: Vec<(&str, String, &crate::console::ConsoleEntry, u32)> = Vec::new();
    for (phase, e) in all() {
        let key = crate::console::repeat_shape(&e.msg);
        match groups.iter_mut().find(|(p, k, f, _)| *p == phase && *k == key && f.level == e.level) {
            Some(g) => g.3 += e.count,
            None => groups.push((phase, key, e, e.count)),
        }
    }
    for (phase, _, e, count) in &groups {
        let repeat = if *count > 1 { format!(" (x{count})") } else { String::new() };
        match &e.source {
            Some((file, line)) => {
                println!("{}: {phase}: {file}:{line}: {}{repeat}", level_str(e.level), e.msg)
            }
            None => println!("{}: {phase}: {}{repeat}", level_str(e.level), e.msg),
        }
    }
    println!("{}", summary_line(steps, asked, simulated, errors, warnings));
    if let Some(c) = clock {
        println!("{}", timing_line(c));
    }
    if let Some(a) = allocated {
        println!(
            "scripts allocate {:.1} KB per frame of Lua heap — measured with the collector \
             stopped, which is the only way it CAN be measured (with it running, a collection \
             inside the window eats part of what the window allocated)",
            a / 1024.0
        );
        for line in alloc_lines(a, by_script) {
            println!("{line}");
        }
    }
    if let Some(seed) = seed {
        println!("seeded with {seed}: math.random and rng() are pinned, so this run repeats");
    }
    i32::from(errors > 0)
}

/// The per-script lines under the `--alloc` total: the largest few, a count of
/// the rest, and what no script hook accounts for.
///
/// Its own function so a test can read what it says. Names are script kinds —
/// what the author calls them — and the figures are per frame like the total,
/// so the column can be read against it directly.
fn alloc_lines(total: f64, by_script: &[(String, f64)]) -> Vec<String> {
    let mut out = Vec::new();
    if by_script.is_empty() {
        return out;
    }
    let width = by_script.iter().take(ALLOC_LINES).map(|(k, _)| k.len()).max().unwrap_or(0);
    let width = width.max("(outside any script hook)".len());
    for (kind, b) in by_script.iter().take(ALLOC_LINES) {
        out.push(format!("  {kind:<width$}  {:>7.1} KB/frame", b / 1024.0));
    }
    if by_script.len() > ALLOC_LINES {
        let rest: f64 = by_script.iter().skip(ALLOC_LINES).map(|(_, b)| b).sum();
        out.push(format!(
            "  {:<width$}  {:>7.1} KB/frame",
            format!("({} more scripts)", by_script.len() - ALLOC_LINES),
            rest / 1024.0
        ));
    }
    let attributed: f64 = by_script.iter().map(|(_, b)| b).sum();
    let outside = (total - attributed).max(0.0);
    out.push(format!("  {:<width$}  {:>7.1} KB/frame", "(outside any script hook)", outside / 1024.0));
    out.push(
        "  per-script figures are sampled around each hook call; the heap counter moves in \
         16 KB pages, so read them over the whole window rather than frame by frame"
            .to_string(),
    );
    out
}

#[cfg(test)]
mod alloc_window_tests {
    use super::AllocWindow;

    /// **The per-script lines add up to the total.** The largest few are named,
    /// the rest are counted, and what no hook accounts for is a number — so a
    /// reader can see where a total that a vector change barely moved is coming
    /// from, without doing the subtraction.
    #[test]
    fn the_alloc_lines_name_the_largest_and_account_for_the_rest() {
        let by: Vec<(String, f64)> = (0..10)
            .map(|i| (format!("script{i}"), (10 - i) as f64 * 1024.0))
            .collect();
        // 55 KB across the scripts, 5 KB outside any hook.
        let lines = super::alloc_lines(60.0 * 1024.0, &by);
        assert!(lines[0].starts_with("  script0"), "{lines:?}");
        assert!(lines[0].contains("10.0 KB/frame"), "{lines:?}");
        assert_eq!(lines.len(), super::ALLOC_LINES + 3, "{lines:?}");
        let more = &lines[super::ALLOC_LINES];
        assert!(more.contains("(2 more scripts)") && more.contains("3.0 KB/frame"), "{more}");
        let outside = &lines[super::ALLOC_LINES + 1];
        assert!(outside.contains("outside any script hook") && outside.contains("5.0 KB/frame"), "{outside}");
        assert!(super::alloc_lines(1.0, &[]).is_empty(), "nothing attributed prints no lines");
    }

    /// **The window sits inside the run, after the opening frames, and spans
    /// more than nothing.** Off by one at either end and the measurement
    /// either never closes — reporting nothing, silently — or describes the
    /// frames that build the world rather than the frames that play it.
    #[test]
    fn the_window_is_inside_the_run_and_never_empty() {
        for asked in AllocWindow::MIN_SPAN..4000 {
            let w = AllocWindow::plan(asked).expect("a long enough run plans a window");
            assert!(w.start >= 1, "{asked}: window starts at {} — no warm-up", w.start);
            assert!(w.end > w.start, "{asked}: window is empty ({}..{})", w.start, w.end);
            assert!(w.end < asked, "{asked}: window closes at {}, past the last step", w.end);
            assert!(w.frames() >= 1);
        }
    }

    /// A run too short to measure says so rather than reporting a number about
    /// the frames that were still opening the project.
    #[test]
    fn too_short_a_run_refuses_rather_than_guessing() {
        for asked in 0..AllocWindow::MIN_SPAN {
            assert!(AllocWindow::plan(asked).is_none(), "{asked} steps should refuse");
        }
        assert!(AllocWindow::plan(AllocWindow::MIN_SPAN).is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_and_frames_meet_at_the_tick_rate() {
        assert_eq!(Span::Frames(90).steps(), 90);
        assert_eq!(Span::Seconds(1.5).steps(), 90, "1.5s at 60 Hz is 90 steps");
        assert_eq!(Span::Seconds(0.0).steps(), 1, "a span nobody can measure is still a step");
    }

    /// **The summary reports time that was SIMULATED** (`floptle/0157`).
    ///
    /// A run whose session is paused — which is what the Play-start terrain hold
    /// makes it until the ground exists — steps its whole span with `dt = 0`.
    /// The verb used to publish `steps × DT` regardless, so the report of a run
    /// that simulated nothing at all was "3600 step(s), 60.00s of simulated
    /// time — nothing raised": a confident answer, at exit 0, that sends the
    /// reader looking for a bug in their game rather than at the runner.
    #[test]
    fn a_run_that_advanced_nothing_does_not_report_the_span_it_was_asked_for() {
        // 3600 steps at 60 Hz is a minute — and the clock did not move.
        let stuck = summary_line(3600, 3600, 0.0, 0, 0);
        assert!(stuck.contains("0.00s of simulated time"), "{stuck}");
        assert!(
            !stuck.contains("60.00s of simulated time"),
            "it must not publish the span it was asked for as though it ran it: {stuck}"
        );
        assert!(stuck.contains("PAUSED"), "and it has to SAY the session was paused: {stuck}");
        assert!(stuck.contains("60.00s"), "…naming how much was stepped without advancing: {stuck}");

        // A run that really did advance reads exactly as it did before.
        let good = summary_line(3600, 3600, 60.0, 0, 0);
        assert_eq!(good, "ran 3600 step(s), 60.00s of simulated time — nothing raised");

        // Half paused, half not: both facts, one line.
        let half = summary_line(120, 120, 1.0, 1, 2);
        assert!(half.contains("1.00s of simulated time"), "{half}");
        assert!(half.contains("PAUSED for 1.00s"), "{half}");
        assert!(half.contains("1 error(s), 2 warning(s)"), "{half}");

        // Ending early and pausing are different things and both get said.
        let early = summary_line(40, 120, 0.0, 0, 0);
        assert!(early.contains("the session ended after 40 of 120"), "{early}");
        assert!(early.contains("PAUSED"), "{early}");
    }

    /// **The percentile names a frame that happened.** Nearest rank, so every
    /// figure printed is a sample and not an average of two.
    #[test]
    fn a_percentile_is_one_of_the_samples() {
        // 1..=100, so the p-th percentile is the number p.
        let s: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        assert_eq!(pct(&s, 0.50), 50.0);
        assert_eq!(pct(&s, 0.95), 95.0);
        assert_eq!(pct(&s, 0.99), 99.0);
        assert_eq!(pct(&s, 1.0), 100.0, "p100 is the worst step, not one past the end");
        assert_eq!(pct(&s, 0.0), 1.0, "p0 is the first sample, not an index of -1");

        // Short runs must not index out of bounds either way.
        assert_eq!(pct(&[7.0], 0.95), 7.0);
        assert_eq!(pct(&[], 0.95), 0.0, "no steps, no percentile");
    }

    /// **p95 is not the mean, and that is the whole reason it is reported.**
    ///
    /// The distribution here is the shape `floptle/0176` describes: mostly
    /// cheap, with a tail. A mean reads as comfortable; p95 does not, and a VM
    /// comparison that averaged its frames would call a collector pause a pass.
    #[test]
    fn the_tail_is_visible_where_a_mean_would_hide_it() {
        // Every figure distinct, so the line cannot pass by printing the wrong
        // one in the right place — which is the mistake this test was written
        // to catch, and did not until the numbers stopped colliding.
        let mut c = Timing::new(100);
        for (n, ms) in [(50, 1.0f32), (45, 2.0), (4, 8.0), (1, 21.0)] {
            for _ in 0..n {
                c.push(ms, true);
            }
        }
        assert_eq!(c.mean(), 1.93, "the mean of this run reads like a 2 ms frame");
        let sorted = c.sorted();
        assert_eq!(pct(&sorted, 0.50), 1.0);
        assert_eq!(pct(&sorted, 0.95), 2.0);
        assert_eq!(pct(&sorted, 0.99), 8.0, "…and one frame in a hundred took eight");
        assert_eq!(pct(&sorted, 1.0), 21.0);

        // The line says which is which. A figure printed under the wrong label
        // is a wrong answer at exit 0, and this whole verb exists to not give
        // one of those.
        let line = timing_line(&c);
        assert!(line.contains("over 100 simulating step(s)"), "{line}");
        assert!(line.contains("p50 1.00 ms"), "{line}");
        assert!(line.contains("p95 2.00 ms"), "{line}");
        assert!(line.contains("p99 8.00 ms"), "{line}");
        assert!(line.contains("max 21.00 ms"), "{line}");
        assert!(line.contains("mean 1.93 ms"), "{line}");
    }

    /// **A step the clock did not move is not a frame** (`floptle/0157` again,
    /// one layer down).
    ///
    /// The Play-start terrain hold steps with `dt = 0`. Those steps are cheap,
    /// and worse, how MANY of them there are depends on the terrain worker — so
    /// letting them into the distribution both understates the frame cost and
    /// makes two runs of the same project incomparable, which is precisely what
    /// a timing probe is for.
    #[test]
    fn the_loading_hold_is_excluded_and_said_out_loud() {
        let mut c = Timing::new(20);
        // Ten paused steps: cheap, and NOT the game.
        for _ in 0..10 {
            c.push(0.01, false);
        }
        // Ten real ones at 5 ms.
        for _ in 0..10 {
            c.push(5.0, true);
        }
        assert_eq!(c.paused, 10);
        assert_eq!(c.sorted().len(), 10, "only the simulating steps are samples");
        assert_eq!(c.mean(), 5.0, "the hold must not drag the mean towards zero");
        assert_eq!(pct(&c.sorted(), 0.95), 5.0);

        let line = timing_line(&c);
        assert!(line.contains("over 10 simulating step(s)"), "{line}");
        assert!(line.contains("p95 5.00 ms"), "{line}");
        assert!(line.contains("10 paused step(s) not counted"), "{line}");
    }

    /// …and a run that simulated NOTHING says that, rather than reporting a
    /// frame cost of zero. A zero here would read as "free", and this file's
    /// whole argument is that a number nobody measured must not look like one
    /// somebody did.
    #[test]
    fn a_run_that_never_advanced_reports_no_timing_rather_than_zeroes() {
        let mut c = Timing::new(4);
        for _ in 0..4 {
            c.push(0.02, false);
        }
        let line = timing_line(&c);
        assert!(line.contains("nothing to time"), "{line}");
        assert!(line.contains("all 4 step(s)"), "{line}");
        assert!(!line.contains("p95"), "there is no p95 to print: {line}");
    }

    /// **Samples arrive unsorted and the report must not care.** `sorted()`
    /// works on a copy so the recorded order survives; a percentile taken off
    /// the raw order would report whichever step happened to be at that index.
    #[test]
    fn the_order_the_steps_ran_in_is_kept() {
        let mut c = Timing::new(4);
        for ms in [9.0, 1.0, 5.0, 3.0] {
            c.push(ms, true);
        }
        assert_eq!(c.sorted(), vec![1.0, 3.0, 5.0, 9.0]);
        assert_eq!(c.samples, vec![9.0, 1.0, 5.0, 3.0], "the run's own order is not destroyed");
        assert_eq!(pct(&c.sorted(), 0.5), 3.0);
    }

    /// **A run is the same length twice.** The whole value of this verb is being
    /// able to say "this raised" and be believed, which a wall clock would take
    /// away — two runs of one project would disagree about how far they got.
    #[test]
    fn the_span_does_not_depend_on_how_fast_the_machine_is() {
        let a = Span::Seconds(2.0).steps();
        let b = Span::Seconds(2.0).steps();
        assert_eq!(a, b);
        assert_eq!(a, 120);
    }
}
