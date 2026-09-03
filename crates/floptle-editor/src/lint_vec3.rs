//! `floptle lint --vec3` — what a project would have to change to switch its
//! `vec3` to `fast` (ADR-0028, Phase 3; platform card `floptle/0176`).
//!
//! **This is a textual scan, and it says so.** A Lua file has no types to read,
//! so nothing here can be complete, and a lint that implied otherwise would be
//! worse than none: somebody would take a clean report as a guarantee, flip the
//! setting, and meet the one case it could not see. What it does is find the
//! shapes that actually appear in this engine's scripts, name them with a file
//! and a line, and be honest in the summary about what it cannot know.
//!
//! Two things change when a project moves from `exact` to `fast`:
//!
//! 1. **A vector stops being mutable.** `v.x = 1` raises. The fix is
//!    `v = v:withX(1)`, which exists in both modes precisely so a project can
//!    be moved over before the setting changes.
//! 2. **`type(v)` stops saying `"userdata"`** and starts saying `"vector"`.
//!    A script branching on that answer silently takes the other branch — no
//!    error, no log, which is the failure shape this whole migration is under
//!    orders not to add to.
//!
//! Everything else — the methods, the operators, the constructor forms, what a
//! method accepts as an argument — is identical between the two, and that
//! identity is asserted by a shared corpus in `floptle-script`'s `math_api`
//! rather than promised here.

use std::path::{Path, PathBuf};

/// One thing a project would have to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) file: PathBuf,
    pub(crate) line: u32,
    pub(crate) kind: Kind,
    /// The source line, trimmed — so the report reads without opening the file.
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `v.x = n` on a value this file binds to a vector.
    Mutation,
    /// `type(v)` on one — the answer changes, and nothing raises.
    TypeCheck,
}

impl Kind {
    pub(crate) fn what(self) -> &'static str {
        match self {
            Kind::Mutation => "a vec3 component is assigned",
            Kind::TypeCheck => "type() is asked about a vec3",
        }
    }

    pub(crate) fn fix(self) -> &'static str {
        match self {
            Kind::Mutation => "use `v = v:withX(n)` (or withY/withZ) — it works in both modes",
            Kind::TypeCheck => {
                "`fast` answers \"vector\" where `exact` answers \"userdata\"; test what you \
                 mean instead (e.g. `v.x ~= nil`)"
            }
        }
    }
}

/// Expressions this engine answers with a vector, used to decide that a local
/// holds one.
///
/// Deliberately a list of ENGINE spellings rather than a guess at any
/// expression: the cost of a false positive here is somebody editing working
/// code, which is worse than the cost of a miss (the miss still raises loudly
/// in `fast`, because mutation is an error there rather than a silent nil).
const VEC_SOURCES: &[&str] = &[
    "vec3(",
    ":normalized(",
    ":cross(",
    ":lerp(",
    ":flatten(",
    ":withX(",
    ":withY(",
    ":withZ(",
    ":rotatedY(",
    ":rotatedAround(",
    ":towards(",
    ".pos",
    ".position",
    ".velocity",
    ".forward",
    ".up",
    ".right",
    ".scale",
];

/// Strip a Lua line comment, so a commented-out `v.x = 1` is not a finding.
///
/// Long strings and `--[[ ]]` blocks are not handled, and that is a deliberate
/// limit rather than an oversight: the scan reports possibilities for a human
/// to read, and the summary says as much.
fn strip_comment(line: &str) -> &str {
    match line.find("--") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The identifier ending at `end` (exclusive), if it is a bare name and not
/// the last segment of a path: in `enemy.pos.x = 1` the owner of `.x` is
/// `enemy.pos`, a node field that stays writable in both modes, and a local
/// called `pos` elsewhere in the file must not make it a finding.
fn ident_ending_at(s: &str, end: usize) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut start = end;
    while start > 0 && is_ident_char(bytes[start - 1] as char) {
        start -= 1;
    }
    if start == end {
        return None;
    }
    if start > 0 && matches!(bytes[start - 1], b'.' | b':') {
        return None;
    }
    Some(&s[start..end])
}

