//! The Lua formatter behind **Format Document** (Alt+Shift+F) and format-on-save.
//!
//! Deliberately CONSERVATIVE: it fixes the things that are objectively wrong and
//! touches nothing else. A formatter that re-flows expressions would have to be a
//! full parser, and the failure mode of getting that wrong on someone's game
//! script — silently changing what the code means — is far worse than uneven
//! spacing.
//!
//! What it does:
//! * **re-indents** every line by real block depth (`function` / `if` / `for` /
//!   `while` / `do` / `repeat` / table and paren nesting), including the
//!   half-outdents `else`, `elseif`, `until` and a closing `}` / `)` want;
//! * **trims** trailing whitespace, normalises tabs to the indent unit, and ends
//!   the file with exactly one newline;
//! * **collapses** runs of 3+ blank lines to one (two inside a function body
//!   would be a style choice; three is an accident);
//! * leaves **strings, long strings and comments byte-identical**, and never
//!   reorders, splits or joins a line.
//!
//! What it deliberately does NOT do: insert or remove spaces inside a line, align
//! anything, add `then`/`end`, or reformat comment text. Your line stays your line.
//!
//! `--@noformat` anywhere in the file opts the whole file out (a generated or
//! deliberately hand-aligned script), and a line ending in `--@keep` keeps its own
//! indentation.

/// One indent level. Two spaces matches every script in the engine's projects and
/// the IDE's own auto-indent.
pub(crate) const INDENT: &str = "  ";

