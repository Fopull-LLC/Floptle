//! `floptle exec` — **change a project correctly, from a script.**
//!
//! Runs a Lua file against a project through the editor's own extension API:
//! `scene.*` to read and write the node graph, `ed.*` to ask the editor about
//! itself, `mesh.*`, `nav.*`, `json.*`, `http.*`.
//!
//! ## Why this instead of a verb per operation
//!
//! Because the alternative is a verb list that grows forever. "Can a script move
//! a node / add a component / retarget a material" would each be a flag, a
//! parser row, a doc line and a test — and the editor already answers all of
//! them, in an API that is documented and held against the live bindings by a
//! test. So the question "can an assistant do X to a project" collapses into "is
//! X in the editor-scripting API", which is one surface growing in one place.
//! Every capability added for a package author arrives here for free, and the
//! other way round.
//!
//! ## It was already headless
//!
//! `crates/floptle-editor/src/ext/` runs package Lua with no window and no GPU,
//! and not by accident: its rule is that **Lua never touches the editor** —
//! every binding reads a per-frame mirror or pushes a command onto a queue the
//! editor drains after the frame. That was adopted so no extension could be
//! holding `&mut Editor` when the editor wanted it back. The side effect is that
//! the whole API is a headless API, and this verb is mostly wiring.
//!
//! ## The commands with no meaning here
//!
//! A script can ask for a window, a dialog, a camera move, the clipboard. None
//! of those exists in a terminal. They are **refused with a line naming the
//! call**, not dropped: a script that thinks it asked a question and got no
//! answer carries on as though the answer were no, and that is the failure mode
//! `mirror_to_stderr` was added to the Console to stop. A correction nobody is
//! told about is one they cannot act on.

use std::path::Path;

use crate::console::ConsoleState;

/// Run `script` against `root`. Returns the process exit code.
pub(crate) fn run(root: &Path, script: &Path, json: bool) -> i32 {
    if !root.join("project.ron").is_file() {
        eprintln!("{} is not a project directory (no project.ron)", root.display());
        return 2;
    }
    if !script.is_file() {
        eprintln!("no script at {}", script.display());
        return 2;
    }

    let mut ed = crate::Editor {
        show_gizmos: false,
        console: ConsoleState { mirror_to_stderr: false, ..Default::default() },
        ..Default::default()
    };
    ed.open_project(root.to_path_buf());
    // The open's own diagnostics belong to the report, and nothing here clears
    // the Console the way pressing Play does — but taking them keeps the two
    // phases separable for the same reason `run` does it.
    let opened: Vec<crate::console::ConsoleEntry> = std::mem::take(&mut ed.console.entries);

    // The mirror the script reads the world through, and the snapshot it reads
    // the editor through: exactly what a package gets on a frame.
    ed.refresh_ext_frame();

    // `World::revision` moves on every spawn, despawn, insert, remove and
    // mutable access, so comparing it either side of the script answers "did
    // this change anything" exactly.
    let before = ed.world.revision();

    let raised = ed.ext.run_file(script, root);
    ed.drain_ext_log();
    // What a terminal cannot do, said rather than silently not done.
    for call in ed.ext.refuse_windowed() {
        ed.console.push(
            floptle_script::LogLevel::Warn,
            format!("`{call}` needs a window, and this is a terminal — the call did nothing"),
            None,
        );
    }
    // Everything else, applied by the editor's own applier — so a node created
    // here is created the way the Hierarchy creates one.
    ed.apply_ext_commands();
    ed.drain_ext_log();

    // **Changed the world and never saved it?** Say so, loudly.
    //
    // In the editor an extension leaves the scene dirty and a person presses
    // Ctrl+S. Here the process exits, and a silent exit takes the work with it:
    // "ran — nothing raised", no file touched, no reason given. That is the same
    // failure the Console's stderr mirror was added to stop — a correction
    // nobody is told about is one they cannot act on.
    //
    // A warning rather than an automatic save, because a script that only reads
    // must not rewrite the scene as a side effect of having looked at it, and
    // `ed.saveScene()` is one line for a script that means it.
    if ed.world.revision() != before && !ed.saved_this_session {
        ed.console.push(
            floptle_script::LogLevel::Warn,
            "this script changed the scene and never called `ed.saveScene()`, so nothing was \
             written — the change lived and died in this process"
                .into(),
            None,
        );
    }

    let script_error = raised.err();
    report(&opened, &ed.console, script_error.as_deref(), json)
}

fn level_str(l: floptle_script::LogLevel) -> &'static str {
    match l {
        floptle_script::LogLevel::Error => "error",
        floptle_script::LogLevel::Warn => "warning",
        floptle_script::LogLevel::Debug => "print",
    }
}

fn report(
    opened: &[crate::console::ConsoleEntry],
    console: &ConsoleState,
    script_error: Option<&str>,
    json: bool,
) -> i32 {
    use floptle_script::LogLevel;
    let all =
        || opened.iter().map(|e| ("open", e)).chain(console.entries.iter().map(|e| ("script", e)));
    let errors = all().filter(|(_, e)| e.level == LogLevel::Error).count()
        + usize::from(script_error.is_some());
    let warnings = all().filter(|(_, e)| e.level == LogLevel::Warn).count();

    if json {
        let log: Vec<serde_json::Value> = all()
            .map(|(phase, e)| {
                let mut o = serde_json::json!({
                    "level": level_str(e.level),
                    "message": e.msg,
                    "count": e.count,
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
            "errors": errors,
            "warnings": warnings,
            "raised": script_error,
            "log": log,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return i32::from(errors > 0);
    }

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
    if let Some(e) = script_error {
        println!("error: script: {e}");
    }
    match (errors, warnings) {
        (0, 0) => println!("ran — nothing raised"),
        (0, w) => println!("ran — {w} warning(s)"),
        (e, 0) => println!("ran — {e} error(s)"),
        (e, w) => println!("ran — {e} error(s), {w} warning(s)"),
    }
    i32::from(errors > 0)
}
