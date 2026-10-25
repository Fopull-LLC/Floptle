//! `floptle lint --vec3`: what a project would have to change to switch its
//! `vec3` to `fast`.
//!
//! A textual scan, and it says so. A Lua file has no types to read, so
//! nothing here can be complete, and a clean report is not a guarantee. It
//! finds the shapes that appear in this engine's scripts, names them with a
//! file and a line, and says in the summary what it cannot know.
//!
//! Two things change when a project moves from `exact` to `fast`:
//!
//! 1. A vector stops being mutable. `v.x = 1` raises. The fix is
//!    `v = v:withX(1)`, which exists in both modes so a project can be moved
//!    over before the setting changes.
//! 2. `type(v)` stops saying `"table"` (`"userdata"` before 0.117) and says `"vector"`. A script
//!    branching on that answer silently takes the other branch.
//!
//! Everything else (the methods, the operators, the constructor forms, what a
//! method accepts as an argument) is identical between the two, and that
//! identity is asserted by a shared corpus in `floptle-script`'s `math_api`.

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
                "`fast` answers \"vector\" where `exact` answers \"table\" (\"userdata\" before 0.117); test what you \
                 mean instead (e.g. `v.x ~= nil`)"
            }
        }
    }
}

/// The source with every string's contents and every comment blanked out,
/// line for line, so a keyword, a `--` or an `=` inside a string is not code
/// and a commented-out `v.x = 1` is not a finding. Long strings and
/// `--[[ ]]` blocks are followed across lines: an `end` inside one would
/// otherwise close a scope that is still open.
fn code_lines(src: &str) -> Vec<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum In {
        Code,
        Short(char),
        Long(usize),
        LineComment,
        LongComment(usize),
    }
    // `[[`, `[=[`, …: the level, and how many characters the opener takes.
    fn long_open(c: &[char]) -> Option<(usize, usize)> {
        if c.first() != Some(&'[') {
            return None;
        }
        let eqs = c[1..].iter().take_while(|&&x| x == '=').count();
        (c.get(1 + eqs) == Some(&'[')).then_some((eqs, eqs + 2))
    }
    fn long_close(c: &[char], level: usize) -> bool {
        c.first() == Some(&']')
            && c.len() > level + 1
            && c[1..=level].iter().all(|&x| x == '=')
            && c[level + 1] == ']'
    }
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut state = In::Code;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            out.push('\n');
            if state == In::LineComment {
                state = In::Code;
            }
            i += 1;
            continue;
        }
        match state {
            In::Code => {
                if c == '-' && chars.get(i + 1) == Some(&'-') {
                    if let Some((level, len)) = long_open(&chars[i + 2..]) {
                        state = In::LongComment(level);
                        out.extend(std::iter::repeat_n(' ', 2 + len));
                        i += 2 + len;
                    } else {
                        state = In::LineComment;
                        out.push_str("  ");
                        i += 2;
                    }
                    continue;
                }
                if let Some((level, len)) = long_open(&chars[i..]) {
                    state = In::Long(level);
                    out.push('"');
                    out.extend(std::iter::repeat_n(' ', len - 1));
                    i += len;
                    continue;
                }
                if c == '"' || c == '\'' {
                    state = In::Short(c);
                }
                out.push(c);
            }
            In::Short(q) => {
                if c == '\\' {
                    out.push(' ');
                    if chars.get(i + 1).is_some_and(|&n| n != '\n') {
                        out.push(' ');
                        i += 1;
                    }
                } else if c == q {
                    state = In::Code;
                    out.push(c);
                } else {
                    out.push(' ');
                }
            }
            In::Long(level) | In::LongComment(level) => {
                if long_close(&chars[i..], level) {
                    let closer = if matches!(state, In::Long(_)) { '"' } else { ' ' };
                    out.extend(std::iter::repeat_n(' ', level + 1));
                    out.push(closer);
                    i += level + 2;
                    state = In::Code;
                    continue;
                }
                out.push(' ');
            }
            In::LineComment => out.push(' '),
        }
        i += 1;
    }
    out.split('\n').map(str::to_string).collect()
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

/// Byte offsets of every assignment `=` on a line — not the `=` of `==`,
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

