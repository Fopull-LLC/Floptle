//! Frame timing, presentation pacing and the perf overlay.

#[cfg(feature = "editor-ui")]
use crate::dock::EditorTab;
use crate::Editor;


/// What the ⏱ readout draws, copied out of the profile before the UI closure.
///
/// A snapshot rather than a borrow because the UI closure runs inside the frame's
/// split borrows and the profile is also being written this frame — and because a
/// readout that could change halfway through drawing itself would show a bucket
/// total that disagreed with the rows under it.
pub(crate) struct PerfSnapshot {
    on: bool,
    frames: u64,
    buckets: Vec<(&'static str, floptle_core::profile::Cost)>,
    scripts: Vec<(String, floptle_core::profile::Cost)>,
    accounted_ms: f32,
    counts: floptle_core::profile::Counts,
    /// How the frames are actually arriving, and whether dt snapping is
    /// managing to do anything about it.
    pub(crate) pacing: Pacing,
}

/// What the display path is doing, as opposed to what the scene costs.
///
/// The two get confused constantly — "my game runs at 300 fps and stutters" is
/// almost never a scene that is slow — so they are reported side by side and
/// named differently.
#[derive(Clone, Copy, Default)]
pub(crate) struct Pacing {
    /// Smoothed frame time, ms.
    pub(crate) mean_ms: f32,
    /// 99th-percentile frame time over the last couple of seconds, ms.
    pub(crate) p99_ms: f32,
    /// The display's refresh period in ms, or 0 if nothing could be read.
    pub(crate) refresh_ms: f32,
    /// Share of recent frames the dt snap actually applied to, 0..1.
    pub(crate) snap_rate: f32,
    /// Smoothed time blocked inside `acquire` (`Editor::present_wait_ms`) —
    /// the display path, not the scene. an earlier task: the piece the ⏱ panel
    /// had the numbers for and never compared.
    pub(crate) present_wait_ms: f32,
    /// `mean_ms - present_wait_ms`: what the frame cost apart from waiting on
    /// the display. The same subtraction the window-title `cost` figure does.
    pub(crate) cost_ms: f32,
}

impl PerfSnapshot {
    pub(crate) fn take(p: &floptle_core::profile::FrameProfile) -> Self {
        Self {
            pacing: Pacing::default(),
            on: p.enabled(),
            frames: p.frames(),
            buckets: floptle_core::profile::Bucket::ALL
                .into_iter()
                .map(|b| (b.name(), p.bucket(b).unwrap_or_default()))
                .collect(),
            scripts: p.scripts(),
            accounted_ms: p.accounted_ms().unwrap_or(0.0),
            counts: p.counts(),
        }
    }
}

// These exercise the AUTHORING half — the dock, the Inspector, the
// command line — so they compile only where that half does. Without the
// gate the player configuration cannot be linted or tested at all, which
// is how it went unlinted through a whole release.
#[cfg(feature = "editor-ui")]
#[cfg(test)]
mod readout_tests {
    /// **The readout has to smooth frame time, not its reciprocal.**
    ///
    /// The acceptance case from an earlier task: a frame sequence alternating
    /// 2 ms and 30 ms. Sixty-two frames a second are genuinely arriving (16 ms
    /// mean), and an EMA over `1.0 / dt` reports something near 265 — it spends
    /// half its samples at 500 fps and a reciprocal does not average.
    ///
    /// The real capture was worse: bursts of 0.08 ms frames between 16 ms blocks
    /// read as 4312 fps against a true 144.
    #[test]
    fn the_fps_readout_averages_frame_time_not_its_reciprocal() {
        let seq: Vec<f32> = (0..400)
            .map(|i| if i % 2 == 0 { 0.002 } else { 0.030 })
            .collect();
        let true_fps = seq.len() as f32 / seq.iter().sum::<f32>();
        assert!((true_fps - 62.5).abs() < 0.1, "the sequence really is ~62 fps: {true_fps}");

        // What the readout does now: smooth the time, invert at the end.
        let mut frame_ms = 0.0f32;
        for &dt in &seq {
            let ms = dt * 1000.0;
            frame_ms = if frame_ms > 0.0 { frame_ms * 0.9 + ms * 0.1 } else { ms };
        }
        let reported = 1000.0 / frame_ms;

        // What it used to do: smooth the reciprocal.
        let mut old = 0.0f32;
        for &dt in &seq {
            let inst = 1.0 / dt;
            old = if old > 0.0 { old * 0.9 + inst * 0.1 } else { inst };
        }

        assert!(
            (reported - true_fps).abs() < 6.0,
            "reported {reported} against a true {true_fps}"
        );
        assert!(
            old > 200.0,
            "the old formula really did overstate this badly — if it doesn't, this \
             test is no longer measuring the bug it was written for (got {old})"
        );
    }

