//! Lua LINTS — the mistakes a syntax check can't see.
//!
//! Lua's defining hazard is that everything undeclared is a global that reads
//! `nil`: `local speed = 4` then `sped = speed * dt` compiles, runs, does nothing,
//! and reports nothing. Multiply that by hot reload and you get an afternoon of
//! staring at a script that "should work". These three lints exist because each
//! one cost real debugging time in this repo's own projects:
//!
//! * **accidental global** — an assignment to a name that is not a local, not a
//!   declared lifecycle hook, and not part of the engine API. The typo catcher.
//! * **unused local** — declared, never read. Usually a rename half-done.
//! * **upvalue pressure** — LuaJIT allows **60** upvalues per function, and a
//!   file-scope `local` is an upvalue of every function below it. `vessel_controller`
//!   hit the ceiling and the error ("too many upvalues") names no fix, so warn at 50
//!   with the fix in the message.
//!
//! Line-oriented and conservative: a lint fires only when the evidence is on one
//! line. Everything reports as a WARNING — never blocks a run, never edits code.
//! `--@nolint` on a line silences that line; anywhere in the file silences all of it.

/// One lint hit: 1-based line, the message, and whether it's worth a colour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lint {
    pub(crate) line: usize,
    pub(crate) message: String,
    pub(crate) kind: LintKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LintKind {
    /// A likely typo: assigning to an undeclared name.
    AccidentalGlobal,
    /// Declared and never read.
    UnusedLocal,
    /// Approaching LuaJIT's hard per-function upvalue limit.
    UpvaluePressure,
    /// A raw key poll where a named action would work: `input.pressed("space")`
    /// instead of `input.justPressed("Jump")`. Advice, not an error — the code
    /// runs. It just can't be rebound, doesn't reach a gamepad, and reads
    /// neutral on a networked Predicted node.
    RawInput,
}

/// Raw key polls, and the named action that does the same job on every device.
/// Keyed by the SHIPPED starter map, so the advice names something that already
/// exists in the project rather than something to go and invent.
const RAW_INPUT_ADVICE: &[(&str, &str, &str)] = &[
    // (raw call, key literal, the suggestion)
    ("pressed", "space", "input.justPressed(\"Jump\")"),
    ("key", "space", "input.action(\"Jump\")"),
    ("key", "shift", "input.action(\"Sprint\")"),
    ("pressed", "shift", "input.justPressed(\"Sprint\")"),
    ("key", "c", "input.action(\"Crouch\")"),
    ("pressed", "c", "input.justPressed(\"Crouch\")"),
    ("pressed", "e", "input.justPressed(\"Interact\")"),
    ("key", "e", "input.action(\"Interact\")"),
    ("pressed", "escape", "input.justPressed(\"Pause\")"),
    ("key", "w", "input.axis2(\"Move\")"),
    ("key", "a", "input.axis2(\"Move\")"),
    ("key", "s", "input.axis2(\"Move\")"),
    ("key", "d", "input.axis2(\"Move\")"),
];

/// LuaJIT's hard limit (`LJ_MAX_UPVAL`). Not raisable without forking it.
const UPVALUE_LIMIT: usize = 60;
/// Where to start warning — far enough out to leave room to restructure.
const UPVALUE_WARN: usize = 50;

/// Names a script may assign at file scope without declaring them: the lifecycle
/// hooks the host calls, and the globals it publishes to other scripts by
/// convention (`piloting = false` in `vessel_controller` is deliberate — other
/// scripts read it through a script handle).
const HOOKS: &[&str] = &[
    "start",
    "update",
    "fixedUpdate",
    "lateUpdate",
    "onCollisionEnter",
    "onCollisionStay",
    "onCollisionExit",
    "onTriggerEnter",
    "onTriggerStay",
    "onTriggerExit",
    "defaults",
    "params",
];

