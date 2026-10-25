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
    /// The fixed tick costs more than the time it simulates.
    overloaded: bool,
    /// Last frame's script-mirror work, and why it last rebuilt.
    mirror: floptle_core::profile::MirrorWork,
    mirror_cause: Option<String>,
    /// How the frames are actually arriving, and whether dt snapping is
    /// managing to do anything about it.
    pub(crate) pacing: Pacing,
}

/// What the display path is doing, as opposed to what the scene costs.
///
/// The two get confused constantly — "my game runs at 300 fps and stutters" is
/// almost never a scene that is slow — so they are reported side by side and
/// named differently.
#[derive(Clone, Default)]
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
    /// the display path, not the scene. The piece the ⏱ panel had the numbers
    /// for and never compared.
    pub(crate) present_wait_ms: f32,
    /// `mean_ms - present_wait_ms`: what the frame cost apart from waiting on
    /// the display. The same subtraction the window-title `cost` figure does.
    pub(crate) cost_ms: f32,
    /// The swapchain's present mode, as `perf.pacing()` names it.
    pub(crate) present_mode: &'static str,
    /// Whether that mode waits for the display. Snapping means nothing
    /// without it.
    pub(crate) vsync: bool,
    /// The game's frame cap, 0 when off.
    pub(crate) frame_cap: f32,
    /// The output the window is on, as the refresh was last read from.
    pub(crate) on: Option<OutputInfo>,
    /// Every output, so a frame paced by another one's clock can be named.
    pub(crate) outputs: Vec<OutputInfo>,
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
            overloaded: p.overloaded(),
            mirror: p.mirror_work(),
            mirror_cause: p.mirror_cause().map(str::to_owned),
        }
    }
}

// These exercise the authoring half — the dock, the Inspector, the
// command line — so they compile only where that half does. Without the
// gate the player configuration cannot be linted or tested at all, which
// is how it went unlinted through a whole release.
#[cfg(feature = "editor-ui")]
#[cfg(test)]
mod readout_tests {
    /// **The readout has to smooth frame time, not its reciprocal.**
    ///
    /// The acceptance case: a frame sequence alternating
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
        assert_eq!(crate::new_project(&dir, "0.0.0-test", "empty", true), 0, "scaffold failed");

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
                    pinned_bools: None,
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

/// One display output, as the refresh choice sees it: what winit says about a
/// monitor, reduced to plain values so the choice can be tested without one.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct OutputInfo {
    /// What the system calls it (`DP-2`), or "" where it says nothing.
    pub(crate) name: String,
    /// Top-left corner and size on the desktop, physical pixels: x, y, w, h.
    pub(crate) rect: [i64; 4],
    /// Its refresh rate in millihertz, where it says.
    pub(crate) mhz: Option<u32>,
}

impl OutputInfo {
    pub(crate) fn of(m: &winit::monitor::MonitorHandle) -> Self {
        let (p, s) = (m.position(), m.size());
        Self {
            name: m.name().unwrap_or_default(),
            rect: [p.x as i64, p.y as i64, s.width as i64, s.height as i64],
            mhz: m.refresh_rate_millihertz().filter(|&r| r > 0),
        }
    }

    /// One refresh, milliseconds, where the output says its rate.
    fn period_ms(&self) -> Option<f32> {
        self.mhz.map(|m| 1_000_000.0 / m as f32)
    }

    /// `DP-2 (144.0 Hz)`, or `an unnamed output` where it has no name.
    fn label(&self) -> String {
        let name = if self.name.is_empty() { "an unnamed output" } else { &self.name };
        match self.mhz {
            Some(m) => format!("{name} ({:.1} Hz)", m as f32 / 1000.0),
            None => name.to_string(),
        }
    }
}

/// The window's outer rectangle, where the platform says where it is: X11,
/// Windows and macOS do, Wayland never does.
pub(crate) fn window_rect(w: &winit::window::Window) -> Option<[i64; 4]> {
    let p = w.outer_position().ok()?;
    let s = w.outer_size();
    Some([p.x as i64, p.y as i64, s.width as i64, s.height as i64])
}

/// How many pixels two rectangles share.
fn overlap(a: [i64; 4], b: [i64; 4]) -> i64 {
    let w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    w.max(0) * h.max(0)
}

