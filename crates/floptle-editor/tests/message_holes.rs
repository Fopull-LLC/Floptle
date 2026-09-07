//! Every sentence the engine says to a person is a whole sentence.
//!
//! Rust's line continuation inside a string is `\` at the end of the line, and
//! the leading whitespace of the next line is then *not* part of the string.
//! Drop the backslash and the string keeps eighteen spaces in the middle of a
//! sentence:
//!
//! ```text
//!   "…is not a node property. See docs/editor-scripting.md, or
//!    scene.info(id) for what a node carries"
//! ```
//!
//! reaches the reader as *"…or                  scene.info(id)…"*. Nothing
//! catches it: it compiles, the test that asserts the message `contains` a
//! phrase still passes, and it is invisible in review because the source looks
//! like a normal wrapped string. Twenty-two of these had accumulated across
//! hover texts, Inspector help and error messages by v0.85.0, and every one of
//! them was a hole in a sentence somebody read.
//!
//! **This is a guard against a specific mechanical accident**, not a style
//! rule. It only fires between two letters, in a literal with no `\n` in it —
//! so an ASCII table, a `println!` column and a Lua sample in a doc string are
//! all left alone, because that is where deliberate runs of spaces live.

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

/// A run of six or more spaces sitting between two letters.
///
/// Six because a deliberate two- or four-space run inside a sentence is
/// conceivable and a six-space one is not; between letters because that is what
/// distinguishes prose from a column. The neighbours include `,`, `.`, `;`, `:`,
/// `—` and `)` on the left, which are how the wrapped line before the missing
/// backslash usually ends.
fn hole_at(body: &str) -> Option<String> {
    // Never in a literal that spans lines itself: those are tables and samples.
    if body.contains("\\n") {
        return None;
    }
    // Nor in a COLUMN LAYOUT. Two or more SEPARATE runs of four spaces is
    // somebody aligning things on purpose, and prose never does it twice.
    //
    // Counting distinct runs, not `split("    ").count()` — that counts chunks
    // *inside* one long run, so an eighteen-space hole read as three columns
    // and exempted itself. The bug the guard is for would have slipped through
    // its own exemption.
    // …but a SENTENCE is never a column, however many holes it has. `anim.rs`
    // carried a message with two of them and the run-count exemption swallowed
    // it whole — two holes in one sentence is worse than one, not evidence of a
    // table. A period followed by a space is the thing tables do not have.
    let prose = body.contains(". ");
    if !prose && runs_of_four(body) >= 2 {
        return None;
    }
    let b: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == ' ' {
            let start = i;
            while i < b.len() && b[i] == ' ' {
                i += 1;
            }
            let run = i - start;
            let before = start.checked_sub(1).and_then(|j| b.get(j)).copied();
            let after = b.get(i).copied();
            // Deliberately NOT `)` or `"`: those are how an aligned code
            // sample ends (`find("Player")   first node in the scene`), which
            // is the one place a wide gap is the point.
            let ok_before = before.is_some_and(|c| c.is_alphabetic() || ",.;:—".contains(c));
            let ok_after = after.is_some_and(|c| c.is_alphabetic() || c == '(');
            if run >= 6 && ok_before && ok_after {
                let from = start.saturating_sub(35);
                let to = (i + 35).min(b.len());
                return Some(b[from..to].iter().collect());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// How many separate runs of four-or-more spaces a string contains.
fn runs_of_four(body: &str) -> usize {
    let mut runs = 0;
    let mut run = 0;
    for c in body.chars() {
        if c == ' ' {
            run += 1;
        } else {
            if run >= 4 {
                runs += 1;
            }
            run = 0;
        }
    }
    if run >= 4 {
        runs += 1;
    }
    runs
}

/// Every double-quoted literal on a line, skipping comment lines.
fn literals(line: &str) -> Vec<String> {
    if line.trim_start().starts_with("//") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let cs: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '"' {
            let mut j = i + 1;
            let mut body = String::new();
            while j < cs.len() {
                if cs[j] == '\\' {
                    body.push(cs[j]);
                    if let Some(n) = cs.get(j + 1) {
                        body.push(*n);
                    }
                    j += 2;
                    continue;
                }
                if cs[j] == '"' {
                    break;
                }
                body.push(cs[j]);
                j += 1;
            }
            out.push(body);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

#[test]
fn no_message_has_a_hole_in_the_middle_of_a_sentence() {
    let root = repo().join("crates");
    let mut found: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            // This file's own fixtures are holes on purpose.
            if p.file_name().is_some_and(|n| n == "message_holes.rs") {
                continue;
            }
            // Probes print aligned tables to a terminal, on purpose. They are
            // developer tools rather than a surface a player or a game
            // developer ever reads.
            if p.components().any(|c| c.as_os_str() == "examples") {
                continue;
            }
            let body = std::fs::read_to_string(&p).unwrap_or_default();
            for (n, line) in body.lines().enumerate() {
                scanned += 1;
                for lit in literals(line) {
                    if let Some(around) = hole_at(&lit) {
                        let rel = p.strip_prefix(repo()).unwrap_or(&p).display();
                        found.push(format!("{rel}:{}: …{around}…", n + 1));
                    }
                }
            }
        }
    }
    assert!(scanned > 50_000, "only {scanned} lines scanned — the walk is broken");
    found.sort();
    assert!(
        found.is_empty(),
        "{} message(s) have a run of spaces in the middle of a sentence, which is a \
         Rust line continuation whose `\\` went missing:\n  {}\n\nThe fix is the backslash \
         at the end of the previous line, not deleting the spaces by hand.",
        found.len(),
        found.join("\n  ")
    );
}

#[test]
fn the_hole_detector_knows_prose_from_a_column() {
    // The real defect, in its real shape.
    assert!(hole_at("or                  scene.info(id) for what a node carries").is_some());
    assert!(hole_at("this element — its scripts get                  hoverStart").is_some());

    // …and every place a run of spaces is deliberate.
    assert!(hole_at("  driver      {}").is_none(), "a printed column");
    assert!(hole_at("after Raise          : worst {w:.2}").is_none(), "an aligned table");
    assert!(hole_at("function f()\\n        aligned = 1\\nend").is_none(), "a code sample");
    assert!(hole_at("save.set(\\\"hp\\\", hp)                 -- a comment").is_none(), "sample");
    assert!(hole_at("normal prose with single spaces").is_none());
    assert!(hole_at("a short   run of three").is_none(), "three is not the accident");
    assert!(hole_at("moon      seed 3: 12 chunks     40 MB").is_none(), "two runs = a table");
    // Two holes in one SENTENCE is still prose, and worse than one.
    assert!(
        hole_at("being ignored.                      Rename one: a file and a file in the                      same folder").is_some(),
        "a sentence with two holes must not read as a table"
    );
    // …and the exemption must not swallow the defect it is next to: ONE long
    // run is a hole however long it is.
    assert_eq!(runs_of_four("or                  scene.info(id)"), 1);
    assert_eq!(runs_of_four("a    b    c"), 2);
}