/// Strip strings and comments, keeping structure (the lints must not read a name
/// out of a comment or a string).
fn code_of(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
                i += 1;
            }
            None => {
                if c == b'-' && b.get(i + 1) == Some(&b'-') {
                    break;
                }
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                    i += 1;
                    continue;
                }
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Identifiers in a line of code, each with the char that follows it (so `a.b`,
/// `a:b` and `a(` can be told apart from a bare `a`).
fn idents(code: &str) -> Vec<(String, char, char)> {
    let chars: Vec<char> = code.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if is_ident_char(chars[i]) && !chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            let before = if start == 0 { ' ' } else { chars[start - 1] };
            let after = chars.get(i).copied().unwrap_or(' ');
            out.push((name, before, after));
        } else {
            i += 1;
        }
    }
    out
}

/// How much this line changes open-bracket nesting (`{`, `(`, `[` vs their
/// closers), counted on code only.
///
/// This is what tells a statement from a table FIELD. A multi-line constructor —
///
/// ```lua
/// local o = {
///   relief = radius * 0.06,   -- not an assignment: a field of `o`
/// }
/// ```
///
/// puts each key on its own line, where it looks exactly like `name = value`. The
/// lints skip any line opened inside brackets; without this, every table in every
/// script reads as a page of accidental globals (18 in `system_generator` alone).
fn bracket_delta(code: &str) -> i32 {
    let opens = code.matches('{').count() + code.matches('(').count() + code.matches('[').count();
    let closes = code.matches('}').count() + code.matches(')').count() + code.matches(']').count();
    opens as i32 - closes as i32
}

/// Lua keywords, which are never variable names.
const KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "self",
];

