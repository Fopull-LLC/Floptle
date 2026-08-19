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