/// The output the window is on.
///
/// * `window` — the window's rectangle, where the platform says ([`window_rect`]).
/// * `current` — `current_monitor()`.
/// * `all` — `available_monitors()`.
///
/// **Where the window's rectangle is known, the output under most of it
/// wins.** X11's `current_monitor()` is the output winit guessed when the
/// window was made, corrected only when the window manager sends a synthetic
/// configure: under XWayland it named the 60 Hz HDMI output for a window on
/// the 144 Hz DisplayPort one. Wayland gives no rectangle, and there
/// `current_monitor()` is the compositor's own answer, the first output the
/// surface entered, which is the best there is.
pub(crate) fn window_output<'a>(
    window: Option<[i64; 4]>,
    current: Option<&'a OutputInfo>,
    all: &'a [OutputInfo],
) -> Option<&'a OutputInfo> {
    let is_current = |o: &OutputInfo| current.is_some_and(|c| c.name == o.name && c.rect == o.rect);
    let under = window.and_then(|w| {
        all.iter()
            .map(|o| (overlap(w, o.rect), o))
            .filter(|(area, _)| *area > 0)
            .max_by(|(a, x), (b, y)| a.cmp(b).then_with(|| is_current(x).cmp(&is_current(y))))
            .map(|(_, o)| o)
    });
    under.or(current)
}

/// The refresh period held, and where it came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RefreshRead {
    /// Seconds, compared against `dt`; 0 when nothing is known.
    pub(crate) period: f32,
    /// The output it was read from.
    pub(crate) output: Option<String>,
    /// Read from the output the window is on this time, rather than held over
    /// a non-answer or guessed. Until it is, the read is repeated every frame.
    pub(crate) confirmed: bool,
}

/// Which refresh period to hold, given what the platform just said.
///
/// Pulled out of [`Editor::reread_refresh_period`] because it cannot be
/// tested through a real window.
///
/// * `held` — what we already believe.
/// * `on` — the output the window is on ([`window_output`]), if anything said.
/// * `all` — every output. Only consulted when nothing is known at all.
///
/// **A non-answer means "ask again", not "there is no display."** Mapping it
/// onto 0 switched dt snapping off for a whole session. And with nothing
/// known, the guess is the FASTEST output: a Wayland window has no output
/// until it is first shown, which is after a game's `start` has run, and a
/// game that sets its frame cap from `app.refreshRate()` there keeps what it
/// read. Guessed too high, the cap holds nothing back for the frame or two
/// until the real answer arrives; guessed too low, as the first-listed 60 Hz
/// output was, it capped a 144 Hz window at 60 for the whole session.
pub(crate) fn chosen_refresh(held: &RefreshRead, on: Option<&OutputInfo>, all: &[OutputInfo]) -> RefreshRead {
    if let Some(o) = on
        && let Some(mhz) = o.mhz
    {
        return RefreshRead { period: 1000.0 / mhz as f32, output: Some(o.name.clone()), confirmed: true };
    }
    if held.period > 0.0 {
        return RefreshRead { confirmed: false, ..held.clone() };
    }
    all.iter()
        .filter_map(|o| o.mhz.map(|m| (m, o)))
        .max_by_key(|(m, _)| *m)
        .map(|(m, o)| RefreshRead { period: 1000.0 / m as f32, output: Some(o.name.clone()), confirmed: false })
        .unwrap_or_default()
}

/// The display holding each frame back, as opposed to the scene being slow.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplayPacing {
    /// Frames are shown every this many refreshes of `output`.
    pub(crate) every: u32,
    /// The output whose refreshes the frames land on.
    pub(crate) output: OutputInfo,
    /// It is not the output the window is on: the compositor is presenting
    /// the window on another output's clock.
    pub(crate) elsewhere: bool,
}

