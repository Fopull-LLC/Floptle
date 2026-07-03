//! The Lua source preprocessor: rewrites the scripting sugar (`+=`, implicit
//! `self`, …) into plain Lua 5.1 before compilation, copying strings/comments
//! (including long brackets) through verbatim.

/// If `b[j]` opens a Lua long bracket (`[`, then `=`×level, then `[`), return its
/// level. Used to copy long strings/comments verbatim.
pub(crate) fn long_bracket_level(b: &[u8], j: usize) -> Option<usize> {
    if b.get(j) != Some(&b'[') {
        return None;
    }
    let mut k = j + 1;
    let mut level = 0;
    while b.get(k) == Some(&b'=') {
        level += 1;
        k += 1;
    }
    if b.get(k) == Some(&b'[') { Some(level) } else { None }
}

/// Copy a long bracket (string or comment) of the given `level` starting at `j`
/// (where `b[j] == '['`) into `out`, through its matching close. Returns the index
/// just past the close (or end of input if unterminated).
pub(crate) fn copy_long_bracket(b: &[u8], mut j: usize, level: usize, out: &mut Vec<u8>) -> usize {
    let span = 2 + level; // '[' + '='*level + '['  (and likewise for the closer)
    for _ in 0..span {
        if j < b.len() {
            out.push(b[j]);
            j += 1;
        }
    }
    while j < b.len() {
        if b[j] == b']' {
            let mut k = j + 1;
            let mut cnt = 0;
            while b.get(k) == Some(&b'=') {
                cnt += 1;
                k += 1;
            }
            if cnt == level && b.get(k) == Some(&b']') {
                for _ in 0..span {
                    out.push(b[j]);
                    j += 1;
                }
                return j;
            }
        }
        out.push(b[j]);
        j += 1;
    }
    j
}

