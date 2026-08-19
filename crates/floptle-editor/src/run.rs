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

use std::path::Path;

use crate::console::ConsoleState;

/// The fixed step. The engine's gameplay tick is 60 Hz and a script's `update`
/// runs once per frame, so one step per tick is the cadence that makes
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

/// Run `root` for `span`. Returns the process exit code.
pub(crate) fn run(root: &Path, scene: Option<&str>, span: Span, json: bool) -> i32 {
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
    if let Some(s) = scene {
        let Some(path) = resolve_scene(root, s) else {
            eprintln!("no scene called {s} under {}", root.join("scenes").display());
            return 1;
        };
        ed.open_scene_file(&path);
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
    let steps = span.steps();
    for _ in 0..steps {
        ed.play_step(DT, true);
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

    report(&opened, &ed.console, steps, json)
}

/// A scene named by path, by project-relative path, or by name.
fn resolve_scene(root: &Path, s: &str) -> Option<String> {
    for candidate in [std::path::PathBuf::from(s), root.join(s)] {
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    let named = root.join("scenes").join(format!("{s}.ron"));
    named.is_file().then(|| named.to_string_lossy().into_owned())
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
fn report(
    opened: &[crate::console::ConsoleEntry],
    console: &ConsoleState,
    steps: u32,
    json: bool,
) -> i32 {
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
        let doc = serde_json::json!({
            "ok": errors == 0,
            "steps": steps,
            "seconds": steps as f32 * DT,
            "errors": errors,
            "warnings": warnings,
            "log": lines,
        });
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
    let ran = format!("ran {steps} step(s), {:.2}s of simulated time", steps as f32 * DT);
    match (errors, warnings) {
        (0, 0) => println!("{ran} — nothing raised"),
        (0, w) => println!("{ran} — {w} warning(s)"),
        (e, 0) => println!("{ran} — {e} error(s)"),
        (e, w) => println!("{ran} — {e} error(s), {w} warning(s)"),
    }
    i32::from(errors > 0)
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