/// Is the display pacing the frame, rather than the scene being slow? And on
/// which output's clock?
///
/// The signature `docs/subsystems/renderer.md` describes: `acquire` blocks for
/// very close to a whole number of refreshes while the frame's own work
/// (`cost_ms`, the subtraction the window title's "cost" figure does) is small
/// next to that wait. A scene that is genuinely heavy can also land near a
/// multiple by coincidence, which is why the cost is the second half of the
/// test: a real 40 ms scene waiting 50 ms is a slow scene, not this.
///
/// **Every output's period is tried, not only the window's.** A compositor can
/// present a window on one output's clock while it sits on another: KWin held a
/// window on a 144 Hz output to every third refresh of a 60 Hz one, 50 ms a
/// frame, which is no whole number of 6.94 ms refreshes. Judged against the
/// window's own output alone, that read as nothing at all. On the window's own
/// output one refresh of waiting is just vsync working, so it takes two or
/// more; on another output a single refresh is already the finding.
pub(crate) fn display_pacing(
    present_wait_ms: f32,
    cost_ms: f32,
    on: Option<&OutputInfo>,
    all: &[OutputInfo],
) -> Option<DisplayPacing> {
    if present_wait_ms <= 0.0 {
        return None;
    }
    // Whole refreshes of `period` the wait spans, if it is close to a whole
    // number of them while the frame itself barely costs anything. The same
    // 12% band `smooth_dt` snaps within, so "close to a multiple" means the
    // same thing in both places.
    let fit = |period: f32| -> Option<(u32, f32)> {
        let n = (present_wait_ms / period).round();
        let off = (present_wait_ms - n * period).abs();
        (n >= 1.0 && off <= period * 0.12 && cost_ms <= period * 0.5).then_some((n as u32, off / period))
    };
    if let Some(o) = on
        && let Some(period) = o.period_ms()
    {
        match fit(period) {
            Some((n, _)) if n >= 2 => return Some(DisplayPacing { every: n, output: o.clone(), elsewhere: false }),
            // One refresh of its own output: vsync doing its job.
            Some(_) => return None,
            None => {}
        }
    }
    let same = |a: &OutputInfo, b: &OutputInfo| a.name == b.name && a.rect == b.rect;
    let min_n = if on.is_some() { 1 } else { 2 };
    all.iter()
        .filter(|o| !on.is_some_and(|w| same(w, o)))
        .filter_map(|o| fit(o.period_ms()?).filter(|(n, _)| *n >= min_n).map(|(n, off)| (n, off, o)))
        // The fewest refreshes explains it best; a long enough wait is
        // close to SOME multiple of any period.
        .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
        .map(|(n, _, o)| DisplayPacing { every: n, output: o.clone(), elsewhere: on.is_some() })
}

/// "1st", "2nd", "3rd", "7th".
fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// What the Console and the ⏱ panel say when the display is pacing the frame.
pub(crate) fn display_pacing_message(p: &DisplayPacing, wait_ms: f32, cost_ms: f32, on: Option<&OutputInfo>) -> String {
    let refresh = if p.every == 1 { "refresh".to_string() } else { format!("{} refresh", ordinal(p.every)) };
    let whose = match (p.elsewhere, on) {
        (true, Some(w)) => format!(
            "every {refresh} of {}, though the window is on {}: the compositor is presenting this \
             window on the other output's clock",
            p.output.label(),
            w.label()
        ),
        _ => format!("every {refresh} of {}", p.output.label()),
    };
    format!(
        "the DISPLAY is pacing this, not the scene: {wait_ms:.1} ms of every frame is spent waiting \
         on `acquire`, {whose}, while the scene itself costs {cost_ms:.1} ms. Project Settings ⏵ \
         Rendering ⏵ Frame pacing chooses how frames are presented."
    )
}

#[cfg(test)]
mod display_pacing_tests {
    use super::{OutputInfo, display_pacing, display_pacing_message};

    fn out(name: &str, x: i64, mhz: u32) -> OutputInfo {
        OutputInfo { name: name.into(), rect: [x, 0, 1920, 1080], mhz: Some(mhz) }
    }

    /// 50.0 ms of `acquire` on a 59.95 Hz output, near-zero scene cost:
    /// three refreshes of the window's own output.
    #[test]
    fn three_refreshes_of_the_windows_own_output() {
        let hdmi = out("HDMI-A-1", 1920, 59_951);
        let p = display_pacing(50.0, 0.3, Some(&hdmi), std::slice::from_ref(&hdmi)).unwrap();
        assert_eq!((p.every, p.output.name.as_str(), p.elsewhere), (3, "HDMI-A-1", false));
    }

