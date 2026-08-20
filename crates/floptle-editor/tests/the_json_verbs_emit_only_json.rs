//! **`--json` means stdout carries a document and nothing else.**
//!
//! This is not a style rule. A caller runs `floptle run --json` and parses what
//! comes back; one line of prose in front of the document and the parse fails,
//! and it fails in the least helpful way available — the caller reports that the
//! project is broken, when what actually happened is that the tool said "opened
//! project" first.
//!
//! It happened immediately: `Project::open_project` ended with a `println!`, so
//! the very first `run --json` emitted invalid JSON. The fix was to send
//! progress chatter to stderr, where progress chatter belongs, and this test is
//! what keeps it there — because the next `println!` will be added by somebody
//! who has never read this file, in a function three calls away from the verb.
//!
//! It runs the REAL binary. A unit test cannot see this: the whole failure is
//! about what reaches the process's stdout, and nothing that stays inside the
//! process can observe that.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fljson-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Scaffold a real project with the tool itself, so what is being checked is a
/// project the tool would actually be pointed at.
fn scaffold(dir: &Path) {
    let out = Command::new(bin())
        .args(["new", &dir.to_string_lossy(), "--template", "platformer"])
        .output()
        .expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Run a verb, but do not wait forever for it.
///
/// A usage error has to be reported *without running anything*, and the way the
/// old code got `--frames inf` wrong was to accept it and simulate `u32::MAX`
/// steps — eight hundred days. A guard for that must fail in seconds rather
/// than hang the suite, so this kills the child and returns `None` instead.
fn code_within(args: &[&str], secs: u64) -> Option<i32> {
    let mut child = Command::new(bin()).args(args).spawn().expect("spawn the verb");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match child.try_wait().expect("wait") {
            Some(status) => return status.code(),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// Run a verb and return its stdout, insisting it parses as one JSON document.
fn json_of(args: &[&str]) -> serde_json::Value {
    let out = Command::new(bin()).args(args).output().expect("run the verb");
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "`floptle {}` did not put a JSON document on stdout ({e}).\n\
             Something printed to stdout that is not the answer — progress chatter belongs on \
             stderr.\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn every_json_verb_puts_a_document_on_stdout_and_nothing_else() {
    let d = temp("all");
    scaffold(&d);
    let p = d.to_string_lossy().into_owned();

    // The two that need no project at all.
    let help = json_of(&["help", "--json"]);
    assert!(help["verbs"].as_array().is_some_and(|v| !v.is_empty()));
    assert!(json_of(&["version", "--json"])["version"].is_string());

    // The reads.
    assert_eq!(json_of(&["check", &p, "--json"])["ok"], true);
    assert!(json_of(&["inspect", &p, "--json"])["scenes"].as_array().is_some());
    assert!(json_of(&["inspect", &p, "--scene", "first", "--json"])["scenes"].is_array());
    assert_eq!(json_of(&["inspect", &p, "--select", "nothing-is-called-this", "--json"])["matched"], 0);
    assert!(json_of(&["api", "setSprite", "--json"])["entries"].as_array().is_some());

    // …and the one that opens a whole project and plays it, which is where the
    // chatter came from.
    let ran = json_of(&["run", &p, "--frames", "5", "--json"]);
    assert_eq!(ran["ok"], true, "the scaffolded project raised: {ran}");
    assert_eq!(ran["steps"], 5);

    let _ = std::fs::remove_dir_all(&d);
}

/// A verb that fails still owes the caller a document — that is when a program
/// most needs to read one, and an exit code alone does not say what went wrong.
#[test]
fn a_verb_that_fails_still_answers_in_json() {
    let d = temp("broken");
    scaffold(&d);

    // Point a node at a script that is not there: the mistake a generated scene
    // makes, and one no parser can catch.
    let scene = d.join("scenes/first.ron");
    let text = std::fs::read_to_string(&scene).expect("read the scaffolded scene");
    std::fs::write(&scene, text.replace("platformMover", "platformMovr")).expect("write");

    let out = Command::new(bin())
        .args(["check", &d.to_string_lossy(), "--json"])
        .output()
        .expect("run check");
    assert_eq!(out.status.code(), Some(1), "a broken project must exit 1");

    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("a failing check still emits JSON");
    assert_eq!(doc["ok"], false);
    let findings = doc["findings"].as_array().expect("findings");
    assert!(
        findings.iter().any(|f| f["message"].as_str().is_some_and(|m| m.contains("platformMovr"))),
        "the finding did not name the missing script: {doc}"
    );

    let _ = std::fs::remove_dir_all(&d);
}

/// **A script that raises is reported, and reporting it does not break the
/// document.**
///
/// Two failures in one test, because they were two halves of the same mistake.
/// The script host holds its log until somebody drains it, and the drain lived
/// inline in the editor's frame — so a headless run collected nothing at all
/// and reported "nothing raised" for a project raising on every single step.
/// That is the worst answer a checker can give: confident, and wrong.
///
/// And the drain mirrors what scripts say to the terminal, which it did with
/// `println!` — so the moment a script logged anything at all, `run --json`
/// stopped emitting JSON. A project with no logging scripts hides both.
///
/// What this does NOT cover, said plainly rather than assumed: `run` drains
/// per step as well as after Stop, and removing the per-step one still passes
/// here. That drain is there to keep the host's buffer bounded on a long run,
/// not to make the log arrive, and three frames cannot tell the difference.
#[test]
fn a_raising_script_is_reported_and_the_document_still_parses() {
    let d = temp("raises");
    scaffold(&d);

    // `platformMover` is attached to a node in the platformer's own scene, so
    // this actually runs. It logs first (the stdout half) and then indexes nil
    // (the drain half).
    std::fs::write(
        d.join("scripts/platformMover.lua"),
        "function update(node, dt)\n  log(\"about to go wrong\")\n  node.pos = node.postion.x\nend\n",
    )
    .expect("write the script");

    let out = Command::new(bin())
        .args(["run", &d.to_string_lossy(), "--frames", "3", "--json"])
        .output()
        .expect("run");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("run --json stopped being JSON once a script logged ({e}):\n{stdout}")
    });

    assert_eq!(doc["ok"], false, "a script that indexes nil must not read as a clean run");
    assert_eq!(out.status.code(), Some(1));
    let log = doc["log"].as_array().expect("log");
    let raised = log
        .iter()
        .find(|l| l["level"] == "error")
        .unwrap_or_else(|| panic!("no error in the log: {doc}"));
    assert!(
        raised["message"].as_str().is_some_and(|m| m.contains("postion")),
        "the error did not name what went wrong: {raised}"
    );
    assert_eq!(raised["phase"], "play", "it happened while playing, not while opening");
    // What a script printed is in there too, and marked as print rather than
    // as a problem.
    assert!(
        log.iter().any(|l| l["level"] == "print"
            && l["message"].as_str().is_some_and(|m| m.contains("about to go wrong"))),
        "the script's own output went missing: {doc}"
    );

    let _ = std::fs::remove_dir_all(&d);
}

/// **`exec` changes the project, and never loses the change quietly.**
///
/// The editor leaves a scene dirty and a person presses Ctrl+S; here the process
/// exits, so a script that edits and forgets to save takes its work with it and
/// reports "nothing raised". That is the worst shape a mutating tool can have,
/// so the unsaved case is a warning naming the call that was missing — and the
/// saved case has to actually land on disk, which only a second process can
/// confirm.
#[test]
fn exec_writes_when_it_is_told_to_and_says_so_when_it_is_not() {
    let d = temp("exec");
    scaffold(&d);
    let p = d.to_string_lossy().into_owned();
    let script = d.join("rename.lua");

    // 1. Edit and DON'T save.
    std::fs::write(
        &script,
        "local id = scene.find(\"Player\")\nscene.setName(id, \"Hero\")\n",
    )
    .expect("write");
    let doc = json_of(&["exec", &script.to_string_lossy(), &p, "--json"]);
    assert_eq!(doc["ok"], true, "editing without saving is not an error: {doc}");
    let log = doc["log"].as_array().expect("log");
    assert!(
        log.iter().any(|l| l["level"] == "warning"
            && l["message"].as_str().is_some_and(|m| m.contains("saveScene"))),
        "a change that was never written went unmentioned: {doc}"
    );
    // …and it really did not write.
    assert_eq!(
        json_of(&["inspect", &p, "--select", "Hero", "--json"])["matched"],
        0,
        "the scene was written by a script that never asked for it"
    );

    // 2. The same edit, saved.
    std::fs::write(
        &script,
        "local id = scene.find(\"Player\")\nscene.setName(id, \"Hero\")\ned.saveScene()\n",
    )
    .expect("write");
    let doc = json_of(&["exec", &script.to_string_lossy(), &p, "--json"]);
    assert_eq!(doc["ok"], true, "{doc}");
    // …and it does NOT warn. Asserted, because without this the warning could
    // fire on every run and the test above would still pass — which is exactly
    // what the first version of this check did.
    assert!(
        !doc["log"]
            .as_array()
            .expect("log")
            .iter()
            .any(|l| l["message"].as_str().is_some_and(|m| m.contains("saveScene"))),
        "a script that saved was told it had not: {doc}"
    );
    assert_eq!(
        json_of(&["inspect", &p, "--select", "Hero", "--json"])["matched"],
        1,
        "the rename did not survive the process that made it"
    );

    let _ = std::fs::remove_dir_all(&d);
}

/// **A save is a moment in the script, not a fact about the run.**
///
/// Commands apply in the order the script queued them and `ed.saveScene()` is
/// one of them, so an edit made after it is an edit nobody wrote. Answering
/// "did anything happen since the last save" with "did a save ever happen"
/// reported a clean run, exited 0, and dropped the second rename in silence.
#[test]
fn exec_notices_an_edit_made_after_the_save() {
    let d = temp("exec-after-save");
    scaffold(&d);
    let p = d.to_string_lossy().into_owned();
    let script = d.join("late.lua");
    std::fs::write(
        &script,
        "local id = scene.find(\"Player\")\nscene.setName(id, \"First\")\ned.saveScene()\n\
         scene.setName(id, \"Second\")\n",
    )
    .expect("write");

    let doc = json_of(&["exec", &script.to_string_lossy(), &p, "--json"]);
    assert!(
        doc["log"]
            .as_array()
            .expect("log")
            .iter()
            .any(|l| l["level"] == "warning"
                && l["message"].as_str().is_some_and(|m| m.contains("saveScene"))),
        "the edit made after the save went unmentioned: {doc}"
    );
    // The save really did land, and the edit after it really did not.
    assert_eq!(json_of(&["inspect", &p, "--select", "First", "--json"])["matched"], 1);
    assert_eq!(json_of(&["inspect", &p, "--select", "Second", "--json"])["matched"], 0);

    let _ = std::fs::remove_dir_all(&d);
}

/// **Asking to play must not delete the report.**
///
/// `ed.play()` reached the applier, which calls `toggle_play`, which CLEARS the
/// Console — and outside the editor the Console is the whole report. A script
/// that logged and then asked to play exited 0 saying nothing had been raised,
/// with its own output gone.
#[test]
fn exec_refuses_play_and_keeps_what_the_script_said() {
    let d = temp("exec-play");
    scaffold(&d);
    let script = d.join("play.lua");
    std::fs::write(&script, "ed.log(\"precious\")\ned.play()\n").expect("write");

    let doc = json_of(&["exec", &script.to_string_lossy(), &d.to_string_lossy(), "--json"]);
    let log = doc["log"].as_array().expect("log");
    assert!(
        log.iter().any(|l| l["message"].as_str().is_some_and(|m| m.contains("precious"))),
        "the script's output was deleted by its own request to play: {doc}"
    );
    assert!(
        log.iter().any(|l| l["level"] == "warning"
            && l["message"].as_str().is_some_and(|m| m.contains("ed.play"))),
        "asking to play did nothing and said nothing: {doc}"
    );

    let _ = std::fs::remove_dir_all(&d);
}

/// **A span is a number of frames, and every other thing typed there is a
/// usage error.**
///
/// `--frames` was read as a float, so `nan` passed the "at least one frame"
/// test and cast to zero — a run that simulated nothing, reported success and
/// exited 0. `inf` cast to `u32::MAX`, which is eight hundred days and reads as
/// a hang. `2.7` silently became 2.
#[test]
fn a_span_that_is_not_a_number_of_frames_is_a_usage_error() {
    let d = temp("span");
    scaffold(&d);
    let p = d.to_string_lossy().into_owned();

    for bad in ["nan", "inf", "-1", "2.7", "lots"] {
        assert_eq!(
            code_within(&["run", &p, "--frames", bad, "--json"], 20),
            Some(2),
            "`--frames {bad}` was not refused as a usage error (None = it was still \
             running after 20s, which is the `inf` failure)"
        );
    }
    for bad in ["inf", "nan", "0", "-3"] {
        assert_eq!(
            code_within(&["run", &p, "--seconds", bad, "--json"], 20),
            Some(2),
            "`--seconds {bad}` was not refused as a usage error"
        );
    }
    // …and a real one still works, reporting what ran and what was asked for.
    let ok = json_of(&["run", &p, "--frames", "3", "--json"]);
    assert_eq!(ok["steps"], 3);
    assert_eq!(ok["requested"], 3);

    let _ = std::fs::remove_dir_all(&d);
}

/// **One mistake, one exit code.** Pointing any verb at a directory that is not
/// a project is the same mistake, and it answered three different ways: 2 from
/// `run`, `shot` and `exec`, 1 from `check`, and 0 from `inspect` — which
/// printed "no readable project.ron" as though that were the answer.
#[test]
fn a_directory_that_is_not_a_project_is_refused_the_same_way_everywhere() {
    let d = temp("notaproject");
    std::fs::create_dir_all(&d).expect("mkdir");
    let p = d.to_string_lossy().into_owned();

    for args in [
        vec!["check", &p],
        vec!["inspect", &p],
        vec!["run", &p, "--frames", "1"],
    ] {
        let out = Command::new(bin()).args(&args).output().expect("run the verb");
        assert_eq!(
            out.status.code(),
            Some(2),
            "`floptle {}` did not refuse a non-project with 2: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            out.stdout.is_empty(),
            "`floptle {}` wrote a refusal to stdout, where the answer goes",
            args.join(" ")
        );
    }

    let _ = std::fs::remove_dir_all(&d);
}

/// **`--scene` means one thing.** `inspect` resolved a scene by stem anywhere
/// under `scenes/`, case-insensitively; `run` matched `scenes/<exact>.ron` and
/// nothing else. So `--scene Arena` worked in one verb and exited 1 in the
/// other while both helps said "by path or by name".
#[test]
fn every_verb_resolves_a_scene_name_the_same_way() {
    let d = temp("scenename");
    scaffold(&d);
    let p = d.to_string_lossy().into_owned();
    // A scene in a subdirectory, named in a case nobody will type back.
    std::fs::create_dir_all(d.join("scenes/levels")).expect("mkdir");
    std::fs::copy(d.join("scenes/first.ron"), d.join("scenes/levels/Arena.ron")).expect("copy");

    for spelling in ["Arena", "arena", "scenes/levels/Arena.ron"] {
        assert!(
            json_of(&["inspect", &p, "--scene", spelling, "--json"])["scenes"]
                .as_array()
                .is_some_and(|s| s.len() == 1),
            "inspect could not find the scene as {spelling:?}"
        );
        let out = Command::new(bin())
            .args(["run", &p, "--scene", spelling, "--frames", "1", "--json"])
            .output()
            .expect("run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "run could not find the scene as {spelling:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let _ = std::fs::remove_dir_all(&d);
}

/// A call that needs a window is refused by name, not quietly skipped. A script
/// that thinks it asked a question and got no answer carries on as though the
/// answer were no.
#[test]
fn exec_names_the_calls_a_terminal_cannot_serve() {
    let d = temp("exec-win");
    scaffold(&d);
    let script = d.join("win.lua");
    std::fs::write(&script, "ed.message(\"a\", \"b\")\ned.copy(\"x\")\n").expect("write");

    let doc = json_of(&["exec", &script.to_string_lossy(), &d.to_string_lossy(), "--json"]);
    let said: Vec<&str> =
        doc["log"].as_array().expect("log").iter().filter_map(|l| l["message"].as_str()).collect();
    for call in ["ed.message", "ed.copy"] {
        assert!(
            said.iter().any(|m| m.contains(call)),
            "{call} did nothing and said nothing: {doc}"
        );
    }

    let _ = std::fs::remove_dir_all(&d);
}

/// **A shot is the editor's picture, or it is worth nothing.**
///
/// It ran the post chain with the tonemap alone and defaulted the rest, so a
/// project with bloom, a vignette, ambient occlusion, posterise or its own
/// `stage post` shader photographed as a scene that has none of them — while
/// the verb's help said the picture is what the editor would show.
///
/// Asserted the only way it can be: render one scene twice, differing only in
/// a full-strength vignette, and insist the two files differ. Under the old
/// code they were byte-identical.
///
/// **Only the vignette**, deliberately. The first version of this toggled a
/// two-band posterise as well, and posterise reaches the picture by another
/// route entirely — so the test passed with the whole chain reverted, on the
/// strength of the setting that was never broken. A guard that can be satisfied
/// by the one thing that already worked is not a guard.
#[test]
fn a_shot_shows_the_post_processing_the_scene_asks_for() {
    let d = temp("post");
    scaffold(&d);
    let base = std::fs::read_to_string(d.join("scenes/first.ron")).expect("read the scene");
    let at = base.rfind("    ],").expect("the node list ends somewhere");

    let node = |vignette: &str, bands: &str| {
        format!(
            "        (\n            name: \"Post Processing\",\n\
             \x20           transform: (translation: (0.0, 0.0, 0.0), \
             rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),\n\
             \x20           matter: PostProcess(\n\
             \x20               enabled: true, bloom: false, bloom_threshold: 1.0, \
             bloom_intensity: 0.7,\n\
             \x20               vignette: {vignette}, vignette_strength: 1.0, \
             vignette_radius: 0.15,\n\
             \x20               ao: Off, ao_strength: 1.0, ao_radius: 0.7,\n\
             \x20               posterize_bands: {bands}, posterize_dither: false,\n\
             \x20           ),\n            scripts: [],\n        ),\n"
        )
    };
    for (name, vignette, bands) in [("on", "true", "0"), ("off", "false", "0")] {
        let mut text = base.clone();
        text.insert_str(at, &node(vignette, bands));
        std::fs::write(d.join(format!("scenes/{name}.ron")), text).expect("write");
    }

    let shoot = |scene: &str| {
        let out = d.join(format!("{scene}.png"));
        let r = Command::new(bin())
            .args([
                "shot",
                &d.to_string_lossy(),
                "--scene",
                scene,
                "--size",
                "160x90",
                "--out",
                &out.to_string_lossy(),
            ])
            .output()
            .expect("run shot");
        (r, out)
    };

    let (on, on_png) = shoot("on");
    if !on.status.success() {
        let why = String::from_utf8_lossy(&on.stderr);
        // **Two ways a machine can be unable to answer this**, and neither is a
        // failure of the thing under test. There may be no adapter at all; or
        // there may be one that cannot build the renderer — CI's is OpenGL,
        // where a texture may not be sampled by two samplers, and the raster
        // pipeline binds the terrain palette to a filtering one and a nearest
        // one. That is a real gap and it is written down in HANDOFF, but it is
        // not what this test is about.
        //
        // Said out loud either way. A skip nobody sees is a test that stopped
        // existing, and this one covers a bug that shipped.
        let cannot_render = why.contains("no GPU") || why.contains("could not build the renderer");
        assert!(cannot_render, "shot failed for a reason that is not the adapter:\n{why}");
        eprintln!("skipped — this machine cannot render:\n{why}");
        let _ = std::fs::remove_dir_all(&d);
        return;
    }
    let (off, off_png) = shoot("off");
    assert!(off.status.success(), "{}", String::from_utf8_lossy(&off.stderr));

    let a = std::fs::read(&on_png).expect("read the shot");
    let b = std::fs::read(&off_png).expect("read the shot");
    assert_ne!(
        a, b,
        "a full-strength vignette changed nothing about the picture, so the scene's own \
         post settings are not reaching the chain"
    );

    let _ = std::fs::remove_dir_all(&d);
}

/// **`bake gi` bakes and exits, with no window anywhere.**
///
/// Its help said "and exit" for a long time while it did the opposite: the flag
/// set a marker, the editor opened a window, and a frame hook ran the bake a
/// slice at a time. On a build server that is not a slow bake, it is no bake —
/// there is no display to open. Nothing about the bake needed the window; it
/// renders offscreen and writes its own file.
#[test]
fn baking_light_probes_needs_no_window_and_writes_the_file() {
    let d = temp("bakegi");
    scaffold(&d);
    let p = d.to_string_lossy().into_owned();

    // A scene with nothing to bake says so, rather than baking nothing.
    let out = Command::new(bin()).args(["bake", "gi", &p]).output().expect("run bake gi");
    let why = String::from_utf8_lossy(&out.stderr);
    if why.contains("could not build the renderer") {
        eprintln!("skipped — this machine cannot render:\n{why}");
        let _ = std::fs::remove_dir_all(&d);
        return;
    }
    assert_eq!(out.status.code(), Some(1), "a scene with no Light Probes node must exit 1");
    assert!(why.contains("Light Probes"), "the refusal did not name what is missing: {why}");

    // …and one with a probes volume produces a bake beside the scene.
    let scene = d.join("scenes/first.ron");
    let text = std::fs::read_to_string(&scene).expect("read the scene");
    let at = text.rfind("    ],").expect("the node list ends somewhere");
    let mut with_probes = text.clone();
    with_probes.insert_str(
        at,
        "        (\n            name: \"Light Probes\",\n\
         \x20           transform: (translation: (0.0, 2.0, 0.0), \
         rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),\n\
         \x20           matter: LightProbes(half_extents: (6.0, 4.0, 6.0), spacing: 3.0, \
         quality: 8),\n            scripts: [],\n        ),\n",
    );
    std::fs::write(&scene, with_probes).expect("write");

    let doc = json_of(&["bake", "gi", &p, "--json"]);
    assert_eq!(doc["ok"], true, "the bake reported a problem: {doc}");

    let baked = d.join("scenes/first.fgi");
    let bytes = std::fs::read(&baked).expect("the bake wrote no file beside the scene");
    assert!(bytes.len() > 64, "the bake wrote {} bytes, which is not a bake", bytes.len());
    // **It rendered the scene, rather than emitting a shape full of nothing.**
    // A grid of zeroed probes is exactly what a bake that never ran would write,
    // and it would still be the right size.
    let nonzero = bytes.iter().filter(|b| **b != 0).count();
    assert!(
        nonzero * 2 > bytes.len(),
        "the bake is mostly zeroes ({nonzero} of {}), so it captured no light",
        bytes.len()
    );

    let _ = std::fs::remove_dir_all(&d);
}