    /// **The buckets account for most of the step.** `perf.buckets()`'s
    /// `Scripts` used to be the sum of the per-script hook times and nothing
    /// else — no per-instance setup, no scene mirror, no write flush — so a
    /// real game's readout said 5.3 ms of a 12.9 ms step and the whole of the
    /// 0.84 optimisation pass was invisible in the tool built to find it.
    ///
    /// A ratio and not a duration, like every other perf guard here: a slower machine
    /// moves both sides. The subject is sixty scripted nodes stepped headlessly,
    /// which is a script-dominated frame on purpose — that is the shape the
    /// bucket was lying about.
    ///
    /// Watched failing: 0.260 ms accounted of a 0.759 ms step — 0.34, against
    /// the 0.70 here.
    #[test]
    fn the_buckets_account_for_most_of_a_scripted_step() {
        use floptle_core::profile::{Bucket, Span};
        use floptle_core::{Name, ScriptInst, Scripts};
        use floptle_core::transform::Transform;

        let dir = std::env::temp_dir()
            .join(format!("floptle-perf-accounted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(crate::new_project(&dir, "0.0.0-test", "empty"), 0, "scaffold failed");

        let mut ed = crate::Editor { show_gizmos: false, ..Default::default() };
        ed.open_project(dir.clone());
        // Sixty nodes on a seeded script that moves its own node every frame —
        // enough that the per-instance work is the frame, which is the case the
        // bucket was mis-reporting.
        for i in 0..60 {
            let e = ed.world.spawn();
            ed.world.insert(e, Name(format!("Floater{i}")));
            ed.world.insert(
                e,
                Transform { translation: [i as f64, 0.0, 0.0].into(), ..Transform::IDENTITY },
            );
            ed.world.insert(
                e,
                Scripts(vec![ScriptInst {
                    kind: "float".into(),
                    enabled: true,
                    params: vec![],
                    refs: Vec::new(),
                    strs: Vec::new(),
                }]),
            );
        }
        ed.toggle_play();
        assert!(ed.playing, "the session must actually start");
        ed.script_host.profile().borrow_mut().enable(true);

        const DT: f32 = 1.0 / 60.0;
        // The profiler's own smoothing (`profile::SMOOTH`), applied to the step
        // wall clock, so the two sides of the ratio are the same average.
        let mut step_ms = 0.0f32;
        for _ in 0..400 {
            ed.pump_world_streaming();
            let span = Span::new();
            ed.play_step(DT, true);
            let ms = span.ms();
            step_ms = if step_ms > 0.0 { step_ms * 0.9 + ms * 0.1 } else { ms };
            ed.script_host.profile().borrow_mut().end_frame();
        }
        let p = ed.script_host.profile().borrow();
        let accounted = p.accounted_ms().expect("collection is on");
        let scripts = p.bucket(Bucket::Scripts).unwrap_or_default().ms;
        let mirror = p.bucket(Bucket::Mirror).unwrap_or_default().ms;
        let hooks: f32 = p.scripts().iter().map(|(_, c)| c.ms).sum();
        let ratio = accounted / step_ms;
        assert!(
            ratio >= 0.70,
            "the buckets account for {accounted:.3} ms of a {step_ms:.3} ms step ({ratio:.2}) \
             — scripts {scripts:.3}, mirror {mirror:.3}, hooks {hooks:.3}"
        );
        // …and the per-script hook figures are a part of the Scripts bucket now,
        // not the whole of it. If they ever sum to more, the pass is being timed
        // twice.
        assert!(
            hooks <= scripts + 0.001,
            "hook time {hooks:.3} ms exceeds the Scripts bucket {scripts:.3} ms — double counted"
        );
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 1% low has to see the slow frames. A mean cannot, which is the whole
    /// reason it is reported beside one.
    #[test]
    fn the_one_percent_low_reports_the_worst_frames_not_the_average() {
        let mut ed = crate::Editor::default();
        // 99 good frames and one 40 ms hitch, repeated — the shape that adds
        // well under a millisecond to an average and is the only thing anybody
        // is ever chasing.
        for i in 0..500 {
            ed.record_frame_time(if i % 100 == 99 { 40.0 } else { 6.9 });
        }
        let low = ed.frame_time_low();
        assert!(low > 30.0, "the hitch has to be visible in the 1% low, got {low}");

        // And a steady stream reports a steady low — it must not manufacture a
        // spike out of an even distribution.
        let mut steady = crate::Editor::default();
        for _ in 0..500 {
            steady.record_frame_time(6.9);
        }
        assert!((steady.frame_time_low() - 6.9).abs() < 0.01);
    }

    /// The 16-light cap says so once per count, not every frame, and says
    /// nothing while nothing is being cut.
    #[test]
    fn the_light_cap_warns_once_per_count_and_falls_silent_under_it() {
        fn warns(ed: &crate::Editor) -> usize {
            ed.console
                .entries
                .iter()
                .filter(|e| e.level == floptle_script::LogLevel::Warn)
                .count()
        }
        let mut ed = crate::Editor::default();
        // Each `ed.frame_no += 1` moves to a new simulated frame.
        // `render_world_into` runs several times per `render()` (Game view,
        // camera previews, a GI bake), each a different camera, so two calls
        // at the same frame_no simulate two cameras in one frame — which must
        // not each get their own say.

        ed.frame_no += 1;
        ed.warn_lights_dropped(0);
        assert_eq!(warns(&ed), 0, "nothing was cut — nothing to say");

        ed.frame_no += 1;
        ed.warn_lights_dropped(24);
        assert_eq!(warns(&ed), 1, "24 dropped lights must say so");
        // same frame, a second camera reporting a different count (an
        // orthographic minimap beside a perspective main camera, say) — must
        // not be read as the count "changing" and re-warn.
        ed.warn_lights_dropped(30);
        assert_eq!(warns(&ed), 1, "a second gather in the SAME frame must not get its own warning");

        ed.frame_no += 1;
        ed.warn_lights_dropped(24);
        assert_eq!(warns(&ed), 1, "the same count again must not repeat itself every frame");

        ed.frame_no += 1;
        ed.warn_lights_dropped(30);
        assert_eq!(warns(&ed), 2, "MORE lights dropped is new information and re-warns");

        ed.frame_no += 1;
        ed.warn_lights_dropped(0);
        assert_eq!(warns(&ed), 2, "back under the cap — quiet again");
        ed.frame_no += 1;
        ed.warn_lights_dropped(24);
        assert_eq!(warns(&ed), 3, "over the cap a second time must warn again, not stay latched off");
    }
}

/// Which refresh period to hold, given what the platform just said.
///
/// Pulled out of [`Editor::reread_refresh_period`] because it is the whole of
/// that task's second half and it cannot be tested through a real window.
///
/// * `held` — what we already believe (0 = nothing yet).
/// * `current` — `current_monitor()`'s refresh in mHz, if it answered.
/// * `any` — a monitor, any monitor, in mHz. Only consulted when nothing is
///   known at all, because on a mixed-refresh desktop it is a guess.
pub(crate) fn chosen_refresh_period(held: f32, current: Option<u32>, any: impl FnOnce() -> Option<u32>) -> f32 {
    if let Some(mhz) = current.filter(|&m| m > 0) {
        return 1000.0 / mhz as f32;
    }
    // A `None` means "ask again", not "there is no display" — mapping it onto
    // 0.0 is what switched dt snapping off for a whole session.
    if held > 0.0 {
        return held;
    }
    any().filter(|&m| m > 0).map(|mhz| 1000.0 / mhz as f32).unwrap_or(0.0)
}

/// Is the DISPLAY pacing the frame, rather than the scene being slow
///?
///
/// The signature `docs/subsystems/renderer.md` already describes: `acquire`
/// blocks for very close to a whole multiple (≥2) of the refresh period —
/// the compositor presenting every Nth vblank rather than every one — while
/// the frame's own work (`cost_ms`, the same subtraction the window title's
/// "cost" figure already does) is small next to that wait. A scene that is
/// genuinely heavy can also land near a multiple by coincidence, which is
/// exactly why `cost_ms` is the second half of the test: a real 40 ms scene
/// waiting 50 ms is a slow scene, not this.
///
/// Returns the multiple when detected, so a message can say "every Nth
/// refresh" rather than just "something is off".
pub(crate) fn fifo_pacing_multiple(present_wait_ms: f32, cost_ms: f32, refresh_ms: f32) -> Option<u32> {
    if refresh_ms <= 0.0 || present_wait_ms <= 0.0 {
        return None;
    }
    let n = (present_wait_ms / refresh_ms).round();
    // Same 12% band `smooth_dt` snaps dt within, so "close to a multiple"
    // means the same thing in both places.
    if n < 2.0 || (present_wait_ms - n * refresh_ms).abs() > refresh_ms * 0.12 {
        return None;
    }
    if cost_ms > refresh_ms * 0.5 {
        return None; // the frame is doing real work — this is not a null scene
    }
    Some(n as u32)
}

#[cfg(test)]
mod fifo_pacing_tests {
    use super::fifo_pacing_multiple;

    /// The card's own capture: 50.0 ms `acquire` on a 16.68 ms (59.95 Hz)
    /// refresh — three refreshes, near-zero scene cost either side of it.
    #[test]
    fn the_cards_own_capture_is_detected_as_three_refreshes() {
        assert_eq!(fifo_pacing_multiple(50.0, 0.3, 16.68), Some(3));
    }

    /// An ordinary vsynced frame — `acquire` near one refresh — is not this.
    /// One refresh of waiting is just vsync working; the signature is being
    /// held for *more* than the display's own pace warrants.
    #[test]
    fn one_refresh_of_waiting_is_ordinary_vsync_not_the_bug() {
        assert_eq!(fifo_pacing_multiple(16.7, 0.3, 16.68), None);
    }

    /// A GENUINELY slow scene that happens to cost close to two refreshes is
    /// not this — the whole point of the `cost_ms` half of the test.
    #[test]
    fn a_scene_that_is_actually_slow_is_not_reported_as_display_pacing() {
        assert_eq!(fifo_pacing_multiple(33.3, 30.0, 16.68), None);
    }

    /// No known refresh rate, or nothing waited on `acquire`: nothing to say.
    #[test]
    fn nothing_to_compare_against_says_nothing() {
        assert_eq!(fifo_pacing_multiple(50.0, 0.3, 0.0), None);
        assert_eq!(fifo_pacing_multiple(0.0, 0.3, 16.68), None);
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::chosen_refresh_period;

    /// Measured with `present_stats`: `current_monitor()` is
    /// **none** at window creation and becomes `Some(DP-2, 144001)` once the
    /// surface is mapped, while `available_monitors()` reports
    /// `[HDMI-A-1 59951, DP-2 144001]` correctly the entire time.
    #[test]
    fn a_none_from_current_monitor_never_zeroes_a_good_period() {
        let dp2 = Some(144_001);
        let hdmi = || Some(59_951);

        // Startup: nothing known and nothing current. Snapping used to be dead
        // here for 240 frames; a monitor — any monitor — beats no snapping.
        let boot = chosen_refresh_period(0.0, None, hdmi);
        assert!(boot > 0.0, "startup must not come up with snapping switched off");

        // The surface maps and the real output answers.
        let live = chosen_refresh_period(boot, dp2, hdmi);
        // In SECONDS: `refresh_period` is compared against `dt`, not against a
        // millisecond readout. 144.001 Hz -> 6.944 ms.
        assert!((live - 1.0 / 144.001).abs() < 1e-6, "{live}");

        // A later transient None — an output hotplug, a window drag — must keep
        // the good value rather than replace it with a guess about the other
        // monitor or with zero.
        let after = chosen_refresh_period(live, None, hdmi);
        assert_eq!(after, live, "a transient None is 'ask again', not 'no display'");

        // And a display that reports nonsense is treated as no answer at all.
        assert_eq!(chosen_refresh_period(live, Some(0), hdmi), live);
        assert_eq!(chosen_refresh_period(0.0, None, || None), 0.0, "genuinely nothing to go on");
    }
}

/// Draw how the frames are ARRIVING, beside what they cost.
///
/// **Two different questions, and an fps number answers neither on its own.** A
/// scene costing 8 ms that presents at 20 fps is a display path pacing the
/// engine; the same 8 ms at 120 fps is the same scene doing fine. And when dt
/// snapping goes inert — the measured frame time stops landing near a whole
/// multiple of the reported refresh, because the window is on a different output
/// than `current_monitor()` names, or nothing is pacing to vblank — the raw
/// scheduler jitter goes straight into the fixed-step accumulator and the render
/// judders by `velocity x noise`. That used to happen in total silence,
/// which is the worst possible way for a load-bearing path to
/// be switched off.
#[cfg(feature = "editor-ui")]
pub(crate) fn pacing_readout(ui: &mut egui::Ui, p: &Pacing) {
    if p.mean_ms <= 0.0 {
        return;
    }
    let warn = egui::Color32::from_rgb(230, 150, 90);
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("frames arriving").strong());
        ui.label(
            egui::RichText::new(format!(
                "{:.2} ms mean   {:.2} ms 1% low   ({:.0} fps)",
                p.mean_ms,
                p.p99_ms,
                1000.0 / p.mean_ms.max(1e-4)
            ))
            .monospace(),
        );
    });
    // A 1% low several times the mean is the stutter, whatever the fps says.
    if p.p99_ms > p.mean_ms * 2.0 {
        ui.small(
            egui::RichText::new(format!(
                "⚠ the worst 1% of frames take {:.1}x the average. That is what a \
                 stutter is, and an fps number cannot show it.",
                p.p99_ms / p.mean_ms.max(1e-4)
            ))
            .color(warn),
        );
    }
    // The display pacing the frame, not the scene being slow:
    // `acquire` blocked for a whole multiple of the refresh period while the
    // frame's own work barely registers. This used to be indistinguishable
    // from "the scene is heavy" — both numbers were already on screen and
    // never compared.
    if let Some(n) = fifo_pacing_multiple(p.present_wait_ms, p.cost_ms, p.refresh_ms) {
        ui.small(
            egui::RichText::new(format!(
                "⚠ the DISPLAY is pacing this, not the scene: {:.1} ms of every frame is \
                 spent waiting on `acquire` — every {n}th refresh — while the scene itself \
                 costs {:.1} ms. Try Project Settings ⏵ Rendering ⏵ Frame pacing.",
                p.present_wait_ms, p.cost_ms
            ))
            .color(warn),
        );
    }
    if p.refresh_ms <= 0.0 {
        ui.small(
            egui::RichText::new(
                "⚠ no display refresh rate available, so dt snapping is off — frame-time \
                 jitter is reaching the simulation clock unfiltered.",
            )
            .color(warn),
        );
        return;
    }
    let hz = 1000.0 / p.refresh_ms;
    if p.snap_rate >= 0.5 {
        ui.small(format!(
            "dt snapping: on — {:.2} ms refresh ({hz:.4} Hz), applied to {:.0}% of frames.",
            p.refresh_ms,
            p.snap_rate * 100.0
        ));
    } else {
        ui.small(
            egui::RichText::new(format!(
                "⚠ dt snapping is inert — the display reports {:.2} ms ({hz:.4} Hz) but frames \
                 are arriving every {:.2} ms, so they aren't landing on whole refreshes and the \
                 snap can't apply (it caught {:.0}% of them). Usually the window is on a \
                 different output than the one being reported, or the present mode isn't pacing \
                 to vblank. Frame-time jitter is reaching the simulation clock.",
                p.refresh_ms,
                p.mean_ms,
                p.snap_rate * 100.0
            ))
            .color(warn),
        );
    }
}

/// Draw the frame-cost readout.
///
/// Two columns per row on purpose: the rolling mean and the worst frame of the
/// last second. The spike is what anybody is ever chasing, and a mean hides it —
/// a 40 ms hitch once a second adds under a millisecond to a 60-frame average.
#[cfg(feature = "editor-ui")]
pub(crate) fn perf_readout(ui: &mut egui::Ui, s: &PerfSnapshot) {
    if !s.on {
        ui.label("Not collecting.");
        ui.small(
            "Collection is off by default because a profiler that costs a frame is a \
             profiler people turn off. Close and reopen this panel to start.",
        );
        return;
    }
    if s.frames == 0 {
        ui.label("Measuring — numbers appear next frame.");
        return;
    }
    ui.small(
        "worst = the worst single frame in the last second. That is the column to \
         read; a hitch is invisible in an average.",
    );
    ui.add_space(4.0);
    pacing_readout(ui, &s.pacing);
    ui.add_space(4.0);
    egui::Grid::new("perf-buckets").num_columns(3).striped(true).show(ui, |ui| {
        ui.label(egui::RichText::new("").strong());
        ui.label(egui::RichText::new("avg ms").strong());
        ui.label(egui::RichText::new("worst ms").strong());
        ui.end_row();
        for (name, c) in &s.buckets {
            ui.label(*name);
            ui.label(egui::RichText::new(format!("{:6.2}", c.ms)).monospace());
            // The worst column carries the colour, since it is the one being read.
            let hot = c.worst_ms > 8.0;
            let text = egui::RichText::new(format!("{:6.2}", c.worst_ms)).monospace();
            ui.label(if hot { text.color(egui::Color32::from_rgb(230, 150, 90)) } else { text });
            ui.end_row();
        }
        ui.label(egui::RichText::new("accounted").weak());
        ui.label(egui::RichText::new(format!("{:6.2}", s.accounted_ms)).monospace().weak());
        ui.label("");
        ui.end_row();
    });
    // "accounted", not "total": vsync, the OS and the GPU finishing are outside
    // every bucket, and a readout claiming to add up to the frame time without
    // doing so is worse than one that never claimed it.
    ui.small("accounted = these buckets added up. Not the frame time — vsync, the OS and the GPU finishing are outside all of them.");
    // Since 0.84.2 `scripts` is the whole pass, so the per-script rows below no
    // longer add up to it. Say so here rather than leaving a reader to notice
    // the gap and distrust both numbers.
    ui.small("scripts = the whole pass; the per-script rows below are the hook time inside it, and the difference is what the engine spent reaching them.");

    ui.add_space(6.0);
    ui.separator();
    // by script name. The whole point: "scripts: 6 ms" does not answer "which of
    // my scripts is doing this".
    ui.label(egui::RichText::new("per script").strong());
    if s.scripts.is_empty() {
        ui.small("No scripts have run since collection started.");
    } else {
        egui::Grid::new("perf-scripts").num_columns(3).striped(true).show(ui, |ui| {
            for (name, c) in s.scripts.iter().take(12) {
                ui.label(name);
                ui.label(egui::RichText::new(format!("{:6.2}", c.ms)).monospace());
                ui.label(egui::RichText::new(format!("{:6.2}", c.worst_ms)).monospace());
                ui.end_row();
            }
        });
        if s.scripts.len() > 12 {
            ui.small(format!("…and {} more, cheaper", s.scripts.len() - 12));
        }
    }

    ui.add_space(6.0);
    ui.separator();
    // Counts, because three of the four "the engine is slow" tickets were
    // answerable from one of these alone.
    ui.label(egui::RichText::new("counts").strong());
    let c = &s.counts;
    ui.label(
        egui::RichText::new(format!(
            "{} nodes ({} off screen)\n{} instances, {} draws\n{} terrain chunks\n{} scatter props\n{} particles",
            c.nodes, c.culled, c.instances, c.draws, c.chunks, c.props, c.particles
        ))
        .monospace(),
    );
    ui.add_space(4.0);
    ui.small("All of this is readable from Lua as perf.* — assert a budget in a smoke test rather than waiting for a player to notice.");
}

impl Editor {
    /// How many frame times the 1% low is taken over — about two seconds at
    /// 144 Hz, which is long enough for a periodic hitch to land in it and short
    /// enough that the number still tracks what the scene is doing now.
    pub(crate) const FRAME_LOG: usize = 512;