    /// **The measured case: a window on the 144 Hz output held to every third
    /// refresh of the 60 Hz one.** 50 ms is 7.2 refreshes of 6.94 ms, so
    /// judged against its own output alone it read as nothing.
    #[test]
    fn a_window_paced_by_another_outputs_clock_names_that_output() {
        let dp = out("DP-2", 0, 144_001);
        let hdmi = out("HDMI-A-1", 1920, 59_951);
        let all = [hdmi.clone(), dp.clone()];
        let p = display_pacing(50.0, 0.3, Some(&dp), &all).expect("the pacing output was not found");
        assert_eq!((p.every, p.output.name.as_str(), p.elsewhere), (3, "HDMI-A-1", true));
        let said = display_pacing_message(&p, 50.0, 0.3, Some(&dp));
        assert!(said.contains("every 3rd refresh of HDMI-A-1 (60.0 Hz)"), "{said}");
        assert!(said.contains("the window is on DP-2 (144.0 Hz)"), "{said}");
    }

    /// One refresh of waiting on the window's own output is vsync working,
    /// and must not be blamed on another output whose period it also fits.
    #[test]
    fn one_refresh_of_waiting_is_ordinary_vsync_not_the_bug() {
        let dp = out("DP-2", 0, 144_001);
        let half = out("slow", 1920, 72_000);
        assert_eq!(display_pacing(6.9, 0.3, Some(&dp), &[dp.clone(), half]), None);
    }

    /// A genuinely slow scene that happens to cost close to two refreshes is
    /// not this: the whole point of the cost half of the test.
    #[test]
    fn a_scene_that_is_actually_slow_is_not_reported_as_display_pacing() {
        let hdmi = out("HDMI-A-1", 1920, 59_951);
        assert_eq!(display_pacing(33.3, 30.0, Some(&hdmi), std::slice::from_ref(&hdmi)), None);
    }

