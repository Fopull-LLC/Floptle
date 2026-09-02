//! Turning a *runtime* error into the engine's voice — and making the two VMs
//! say the same thing.
//!
//! [`crate::load_error`] does this for errors that fire before a script runs.
//! This is its sibling for the ones that fire while it does, and it exists
//! because of a difference measured while porting the engine to Luau
//! (ADR-0028). Given the commonest mistake anybody makes in a game script —
//! one transposed letter in a field name:
//!
//! ```lua
//! node.pos = node.postion.x
//! ```
//!
//! | | what the VM says |
//! | --- | --- |
//! | LuaJIT | `attempt to index field 'postion' (a nil value)` |
//! | Luau | `attempt to index nil with 'x'` |
//!
//! Luau names the field being read *from* the nil rather than the expression
//! that *was* nil, so the typo — the only part the reader can act on — is the
//! one word missing. Worse, `missingGlobal.x` and a nil local `t.x` produce
//! that identical sentence, so three distinct bugs are one message.
//!
//! ## What this does about it
//!
//! The error carries the script and the line; the engine has the source. So
//! read the line and name **both halves**:
//!
//! ```text
//! platformMover.lua:3: attempt to index nil with 'x'
//!   `node.postion` is nil, and `.x` was read from it.
//!       node.pos = node.postion.x
//! ```
//!
//! Two properties are the point, and both are worth more than the extra words:
//!
//! 1. **It names the typo**, which neither VM does on its own — LuaJIT names
//!    `postion` without the receiver, Luau names `x` which is not the problem.
//! 2. **Both VMs produce the identical sentence.** The rewrite is driven by the
//!    source line, not by the VM's phrasing, so the dual-VM diff harness sees
//!    one message. That is the property that lets the default flip without a
//!    game's error output changing.
//!
//! ## What it refuses to do
//!
//! **Guess.** If a line indexes `.x` on two different receivers, or reaches
//! through a `]` this does not parse, no name is offered — only the source
//! line, which is still more than either VM gives. A confidently wrong name
//! would send somebody to fix the wrong expression, which is worse than the
//! terse message it replaced.
//!
//! The original text is always kept, first and unmodified: it is what a reader
//! searches for, and other tools match on it.

