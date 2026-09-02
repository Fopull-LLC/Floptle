//! Turning LuaJIT's load-time errors into the engine's own voice.
//!
//! A script that fails to *load* is the worst failure the scripting layer has:
//! it fires before a single line runs, so the symptom is a whole subsystem
//! silently absent, and every other script that asked for it gets `nil`. The
//! raw messages are terse to the point of being unhelpful — LuaJIT's upvalue
//! ceiling reports as
//!
//! ```text
//! vessel_controller.lua:3669: function at line 2864 has more than 60 upvalues
//! ```
//!
//! which names a line the author did not touch (the END of the offending
//! function, not the reference that tipped it over), does not say that a limit
//! exists, and never mentions the word that would let anyone search for it.
//!
//! So every load error goes through [`explain`] on its way to the Console
//! (`floptle/0086`).

/// LuaJIT's hard ceiling, as its own error message states it.
///
/// This is a **parser detail**, not a policy: it is the number LuaJIT puts in
/// the text [`explain`] rewrites, so the rewrite can quote it back. Ask
/// [`UPVALUE_LIMIT`] whether *this build* has a ceiling at all.
const LUAJIT_UPVALUE_LIMIT: usize = 60;

/// The upvalue ceiling this build's VM enforces — `None` where there is none.
///
/// **Measured, not quoted** (`tests/vm_dialect.rs`, which is the only thing
/// allowed to set this number): LuaJIT refuses a function closing over more
/// than 60, and refuses a chunk declaring more than 200 file-scope locals
/// besides. **Luau enforces neither** — a function closing over 4096 file-scope
/// locals compiles, runs, and returns the right answer.
///
/// `None` rather than a very large number on purpose. A warning threshold of
/// `usize::MAX` is a warning that silently never fires, and every consumer
/// below would keep telling the reader about a limit that is not there. An
/// `Option` makes each of them say what it does when the ceiling is gone.
pub const UPVALUE_LIMIT: Option<usize> =
    if cfg!(feature = "vm-luau") { None } else { Some(LUAJIT_UPVALUE_LIMIT) };

/// Warn once a function is within ten upvalues of the ceiling, where there is
/// one. A script that close is one ordinary edit — one more file-level `local`
/// that a long function happens to reference — from being unloadable.
pub const UPVALUE_WARN: Option<usize> = match UPVALUE_LIMIT {
    Some(n) => Some(n - 10),
    None => None,
};

/// How many file-scope `local`s a script declares — its upvalue pressure.
///
/// Every file-scope local is an upvalue of every function below it, so this
/// number *is* the count LuaJIT will apply to the file's longest function, and
/// [`UPVALUE_LIMIT`] is where that function stops compiling.
///
/// Counted from the source rather than asked of the compiler because mlua runs
/// the engine's Lua in safe mode, where the `debug` library — the only thing
/// that can report a real `nups` — cannot be loaded. Both consumers use this
/// one implementation: the host warns with it at load, and the editor's Lua
/// lint draws its squiggle from it.
///
/// Deliberately line-based, like the lint around it: a full parse would be more
/// exact about oddities nobody writes, and would still be counting the same
/// declarations.
pub fn file_scope_locals(src: &str) -> usize {
    let mut count = 0usize;
    let mut depth = 0i32;
    for raw in src.lines() {
        let code = strip_comments_and_strings(raw);
        let t = code.trim();
        if let Some(rest) = t.strip_prefix("local ") {
            let rest = rest.trim_start();
            let names: Vec<String> = if let Some(f) = rest.strip_prefix("function ") {
                vec![f.split('(').next().unwrap_or("").trim().to_string()]
            } else {
                rest.split('=')
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect()
            };
            if depth == 0 {
                count += names
                    .iter()
                    .filter(|n| !n.is_empty() && n.chars().all(is_ident_char))
                    .count();
            }
        }
        // Block depth, so "file scope" means what it says.
        let words: Vec<&str> = t.split(|c: char| !is_ident_char(c)).filter(|w| !w.is_empty()).collect();
        for (i, w) in words.iter().enumerate() {
            match *w {
                "function" | "if" | "while" | "for" | "repeat" => depth += 1,
                // `do` closes a `for`/`while` header that already counted.
                "do" if !words[..i].iter().any(|p| *p == "for" || *p == "while") => depth += 1,
                "end" | "until" => depth -= 1,
                _ => {}
            }
        }
        depth = depth.max(0);
    }
    count
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A line with its comments and string literals blanked out, so `local` inside
/// a string or a `-- local note` never counts.
fn strip_comments_and_strings(line: &str) -> String {
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
                out.push(' ');
            }
            None => {
                if c == b'-' && b.get(i + 1) == Some(&b'-') {
                    break;
                }
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                    out.push(' ');
                } else {
                    out.push(c as char);
                }
            }
        }
        i += 1;
    }
    out
}