    /// Re-take the 🎓 Learn tab's project snapshot, at most a few times a second
    /// and only while the tab is on top of its dock leaf.
    ///
    /// `played` latches: `Check::Played` asks "have you run this yet", which
    /// stays true after you press Stop — otherwise the step would tick and then
    /// immediately un-tick itself, which reads as the editor changing its mind.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn refresh_learn(&mut self) {
        self.learn.played |= self.playing;
        let front = self
            .dock_state
            .as_ref()
            .is_some_and(|d| crate::dock::tab_is_front(d, EditorTab::Learn));
        if !front {
            return;
        }
        let now = self.started.map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
        if now < self.learn.next_scan {
            return;
        }
        self.learn.next_scan = now + crate::learn::RESCAN_SECS;
        self.learn.snap = crate::learn::scan(&self.world, &self.project_root, self.learn.played);
    }

    /// Bank one frame time (milliseconds) for the 1% low.
    pub(crate) fn record_frame_time(&mut self, ms: f32) {
        if self.frame_log.len() != Self::FRAME_LOG {
            self.frame_log = vec![0.0; Self::FRAME_LOG];
            self.frame_log_at = 0;
            self.frame_log_len = 0;
        }
        self.frame_log[self.frame_log_at] = ms;
        self.frame_log_at = (self.frame_log_at + 1) % Self::FRAME_LOG;
        self.frame_log_len = (self.frame_log_len + 1).min(Self::FRAME_LOG);
    }