    /// No outputs to compare against, or nothing waited on `acquire`.
    #[test]
    fn nothing_to_compare_against_says_nothing() {
        let hdmi = out("HDMI-A-1", 1920, 59_951);
        assert_eq!(display_pacing(50.0, 0.3, None, &[]), None);
        assert_eq!(display_pacing(0.0, 0.3, Some(&hdmi), std::slice::from_ref(&hdmi)), None);
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::{OutputInfo, RefreshRead, chosen_refresh, window_output};

    // The machine the bug was measured on: a 144 Hz DisplayPort output at the
    // desktop's origin and a 60 Hz HDMI one to its right, listed HDMI first.
    fn dp2() -> OutputInfo {
        OutputInfo { name: "DP-2".into(), rect: [0, 0, 1920, 1080], mhz: Some(144_001) }
    }
    fn hdmi() -> OutputInfo {
        OutputInfo { name: "HDMI-A-1".into(), rect: [1920, 0, 2560, 1440], mhz: Some(59_951) }
    }
    fn hz(r: &RefreshRead) -> f32 {
        1.0 / r.period
    }

    /// **A window moved between a 60 Hz and a 144 Hz output reports the
    /// right refresh on each**, on both kinds of platform.
    #[test]
    fn a_window_moved_between_outputs_reports_each_outputs_refresh() {
        let all = [hdmi(), dp2()];
        let on_dp = [100, 100, 1280, 720];
        let on_hdmi = [2200, 100, 1280, 720];
        // Mostly on HDMI, a corner still on DP-2.
        let straddling = [1500, 100, 1280, 720];

        // X11: the window's rectangle is known, and `current_monitor()` is the
        // output guessed at creation — HDMI, the whole time.
        let stale = hdmi();
        let mut r = RefreshRead::default();
        r = chosen_refresh(&r, window_output(Some(on_dp), Some(&stale), &all), &all);
        assert!((hz(&r) - 144.001).abs() < 0.01, "on DP-2 under X11: {r:?}");
        assert_eq!(r.output.as_deref(), Some("DP-2"));
        r = chosen_refresh(&r, window_output(Some(on_hdmi), Some(&stale), &all), &all);
        assert!((hz(&r) - 59.951).abs() < 0.01, "moved to HDMI under X11: {r:?}");
        r = chosen_refresh(&r, window_output(Some(straddling), Some(&dp2()), &all), &all);
        assert_eq!(r.output.as_deref(), Some("HDMI-A-1"), "the output under most of the window");
        r = chosen_refresh(&r, window_output(Some(on_dp), Some(&stale), &all), &all);
        assert!((hz(&r) - 144.001).abs() < 0.01, "and back: {r:?}");

        // Wayland: no rectangle, ever. `current_monitor()` is None until the
        // surface is shown, then follows the window, with a None between
        // outputs while it is dragged.
        let mut r = RefreshRead::default();
        r = chosen_refresh(&r, window_output(None, None, &all), &all);
        assert!((hz(&r) - 144.001).abs() < 0.01, "boot guesses the fastest output, not the first listed: {r:?}");
        assert!(!r.confirmed, "a guess is re-asked every frame until it is confirmed");
        let (d, h) = (dp2(), hdmi());
        r = chosen_refresh(&r, window_output(None, Some(&h), &all), &all);
        assert!((hz(&r) - 59.951).abs() < 0.01, "shown on HDMI: {r:?}");
        assert!(r.confirmed);
        r = chosen_refresh(&r, window_output(None, None, &all), &all);
        assert!((hz(&r) - 59.951).abs() < 0.01, "a None mid-drag keeps the last answer: {r:?}");
        assert!(!r.confirmed, "…and asks again next frame");
        r = chosen_refresh(&r, window_output(None, Some(&d), &all), &all);
        assert!((hz(&r) - 144.001).abs() < 0.01, "dropped on DP-2: {r:?}");
        assert_eq!(r.output.as_deref(), Some("DP-2"));
    }

    /// A move, a resize or a scale change asks which output the window is on
    /// at the next frame rather than at the next poll; other events do not.
    #[test]
    fn a_moved_window_is_asked_again_at_once() {
        use winit::event::WindowEvent as E;
        let mut ed = crate::Editor { refresh_poll: 50, ..Default::default() };
        ed.note_window_moved(&E::Focused(true));
        assert_eq!(ed.refresh_poll, 50, "an unrelated event re-read the refresh");
        ed.note_window_moved(&E::Moved(winit::dpi::PhysicalPosition::new(2200, 100)));
        assert_eq!(ed.refresh_poll, 0, "a moved window kept the old output until the next poll");
        ed.refresh_poll = 50;
        ed.note_window_moved(&E::Resized(winit::dpi::PhysicalSize::new(2560, 1440)));
        assert_eq!(ed.refresh_poll, 0, "a fullscreen toggle can land on another output");
    }

    /// Measured with `present_stats`: `current_monitor()` is None at window
    /// creation and becomes `Some(DP-2, 144001)` once the surface is mapped.
    /// A None never zeroes a good period, and a display that reports no rate
    /// is treated as no answer.
    #[test]
    fn a_none_from_current_monitor_never_zeroes_a_good_period() {
        let all = [hdmi(), dp2()];
        let live = chosen_refresh(&RefreshRead::default(), Some(&dp2()), &all);
        // In seconds: compared against `dt`. 144.001 Hz -> 6.944 ms.
        assert!((live.period - 1.0 / 144.001).abs() < 1e-6, "{live:?}");
        assert_eq!(chosen_refresh(&live, None, &all).period, live.period);
        let silent = OutputInfo { mhz: None, ..dp2() };
        assert_eq!(chosen_refresh(&live, Some(&silent), &all).period, live.period);
        assert_eq!(chosen_refresh(&RefreshRead::default(), None, &[]).period, 0.0, "genuinely nothing to go on");
    }
}

/// The ⏱ panel's dt-snapping line, and whether it is a warning.
///
/// **Snapping only means something when frames wait for the display.** Under
/// vsync a frame is on screen for a whole number of refreshes, and the snap
/// removes the scheduler's noise from the measurement. Under mailbox or
/// immediate nothing waits for a refresh, so frame times are the scene's own
/// and there is no grid for them to land on. The old line warned there that
/// the snap was inert and blamed the display's reported rate, which sent a
/// reader after a display fault that did not exist. Under vsync an inert snap
/// IS a finding, and the line names the output the frames are really landing
/// on when it can tell.
pub(crate) fn snap_line(p: &Pacing) -> (String, bool) {
    let hz = if p.refresh_ms > 0.0 { 1000.0 / p.refresh_ms } else { 0.0 };
    let on = p.on.as_ref().map(OutputInfo::label).unwrap_or_else(|| "the display".to_string());
    if !p.vsync {
        let mode = p.present_mode;
        if p.snap_rate >= 0.5 {
            return (
                format!(
                    "{mode}: dt snapping on — frames are held to whole refreshes of {on} by the frame \
                     cap, applied to {:.0}% of them.",
                    p.snap_rate * 100.0
                ),
                false,
            );
        }
        let hold = if p.frame_cap > 0.0 {
            format!("the frame cap ({:.0} fps) holds them to its own clock", p.frame_cap)
        } else if hz > 0.0 {
            format!("app.setFrameCap({hz:.0}) would hold them to the refresh of {on}")
        } else {
            "app.setFrameCap holds them to a steady rate".to_string()
        };
        return (
            format!(
                "{mode}: dt snapping does not apply — nothing in this present mode waits for the \
                 display, so frame times are the scene's own and have no refresh to land on. Not a \
                 fault; {hold}."
            ),
            false,
        );
    }
    if p.refresh_ms <= 0.0 {
        return (
            "⚠ no display refresh rate available, so dt snapping is off — frame-time jitter is \
             reaching the simulation clock unfiltered."
                .to_string(),
            true,
        );
    }
    if p.snap_rate >= 0.5 {
        return (
            format!(
                "dt snapping: on — {:.2} ms refresh of {on}, applied to {:.0}% of frames.",
                p.refresh_ms,
                p.snap_rate * 100.0
            ),
            false,
        );
    }
    let why = match display_pacing(p.present_wait_ms, p.cost_ms, p.on.as_ref(), &p.outputs) {
        Some(d) if d.elsewhere => format!(
            "they land on {} of {} instead: the compositor is presenting this window on that \
             output's clock",
            if d.every == 1 { "every refresh".to_string() } else { format!("every {} refreshes", d.every) },
            d.output.label()
        ),
        _ => "the present mode waits for vblank, so frames that miss every whole refresh are being \
              shown on some other clock than the one reported"
            .to_string(),
    };
    (
        format!(
            "⚠ dt snapping is inert — the window is on {on}, {:.2} ms a refresh, but frames arrive \
             every {:.2} ms, no whole number of those refreshes (the snap caught {:.0}% of them): \
             {why}. Frame-time jitter is reaching the simulation clock.",
            p.refresh_ms,
            p.mean_ms,
            p.snap_rate * 100.0
        ),
        true,
    )
}

#[cfg(test)]
mod snap_line_tests {
    use super::{OutputInfo, Pacing, snap_line};