/// Rewrite a load failure into a message that names the script, the limit and
/// the fix. Anything unrecognised passes through with the script's name in
/// front of it, which is still more than the raw error carries.
pub fn explain(name: &str, raw: &str) -> String {
    let first = raw.lines().next().unwrap_or(raw).trim();

    // "function at line N has more than 60 upvalues" — the ceiling.
    if let Some(at) = first.find("has more than")
        && first[at..].contains("upvalues")
    {
        let at_line = first
            .split_whitespace()
            .skip_while(|w| *w != "line")
            .nth(1)
            .and_then(|w| w.trim_end_matches(&[',', ':'][..]).parse::<u32>().ok());
        let where_ = match at_line {
            Some(l) => format!("the function ending at line {l}"),
            None => "one of its functions".to_string(),
        };
        return format!(
            "{name}.lua did not load: {where_} closes over more than {LUAJIT_UPVALUE_LIMIT} upvalues, \
             which is LuaJIT's hard limit. Every file-scope `local` is an upvalue of every \
             function below it, so the fix is to hold related state in ONE table \
             (`local s = {{ … }}` read as `s.foo`) or to split the function — not to move the \
             line the error names, which is only where that function ends."
        );
    }

    // "too many upvalues" — the same wall, phrased differently by some builds.
    if first.contains("too many upvalues") {
        return format!(
            "{name}.lua did not load: a function closes over more than {LUAJIT_UPVALUE_LIMIT} upvalues, \
             which is LuaJIT's hard limit. Every file-scope `local` is an upvalue of every \
             function below it — hold related state in one table (`local s = {{ … }}`) or split \
             the function."
        );
    }

    // Two other LuaJIT ceilings with the same shape: terse, and silent about
    // being limits at all.
    if first.contains("too many local variables") {
        return format!(
            "{name}.lua did not load: one scope declares more than 200 locals, which is \
             LuaJIT's hard limit. Group them into a table, or split the function."
        );
    }
    if first.contains("too many constants") || first.contains("main function has more than") {
        return format!("{name}.lua did not load: {first} — this is a LuaJIT limit on one function; splitting it is the fix.");
    }

    // Everything else: a syntax or top-level runtime error. Name the script,
    // because the raw message names only a chunk.
    format!("{name}.lua did not load: {first}")
}

/// The one-line form for a *consumer* of a broken script — what `findScript`'s
/// caller is told when the handle it holds can never answer, so that a failed
/// load stops looking like "there is no such export".
pub fn unavailable(name: &str, key: &str) -> String {
    format!(
        "`{name}` did not load, so reading `{key}` from its handle gives nil — this is a \
         BROKEN script, not a missing export. Fix the load error reported for {name}.lua; \
         until then nothing this script exports exists."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real message from `solar/scripts/vessel_controller.lua`, twice, a
    /// release apart. It must come out naming the file, the limit and the fix.
    #[test]
    fn the_upvalue_ceiling_is_named_not_echoed() {
        let msg = explain(
            "vessel_controller",
            "solar/scripts/vessel_controller.lua:3669: function at line 2864 has more than 60 upvalues",
        );
        assert!(msg.contains("vessel_controller.lua"), "{msg}");
        assert!(msg.contains("60 upvalues"), "{msg}");
        assert!(msg.contains("LuaJIT"), "{msg}");
        assert!(msg.contains("line 2864"), "names where the function ENDS: {msg}");
        assert!(msg.contains("only where that function ends"), "{msg}");
        assert!(msg.contains("local s ="), "names the fix: {msg}");
    }

    /// An ordinary syntax error still gets the script's name, which the raw
    /// message (`[string "chunk"]:12: ...`) does not carry.
    #[test]
    fn an_ordinary_error_keeps_its_text_and_gains_a_name() {
        let msg = explain("hud", "[string \"hud\"]:12: '=' expected near 'then'");
        assert!(msg.starts_with("hud.lua did not load:"), "{msg}");
        assert!(msg.contains("'=' expected near 'then'"), "{msg}");
    }

    /// The count is of FILE-SCOPE locals only — a local inside a function is
    /// not an upvalue of anything, and moving state in there is the fix the
    /// message recommends, so counting them would punish taking the advice.
    #[test]
    fn only_file_scope_locals_count() {
        let mut src = String::new();
        for i in 0..52 {
            src.push_str(&format!("local v{i} = {i}\n"));
        }
        src.push_str("function update(node, dt)\n  local a, b, c = 1, 2, 3\n  print(a, b, c)\nend\n");
        assert_eq!(file_scope_locals(&src), 52);

        // The fix itself: everything inside the function.
        let fixed = "function update(node, dt)\n  local a, b, c = 1, 2, 3\n  print(a, b, c)\nend\n";
        assert_eq!(file_scope_locals(fixed), 0);

        // `local function` is a local too, and one table is one upvalue.
        assert_eq!(file_scope_locals("local function f() end\nlocal s = { a = 1, b = 2 }\n"), 2);

        // …and neither a comment nor a string is code.
        assert_eq!(file_scope_locals("-- local hidden = 1\nlocal s = \"local also = 2\"\n"), 1);
    }

    /// A broken script and a missing export both read `nil` at the call site;
    /// only the message can tell them apart.
    #[test]
    fn a_broken_script_says_broken_not_missing() {
        let msg = unavailable("orbit_map", "focus");
        assert!(msg.contains("did not load"), "{msg}");
        assert!(msg.contains("not a missing export"), "{msg}");
    }
}