/// The bare names an assignment's left-hand side binds, and whether it
/// declares them `local`: the part of `lhs` after the statement it shares
/// the line with (`;`, or a `then`/`do`/`else`/`repeat` as a whole word),
/// split on commas. Anything that is not a plain identifier — `t.pos`,
/// `a[1]` — binds no local and is dropped.
fn names_on_the_left(lhs: &str) -> (bool, Vec<String>) {
    let mut stmt = lhs.rsplit(';').next().unwrap_or(lhs);
    for kw in ["then", "do", "else", "repeat"] {
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
    let (local, stmt) = match stmt.strip_prefix("local ") {
        Some(rest) => (true, rest),
        None => (false, stmt),
    };
    let names = stmt
        .split(',')
        .map(str::trim)
        .filter(|n| {
            !n.is_empty()
                && n.chars().all(is_ident_char)
                && !n.starts_with(|c: char| c.is_ascii_digit())
        })
        .map(str::to_string)
        .collect();
    (local, names)
}

/// Expressions this engine answers with a vector, used to decide that a
/// name holds one.
///
/// Deliberately a list of engine spellings rather than a guess at any
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

/// Is the value of `rhs` a vector? Only when a source is the expression
/// itself, not a piece of it: `#scatter.near(id, vec3(x, y, z), r)` is a
/// count that takes a vector, `(a.pos - b.pos):length()` and `node.pos.y`
/// are numbers, and `{ at = node.pos }` is a table.
fn rhs_is_vector(rhs: &str) -> bool {
    let b = rhs.as_bytes();
    // What follows a value: a field, an index or a method makes it something
    // else (a vector-returning method is a source of its own).
    let used = |at: usize| {
        let rest = rhs[at..].trim_start();
        rest.starts_with(['.', ':', '['])
    };
    VEC_SOURCES.iter().any(|src| {
        rhs.match_indices(src).any(|(at, _)| {
            let depth = rhs[..at].bytes().fold(0i32, |d, c| match c {
                b'(' | b'[' | b'{' => d + 1,
                b')' | b']' | b'}' => d - 1,
                _ => d,
            });
            if depth != 0 {
                return false;
            }
            let end = at + src.len();
            if src.ends_with('(') {
                // `vec3(` must not be the end of a longer name.
                if !src.starts_with([':', '.']) && at > 0 && (is_ident_char(b[at - 1] as char) || matches!(b[at - 1], b'.' | b':')) {
                    return false;
                }
                let mut d = 1;
                let mut j = end;
                while j < b.len() && d > 0 {
                    match b[j] {
                        b'(' => d += 1,
                        b')' => d -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                d == 0 && !used(j)
            } else {
                // `.up` is not `.upgrade`.
                !rhs[end..].starts_with(is_ident_char) && !used(end)
            }
        })
    })
}

/// What the scan knows about names at one point in a file: a stack of
/// scopes, innermost last, each naming whether a name holds a vector.
///
/// A name is only a vector from where it is bound until its scope ends, and
/// a parameter or a later binding of the same name is not. A file-wide set
/// of names flagged a `local s = w.scale` in one function and every `s.x =`
/// and `type(s)` in every other function.
struct Scopes(Vec<Vec<(String, bool)>>);

impl Scopes {
    fn new() -> Self {
        Self(vec![Vec::new()])
    }

    fn open(&mut self) {
        self.0.push(Vec::new());
    }

    fn close(&mut self) {
        if self.0.len() > 1 {
            self.0.pop();
        }
    }

    fn declare(&mut self, name: &str, vector: bool) {
        if let Some(top) = self.0.last_mut() {
            top.retain(|(n, _)| n != name);
            top.push((name.to_string(), vector));
        }
    }

    /// An assignment without `local`: the innermost scope that has the name,
    /// or a global.
    fn assign(&mut self, name: &str, vector: bool) {
        for scope in self.0.iter_mut().rev() {
            if let Some(slot) = scope.iter_mut().find(|(n, _)| n == name) {
                slot.1 = vector;
                return;
            }
        }
        self.0[0].push((name.to_string(), vector));
    }

    fn is_vector(&self, name: &str) -> bool {
        self.0.iter().rev().find_map(|s| s.iter().find(|(n, _)| n == name).map(|(_, v)| *v)).unwrap_or(false)
    }
}

/// The identifiers in a comma list starting at `from`, up to the first
/// thing that is neither: `a, b` of `local a, b = …`, `i, v` of `for i, v in`.
fn ident_list(s: &str, from: usize) -> Vec<String> {
    let mut out = Vec::new();
    for part in s[from..].split(',') {
        let name: String = part.trim_start().chars().take_while(|c| is_ident_char(*c)).collect();
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        let rest = part.trim_start()[name.len()..].trim();
        out.push(name);
        if !rest.is_empty() {
            break;
        }
    }
    out
}

/// One thing that happens at a byte offset of a line, in the order the
/// scan meets them.
enum Event {
    Word(&'static str),
    Assign(usize),
    Mutation(String),
    TypeOf(String),
}

const WORDS: &[&str] = &["function", "do", "then", "repeat", "else", "elseif", "end", "until", "for", "local"];

/// Scan one script.
pub(crate) fn scan(file: &Path, src: &str) -> Vec<Finding> {
    let raw_lines: Vec<&str> = src.lines().collect();
    let mut scopes = Scopes::new();
    // Names a `for` declares, waiting for its `do`.
    let mut loop_vars: Vec<String> = Vec::new();
    // Inside a table constructor, `name = value` is a field, not a binding;
    // a constructor can run over several lines.
    let mut braces = 0i32;
    let mut out = Vec::new();
    for (i, line) in code_lines(src).iter().enumerate() {
        let line = line.as_str();
        let raw = raw_lines.get(i).copied().unwrap_or("");
        let line_no = i as u32 + 1;
        let mut events: Vec<(usize, Event)> = Vec::new();

        // Keywords, as whole words and never as the name after a `.` or `:`.
        let bytes = line.as_bytes();
        let mut j = 0;
        while j < bytes.len() {
            if is_ident_char(bytes[j] as char) {
                let start = j;
                while j < bytes.len() && is_ident_char(bytes[j] as char) {
                    j += 1;
                }
                let word = &line[start..j];
                let field = start > 0 && matches!(bytes[start - 1], b'.' | b':');
                if !field && let Some(w) = WORDS.iter().find(|w| **w == word) {
                    events.push((start, Event::Word(w)));
                }
            } else {
                j += 1;
            }
        }

        // Assignments outside a table constructor.
        let mut depth_at = Vec::with_capacity(bytes.len() + 1);
        let mut d = braces;
        for &c in bytes {
            depth_at.push(d);
            match c {
                b'{' => d += 1,
                b'}' => d -= 1,
                _ => {}
            }
        }
        braces = d.max(0);
        for eq in assignment_eqs(line) {
            if depth_at.get(eq).copied().unwrap_or(0) <= 0 {
                events.push((eq, Event::Assign(eq)));
            }
        }

        // A component assignment: `.x =`, not `.x ==` and not `.xs =`.
        for comp in [".x", ".y", ".z"] {
            let mut from = 0;
            while let Some(rel) = line[from..].find(comp) {
                let at = from + rel;
                from = at + comp.len();
                let rest = line[from..].trim_start();
                if !rest.starts_with('=') || rest.starts_with("==") {
                    continue;
                }
                if line[from..].starts_with(is_ident_char) {
                    continue;
                }
                // `a.b.x = ` — the base is a path, so the owner is `b`; a node
                // field stays mutable in both modes and must not be flagged.
                if let Some(name) = ident_ending_at(line, at) {
                    events.push((at, Event::Mutation(name.to_string())));
                }
            }
        }

        // `type(v)`, not `typeof(` and not a longer identifier ending in `type`.
        let mut from = 0;
        while let Some(rel) = line[from..].find("type(") {
            let at = from + rel;
            from = at + "type(".len();
            if at > 0 && (is_ident_char(bytes[at - 1] as char) || matches!(bytes[at - 1], b'.' | b':')) {
                continue;
            }
            let arg: String = line[from..].chars().take_while(|c| is_ident_char(*c)).collect();
            if !arg.is_empty() {
                events.push((at, Event::TypeOf(arg)));
            }
        }

        events.sort_by_key(|(at, _)| *at);
        let eqs = assignment_eqs(line);
        let mut flagged = (false, false);
        for (at, ev) in events {
            match ev {
                Event::Word("function") => {
                    scopes.open();
                    // The parameters: plain names, never vectors as far as
                    // this file can tell.
                    if let Some(open) = line[at..].find('(').map(|o| at + o) {
                        let close = line[open..].find(')').map_or(line.len(), |c| open + c);
                        for p in line[open + 1..close].split(',') {
                            let p = p.trim();
                            if !p.is_empty() && p.chars().all(is_ident_char) {
                                scopes.declare(p, false);
                            }
                        }
                    }
                }
                Event::Word("for") => loop_vars = ident_list(line, at + "for".len()),
                Event::Word("do") => {
                    scopes.open();
                    for v in std::mem::take(&mut loop_vars) {
                        scopes.declare(&v, false);
                    }
                }
                Event::Word("then" | "repeat") => scopes.open(),
                Event::Word("else") => {
                    scopes.close();
                    scopes.open();
                }
                Event::Word("elseif" | "end" | "until") => scopes.close(),
                Event::Word("local") => {
                    // `local a, b` declares before any `=`, so an assignment
                    // after it is to this local, not to a global.
                    let names = ident_list(line, at + "local".len());
                    if names.first().is_some_and(|n| n != "function") {
                        for n in names {
                            scopes.declare(&n, false);
                        }
                    }
                }
                Event::Word(_) => {}
                Event::Assign(eq) => {
                    // The right-hand side runs to the next assignment on the
                    // line, or to the next statement — `x = 1; v = vec3()`
                    // must not make `x` a vector.
                    let next = eqs.iter().copied().find(|&e| e > eq).unwrap_or(line.len());
                    let rhs = &line[eq + 1..next];
                    let rhs = rhs.split(';').next().unwrap_or(rhs);
                    // …and it stops at a keyword that ends the statement.
                    let rhs = [" end", " then", " do", " else"]
                        .iter()
                        .filter_map(|k| rhs.find(k))
                        .min()
                        .map_or(rhs, |k| &rhs[..k]);
                    let vector = rhs_is_vector(rhs);
                    let (local, names) = names_on_the_left(&line[..eq]);
                    for n in names {
                        if local {
                            scopes.declare(&n, vector);
                        } else {
                            scopes.assign(&n, vector);
                        }
                    }
                }
                Event::Mutation(name) => {
                    if !flagged.0 && scopes.is_vector(&name) {
                        flagged.0 = true;
                        out.push(Finding {
                            file: file.to_path_buf(),
                            line: line_no,
                            kind: Kind::Mutation,
                            text: raw.trim().to_string(),
                        });
                    }
                }
                Event::TypeOf(name) => {
                    if !flagged.1 && scopes.is_vector(&name) {
                        flagged.1 = true;
                        out.push(Finding {
                            file: file.to_path_buf(),
                            line: line_no,
                            kind: Kind::TypeCheck,
                            text: raw.trim().to_string(),
                        });
                    }
                }
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
/// at 0. `0` means there is nothing to change; `1` means there is,
/// which is what a script wants in order to gate on it; `2` means the scan
/// could not be run at all and the `0`/`1` distinction is therefore
/// meaningless. A project with no scripts is a clean `0`, not an error.
pub(crate) fn run(root: &Path, json: bool) -> i32 {
    let scripts = root.join("scripts");
    let dir = if scripts.is_dir() { scripts } else { root.to_path_buf() };
    if !dir.is_dir() {
        floptle_say::say_err!("{} is not a directory", dir.display());
        return 2;
    }

    let files = lua_files(&dir);
    let mut findings = Vec::new();
    for f in &files {
        match std::fs::read_to_string(f) {
            Ok(src) => findings.extend(scan(f, &src)),
            Err(e) => {
                floptle_say::say_err!("could not read {}: {e}", f.display());
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
        floptle_say::say!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        for f in &findings {
            floptle_say::say!("{}:{}: {} — {}", rel(&f.file), f.line, f.kind.what(), f.kind.fix());
            floptle_say::say!("    {}", f.text);
        }
        floptle_say::say!();
        if findings.is_empty() {
            floptle_say::say!(
                "nothing to change in {} script{} — this project looks ready for \
                 `script_vec3: Fast`.",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            );
        } else {
            floptle_say::say!(
                "{} thing{} to change across {} script{}.",
                findings.len(),
                if findings.len() == 1 { "" } else { "s" },
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            );
        }
        // Said on both paths, and last, because the sentence people act on is
        // the clean one: a textual scan cannot see a vector that arrived
        // through a table, a function's return, or a name it could not follow.
        floptle_say::say!(
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

    /// **A name is a vector only where it is bound to one.** Every shape here
    /// was a false positive on a real game's scripts: the scan kept one set of
    /// names for the whole file, so a `local s = w.scale` in one function made
    /// a string parameter `s` in another a vector, and a `local n = vec3(…)`
    /// near the end of a file flagged an `n.x =` two thousand lines above it.
    #[test]
    fn a_vector_local_does_not_reach_other_functions_or_earlier_lines() {
        let hits = scan_str(
            "function scale_of(w)\n\
             \x20 local s = w.scale or 1\n\
             \x20 return s\n\
             end\n\
             function decode(s)\n\
             \x20 if type(s) ~= \"string\" then return nil end\n\
             end\n\
             function fill(n, s)\n\
             \x20 n.x, n.y, n.z = 1, 2, 3\n\
             \x20 s.x, s.y, s.z = 4, 5, 6\n\
             end\n\
             function place()\n\
             \x20 local o = vec3(0, 0, 0)\n\
             \x20 return o\n\
             end\n\
             function offsets()\n\
             \x20 local o = {}\n\
             \x20 o.x = 1\n\
             \x20 if true then w.x, w.y = 1, 2 end\n\
             end\n\
             function later()\n\
             \x20 local n = vec3(1, 2, 3)\n\
             \x20 local w = vec3(1, 2, 3)\n\
             end\n",
        );
        assert!(hits.is_empty(), "{hits:?}");
    }

    /// **A vector given to a call is not what the call returns.** A count of
    /// props near a point, a length, a component and a table holding a
    /// vector are none of them vectors, and a field inside a table
    /// constructor (on its own line or not) binds no name.
    #[test]
    fn a_value_that_only_contains_a_vector_is_not_one() {
        let hits = scan_str(
            "local n = #scatter.near(id, vec3(1, 2, 3), 5)\n\
             local d = (a.pos - b.pos):length()\n\
             local h = node.pos.y\n\
             local t = { at = node.pos }\n\
             local u = node.upgrade\n\
             cfg = {\n\
             \x20 dir = node.forward,\n\
             }\n\
             if type(n) == \"number\" and type(d) == \"number\" and type(h) == \"number\" then end\n\
             t.x = 1\n\
             u.x = 1\n\
             dir.x = 1\n",
        );
        assert!(hits.is_empty(), "{hits:?}");
    }

    /// **The same scan still finds what it should**, scoped: a vector in a
    /// function is a vector to the end of it, inside its loops and branches,
    /// a parameter shadows an outer vector only inside its function, and a
    /// string or a comment holding `end` closes nothing.
    #[test]
    fn scoped_vectors_are_still_found() {
        let hits = scan_str(
            "local p = node.pos\n\
             function f(p)\n\
             \x20 p.x = 1\n\
             end\n\
             p.y = 2\n\
             function g()\n\
             \x20 local v = vec3(1, 2, 3):normalized()\n\
             \x20 print(\"the end\") --[[ end\n\
             \x20 end ]]\n\
             \x20 for i = 1, 3 do\n\
             \x20   if i == 2 then v.z = i end\n\
             \x20 end\n\
             \x20 if type(v) == \"table\" then end\n\
             end\n",
        );
        let lines: Vec<(u32, Kind)> = hits.iter().map(|h| (h.line, h.kind)).collect();
        assert_eq!(lines, vec![(5, Kind::Mutation), (11, Kind::Mutation), (13, Kind::TypeCheck)], "{hits:?}");
    }

    /// **The exit code is the answer.** The rule this command line follows is
    /// that a failure must be a failure rather than a wrong answer at 0
    ///, and for a lint that means three distinguishable states:
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

        // A project with no scripts is clean, not an error: there is genuinely
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