    fn dp2() -> OutputInfo {
        OutputInfo { name: "DP-2".into(), rect: [0, 0, 1920, 1080], mhz: Some(144_001) }
    }
    fn hdmi() -> OutputInfo {
        OutputInfo { name: "HDMI-A-1".into(), rect: [1920, 0, 2560, 1440], mhz: Some(59_951) }
    }

    /// **Mailbox is not a display fault.** Slow, jittery frames under mailbox
    /// land on no multiple of anything, and that is the present mode doing what
    /// it does — the line says snapping does not apply, and does not warn.
    #[test]
    fn mailbox_says_snapping_does_not_apply_rather_than_warning() {
        let p = Pacing {
            present_mode: "mailbox",
            vsync: false,
            mean_ms: 11.3,
            refresh_ms: 1000.0 / 59.951,
            snap_rate: 0.05,
            on: Some(hdmi()),
            outputs: vec![hdmi(), dp2()],
            ..Default::default()
        };
        let (said, warn) = snap_line(&p);
        assert!(!warn, "{said}");
        assert!(said.starts_with("mailbox: dt snapping does not apply"), "{said}");
        assert!(!said.contains("inert"), "{said}");
        assert!(said.contains("app.setFrameCap(60)"), "{said}");
    }

    /// Under vsync an inert snap is a real finding, and when the frames fit
    /// another output's clock the line names it.
    #[test]
    fn an_inert_snap_under_vsync_names_the_output_pacing_the_window() {
        let p = Pacing {
            present_mode: "fifo",
            vsync: true,
            mean_ms: 50.0,
            present_wait_ms: 49.7,
            cost_ms: 0.3,
            refresh_ms: 1000.0 / 144.001,
            snap_rate: 0.0,
            on: Some(dp2()),
            outputs: vec![hdmi(), dp2()],
            ..Default::default()
        };
        let (said, warn) = snap_line(&p);
        assert!(warn, "{said}");
        assert!(said.contains("the window is on DP-2 (144.0 Hz)"), "{said}");
        assert!(said.contains("every 3 refreshes of HDMI-A-1 (60.0 Hz)"), "{said}");
    }