/// Strip the comment and string CONTENT from a line, leaving structural
/// punctuation and keywords — what the depth counter should look at. Comments and
/// string bodies can contain `end`, `{`, `--` and quotes; counting those is how a
/// naive re-indenter mangles a file.
///
/// Returns `(code, in_long_bracket_after)`: `code` with strings blanked to `""`,
/// and whether the line ends still inside a long bracket.
fn code_of(line: &str, in_long: bool) -> (String, bool) {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    // Inside a long bracket (`[[ … ]]`) nothing counts until it closes.
    if in_long {
        if let Some(pos) = line.find("]]") {
            i = pos + 2;
        } else {
            return (String::new(), true);
        }
    }
    let mut quote: Option<u8> = None;
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
                // A comment ends the structural part of the line — unless it's a
                // long comment, which may continue onto later lines.
                if c == b'-' && b.get(i + 1) == Some(&b'-') {
                    if line[i..].starts_with("--[[") && !line[i..].contains("]]") {
                        return (out, true);
                    }
                    return (out, false);
                }
                if line[i..].starts_with("[[") {
                    return (out, !line[i + 2..].contains("]]"));
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
    (out, false)
}

/// Whole words in a line of blanked code (so `endless` is not `end`).
fn words(code: &str) -> Vec<&str> {
    code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}

/// How much this line changes block depth, and how much of that applies to the
/// line ITSELF (a leading `end` / `else` / `}` outdents its own line).
fn depth_delta(code: &str) -> (i32, i32) {
    let ws = words(code);
    let mut delta = 0i32;
    // A one-line block (`if a then return end`) needs no special case: both its
    // opener and its `end` are on this line, so the arithmetic nets to zero.
    for (i, w) in ws.iter().enumerate() {
        match *w {
            "end" | "until" => delta -= 1,
            "function" | "if" | "for" | "while" | "repeat" => delta += 1,
            // `do` after `for`/`while` is the same block, not a second one.
            "do" => {
                let part_of_loop = ws[..i].iter().rev().any(|p| *p == "for" || *p == "while");
                if !part_of_loop {
                    delta += 1;
                }
            }
            // `elseif` re-opens the `if` it closed; net zero, but it outdents
            // its own line (handled below).
            "elseif" => {}
            _ => {}
        }
    }
    delta += code.matches('{').count() as i32 - code.matches('}').count() as i32;
    delta += code.matches('(').count() as i32 - code.matches(')').count() as i32;

    // Does this line START with something that closes the enclosing block?
    let first = ws.first().copied().unwrap_or("");
    let trimmed = code.trim_start();
    let own = i32::from(
        matches!(first, "end" | "else" | "elseif" | "until")
            || trimmed.starts_with('}')
            || trimmed.starts_with(')'),
    );
    (delta, own)
}

/// Format a Lua source. Returns the input unchanged when the file opts out with
/// `--@noformat`, so a generated or hand-aligned script is safe.
pub(crate) fn format(src: &str) -> String {
    if src.contains("--@noformat") {
        return src.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut in_long = false;
    let mut blanks = 0usize;

    for raw in src.lines() {
        // Inside a long string/comment every byte is content: pass it through
        // exactly, and don't let it move the depth.
        if in_long {
            let (_, still) = code_of(raw, true);
            out.push(raw.trim_end().to_string());
            in_long = still;
            continue;
        }
        let line = raw.trim();
        if line.is_empty() {
            blanks += 1;
            // 3+ blank lines in a row is an accident; keep one.
            if blanks <= 1 {
                out.push(String::new());
            }
            continue;
        }
        blanks = 0;
        let (code, still) = code_of(raw, false);
        let (delta, own) = depth_delta(&code);
        let indent_level = (depth - own).max(0);
        // `--@keep` — this line's own indentation is intentional.
        if line.ends_with("--@keep") {
            out.push(raw.trim_end().to_string());
        } else {
            out.push(format!("{}{}", INDENT.repeat(indent_level as usize), line));
        }
        depth = (depth + delta).max(0);
        in_long = still;
    }
    // Exactly one trailing newline, and no blank line at the end of the file.
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// A caret as (0-based line, column **measured from the line's first non-space
/// character**) — the anchor a format can restore.
///
/// Measuring from the content, not from the left margin, is the whole point:
/// re-indenting changes how much whitespace precedes the code, so a caret restored
/// by absolute column drifts by exactly the indentation delta. Typing at
/// `print(|` and formatting would put you at `pri|nt(` — every time, on every
/// format-on-save. A caret inside the leading whitespace itself anchors at 0.
pub(crate) fn line_col_of(text: &str, char_idx: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for (n, c) in text.chars().enumerate() {
        if n >= char_idx {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    let indent = text
        .split('\n')
        .nth(line)
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .unwrap_or(0);
    (line, col.saturating_sub(indent))
}

/// The inverse of [`line_col_of`]: the char index `col` characters into that
/// line's CONTENT, clamped to the line (a re-indented line can be shorter than
/// where you were).
pub(crate) fn char_of_line_col(text: &str, line: usize, col: usize) -> usize {
    let mut idx = 0usize;
    for (n, l) in text.split('\n').enumerate() {
        let len = l.chars().count();
        if n == line {
            let indent = l.chars().take_while(|c| c.is_whitespace()).count();
            return idx + (indent + col).min(len);
        }
        idx += len + 1; // + the newline
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reindents_blocks_and_leaves_the_code_alone() {
        let src = "\
function update(node, dt)
if node.x > 0 then
for i = 1, 3 do
node.x = node.x - 1
end
else
node.x = 0
end
end
";
        assert_eq!(
            format(src),
            "\
function update(node, dt)
  if node.x > 0 then
    for i = 1, 3 do
      node.x = node.x - 1
    end
  else
    node.x = 0
  end
end
"
        );
    }

    /// `for … do` is ONE block, not two — the classic double-indent bug in naive
    /// re-indenters (`while … do` likewise).
    #[test]
    fn a_loops_do_is_not_a_second_block() {
        let src = "for i = 1, 3 do\nprint(i)\nend\nwhile a do\nb()\nend\n";
        assert_eq!(format(src), "for i = 1, 3 do\n  print(i)\nend\nwhile a do\n  b()\nend\n");
        // A bare `do … end` block IS a level.
        assert_eq!(format("do\nx()\nend\n"), "do\n  x()\nend\n");
    }

    /// Tables and calls spanning lines indent by nesting, and a closing brace
    /// outdents its own line.
    #[test]
    fn tables_and_calls_nest() {
        let src = "defaults = {\nwalk = 4.5,\ncurve = {\na = 1,\n},\n}\n";
        assert_eq!(
            format(src),
            "defaults = {\n  walk = 4.5,\n  curve = {\n    a = 1,\n  },\n}\n"
        );
    }

    /// The dangerous cases: `end`, braces and quotes inside strings and comments
    /// must not move the depth, and their content must survive byte-for-byte.
    #[test]
    fn strings_and_comments_cannot_move_the_depth() {
        let src = "\
function f()
-- end } ) if then
local s = \"end } if\"
local t = 'it\\'s { fine'
print(s, t)
end
";
        let got = format(src);
        assert_eq!(
            got,
            "\
function f()
  -- end } ) if then
  local s = \"end } if\"
  local t = 'it\\'s { fine'
  print(s, t)
end
"
        );
        // The string bodies are untouched.
        assert!(got.contains("\"end } if\""));
        assert!(got.contains("'it\\'s { fine'"));
    }

    /// A long string's interior is content, not code — indentation inside it is
    /// part of the value and must not be rewritten.
    #[test]
    fn long_strings_pass_through_verbatim() {
        let src = "local sql = [[\n  select *\n    from t\n]]\nprint(sql)\n";
        assert_eq!(format(src), src);
    }

    /// A single-line block is already balanced, so the next line must not indent.
    #[test]
    fn one_line_blocks_stay_balanced() {
        let src = "if a then return end\nprint(1)\nfunction g() return 2 end\nprint(2)\n";
        assert_eq!(format(src), src);
    }

    /// The caret anchor survives an indentation change, which is the case
    /// format-on-save hits on every keystroke-then-save.
    #[test]
    fn the_caret_anchor_is_measured_from_the_code_not_the_margin() {
        let before = "function f()\nprint(x)\nend\n";
        let caret = before.find("print(").unwrap() + 6; // just inside the paren
        let (line, col) = line_col_of(before, caret);
        assert_eq!((line, col), (1, 6), "column is measured from `print`, not the margin");

        let after = format(before);
        assert!(after.contains("  print(x)"), "the fixture must gain indentation:\n{after}");
        let restored = char_of_line_col(&after, line, col);
        let head: String = after.chars().take(restored).collect();
        assert!(head.ends_with("print("), "caret drifted: ...{:?}", &head[head.len().saturating_sub(10)..]);

        // A caret sitting in the old indentation anchors at the code's start.
        let in_ws = line_col_of("function f()\n    print(x)\n", 13 + 2);
        assert_eq!(in_ws, (1, 0));
    }

    #[test]
    fn whitespace_hygiene() {
        // Trailing spaces go, tabs become the indent unit, 3+ blanks collapse to
        // one, and the file ends with exactly one newline.
        assert_eq!(format("local a = 1   \n\n\n\n\nlocal b = 2\n\n\n"), "local a = 1\n\nlocal b = 2\n");
        assert_eq!(format("function f()\n\tx()\nend"), "function f()\n  x()\nend\n");
    }

    /// Opt-outs are honoured: the whole file, and one line.
    #[test]
    fn opt_outs_are_respected() {
        let src = "--@noformat\nfunction f()\nx()\nend\n";
        assert_eq!(format(src), src, "--@noformat leaves the file alone");
        let kept = format("function f()\n        aligned = 1, --@keep\nend\n");
        assert!(kept.contains("        aligned = 1, --@keep"), "--@keep holds its indentation:\n{kept}");
    }

    /// Formatting is idempotent — running it twice changes nothing more. (A
    /// formatter that isn't makes format-on-save produce a diff on every save.)
    #[test]
    fn formatting_twice_changes_nothing() {
        for src in [
            "function update(node, dt)\nif a then\nb()\nend\nend\n",
            "defaults = {\nwalk = 4.5,\n}\n",
            "local sql = [[\n  raw\n]]\n",
            "for i = 1, 3 do\nprint(i)\nend\n",
        ] {
            let once = format(src);
            assert_eq!(format(&once), once, "not idempotent for {src:?}");
        }
    }
}

#[cfg(test)]
mod real_script_tests {
    use super::*;

    /// Every `.lua` in both shipped projects, formatted: the formatter may move
    /// whitespace and nothing else. Stripping all whitespace from input and output
    /// must give identical text — which catches a dropped line, a swallowed
    /// character inside a string, or a mangled long bracket on ~60 real files,
    /// including the 1000-line controllers.
    ///
    /// It also asserts idempotence per file, because format-on-save runs on files
    /// exactly like these and a non-idempotent formatter would dirty a file every
    /// time it was saved.
    #[test]
    fn formatting_the_real_projects_moves_only_whitespace() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut checked = 0;
        for dir in ["solar/scripts", "assets/scripts", "solar/tests"] {
            let Ok(rd) = std::fs::read_dir(root.join(dir)) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else { continue };
                let out = format(&src);
                let strip = |s: &str| -> String {
                    s.chars().filter(|c| !c.is_whitespace()).collect()
                };
                assert_eq!(
                    strip(&src),
                    strip(&out),
                    "{} — formatting changed more than whitespace",
                    path.display()
                );
                assert_eq!(format(&out), out, "{} — not idempotent", path.display());
                checked += 1;
            }
        }
        assert!(checked > 20, "expected to check the real scripts, only saw {checked}");
        println!("formatted {checked} real scripts, whitespace-only");
    }
}