/// Lint a script. `api` is every global the engine provides (the IDE's own API
/// table, so the lint can never disagree with what autocomplete offers).
pub(crate) fn lint(src: &str, api: &[&str]) -> Vec<Lint> {
    if src.contains("--@nolint") && src.lines().any(|l| l.trim() == "--@nolint") {
        return Vec::new();
    }
    let mut out = Vec::new();

    // Pass 1: collect declarations.
    //
    // `locals` are what the unused-local lint looks at: `local x`, parameters,
    // loop variables. `known` is everything a later assignment may legitimately
    // name, which ALSO includes globals the script publishes on purpose —
    // `piloting = false` at file scope, read by other scripts through a script
    // handle (docs/scripting.md §14). That convention is the reason this lint
    // can't simply flag every bare assignment: in the real projects those
    // publications outnumber typos 100 to 1, and a lint that cries wolf on
    // `fuel = 0` is a lint everyone turns off.
    let mut declared: Vec<(String, usize)> = Vec::new();
    // Parameters, kept apart: they are declarations (assigning to one is not a
    // global) but they are NOT candidates for the unused lint. A lifecycle
    // hook's signature belongs to the ENGINE — `function update(node, dt)`
    // with an unused `dt` is correct code, and every second script has one, so
    // reporting them is how a warnings strip becomes something people switch
    // off. The same goes for a callback that ignores an argument it is handed.
    let mut param_decls: Vec<(String, usize)> = Vec::new();
    let mut published: Vec<String> = Vec::new();
    // File-scope locals, for the upvalue count.
    let mut file_locals = 0usize;
    let mut depth = 0i32;
    let mut bracket = 0i32;
    for (n, raw) in src.lines().enumerate() {
        let code = code_of(raw);
        let t = code.trim();
        let open_before = bracket;
        bracket = (bracket + bracket_delta(&code)).max(0);
        if let Some(rest) = t.strip_prefix("local ") {
            let rest = rest.trim_start();
            let names = if let Some(f) = rest.strip_prefix("function ") {
                vec![f.split('(').next().unwrap_or("").trim().to_string()]
            } else {
                rest.split('=')
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            for name in names {
                if name.chars().all(is_ident_char) && !name.is_empty() {
                    declared.push((name, n + 1));
                    if depth == 0 {
                        file_locals += 1;
                    }
                }
            }
        }
        // Function parameters and loop variables are declarations too.
        if let Some(open) = t.find('(')
            && (t.starts_with("function ") || t.starts_with("local function ") || t.contains("function("))
            && let Some(close) = t[open..].find(')')
        {
            for p in t[open + 1..open + close].split(',') {
                let p = p.trim();
                if !p.is_empty() && p.chars().all(is_ident_char) {
                    declared.push((p.to_string(), n + 1));
                    param_decls.push((p.to_string(), n + 1));
                }
            }
        }
        if let Some(rest) = t.strip_prefix("for ") {
            let head = rest.split(" in ").next().unwrap_or("").split('=').next().unwrap_or("");
            for v in head.split(',') {
                let v = v.trim();
                if !v.is_empty() && v.chars().all(is_ident_char) {
                    declared.push((v.to_string(), n + 1));
                }
            }
        }
        // A bare `name = …` at FILE SCOPE publishes a global deliberately —
        // unless we're inside an open bracket, where it's a table field.
        if depth == 0
            && open_before == 0
            && let Some(eq) = find_assignment(&code)
        {
            for target in code[..eq].split(',') {
                let target = target.trim();
                if !target.is_empty()
                    && target.chars().all(is_ident_char)
                    && !target.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && !KEYWORDS.contains(&target)
                {
                    published.push(target.to_string());
                }
            }
        }
        // `function name(...)` at file scope is a published function.
        if depth == 0 && let Some(rest) = t.strip_prefix("function ") {
            let name = rest.split(['(', '.', ':']).next().unwrap_or("").trim();
            if !name.is_empty() && name.chars().all(is_ident_char) {
                published.push(name.to_string());
            }
        }
        // Track block depth so "file scope" means what it says.
        let words: Vec<&str> = code
            .split(|c: char| !is_ident_char(c))
            .filter(|w| !w.is_empty())
            .collect();
        for (i, w) in words.iter().enumerate() {
            match *w {
                "function" | "if" | "while" | "for" | "repeat" => depth += 1,
                "do" => {
                    if !words[..i].iter().any(|p| *p == "for" || *p == "while") {
                        depth += 1;
                    }
                }
                "end" | "until" => depth -= 1,
                _ => {}
            }
        }
        depth = depth.max(0);
    }
    let is_declared = |name: &str| declared.iter().any(|(d, _)| d == name);
    let is_known = |name: &str| is_declared(name) || published.iter().any(|p| p == name);

    // Pass 2: accidental globals. An ASSIGNMENT (`name =`, not `==`) to a name
    // that is not declared, not a hook, and not engine API.
    let mut bracket = 0i32;
    for (n, raw) in src.lines().enumerate() {
        let code = code_of(raw);
        let t = code.trim();
        let open_before = bracket;
        bracket = (bracket + bracket_delta(&code)).max(0);
        // Inside an open bracket this is a table FIELD or a named argument, not a
        // statement — see `bracket_delta`.
        if open_before > 0 {
            continue;
        }
        if raw.contains("--@nolint") || t.starts_with("local ") || t.starts_with("function ") {
            continue;
        }
        // `a, b = f()` assigns every name on the left.
        let Some(eq) = find_assignment(&code) else { continue };
        let lhs = &code[..eq];
        for target in lhs.split(',') {
            let target = target.trim();
            // Only a BARE name can be an accidental global: `t.x = 1` and
            // `t[k] = 1` are writes into an existing table.
            if target.is_empty() || !target.chars().all(is_ident_char) {
                continue;
            }
            if target.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            if KEYWORDS.contains(&target)
                || HOOKS.contains(&target)
                || is_known(target)
                || api.contains(&target)
            {
                continue;
            }
            // A near-miss on a declared name is almost certainly a typo, and
            // saying so is the whole value of this lint.
            let hint = closest(target, &declared)
                .map(|c| format!(" — did you mean `{c}`?"))
                .unwrap_or_default();
            out.push(Lint {
                line: n + 1,
                message: format!(
                    "`{target}` is not declared, so this writes a GLOBAL{hint} \
                     (add `local`, or ignore with --@nolint)"
                ),
                kind: LintKind::AccidentalGlobal,
            });
        }
    }

    // Pass 3: unused locals. Counted by whole-word appearances outside the
    // declaration itself.
    for (name, line) in &declared {
        if name == "_" || name.starts_with('_') {
            continue; // the conventional "I know" name
        }
        if param_decls.contains(&(name.clone(), *line)) {
            continue; // a parameter — see `param_decls`
        }
        let uses = src
            .lines()
            .enumerate()
            .filter(|(n, _)| *n + 1 != *line)
            .filter(|(_, l)| !l.contains("--@nolint"))
            .map(|(_, l)| code_of(l))
            .filter(|c| {
                idents(c).iter().any(|(id, before, _)| id == name && *before != '.' && *before != ':')
            })
            .count();
        // The declaring line can use the name too — `local a = a + 1`, and the
        // one-liner `for k, v in pairs(t) do print(k, v) end`, where the loop
        // variables are declared and consumed on the same line. A SECOND
        // occurrence on that line is a use.
        let self_line_use = src
            .lines()
            .nth(line - 1)
            .map(code_of)
            .map(|c| {
                idents(&c)
                    .iter()
                    .filter(|(id, before, _)| id == name && *before != '.' && *before != ':')
                    .count()
                    > 1
            })
            .unwrap_or(false);
        if uses == 0 && !self_line_use && !src.lines().nth(line - 1).is_some_and(|l| l.contains("--@nolint")) {
            out.push(Lint {
                line: *line,
                message: format!("`{name}` is declared and never used"),
                kind: LintKind::UnusedLocal,
            });
        }
    }

    // Pass 4: upvalue pressure. Every file-scope local is an upvalue of every
    // function in the file; LuaJIT's ceiling is 60 per function.
    if file_locals >= UPVALUE_WARN {
        out.push(Lint {
            line: 1,
            message: format!(
                "{file_locals} file-scope locals — LuaJIT allows {UPVALUE_LIMIT} upvalues per \
                 function. Group related state in one table (`local s = {{ … }}`) to stay under it"
            ),
            kind: LintKind::UpvaluePressure,
        });
    }

    // Pass 5: raw key polls. The action map is the recommended path and there is
    // nothing in the editor that ever says so — you only find out when a player
    // can't rebind, when the gamepad does nothing, or when a networked character
    // reads its own keys as neutral. So the advice comes to the code, with the
    // exact replacement line, at the place that needs it.
    for (n, raw) in src.lines().enumerate() {
        // The RAW line: this lint is about the string ARGUMENT, which `code_of`
        // (rightly, for every other pass) strips out.
        if raw.contains("--@nolint") {
            continue;
        }
        let code = raw;
        for (call, key, better) in RAW_INPUT_ADVICE {
            let needle = format!("input.{call}(");
            let mut from = 0;
            while let Some(rel) = code[from..].find(&needle) {
                let at = from + rel;
                from = at + needle.len();
                let rest = code[from..].trim_start();
                let lit = rest
                    .strip_prefix('"')
                    .and_then(|r| r.split('"').next())
                    .or_else(|| rest.strip_prefix('\'').and_then(|r| r.split('\'').next()));
                if lit != Some(*key) {
                    continue;
                }
                out.push(Lint {
                    line: n + 1,
                    message: format!(
                        "`input.{call}(\"{key}\")` polls the keyboard directly — {better} does \
                         the same job through the action map, so it can be rebound, works on a \
                         gamepad, and survives multiplayer prediction (Project Settings ⏵ Input)"
                    ),
                    kind: LintKind::RawInput,
                });
            }
        }
    }

    out.sort_by_key(|l| (l.line, l.message.clone()));
    out.dedup();
    out
}

/// The byte index of a top-level `=` that is an ASSIGNMENT, not a comparison and
/// not inside brackets/parens. None when the line assigns nothing.
fn find_assignment(code: &str) -> Option<usize> {
    let b = code.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = if i == 0 { b' ' } else { b[i - 1] };
                let next = b.get(i + 1).copied().unwrap_or(b' ');
                // `==`, `~=`, `<=`, `>=` are comparisons.
                if next == b'=' || matches!(prev, b'=' | b'~' | b'<' | b'>' | b'!') {
                    continue;
                }
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

/// The closest declared name within a small edit distance — the "did you mean"
/// hint. Only offered for a genuinely near miss, since a wrong guess is noise.
fn closest(name: &str, declared: &[(String, usize)]) -> Option<String> {
    let max = if name.len() <= 4 { 1 } else { 2 };
    let mut best: Option<(usize, &str)> = None;
    for (d, _) in declared {
        let dist = edit_distance(name, d);
        if dist <= max && best.is_none_or(|(b, _)| dist < b) {
            best = Some((dist, d));
        }
    }
    best.map(|(_, d)| d.to_string())
}

/// Levenshtein distance, capped by the caller's tolerance.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: &[&str] = &["print", "find", "time", "node", "vec3", "math", "spawn", "input"];

    fn kinds(src: &str) -> Vec<(usize, LintKind)> {
        lint(src, API).into_iter().map(|l| (l.line, l.kind)).collect()
    }

    /// The lint this whole module exists for: a typo'd assignment writes a global
    /// that reads nil, silently, forever.
    #[test]
    fn it_catches_the_typo_that_lua_cannot() {
        let src = "\
local speed = 4
function update(node, dt)
  sped = speed * dt
  node.x = node.x + speed * dt
end
";
        let hits = lint(src, API);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 3);
        assert_eq!(hits[0].kind, LintKind::AccidentalGlobal);
        assert!(hits[0].message.contains("did you mean `speed`?"), "{}", hits[0].message);
    }

    /// What must NOT fire, or the lint is worse than nothing: locals, hooks,
    /// engine API, table fields, indexes, comparisons, and strings.
    #[test]
    fn it_stays_quiet_on_everything_legitimate() {
        let src = "\
defaults = { speed = 1 }
piloting = false --@nolint
local t = { a = 1 }
local i = 0
function start(node)
  t.a = 2
  t[i] = 3
  i = i + 1
  if t.a == 2 then print(\"a = 2\") end
end
function update(node, dt)
  local hp = 10
  hp = hp - dt
  print(hp, i)
end
for k, v in pairs(t) do print(k, v) end
";
        let hits = lint(src, API);
        assert!(hits.is_empty(), "false positives: {hits:?}");
    }

    #[test]
    fn unused_locals_are_reported_once() {
        let src = "\
local used = 1
local unused = 2
local _ignored = 3
print(used)
";
        let hits = lint(src, API);
        assert_eq!(kinds(src), [(2, LintKind::UnusedLocal)], "{hits:?}");
        assert!(hits[0].message.contains("`unused`"));
    }

    /// The upvalue ceiling: warn before LuaJIT's error, and say what to do.
    /// The nudge everyone needs and nobody gets: raw key polling still works,
    /// so nothing ever tells you the action map exists until a player asks why
    /// they can't rebind. The lint names the replacement line, and stays quiet
    /// about code that is already using actions.
    #[test]
    fn a_raw_key_poll_suggests_the_action_that_replaces_it() {
        let api: Vec<&str> = Vec::new();
        let ls = lint("function update(node, dt)\n  if input.pressed(\"space\") then jump() end\nend\n", &api);
        let hit = ls.iter().find(|l| l.kind == LintKind::RawInput).expect("raw poll flagged");
        assert_eq!(hit.line, 2);
        assert!(hit.message.contains("input.justPressed(\"Jump\")"), "names the fix: {}", hit.message);
        // Already on actions: silent.
        let ok = lint("function update(node, dt)\n  if input.justPressed(\"Jump\") then jump() end\nend\n", &api);
        assert!(!ok.iter().any(|l| l.kind == LintKind::RawInput));
        // A key with no shipped action isn't second-guessed.
        let quiet = lint("function update(node, dt)\n  if input.pressed(\"k\") then k() end\nend\n", &api);
        assert!(!quiet.iter().any(|l| l.kind == LintKind::RawInput));
    }

    #[test]
    fn upvalue_pressure_warns_before_luajit_errors() {
        let mut src = String::new();
        for i in 0..52 {
            src.push_str(&format!("local v{i} = {i}\n"));
        }
        src.push_str("function update(node, dt)\n");
        for i in 0..52 {
            src.push_str(&format!("  print(v{i})\n"));
        }
        src.push_str("end\n");
        let hits = lint(&src, API);
        let up: Vec<&Lint> = hits.iter().filter(|l| l.kind == LintKind::UpvaluePressure).collect();
        assert_eq!(up.len(), 1, "{hits:?}");
        assert!(up[0].message.contains("60 upvalues"), "{}", up[0].message);
        assert!(up[0].message.contains("one table"), "the fix must be in the message");

        // A file with locals inside functions instead is fine — that's the fix.
        let ok = "function update(node, dt)\n  local a, b, c = 1, 2, 3\n  print(a, b, c)\nend\n";
        assert!(lint(ok, API).iter().all(|l| l.kind != LintKind::UpvaluePressure));
    }

    /// `--@nolint` on a line, and on its own line for the whole file.
    #[test]
    fn nolint_silences_a_line_and_a_file() {
        let one = "local a = 1\nb = 2 --@nolint\nprint(a)\n";
        assert!(lint(one, API).is_empty(), "{:?}", lint(one, API));
        let all = "--@nolint\nlocal a = 1\nb = 2\nc = 3\n";
        assert!(lint(all, API).is_empty());
    }

    /// Real scripts from both shipped projects: the lints may only report things
    /// worth reporting, so a clean file must stay clean. This is the guard against
    /// a lint that cries wolf on every script in the engine.
    #[test]
    fn the_real_projects_are_reasonably_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let api = crate::ide::api_labels();
        let api_refs: Vec<&str> = api.iter().map(|s| s.as_str()).collect();
        let mut noisy: Vec<String> = Vec::new();
        let mut files = 0;
        for dir in ["solar/scripts", "assets/scripts"] {
            let Ok(rd) = std::fs::read_dir(root.join(dir)) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else { continue };
                files += 1;
                let hits = lint(&src, &api_refs);
                let globals = hits
                    .iter()
                    .filter(|l| l.kind == LintKind::AccidentalGlobal)
                    .count();
                // Zero is the bar for a clean file; a few would be real finds. This
                // threshold is what fails if a future change makes the lint
                // structural-noise-prone again (table fields, published globals,
                // loop variables — each of those cost 18-130 false hits before).
                if globals > 4 {
                    noisy.push(format!("{}: {globals} accidental-global hits", path.display()));
                }
            }
        }
        assert!(files > 20, "expected the real scripts, saw {files}");
        assert!(noisy.is_empty(), "the lint is too noisy on real code:\n{}", noisy.join("\n"));
    }

    /// An unused PARAMETER is not a finding. A lifecycle hook's signature is
    /// the engine's — `function update(node, dt)` that doesn't need `dt` is
    /// correct code, and nagging about it is how a warnings strip becomes
    /// something people switch off. An unused `local` still reports.
    #[test]
    fn an_unused_parameter_is_not_a_finding_but_an_unused_local_is() {
        let src = "\
function update(node, dt)
  local unused = 1
  node.y = 0
end
function onCollisionEnter(node, other, hit)
  other:destroy()
end
";
        let hits = lint(src, API);
        assert_eq!(
            hits.iter().filter(|l| l.kind == LintKind::UnusedLocal).count(),
            1,
            "only the local, not dt/hit: {hits:?}"
        );
        assert!(hits.iter().any(|l| l.message.contains("unused")), "{hits:?}");
    }

    /// The scripts we SHIP as examples must be lint-clean — all of them, every
    /// kind, zero hits.
    ///
    /// They are the first Lua anyone reads, they get copied into real projects,
    /// and a warning triangle on the engine's own example teaches exactly one
    /// lesson: that the warnings don't mean anything. It has also earned its
    /// keep in the other direction — it is what showed that unused *parameters*
    /// were being reported, which no correct `function update(node, dt)` can
    /// avoid.
    #[test]
    fn the_shipped_example_scripts_are_lint_clean() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/scripts");
        let api = crate::ide::api_labels();
        let api_refs: Vec<&str> = api.iter().map(|s| s.as_str()).collect();
        let mut bad: Vec<String> = Vec::new();
        let mut files = 0;
        for entry in std::fs::read_dir(&dir).expect("the examples ship with the engine").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("readable");
            files += 1;
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            for l in lint(&src, &api_refs) {
                bad.push(format!("{name}:{}: {}", l.line, l.message));
            }
        }
        assert!(files >= 15, "expected the shipped examples, saw {files}");
        assert!(bad.is_empty(), "shipped examples must be lint-clean:\n{}", bad.join("\n"));
    }
}