/// Byte offsets of every ASSIGNMENT `=` on a line — not the `=` of `==`,
/// `~=`, `<=` or `>=`. All of them, because `if a == b then v = vec3() end`
/// binds `v` at its second `=`, and a scan that took the first and stopped
/// missed every binding that shared a line with a comparison.
fn assignment_eqs(line: &str) -> Vec<usize> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' {
            if b.get(i + 1) == Some(&b'=') {
                i += 2;
                continue;
            }
            if i > 0 && matches!(b[i - 1], b'~' | b'<' | b'>') {
                i += 1;
                continue;
            }
            out.push(i);
        }
        i += 1;
    }
    out
}

/// The bare names an assignment's left-hand side binds: the part of `lhs`
/// after the statement it shares the line with (`;`, or a `then`/`do`/`else`
/// as a whole word), less a leading `local`, split on commas. Anything that is
/// not a plain identifier — `t.pos`, `a[1]` — binds no local and is dropped.
fn names_on_the_left(lhs: &str) -> Vec<String> {
    let mut stmt = lhs.rsplit(';').next().unwrap_or(lhs);
    for kw in ["then", "do", "else"] {
        let mut cut = None;
        for (i, _) in stmt.match_indices(kw) {
            let before = stmt[..i].chars().next_back().is_none_or(|c| !is_ident_char(c));
            let after = stmt[i + kw.len()..].chars().next().is_none_or(|c| !is_ident_char(c));
            if before && after {
                cut = Some(i + kw.len());
            }
        }
        if let Some(c) = cut {
            stmt = &stmt[c..];
        }
    }
    let stmt = stmt.trim();
    let stmt = stmt.strip_prefix("local ").unwrap_or(stmt);
    stmt.split(',')
        .map(str::trim)
        .filter(|n| {
            !n.is_empty()
                && n.chars().all(is_ident_char)
                && !n.starts_with(|c: char| c.is_ascii_digit())
        })
        .map(str::to_string)
        .collect()
}

/// Names this file binds to a vector.
fn vector_locals(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    for raw in src.lines() {
        let line = strip_comment(raw);
        let eqs = assignment_eqs(line);
        for (i, &eq) in eqs.iter().enumerate() {
            // The right-hand side runs to the next assignment on the line, or
            // to the next statement — `x = 1; v = vec3()` must not make `x` a
            // vector.
            let rhs_end = eqs.get(i + 1).copied().unwrap_or(line.len());
            let rhs = &line[eq + 1..rhs_end];
            let rhs = rhs.split(';').next().unwrap_or(rhs);
            if !VEC_SOURCES.iter().any(|p| rhs.contains(p)) {
                continue;
            }
            names.extend(names_on_the_left(&line[..eq]));
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Scan one script.
pub(crate) fn scan(file: &Path, src: &str) -> Vec<Finding> {
    let names = vector_locals(src);
    let mut out = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let line = strip_comment(raw);
        let line_no = i as u32 + 1;

        // A component assignment on a name we believe is a vector.
        for comp in [".x", ".y", ".z"] {
            let mut from = 0;
            while let Some(rel) = line[from..].find(comp) {
                let at = from + rel;
                from = at + comp.len();
                // Must be an assignment, not a read: `= ` after, and not `==`.
                let rest = line[from..].trim_start();
                if !rest.starts_with('=') || rest.starts_with("==") {
                    continue;
                }
                // The character after the component must end the path — `v.xs`
                // is not `v.x`.
                if line[from..].starts_with(is_ident_char) {
                    continue;
                }
                let Some(name) = ident_ending_at(line, at) else { continue };
                // `a.b.x = ` — the base is a path, so the owner is `b`; a node
                // field stays mutable in both modes and must not be flagged.
                if names.iter().any(|n| n == name) {
                    out.push(Finding {
                        file: file.to_path_buf(),
                        line: line_no,
                        kind: Kind::Mutation,
                        text: raw.trim().to_string(),
                    });
                    break;
                }
            }
        }

        // `type(v)` where v is one of ours.
        let mut from = 0;
        while let Some(rel) = line[from..].find("type(") {
            let at = from + rel;
            from = at + "type(".len();
            // Not `typeof(` and not a longer identifier ending in `type`.
            if at > 0 && is_ident_char(line.as_bytes()[at - 1] as char) {
                continue;
            }
            let arg: String =
                line[from..].chars().take_while(|c| is_ident_char(*c)).collect();
            if !arg.is_empty() && names.contains(&arg) {
                out.push(Finding {
                    file: file.to_path_buf(),
                    line: line_no,
                    kind: Kind::TypeCheck,
                    text: raw.trim().to_string(),
                });
            }
        }
    }
    out
}

/// Every `.lua` under `root`, depth-first, skipping hidden and `target`.
fn lua_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(p);
                }
            } else if p.extension().and_then(|e| e.to_str()) == Some("lua") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// `floptle lint --vec3 [PROJECT]`.