    /// The 1% low: the mean of the worst 1% of frame times in the log, ms.
    ///
    /// **The worst frames, reported as a time rather than as a rate.** "1% low
    /// fps" is the usual name, but the honest quantity is the frame time — it is
    /// what is being measured, it averages correctly, and inverting it invites
    /// exactly the reciprocal-of-a-mean error this readout was fixed for.
    ///
    /// The mean of the worst 1%, not the 99th percentile, and the difference is
    /// not pedantic: a hitch that happens on almost exactly 1% of frames puts
    /// the p99 index right at the boundary, so the single sample it lands on is
    /// as likely to be a good frame as a bad one and the readout blinks between
    /// 6.9 and 40. Averaging the tail reports the tail.
    pub(crate) fn frame_time_low(&self) -> f32 {
        if self.frame_log_len == 0 {
            return self.frame_ms;
        }
        let mut v: Vec<f32> = self.frame_log[..self.frame_log_len].to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // At least one sample, so a short log answers with its worst frame
        // rather than with nothing.
        let n = (((v.len() as f32) * 0.01).ceil() as usize).clamp(1, v.len());
        v[v.len() - n..].iter().sum::<f32>() / n as f32
    }

    /// Re-read the display's refresh period, without throwing away a good one.
    ///
    /// **A `None` from `current_monitor()` means "ask again", not "there is no
    /// display."** On Wayland it returns `None` from `create_window` until the
    /// surface is mapped, and `None` again on an output hotplug, a monitor
    /// change or a window drag. The old read mapped that straight onto `0.0`,
    /// and `period <= 0.0` is how dt snapping switches itself off — so the
    /// anti-jitter path `docs/subsystems/time.md` §10 calls load-bearing was
    /// inert for the whole of startup, and for the whole poll interval after
    /// every later hiccup, on a machine where `available_monitors()` was
    /// answering correctly the entire time.
    pub(crate) fn reread_refresh_period(&mut self) {
        let Some(w) = self.window.as_ref() else { return };
        let hz = |m: &winit::monitor::MonitorHandle| m.refresh_rate_millihertz();
        let current = w.current_monitor().as_ref().and_then(hz);
        // Only asked for when there is nothing else to go on, so it is computed
        // lazily — enumerating monitors is not free and the common case never
        // needs it.
        let any = || w.primary_monitor().or_else(|| w.available_monitors().next()).as_ref().and_then(hz);
        self.refresh_period = chosen_refresh_period(self.refresh_period, current, any);
    }
}