    /// Working snapping says so, quietly.
    #[test]
    fn working_snapping_is_not_a_warning() {
        let p = Pacing {
            present_mode: "fifo",
            vsync: true,
            mean_ms: 6.94,
            refresh_ms: 6.94,
            snap_rate: 0.97,
            on: Some(dp2()),
            ..Default::default()
        };
        let (said, warn) = snap_line(&p);
        assert!(!warn && said.starts_with("dt snapping: on"), "{said}");
    }
}

/// Draw how the frames are arriving, beside what they cost.
///
/// **Two different questions, and an fps number answers neither on its own.** A
/// scene costing 8 ms that presents at 20 fps is a display path pacing the
/// engine; the same 8 ms at 120 fps is the same scene doing fine. And when dt
/// snapping goes inert under vsync, the raw scheduler jitter goes straight into
/// the fixed-step accumulator and the render judders by `velocity x noise`.
/// The readout says so: a load-bearing path that switches off in silence is
/// the worst kind.
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
    // The display pacing the frame, not the scene being slow: `acquire`
    // blocked for a whole number of some output's refreshes while the frame's
    // own work barely registers.
    if let Some(d) = display_pacing(p.present_wait_ms, p.cost_ms, p.on.as_ref(), &p.outputs) {
        ui.small(
            egui::RichText::new(format!(
                "⚠ {}",
                display_pacing_message(&d, p.present_wait_ms, p.cost_ms, p.on.as_ref())
            ))
            .color(warn),
        );
    }
    let (line, warning) = snap_line(p);
    if warning {
        ui.small(egui::RichText::new(line).color(warn));
    } else {
        ui.small(line);
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
    let warn = egui::Color32::from_rgb(230, 150, 90);
    if s.overloaded {
        ui.label(
            egui::RichText::new(
                "The fixed tick costs more than the time it simulates: the game is \
                 playing in slow motion. The largest rows below are why.",
            )
            .color(warn),
        );
    }
    // A rebuild is O(scene) and lands in `mirror` with no script to blame,
    // so name what caused it when it is happening every frame.
    if s.mirror.rebuilds > 0
        && let Some(cause) = &s.mirror_cause
    {
        ui.label(
            egui::RichText::new(format!(
                "The scripts' copy of the scene was rebuilt {}× last frame (cost grows with \
                 the scene). Last cause: {cause}.",
                s.mirror.rebuilds
            ))
            .color(warn),
        );
    }
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

    /// Re-read the display's refresh period from the output the window is
    /// on, without throwing away a good one. See [`window_output`] for which
    /// output that is, and [`chosen_refresh`] for what a non-answer keeps.
    pub(crate) fn reread_refresh_period(&mut self) {
        let Some(w) = self.window.as_ref() else { return };
        let outputs: Vec<OutputInfo> = w.available_monitors().map(|m| OutputInfo::of(&m)).collect();
        let current = w.current_monitor().map(|m| OutputInfo::of(&m));
        let rect = window_rect(w);
        self.apply_refresh_read(rect, current, outputs);
    }

    /// What [`Self::reread_refresh_period`] does with the platform's answers,
    /// apart from asking for them, so a test can play a window moving between
    /// outputs without a display.
    pub(crate) fn apply_refresh_read(&mut self, rect: Option<[i64; 4]>, current: Option<OutputInfo>, outputs: Vec<OutputInfo>) {
        let on = window_output(rect, current.as_ref(), &outputs);
        self.refresh = chosen_refresh(&self.refresh, on, &outputs);
        self.refresh_period = self.refresh.period;
        self.refresh_outputs = outputs;
    }

    /// Ask which output the window is on at the next frame, rather than at
    /// the next poll, when the window has moved, resized or changed scale:
    /// each can put it on another output (a fullscreen toggle, a drag, a
    /// Wayland surface entering an output of another scale).
    pub(crate) fn note_window_moved(&mut self, event: &winit::event::WindowEvent) {
        use winit::event::WindowEvent as E;
        if matches!(event, E::Moved(_) | E::Resized(_) | E::ScaleFactorChanged { .. }) {
            self.refresh_poll = 0;
        }
    }

    /// The output the refresh was last read from, for a message to name.
    pub(crate) fn refresh_output(&self) -> Option<&OutputInfo> {
        let name = self.refresh.output.as_deref()?;
        self.refresh_outputs.iter().find(|o| o.name == name)
    }
}