/// Walk backward over already-emitted `out` from `end` to the start of the lvalue
/// the compound operator applies to: a name, dotted field chain (`a.b.c`), or
/// index chain (`a[i]`, `t[k].x`, nested brackets balanced). Stops at the first
/// byte that can't be part of an lvalue (whitespace, `=`, `(`, a keyword boundary…).
pub(crate) fn lvalue_start(out: &[u8], end: usize) -> usize {
    let mut j = end;
    while j > 0 && matches!(out[j - 1], b' ' | b'\t') {
        j -= 1;
    }
    loop {
        if j == 0 {
            break;
        }
        let c = out[j - 1];
        if c == b']' {
            // Balance back to the matching '['.
            let mut depth = 0;
            while j > 0 {
                let d = out[j - 1];
                j -= 1;
                if d == b']' {
                    depth += 1;
                } else if d == b'[' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            continue;
        }
        if c == b')' {
            // Balance back to the matching '(' — a call/parenthesized receiver, so
            // `f().x` / `(a).b` are captured whole rather than just the trailing field.
            let mut depth = 0;
            while j > 0 {
                let d = out[j - 1];
                j -= 1;
                if d == b')' {
                    depth += 1;
                } else if d == b'(' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b':' {
            j -= 1;
            continue;
        }
        break;
    }
    j
}

/// Rewrite Lua compound-assignment operators (`+= -= *= /= %= ^= ..=`) — which
/// Lua 5.1 / LuaJIT do NOT support — into plain assignments before compiling, e.g.
/// `x += y` → `x = x + (y)`, `t.k *= a + b` → `t.k = t.k * (a + b)`. A single-pass
/// scanner skips strings and comments (so `"a += b"` and `-- a += b` are untouched)
/// and adds NO newlines, so error line numbers stay correct. The `(R)` parentheses
/// preserve precedence.
pub(crate) fn preprocess(src: &str) -> String {
    let b = src.as_bytes();
    let n = b.len();
    let mut out: Vec<u8> = Vec::with_capacity(n + 16);
    let mut i = 0;
    let mut pending_close = false; // inside a rewritten RHS — emit ')' at statement end
    while i < n {
        let c = b[i];

        // Line / long comment.
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            // A comment can't be part of a rewritten RHS — close it first, so
            // `x += 1 -- note` becomes `x = x + (1) -- note`, not `(1 -- note)`.
            if pending_close {
                while out.last().is_some_and(|&p| p == b' ' || p == b'\t') {
                    out.pop();
                }
                out.push(b')');
                out.push(b' ');
                pending_close = false;
            }
            out.push(b'-');
            out.push(b'-');
            i += 2;
            if let Some(level) = long_bracket_level(b, i) {
                i = copy_long_bracket(b, i, level, &mut out);
            } else {
                while i < n && b[i] != b'\n' {
                    out.push(b[i]);
                    i += 1;
                }
            }
            continue;
        }

        // Short string.
        if c == b'"' || c == b'\'' {
            out.push(c);
            i += 1;
            while i < n {
                let d = b[i];
                out.push(d);
                i += 1;
                if d == b'\\' && i < n {
                    out.push(b[i]);
                    i += 1;
                    continue;
                }
                if d == c || d == b'\n' {
                    break;
                }
            }
            continue;
        }

        // Long string.
        if c == b'['
            && let Some(level) = long_bracket_level(b, i) {
                i = copy_long_bracket(b, i, level, &mut out);
                continue;
            }

        // Statement terminator — close any pending rewritten RHS.
        if c == b'\n' || c == b';' {
            if pending_close {
                out.push(b')');
                pending_close = false;
            }
            out.push(c);
            i += 1;
            continue;
        }

        // A block-ending or statement-introducing keyword also terminates a rewritten
        // RHS (the `end` in `if c then x += 1 end`, or the `return` in
        // `function f() x += 1 return x end`). These are reserved words that can't be
        // part of the expression, so close the paren before copying the keyword.
        // (`function` is excluded — it can begin an anonymous-function expression.)
        if pending_close && (c.is_ascii_alphabetic() || c == b'_') {
            let prev_ident = out.last().is_some_and(|&p| p.is_ascii_alphanumeric() || p == b'_');
            if !prev_ident {
                let mut k = i;
                while k < n && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                    k += 1;
                }
                let word = std::str::from_utf8(&b[i..k]).unwrap_or("");
                if matches!(
                    word,
                    "end" | "else"
                        | "elseif"
                        | "then"
                        | "do"
                        | "until"
                        | "return"
                        | "local"
                        | "break"
                        | "goto"
                        | "if"
                        | "while"
                        | "for"
                        | "repeat"
                ) {
                    while out.last().is_some_and(|&p| p == b' ' || p == b'\t') {
                        out.pop();
                    }
                    out.push(b')');
                    out.push(b' ');
                    pending_close = false;
                }
            }
        }

        // Compound single-char ops: + - * / % ^  followed by '=' (but not "==").
        if !pending_close
            && matches!(c, b'+' | b'-' | b'*' | b'/' | b'%' | b'^')
            && b.get(i + 1) == Some(&b'=')
            && b.get(i + 2) != Some(&b'=')
        {
            let start = lvalue_start(&out, out.len());
            let lhs = std::str::from_utf8(&out[start..]).unwrap_or("").trim().to_string();
            if !lhs.is_empty() {
                out.extend_from_slice(b"= ");
                out.extend_from_slice(lhs.as_bytes());
                out.push(b' ');
                out.push(c);
                out.extend_from_slice(b" (");
                pending_close = true;
                i += 2;
                while i < n && matches!(b[i], b' ' | b'\t') {
                    i += 1;
                }
                continue;
            }
        }

        // Compound concat: ..=  (but not the start of a longer run).
        if !pending_close
            && c == b'.'
            && b.get(i + 1) == Some(&b'.')
            && b.get(i + 2) == Some(&b'=')
            && b.get(i + 3) != Some(&b'=')
        {
            let start = lvalue_start(&out, out.len());
            let lhs = std::str::from_utf8(&out[start..]).unwrap_or("").trim().to_string();
            if !lhs.is_empty() {
                out.extend_from_slice(b"= ");
                out.extend_from_slice(lhs.as_bytes());
                out.extend_from_slice(b" .. (");
                pending_close = true;
                i += 3;
                while i < n && matches!(b[i], b' ' | b'\t') {
                    i += 1;
                }
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
    if pending_close {
        out.push(b')');
    }
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}