///
/// **Exit codes are the answer, not decoration** — the rule this command line
/// follows is that every failure must be a failure rather than a wrong answer
/// at 0 (ADR-0027). `0` means there is nothing to change; `1` means there is,
/// which is what a script wants in order to gate on it; `2` means the scan
/// could not be run at all and the `0`/`1` distinction is therefore
/// meaningless. A project with no scripts is a clean `0`, not an error.
pub(crate) fn run(root: &Path, json: bool) -> i32 {
    let scripts = root.join("scripts");
    let dir = if scripts.is_dir() { scripts } else { root.to_path_buf() };
    if !dir.is_dir() {
        eprintln!("{} is not a directory", dir.display());
        return 2;
    }

    let files = lua_files(&dir);
    let mut findings = Vec::new();
    for f in &files {
        match std::fs::read_to_string(f) {
            Ok(src) => findings.extend(scan(f, &src)),
            Err(e) => {
                eprintln!("could not read {}: {e}", f.display());
                return 2;
            }
        }
    }

    let rel = |p: &Path| -> String {
        p.strip_prefix(root).unwrap_or(p).display().to_string()
    };

    if json {
        // Through serde, like every other verb: a `{:?}` of a String is not
        // JSON — a non-ASCII character in a flagged line comes out as
        // `\u{e9}`, which no parser accepts.
        let items: Vec<serde_json::Value> = findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "file": rel(&f.file),
                    "line": f.line,
                    "kind": match f.kind {
                        Kind::Mutation => "mutation",
                        Kind::TypeCheck => "type_check",
                    },
                    "what": f.kind.what(),
                    "fix": f.kind.fix(),
                    "text": f.text,
                })
            })
            .collect();
        let doc = serde_json::json!({
            "ok": findings.is_empty(),
            "scanned": files.len(),
            "findings": items,
            "complete": false,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        for f in &findings {
            println!("{}:{}: {} — {}", rel(&f.file), f.line, f.kind.what(), f.kind.fix());
            println!("    {}", f.text);
        }
        println!();
        if findings.is_empty() {
            println!(
                "nothing to change in {} script{} — this project looks ready for \
                 `script_vec3: Fast`.",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            );
        } else {
            println!(
                "{} thing{} to change across {} script{}.",
                findings.len(),
                if findings.len() == 1 { "" } else { "s" },
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            );
        }
        // Said on BOTH paths, and last, because the sentence people act on is
        // the clean one: a textual scan cannot see a vector that arrived
        // through a table, a function's return, or a name it could not follow.
        println!(
            "This is a textual scan, not a type checker — it finds the common shapes and \
             cannot promise it found them all. In `fast`, a mutation it missed RAISES rather \
             than passing silently."
        );
    }

    i32::from(!findings.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_str(src: &str) -> Vec<Finding> {
        scan(Path::new("t.lua"), src)
    }

    /// The two shapes that actually change, found and named.
    #[test]
    fn a_component_assignment_and_a_type_check_are_both_found() {
        let hits = scan_str(
            "local v = vec3(1, 2, 3)\n\
             v.x = 5\n\
             if type(v) == 'userdata' then end\n",
        );
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].kind, Kind::Mutation);
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[1].kind, Kind::TypeCheck);
        assert_eq!(hits[1].line, 3);
    }

    /// **A node's own fields stay mutable in both modes**, so flagging them
    /// would send somebody to rewrite working code — the expensive kind of
    /// false positive.
    #[test]
    fn a_nodes_own_component_is_not_a_finding() {
        let hits = scan_str("function update(node, dt)\n  node.x = node.x + dt\nend\n");
        assert!(hits.is_empty(), "a node field was flagged: {hits:?}");
    }

    /// Reading a component is not changing one.
    #[test]
    fn reading_a_component_is_not_a_finding() {
        let hits = scan_str(
            "local v = vec3(1, 2, 3)\n\
             local a = v.x\n\
             if v.x == 3 then end\n\
             print(v.x, v.y, v.z)\n",
        );
        assert!(hits.is_empty(), "a read was flagged: {hits:?}");
    }

    /// A commented-out line is not code.
    #[test]
    fn a_commented_out_mutation_is_not_a_finding() {
        let hits = scan_str("local v = vec3(1,2,3)\n-- v.x = 5\n");
        assert!(hits.is_empty(), "{hits:?}");
    }

    /// A vector that came from the engine rather than the constructor counts —
    /// `node.pos:withY(0)` is the shape this migration is actually about.
    #[test]
    fn a_vector_from_the_engine_is_tracked_too() {
        let hits = scan_str("local p = node.pos\np.y = 0\n");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].kind, Kind::Mutation);
    }

    /// **A binding that shares its line with a comparison is still a binding.**
    /// `if a == b then v = vec3() end` used to register nothing — the scan took
    /// the line's first `=`, found it was `==`, and gave up on the line — so the
    /// `v.x = 1` below it was never a finding. A miss here raises in `fast`
    /// rather than passing, but this shape is everyday game code.
    #[test]
    fn a_binding_after_a_comparison_on_the_same_line_is_tracked() {
        let hits = scan_str(
            "local a, b = 1, 2\n\
             if a == b then v = vec3() end\n\
             for i = 1, 3 do w = node.pos end\n\
             x = 1; u = vec3(1, 2, 3)\n\
             v.x = 1\n\
             w.y = 2\n\
             u.z = 3\n\
             x.x = 4\n",
        );
        let lines: Vec<u32> = hits.iter().map(|h| h.line).collect();
        assert_eq!(lines, vec![5, 6, 7], "{hits:?}");
    }

    /// **A field that shares a local's name is a field.** `enemy.pos.x = 1`
    /// writes a node field, mutable in both modes, and a `local pos = vec3(…)`
    /// elsewhere in the file must not turn it into a finding — that is the
    /// expensive kind of false positive, sending somebody to rewrite code that
    /// works.
    #[test]
    fn a_field_named_like_a_vector_local_is_not_a_finding() {
        let hits = scan_str(
            "local pos = vec3(1, 2, 3)\n\
             enemy.pos.x = 1\n\
             self:target().pos.y = 2\n\
             pos.z = 3\n",
        );
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 4);
    }

    /// `v.xs = 1` is a different field, and `typeof` is not `type`.
    #[test]
    fn near_misses_are_not_findings() {
        let hits = scan_str(
            "local v = vec3(1,2,3)\n\
             local t = { }\n\
             t.xs = 1\n\
             local k = typeof(v)\n",
        );
        assert!(hits.is_empty(), "{hits:?}");
    }

    /// **The exit code is the answer.** The rule this command line follows is
    /// that a failure must be a failure rather than a wrong answer at 0
    /// (ADR-0027), and for a lint that means three distinguishable states:
    /// nothing to do, something to do, and could-not-look. A caller gating a
    /// migration on this reads the code, not the prose.
    #[test]
    fn the_exit_codes_tell_the_three_states_apart() {
        let root = std::env::temp_dir().join(format!("floptle_lint_{}", std::process::id()));
        let scripts = root.join("scripts");
        let _ = std::fs::create_dir_all(&scripts);

        // Clean: a project with scripts and nothing to change.
        std::fs::write(scripts.join("ok.lua"), "local v = vec3(1,2,3)\nlocal a = v.x\n").unwrap();
        assert_eq!(run(&root, false), 0, "a clean project must exit 0");

        // Something to change.
        std::fs::write(scripts.join("bad.lua"), "local v = vec3(1,2,3)\nv.x = 9\n").unwrap();
        assert_eq!(run(&root, false), 1, "a project with work to do must exit 1");

        // Could not look at all — distinct from "looked and found nothing".
        assert_eq!(
            run(&root.join("nope"), false),
            2,
            "a directory that is not there must not read as clean"
        );

        // A project with no scripts is CLEAN, not an error: there is genuinely
        // nothing to change.
        let empty = root.join("empty");
        let _ = std::fs::create_dir_all(empty.join("scripts"));
        assert_eq!(run(&empty, false), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The fix text names the replacement, because a finding whose remedy is
    /// not in it is a finding somebody has to go and research.
    #[test]
    fn every_finding_carries_its_fix() {
        for k in [Kind::Mutation, Kind::TypeCheck] {
            assert!(!k.fix().is_empty());
        }
        assert!(Kind::Mutation.fix().contains("withX"), "{}", Kind::Mutation.fix());
    }
}