/// Rewrite one runtime error, given the source line it names.
///
/// `raw` is the whole message as the VM produced it (possibly with a stack
/// traceback under it). `line_text` is the script's own text at the line the
/// message names, or `None` if it could not be read — in which case `raw` comes
/// back untouched, because every improvement here is derived from that line.
pub fn explain(raw: &str, line_text: Option<&str>) -> String {
    let Some(text) = line_text else { return raw.to_string() };
    let text = text.trim_end();
    if text.trim().is_empty() {
        return raw.to_string();
    }

    let mut head = raw.lines();
    let first = head.next().unwrap_or(raw);
    let rest: Vec<&str> = head.collect();

    let mut out = String::from(first);
    if let Some(nil_expr) = nil_expression(first, text.trim()) {
        out.push_str(&format!("\n  `{}` is nil", nil_expr.name));
        if let Some(read) = nil_expr.read {
            out.push_str(&format!(", and `.{read}` was read from it"));
        }
        out.push('.');
    }
    // The line itself, always. Even where nothing could be named, seeing the
    // statement beats being told a line number and going to look it up.
    out.push_str(&format!("\n      {}", text.trim()));
    for line in rest {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// What was nil, and (where there was one) the field read from it.
struct NilExpr {
    name: String,
    read: Option<String>,
}

/// Work out what the message is really about, from the message and the line.
///
/// The two VMs are read differently on purpose — each is asked for the part it
/// actually knows — and both are resolved against the same source line, so the
/// answer comes out identical.
fn nil_expression(msg: &str, line: &str) -> Option<NilExpr> {
    // Luau: `attempt to index nil with 'x'`. It names the field being READ; the
    // nil is whatever precedes it in the source.
    if let Some(field) = quoted_after(msg, "attempt to index nil with ") {
        let recv = receiver_of(line, &field)?;
        return Some(NilExpr { name: recv, read: Some(field) });
    }
    // LuaJIT: `attempt to index field 'postion' (a nil value)`. It names the
    // nil itself, but only its last component — so the receiver is recovered
    // from the line too, to produce the same full expression Luau's path does.
    if let Some(field) = quoted_after(msg, "attempt to index field ") {
        let full = match receiver_of(line, &field) {
            Some(recv) => format!("{recv}.{field}"),
            None => field.clone(),
        };
        return Some(NilExpr { name: full, read: read_after(line, &field) });
    }
    // LuaJIT: a bare name, already complete.
    for prefix in ["attempt to index global ", "attempt to index local ", "attempt to index upvalue "]
    {
        if let Some(name) = quoted_after(msg, prefix) {
            let read = read_after(line, &name);
            return Some(NilExpr { name, read });
        }
    }
    None
}

/// The contents of the first `'…'` following `prefix` in `msg`.
fn quoted_after(msg: &str, prefix: &str) -> Option<String> {
    let at = msg.find(prefix)? + prefix.len();
    let rest = &msg[at..];
    let open = rest.find('\'')?;
    let close = rest[open + 1..].find('\'')?;
    Some(rest[open + 1..open + 1 + close].to_string())
}

/// The expression immediately before `.field` in `line` — the thing that was nil.
///
/// Returns `None` when the line does not contain that access, when the
/// receiver cannot be read as a plain chain of names, or when **two different**
/// receivers index the same field on one line, which is the ambiguous case this
/// refuses to guess at.
fn receiver_of(line: &str, field: &str) -> Option<String> {
    let needle = format!(".{field}");
    let mut found: Option<String> = None;
    let mut at = 0usize;
    while let Some(rel) = line[at..].find(&needle) {
        let dot = at + rel;
        at = dot + needle.len();
        // `.x` must not be the head of `.xyz`.
        if line[at..].chars().next().is_some_and(is_name_char) {
            continue;
        }
        let Some(recv) = chain_ending_at(line, dot) else { continue };
        match &found {
            Some(prev) if *prev != recv => return None, // ambiguous — say nothing
            _ => found = Some(recv),
        }
    }
    found
}

/// The field read from `name` on this line, if `name` is followed by one.
///
/// Only used to complete the sentence where the VM already named the nil, so a
/// miss is fine — the message just says less.
fn read_after(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!("{name}."))? + name.len() + 1;
    let end = line[at..].find(|c: char| !is_name_char(c)).unwrap_or(line.len() - at);
    let read = &line[at..at + end];
    (!read.is_empty()).then(|| read.to_string())
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Read backwards from `end` (the position of a `.`) over `a.b.c`.
///
/// Deliberately narrow. A chain of plain names is the case that covers almost
/// every real script, and anything else — an index by `[…]`, a call's result,
/// a string literal — returns `None` so no name is offered rather than a
/// misleading one. A leading digit means a number literal (`1.5`), not a name.
fn chain_ending_at(line: &str, end: usize) -> Option<String> {
    let b = line.as_bytes();
    let mut start = end;
    while start > 0 {
        let c = b[start - 1] as char;
        if is_name_char(c) || c == '.' {
            start -= 1;
        } else {
            break;
        }
    }
    let chain = line.get(start..end)?.trim_matches('.');
    if chain.is_empty() || chain.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    // A chain that reached backwards past a `]` or a `)` is not a plain name.
    if start > 0 && matches!(b[start - 1] as char, ']' | ')' | '"' | '\'') {
        return None;
    }
    Some(chain.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both VMs' phrasings of the SAME mistake must come out as the same
    /// sentence. This is the property the whole module exists for: the dual-VM
    /// diff harness compares what a game prints, and a message that differs by
    /// VM is a difference a player would see.
    #[test]
    fn the_two_vms_produce_the_same_explanation_for_a_typo() {
        let line = "  node.pos = node.postion.x";
        let luau = explain("[string \"mover\"]:3: attempt to index nil with 'x'", Some(line));
        let luajit = explain(
            "[string \"mover\"]:3: attempt to index field 'postion' (a nil value)",
            Some(line),
        );
        assert!(luau.contains("`node.postion` is nil"), "{luau}");
        assert!(luajit.contains("`node.postion` is nil"), "{luajit}");
        assert!(luau.contains("`.x` was read from it"), "{luau}");
        assert!(luajit.contains("`.x` was read from it"), "{luajit}");
        // …and both quote the offending statement.
        assert!(luau.contains("node.pos = node.postion.x"), "{luau}");
        assert!(luajit.contains("node.pos = node.postion.x"), "{luajit}");
    }

    /// A missing global and a nil local are DIFFERENT bugs, and Luau gives them
    /// one message. The line tells them apart.
    #[test]
    fn a_missing_global_and_a_nil_local_stop_reading_alike() {
        let g = explain(
            "[string \"m\"]:1: attempt to index nil with 'x'",
            Some("return missingGlobal.x"),
        );
        let l = explain("[string \"m\"]:2: attempt to index nil with 'x'", Some("  return t.x"));
        assert!(g.contains("`missingGlobal` is nil"), "{g}");
        assert!(l.contains("`t` is nil"), "{l}");
        assert_ne!(g, l, "the whole point is that these two no longer read the same");
    }

    /// The original text survives, first and unchanged: it is what a reader
    /// searches for and what other tooling matches on.
    #[test]
    fn the_raw_message_is_kept_at_the_front() {
        let raw = "[string \"m\"]:3: attempt to index nil with 'x'";
        let out = explain(raw, Some("  a = b.postion.x"));
        assert!(out.starts_with(raw), "{out}");
    }

    /// A stack traceback stays attached, and stays BELOW the explanation — the
    /// sentence a reader needs must not be buried under twenty frames.
    #[test]
    fn a_traceback_is_kept_and_stays_underneath() {
        let raw = "[string \"m\"]:3: attempt to index nil with 'x'\nstack traceback:\n\t[C]: in ?";
        let out = explain(raw, Some("  a = b.postion.x"));
        assert!(out.contains("stack traceback:"), "{out}");
        let expl = out.find("is nil").expect("explained");
        assert!(expl < out.find("stack traceback:").expect("traceback"), "{out}");
    }

    /// **It refuses to guess.** Two different receivers indexing the same field
    /// on one line means no name is offered — only the line.
    #[test]
    fn an_ambiguous_line_names_nothing_and_still_shows_itself() {
        let out = explain(
            "[string \"m\"]:4: attempt to index nil with 'x'",
            Some("  local d = a.pos.x - b.pos.x"),
        );
        assert!(!out.contains("is nil"), "a guess was made: {out}");
        assert!(out.contains("a.pos.x - b.pos.x"), "{out}");
    }

    /// The same receiver twice is not ambiguous — it is one answer.
    #[test]
    fn the_same_receiver_twice_is_still_one_answer() {
        let out = explain(
            "[string \"m\"]:4: attempt to index nil with 'x'",
            Some("  local d = a.pos.x * a.pos.x"),
        );
        assert!(out.contains("`a.pos` is nil"), "{out}");
    }

    /// An expression this does not parse gets no name rather than a wrong one.
    #[test]
    fn a_receiver_that_is_not_a_plain_chain_is_left_alone() {
        let out = explain(
            "[string \"m\"]:4: attempt to index nil with 'x'",
            Some("  local d = list[i].x"),
        );
        assert!(!out.contains("is nil"), "guessed at an indexed receiver: {out}");
        assert!(out.contains("list[i].x"), "{out}");
    }

    /// `.x` must not match inside `.xyz`.
    #[test]
    fn a_field_name_is_matched_whole() {
        let out = explain(
            "[string \"m\"]:4: attempt to index nil with 'x'",
            Some("  return node.xyz + 1"),
        );
        assert!(!out.contains("is nil"), "matched a prefix of another name: {out}");
    }

    /// A number literal is not a receiver.
    #[test]
    fn a_decimal_point_is_not_a_field_access() {
        let out = explain("[string \"m\"]:4: attempt to index nil with '5'", Some("  return 1.5"));
        assert!(!out.contains("is nil"), "{out}");
    }

    /// With no source to read, the message passes through exactly as it came.
    #[test]
    fn without_the_source_line_nothing_is_invented() {
        let raw = "[string \"m\"]:3: attempt to index nil with 'x'";
        assert_eq!(explain(raw, None), raw);
        assert_eq!(explain(raw, Some("   ")), raw);
    }

    /// Anything unrecognised still gains its source line, which is the half of
    /// the improvement that needs no parsing at all.
    #[test]
    fn an_unrecognised_error_still_gets_its_line() {
        let out = explain(
            "[string \"m\"]:7: attempt to perform arithmetic (add) on nil and number",
            Some("  node.y = node.speed + 1"),
        );
        assert!(out.contains("node.y = node.speed + 1"), "{out}");
    }
}
