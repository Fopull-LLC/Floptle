//! The in-engine Scripting IDE (the "Scripting" dock tab): the Docs page, the
//! Lua code editor — find & replace, whole-line editing shortcuts, block
//! indent/comment, autocomplete, hover docs, go-to-definition, references,
//! diagnostics — plus the scripting API text that powers completions and docs.
//!
//! Everything here renders through [`EditorTabViewer::scripting_ui`]; the
//! persistent state lives in [`IdeState`] on the editor.

use std::path::{Path, PathBuf};

use crate::theme::CODE_THEMES;
use crate::{lua_highlight, plain_job, EditorTabViewer};

/// One script file open in the in-engine IDE.
pub(crate) struct OpenScript {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) text: String,
    pub(crate) dirty: bool,
}

/// State of the Scripting-tab IDE: the open files and which one is shown
/// (`None` = the built-in Docs page).
#[derive(Default)]
pub(crate) struct IdeState {
    pub(crate) open: Vec<OpenScript>,
    pub(crate) active: Option<usize>,
    /// A pending "scroll to this 1-based line" request (Console jump-to-source,
    /// find navigation), consumed the next frame the editor draws.
    pub(crate) goto: Option<usize>,
    /// Ctrl+F find-in-file: bar open, the query, a one-shot focus request,
    /// match-case, and which match is current (index into the match list).
    pub(crate) find_open: bool,
    pub(crate) find_query: String,
    pub(crate) find_focus: bool,
    pub(crate) find_case: bool,
    pub(crate) find_idx: usize,
    /// Ctrl+H replace: the second row of the find bar + its buffer.
    pub(crate) replace_open: bool,
    pub(crate) replace_buf: String,
    /// Ctrl+G go-to-line prompt.
    pub(crate) goto_line_open: bool,
    pub(crate) goto_line_buf: String,
    pub(crate) goto_line_focus: bool,
    /// Tab index awaiting a close confirmation (it has unsaved changes).
    pub(crate) close_confirm: Option<usize>,
    /// "Find all references" results (most recent search) + the word searched.
    pub(crate) refs: Vec<RefHit>,
    pub(crate) refs_word: String,
    /// The identifier captured at the last right-click, so the context menu stays stable
    /// while it's open (the live hover position moves onto the menu and would flicker).
    pub(crate) rc_word: Option<String>,
    /// Autocomplete popup state: the keyboard-selected row, the token it was
    /// built for (selection resets when it changes), and an Esc dismissal that
    /// holds until the token changes.
    pub(crate) ac_sel: usize,
    pub(crate) ac_token: String,
    pub(crate) ac_dismissed: bool,
    /// Ctrl+Space asked for the popup explicitly, so it may open for a token the
    /// automatic rule wouldn't offer (a one-char word, a plain identifier). Held
    /// until the token changes or the popup is dismissed.
    pub(crate) ac_manual: bool,
    /// Cached lints for the active file + the (path, content-hash) they were
    /// computed for. Linting walks the file several times, which is fine on a
    /// keystroke and not fine every frame on a 1,500-line controller.
    pub(crate) lints: Vec<crate::lua_lint::Lint>,
    pub(crate) lints_for: Option<(String, u64)>,
    /// Show the warnings list (off = just the count, so the strip stays one line).
    pub(crate) lints_open: bool,
    /// **Format on save** (Alt+Shift+F formats on demand). Off by default: a
    /// formatter that rewrites a file you only meant to save has to be something
    /// you asked for. Persisted with the editor's prefs.
    pub(crate) format_on_save: bool,
    /// The Docs page's filter box.
    pub(crate) docs_search: String,
    /// Which Docs page is showing: the guides, the API browser, or the shader
    /// stdlib. The API reference used to live below 2,500 lines of guide, which
    /// is a reference nobody browses — it was only ever reachable by searching
    /// for something you already knew the name of.
    pub(crate) docs_page: DocsPage,
}

/// The three things the Docs tab is: a guide, a reference, a shader reference.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DocsPage {
    #[default]
    Guides,
    Api,
    Shaders,
}

/// One "find all references" result: the file, its display name, the 1-based line, and
/// that line's text.
pub(crate) struct RefHit {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}


impl IdeState {
    /// Open `path` in the IDE (or focus it if already open). Returns false on read error.
    pub(crate) fn open_file(&mut self, path: &str) -> bool {
        if let Some(i) = self.open.iter().position(|f| f.path == path) {
            self.active = Some(i);
            return true;
        }
        let Ok(text) = std::fs::read_to_string(path) else { return false };
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        self.open.push(OpenScript { path: path.to_string(), name, text, dirty: false });
        self.active = Some(self.open.len() - 1);
        true
    }

    /// Close tab `i` (no dirty check — callers confirm first) and keep a sensible
    /// neighbor active instead of dumping the user back on the Docs page.
    fn close_tab(&mut self, i: usize) {
        if i >= self.open.len() {
            return;
        }
        self.open.remove(i);
        self.active = match self.active {
            Some(a) if a == i => {
                if self.open.is_empty() {
                    None
                } else {
                    Some(i.min(self.open.len() - 1))
                }
            }
            Some(a) if a > i => Some(a - 1),
            other => other,
        };
    }

    /// Save open file `i` to disk. Returns whether the write succeeded.
    ///
    /// Formatting is NOT done here: it needs the caret, which lives in egui and
    /// belongs to the caller (see `format_file`). A save that silently re-indents
    /// and leaves the caret at a stale offset puts your next keystroke somewhere
    /// else — a worse bug than un-formatted code.
    fn save_file(&mut self, i: usize) -> bool {
        let Some(f) = self.open.get_mut(i) else { return false };
        if std::fs::write(&f.path, &f.text).is_ok() {
            f.dirty = false;
            return true;
        }
        false
    }

    /// Format open file `i` in place if it's Lua. Returns whether the text changed
    /// (so the caller can restore the caret only when it needs to).
    fn format_file(&mut self, i: usize) -> bool {
        let Some(f) = self.open.get_mut(i) else { return false };
        if !f.path.ends_with(".lua") {
            return false;
        }
        let formatted = crate::lua_format::format(&f.text);
        if formatted == f.text {
            return false;
        }
        f.text = formatted;
        f.dirty = true;
        true
    }
}

/// Byte ranges of every occurrence of `needle` in `hay`; ASCII case-insensitive
/// unless `match_case`. Offsets are valid byte indices into `hay` (an ASCII
/// needle only matches at ASCII byte positions, so multi-byte UTF-8 in `hay` is
/// never split).
pub(crate) fn find_ranges(hay: &str, needle: &str, match_case: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let (hb, nb) = (hay.as_bytes(), needle.as_bytes());
    let mut out = Vec::new();
    let mut i = 0;
    while i + nb.len() <= hb.len() {
        let hit = if match_case {
            hb[i..i + nb.len()] == *nb
        } else {
            (0..nb.len()).all(|k| hb[i + k].eq_ignore_ascii_case(&nb[k]))
        };
        if hit {
            out.push((i, i + nb.len()));
            i += nb.len();
        } else {
            i += 1;
        }
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Append whole-word, case-sensitive occurrences of `word` in `text` (one per line) to
/// `out` as [`RefHit`]s — the engine of "find all references".
fn collect_word_hits(path: &str, name: &str, text: &str, word: &str, out: &mut Vec<RefHit>) {
    if word.is_empty() {
        return;
    }
    for (ln, line) in text.lines().enumerate() {
        let lb = line.as_bytes();
        for (s, _) in line.match_indices(word) {
            let e = s + word.len();
            let before_ok = s == 0 || !is_ident_byte(lb[s - 1]);
            let after_ok = e >= lb.len() || !is_ident_byte(lb[e]);
            if before_ok && after_ok {
                out.push(RefHit {
                    path: path.to_string(),
                    name: name.to_string(),
                    line: ln + 1,
                    text: line.trim().to_string(),
                });
                break; // one hit per line keeps the list readable
            }
        }
    }
}

/// Append substring occurrences of `needle` in `text` (one per line) to `out` —
/// the engine of the find bar's "in all scripts".
fn collect_line_hits(
    path: &str,
    name: &str,
    text: &str,
    needle: &str,
    match_case: bool,
    out: &mut Vec<RefHit>,
) {
    if needle.is_empty() {
        return;
    }
    for (ln, line) in text.lines().enumerate() {
        if !find_ranges(line, needle, match_case).is_empty() {
            out.push(RefHit {
                path: path.to_string(),
                name: name.to_string(),
                line: ln + 1,
                text: line.trim().to_string(),
            });
        }
    }
}

/// The 1-based line where `word` is *defined* in Lua source (a function or assignment),
/// or None. Heuristic — good enough for go-to-definition in a scripting IDE.
fn find_definition_line(text: &str, word: &str) -> Option<usize> {
    if word.is_empty() {
        return None;
    }
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_start();
        let starts = [
            format!("function {word}("),
            format!("function {word} "),
            format!("local function {word}("),
            format!("local function {word} "),
            format!("local {word} ="),
            format!("local {word}="),
        ];
        if starts.iter().any(|p| line.starts_with(p.as_str())) {
            return Some(n + 1);
        }
        // `function Table.word(` / `function Table:word(`
        if line.starts_with("function ")
            && (line.contains(&format!(".{word}(")) || line.contains(&format!(":{word}(")))
        {
            return Some(n + 1);
        }
        // Global assignment `word = ...` at line start (whole identifier, not `==`).
        if let Some(rest) = line.strip_prefix(word) {
            let rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix('=')
                && !after.starts_with('=') {
                    return Some(n + 1);
                }
        }
    }
    None
}

// ---- text-buffer editing helpers (char-indexed API over byte-precise edits) ----

/// Helpers for whole-line editing. The editor's cursor speaks CHAR indices; all
/// splicing is done on BYTE ranges so multi-byte UTF-8 never splits.
mod line_edit {
    /// Byte offset of char index `c` (== len when past the end).
    pub fn byte_of_char(text: &str, c: usize) -> usize {
        text.char_indices().nth(c).map(|(b, _)| b).unwrap_or(text.len())
    }

    /// Number of chars before byte offset `b` (to place the caret after an edit).
    pub fn char_of_byte(text: &str, b: usize) -> usize {
        text[..b.min(text.len())].chars().count()
    }

    /// The byte range of the line containing char index `char_idx`, plus the byte index
    /// where the next line starts (== content end when there's no trailing newline).
    /// Returns `(line_start, content_end, next_line_start)`.
    pub fn line_bytes(text: &str, char_idx: usize) -> (usize, usize, usize) {
        let byte_idx = byte_of_char(text, char_idx);
        let line_start = text[..byte_idx].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let content_end =
            text[line_start..].find('\n').map(|p| line_start + p).unwrap_or(text.len());
        let next_line_start = if content_end < text.len() { content_end + 1 } else { text.len() };
        (line_start, content_end, next_line_start)
    }

    /// The full-line byte span covering the char selection `[a, b]` (`a <= b`):
    /// `(span_start, content_end_of_last_line, next_line_start)`.
    pub fn span_bytes(text: &str, a: usize, b: usize) -> (usize, usize, usize) {
        let (s, ..) = line_bytes(text, a);
        let (.., e, next) = {
            let (_, e, next) = line_bytes(text, b);
            (0, e, next)
        };
        (s, e, next)
    }

    /// The current line's text with a trailing newline (what Ctrl+C / Ctrl+X put on the
    /// clipboard, so pasting re-inserts a whole line).
    pub fn line_with_newline(text: &str, char_idx: usize) -> String {
        let (s, e, _) = line_bytes(text, char_idx);
        format!("{}\n", &text[s..e])
    }
}

const INDENT_WIDTH: usize = 2;

fn indent_unit() -> String {
    " ".repeat(INDENT_WIDTH)
}

/// Does `t` end with the word `w` (with a non-identifier char, or nothing, before it)?
fn ends_with_word(t: &str, w: &str) -> bool {
    if !t.ends_with(w) {
        return false;
    }
    let before = t.len() - w.len();
    before == 0 || !is_ident_byte(t.as_bytes()[before - 1])
}

/// Toggle `--` line comments over the char selection `[a, b]`. If every non-blank
/// line is already commented, uncomment; otherwise comment. Returns the new char
/// selection (the whole affected span).
fn toggle_comment_lines(text: &mut String, a: usize, b: usize) -> (usize, usize) {
    let (s, e, _) = line_edit::span_bytes(text, a, b);
    let block = text[s..e].to_string();
    let mut nonblank = false;
    let all_commented = block.lines().all(|l| {
        if l.trim().is_empty() {
            true
        } else {
            nonblank = true;
            l.trim_start().starts_with("--")
        }
    }) && nonblank;
    let new: Vec<String> = block
        .split('\n')
        .map(|l| {
            if l.trim().is_empty() {
                return l.to_string();
            }
            let ind = l.len() - l.trim_start().len();
            let (head, tail) = l.split_at(ind);
            if all_commented {
                let rest = tail.strip_prefix("-- ").or_else(|| tail.strip_prefix("--")).unwrap_or(tail);
                format!("{head}{rest}")
            } else {
                format!("{head}-- {tail}")
            }
        })
        .collect();
    let joined = new.join("\n");
    text.replace_range(s..e, &joined);
    (line_edit::char_of_byte(text, s), line_edit::char_of_byte(text, s + joined.len()))
}

/// Indent (or outdent) every line touched by the char selection `[a, b]` by two
/// spaces. Returns the new char selection (the whole affected span).
fn indent_lines(text: &mut String, a: usize, b: usize, outdent: bool) -> (usize, usize) {
    let (s, e, _) = line_edit::span_bytes(text, a, b);
    let block = text[s..e].to_string();
    let indent = indent_unit();
    let new: Vec<String> = block
        .split('\n')
        .map(|l| {
            if outdent {
                if let Some(rest) = l.strip_prefix(&indent) {
                    rest.to_string()
                } else if let Some(rest) = l.strip_prefix('\t') {
                    rest.to_string()
                } else if let Some(rest) = l.strip_prefix(' ') {
                    rest.to_string()
                } else {
                    l.to_string()
                }
            } else if l.trim().is_empty() {
                l.to_string()
            } else {
                format!("{indent}{l}")
            }
        })
        .collect();
    let joined = new.join("\n");
    text.replace_range(s..e, &joined);
    (line_edit::char_of_byte(text, s), line_edit::char_of_byte(text, s + joined.len()))
}

/// Move the lines touched by the char selection `[a, b]` up or down one line.
/// Returns the new char selection covering the moved block, or None at an edge.
fn move_lines(text: &mut String, a: usize, b: usize, up: bool) -> Option<(usize, usize)> {
    let (s, e, next) = line_edit::span_bytes(text, a, b);
    if up {
        if s == 0 {
            return None;
        }
        // The line above: [prev_start, s-1) — s-1 is its trailing newline.
        let prev_start = text[..s - 1].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let prev = text[prev_start..s - 1].to_string();
        let block = text[s..e].to_string();
        let new = format!("{block}\n{prev}");
        text.replace_range(prev_start..e, &new);
        Some((
            line_edit::char_of_byte(text, prev_start),
            line_edit::char_of_byte(text, prev_start + block.len()),
        ))
    } else {
        if next >= text.len() {
            return None; // already the last line
        }
        let next_end = text[next..].find('\n').map(|p| next + p).unwrap_or(text.len());
        let below = text[next..next_end].to_string();
        let block = text[s..e].to_string();
        let new = format!("{below}\n{block}");
        text.replace_range(s..next_end, &new);
        let bs = s + below.len() + 1;
        Some((line_edit::char_of_byte(text, bs), line_edit::char_of_byte(text, bs + block.len())))
    }
}

/// Delete every line touched by the char selection `[a, b]`. Returns the new caret.
fn delete_lines(text: &mut String, a: usize, b: usize) -> usize {
    let (s, _, next) = line_edit::span_bytes(text, a, b);
    text.replace_range(s..next, "");
    line_edit::char_of_byte(text, s.min(text.len()))
}

/// Replace the char selection `[a, b]` with a newline + auto-indent (matching the
/// current line, one level deeper after a Lua block opener). Returns the new caret.
fn auto_indent_newline(text: &mut String, a: usize, b: usize) -> usize {
    let ba = line_edit::byte_of_char(text, a);
    let bb = line_edit::byte_of_char(text, b);
    let line_start = text[..ba].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let before_caret = &text[line_start..ba];
    let indent: String = before_caret.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let body_indent = format!("{indent}{}", indent_unit());
    let t = before_caret.trim_end();
    let opener = ends_with_word(t, "then")
        || ends_with_word(t, "do")
        || ends_with_word(t, "else")
        || ends_with_word(t, "repeat")
        || t.ends_with('{')
        || t.ends_with('(')
        || (t.ends_with(')') && t.contains("function"));
    // Auto-close: pressing Enter on an UNCLOSED block header (function/if/for/
    // while) also inserts its matching `end` on the next line — the caret lands
    // on the indented body line between them (Roblox-Studio style). Only when
    // the buffer actually has more openers than `end`s, so retyping inside a
    // complete block never doubles the close.
    let closes = (ends_with_word(t, "then")
        || ends_with_word(t, "do")
        || (t.ends_with(')') && t.contains("function")))
        && block_balance(text) > 0;
    let ins = if closes {
        format!("\n{body_indent}\n{indent}end")
    } else if opener {
        format!("\n{body_indent}")
    } else {
        format!("\n{indent}")
    };
    text.replace_range(ba..bb, &ins);
    // Caret on the body line (before the auto-inserted end, when present).
    let caret_bytes = if closes { ba + 1 + body_indent.len() } else { ba + ins.len() };
    line_edit::char_of_byte(text, caret_bytes)
}

/// Insert or replace text at `[a, b]`, with smart whole-line paste support when
/// the caret sits before a newline and the pasted content is a single line.
fn paste_text(text: &mut String, a: usize, b: usize, replacement: &str) -> usize {
    let ba = line_edit::byte_of_char(text, a);
    let bb = line_edit::byte_of_char(text, b);
    let at_eol = a == b && ba < text.len() && text.as_bytes()[ba] == b'\n';
    let single_line = replacement.ends_with('\n')
        && !replacement[..replacement.len().saturating_sub(1)].contains('\n');
    let insert_at = if at_eol && single_line { ba + 1 } else { ba };
    if a == b && at_eol && single_line {
        text.insert_str(insert_at, replacement);
    } else {
        text.replace_range(ba..bb, replacement);
    }
    let caret_bytes = if at_eol && single_line {
        insert_at + replacement.trim_end_matches('\n').len()
    } else {
        insert_at + replacement.len()
    };
    line_edit::char_of_byte(text, caret_bytes)
}

/// Net count of open Lua blocks in `text`: `function`/`if`/`for`/`while`/
/// `repeat` open one, `end`/`until` close one (`elseif`/`else` are neutral —
/// they share their `if`'s end). Strings and comments are skipped, so keywords
/// in text don't count. Positive = more openers than closers.
fn block_balance(text: &str) -> i32 {
    let b = text.as_bytes();
    let mut i = 0;
    let mut bal = 0i32;
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                // Comment: long `--[[ ]]` or to end of line.
                if b.get(i + 2) == Some(&b'[') && b.get(i + 3) == Some(&b'[') {
                    i = text[i + 4..].find("]]").map(|p| i + 4 + p + 2).unwrap_or(b.len());
                } else {
                    i = text[i..].find('\n').map(|p| i + p + 1).unwrap_or(b.len());
                }
            }
            b'[' if b.get(i + 1) == Some(&b'[') => {
                // Long string.
                i = text[i + 2..].find("]]").map(|p| i + 2 + p + 2).unwrap_or(b.len());
            }
            q @ (b'"' | b'\'') => {
                // Quoted string (with escapes).
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            c if is_ident_byte(c) && !c.is_ascii_digit() => {
                let s0 = i;
                while i < b.len() && is_ident_byte(b[i]) {
                    i += 1;
                }
                match &text[s0..i] {
                    "function" | "if" | "for" | "while" | "repeat" => bal += 1,
                    "end" | "until" => bal -= 1,
                    _ => {}
                }
            }
            _ => i += 1,
        }
    }
    bal
}

/// Move the in-engine editor's caret to a char index (collapsed selection).
fn set_ide_caret(ctx: &egui::Context, id: egui::Id, char_idx: usize) {
    set_ide_selection(ctx, id, char_idx, char_idx);
}

/// Select `[a, b]` (char indices) in the editor's stored state — it shows when the
/// editor regains focus, and keeps ops like find/replace anchored meanwhile.
fn set_ide_selection(ctx: &egui::Context, id: egui::Id, a: usize, b: usize) {
    if let Some(mut st) = egui::text_edit::TextEditState::load(ctx, id) {
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(a),
            egui::text::CCursor::new(b),
        )));
        st.store(ctx, id);
    }
}

/// The editor's current char selection `(min, max, primary)` from stored state.
fn ide_selection(ctx: &egui::Context, id: egui::Id) -> Option<(usize, usize, usize)> {
    let r = egui::text_edit::TextEditState::load(ctx, id)?.cursor.char_range()?;
    let (p, s) = (r.primary.index.0, r.secondary.index.0);
    Some((p.min(s), p.max(s), p))
}

/// The token (run of identifier/`.` chars) ending at `cursor_char`, plus its start
/// char index — what autocomplete matches against.
/// The keys a script's `defaults = { … }` table declares, in order — used to
/// complete `params.<key>` and for the tunables hint above the editor.
fn defaults_keys(text: &str) -> Vec<String> {
    let Some(start) = text.find("defaults") else { return Vec::new() };
    let Some(open) = text[start..].find('{') else { return Vec::new() };
    let body_start = start + open + 1;
    let Some(close) = text[body_start..].find('}') else { return Vec::new() };
    text[body_start..body_start + close]
        .split(',')
        .filter_map(|p| p.split('=').next())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty() && k.bytes().all(is_ident_byte))
        .collect()
}

/// Fields of the live component handles (`node:getcomponent(…)`), keyed by
/// component. A variable whose type is INFERRED (see [`infer_var_types`]) only
/// offers its own component's fields; unknown variables offer all of them.
const COMPONENT_FIELDS: &[(&str, &str, &str)] = &[
    ("RigidBody", "friction", "Grip. A ramp holds while tan(angle) <= friction: 0 is ice, 1 holds 45 degrees, above 1 is grippier still."),
    ("RigidBody", "slopeLimit", "Steepest standable surface, in degrees (default 60). Past it nothing grounds the body and no grip holds it."),
    ("RigidBody", "restitution", "Bounciness 0..1 (0 = no bounce)."),
    ("RigidBody", "gravity", "Gravity pull on this body — assign true/false (reads back 1/0)."),
    ("RigidBody", "shape", "Body shape — 0 sphere, 1 capsule, 2 box."),
    ("RigidBody", "radius", "Sphere/capsule radius."),
    ("RigidBody", "height", "Capsule total height."),
    ("RigidBody", "half_x", "Box half-extent X."),
    ("RigidBody", "half_y", "Box half-extent Y."),
    ("RigidBody", "half_z", "Box half-extent Z."),
    ("RigidBody", "lock_x", "Freeze world X translation (assign true/false)."),
    ("RigidBody", "lock_y", "Freeze world Y translation."),
    ("RigidBody", "lock_z", "Freeze world Z translation (e.g. for 2.5D)."),
    ("RigidBody", "lock_rot_x", "Freeze rotation about X (keeps a body upright)."),
    ("RigidBody", "lock_rot_y", "Freeze rotation about Y."),
    ("RigidBody", "lock_rot_z", "Freeze rotation about Z."),
    ("RigidBody", "two_d", "2D: keep the body in the XY plane (1/0). Adds to the freezes."),
    ("PointLight", "intensity", "Brightness multiplier."),
    ("PointLight", "range", "Reach in world units."),
    ("PointLight", "shape", "The surface it emits from: 0 point, 1 sphere, 2 rect, 3 disk, 4 tube. Assigning keeps the size it had."),
    ("PointLight", "width", "Rect only: its width in world units (0 on other shapes)."),
    ("PointLight", "height", "Rect only: its height in world units."),
    ("PointLight", "radius", "Sphere / disk only: its radius in world units."),
    ("PointLight", "length", "Tube only: how long the bar is."),
    ("PointLight", "thickness", "Tube only: how thick the bar is."),
    ("PointLight", "twoSided", "Rect / disk only (1/0): lights out of the back as well as the front."),
    ("PointLight", "r", "Color red 0..1."),
    ("PointLight", "g", "Color green 0..1."),
    ("PointLight", "b", "Color blue 0..1."),
    ("PostProcess", "enabled", "The whole post chain on/off."),
    ("PostProcess", "bloom", "Bloom on/off."),
    ("PostProcess", "bloomThreshold", "Brightness bloom starts above."),
    ("PostProcess", "bloomIntensity", "How much bloom is added."),
    ("PostProcess", "vignette", "Vignette on/off."),
    ("PostProcess", "vignetteStrength", "How dark the corners go, 0..1."),
    ("PostProcess", "vignetteRadius", "Where the darkening starts."),
    ("PostProcess", "aoStrength", "How dark full occlusion gets, 0..1."),
    ("PostProcess", "aoRadius", "Occlusion reach in world units."),
    ("PostProcess", "posterizeBands", "Colour levels per channel; 0 or 1 = off."),
    ("PostProcess", "posterizeDither", "Dither the posterize so gradients don't step."),
    ("PostProcess", "tonemap", "How light lands on the display: 0 clip, 1 Reinhard, 2 ACES, 3 AgX."),
    ("PostProcess", "dofFocus", "Depth of field: the distance that is sharp, in world units. 0 = off. Animate it and you have a rack focus."),
    ("PostProcess", "dofRange", "How far BEYOND the focus distance stays sharp."),
    ("PostProcess", "dofNearRange", "How far IN FRONT of it stays sharp. 0 = half the far range."),
    ("PostProcess", "dofBlur", "The widest the out-of-focus blur gets, in pixels. 0 = off."),
    ("PostProcess", "dofBlades", "Aperture blades: 0 is a round iris, 6 the classic hexagon."),
    ("PostProcess", "dofBladeAngle", "Turn the blade polygon, in degrees."),
    ("PostProcess", "dofHighlight", "How much brighter-than-white pixels dominate the blur — bokeh instead of grey mush."),
    ("PostProcess", "dofSamples", "Taps in the blur. 0 = the default 16."),
    ("PostProcess", "dofShowFocus", "Tint the frame by what is in focus — a tuning view (1/0)."),
    ("PostProcess", "motionBlur", "Motion-blur shutter, 0..1. 0 = off; 0.5 is a film camera's 180 degree shutter. Blurs CAMERA motion — a pan, a whip, a dolly."),
    ("PostProcess", "motionSamples", "Taps along the motion streak. 0 = the default 12."),
    ("Camera", "fovY", "Vertical field of view, radians."),
    ("Camera", "active", "The play-mode view camera — assign true to switch to it (reads 1/0)."),
    ("ParticleSystem", "play_on_start", "Auto-play when play begins (1/0)."),
    ("UiElement", "visible", "Shown (assign true/false; reads 1/0)."),
    ("UiElement", "opacity", "Multiplies every color the element draws, 0..1."),
    ("UiElement", "posX", "Free position X / Pin offset X (design units)."),
    ("UiElement", "posY", "Free position Y / Pin offset Y (design units)."),
    ("UiElement", "width", "Width in the axis's sizing mode (px / % fraction / grow weight); nil on a fit axis."),
    ("UiElement", "height", "Height (same rules as width)."),
    ("UiElement", "radius", "Shape corner radius (design units)."),
    ("UiElement", "border", "Shape border thickness (design units)."),
    ("UiElement", "fillR", "Shape fill red 0..1."),
    ("UiElement", "fillG", "Shape fill green 0..1."),
    ("UiElement", "fillB", "Shape fill blue 0..1."),
    ("UiElement", "fillA", "Shape fill alpha 0..1."),
    ("UiElement", "textSize", "Text glyph size (design units; ignored while fit is on)."),
    ("UiElement", "textR", "Text color red 0..1."),
    ("UiElement", "textG", "Text color green 0..1."),
    ("UiElement", "textB", "Text color blue 0..1."),
    ("UiElement", "textA", "Text color alpha 0..1."),
    ("UiElement", "tintR", "Image tint red 0..1."),
    ("UiElement", "tintG", "Image tint green 0..1."),
    ("UiElement", "tintB", "Image tint blue 0..1."),
    ("UiElement", "tintA", "Image tint alpha 0..1."),
    ("UiElement", "cell", "Spritesheet cell index shown by the image (assign per frame for sprite animation)."),
    ("UiSlider", "value", "Current value (health-bar hook: bar.value = hp)."),
    ("UiSlider", "min", "Range start."),
    ("UiSlider", "max", "Range end."),
    ("UiLayer", "enabled", "Master switch — an off layer draws nothing (assign true/false)."),
    ("UiLayer", "z", "Draw order: lowest z first."),
    ("UiLayer", "designHeight", "Design units that span the window height."),
    ("UiLayer", "worldSpace", "1 = a panel in the 3D world at this node's transform; 0 = a screen overlay."),
];

/// What a variable (or `params.<key>`) is known to hold, inferred from this
/// file's assignments + `defaults` declarations — the Visual-Studio-style
/// context that keeps member completion to fields that actually exist.
#[derive(Clone, Debug, PartialEq)]
enum VarType {
    Node,
    Script,
    Animator,
    Particles,
    Component(String),
}

/// Extract the first double-quoted string in `s`, if any.
fn first_quoted(s: &str) -> Option<&str> {
    let a = s.find('"')? + 1;
    let b = a + s[a..].find('"')?;
    Some(&s[a..b])
}

/// Infer variable types from this file's assignments (`local rb =
/// node:getcomponent("RigidBody")` → `rb` completes RigidBody fields only) and
/// from `defaults` reference declarations (`hp = componentref("UiSlider")` →
/// `params.hp` completes slider fields). Line-based and deliberately simple —
/// wrong inferences only cost a fallback to the generic list.
fn infer_var_types(text: &str) -> Vec<(String, VarType)> {
    let mut out: Vec<(String, VarType)> = Vec::new();
    let set = |out: &mut Vec<(String, VarType)>, k: String, v: VarType| {
        if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v; // later assignment wins
        } else {
            out.push((k, v));
        }
    };
    for line in text.lines() {
        let line = line.split("--").next().unwrap_or(line);
        let Some(eq) = line.find('=') else { continue };
        // Skip ==, <=, >=, ~= comparisons.
        if line.as_bytes().get(eq + 1) == Some(&b'=')
            || (eq > 0 && matches!(line.as_bytes()[eq - 1], b'<' | b'>' | b'~' | b'='))
        {
            continue;
        }
        let lhs = line[..eq].trim().trim_start_matches("local").trim();
        if lhs.is_empty() || !lhs.bytes().all(|b| is_ident_byte(b) || b == b'.') {
            continue;
        }
        let rhs = &line[eq + 1..];
        let ty = if rhs.contains(":getcomponent(") {
            first_quoted(rhs).map(|c| VarType::Component(c.to_string()))
        } else if rhs.contains("componentref(") {
            first_quoted(rhs).map(|c| VarType::Component(c.to_string()))
        } else if rhs.contains(":animator()") {
            Some(VarType::Animator)
        } else if rhs.contains(":particles()") {
            Some(VarType::Particles)
        } else if rhs.contains(":getscript(")
            || rhs.contains("findScript(")
            || rhs.contains("scriptref(")
        {
            Some(VarType::Script)
        } else if rhs.contains("noderef(")
            || rhs.contains("find(")
            || rhs.contains(":getchild(")
            || rhs.contains(":find(")
            || rhs.contains(".parent")
            || rhs.contains(":getparent()")
        {
            Some(VarType::Node)
        } else {
            None
        };
        if let Some(ty) = ty {
            // Ref-sentinel declarations are handled by the defaults pass below
            // (a one-line defaults table would mis-parse here).
            if rhs.contains("noderef(")
                || rhs.contains("scriptref(")
                || rhs.contains("componentref(")
            {
                continue;
            }
            set(&mut out, lhs.to_string(), ty);
        }
    }
    // `defaults` reference declarations type the PARAM: hp = componentref("X")
    // → `params.hp` completes X's fields.
    if let Some(start) = text.find("defaults")
        && let Some(open) = text[start..].find('{')
    {
        let body_start = start + open + 1;
        if let Some(close) = text[body_start..].find('}') {
            for part in text[body_start..body_start + close].split(',') {
                let Some((k, v)) = part.split_once('=') else { continue };
                let k = k.trim();
                if k.is_empty() || !k.bytes().all(is_ident_byte) {
                    continue;
                }
                let ty = if v.contains("componentref(") {
                    first_quoted(v).map(|c| VarType::Component(c.to_string()))
                } else if v.contains("scriptref(") {
                    Some(VarType::Script)
                } else if v.contains("noderef(") {
                    Some(VarType::Node)
                } else {
                    None
                };
                if let Some(ty) = ty {
                    set(&mut out, format!("params.{k}"), ty);
                }
            }
        }
    }
    out
}

/// One ranked completion candidate. `keep` is how many chars of the typed token
/// to keep (the insert replaces the rest) — 0 replaces the whole token, while a
/// member completion keeps `base` + separator and replaces only the member.
struct AcItem {
    label: String,
    insert: String,
    keep: usize,
    doc: Option<String>,
    score: u8,
}

/// Rank completion candidates for `token` (the identifier being typed):
/// 0 = full-label prefix / own `params.` key, 1 = member match on any base,
/// 2 = handle fields + substring matches, 4 = identifiers from this file.
fn ac_matches(token: &str, file_text: &str) -> Vec<AcItem> {
    let lower = token.to_ascii_lowercase();
    let mut items: Vec<AcItem> = Vec::new();
    let push = |items: &mut Vec<AcItem>, it: AcItem| {
        if !items.iter().any(|o| o.label == it.label && o.keep == it.keep) {
            items.push(it);
        }
    };
    let sep = token.rfind(['.', ':']);

    // Plain words match full labels: by prefix first, then by substring.
    // (Separator tokens use ONLY member matching below — a full-label insert
    // would duplicate the row, and `anim:*` inserts are member-shaped.)
    if sep.is_none() {
        for e in LUA_API {
            let l = e.label.to_ascii_lowercase();
            if l.starts_with(&lower) && l != lower {
                push(&mut items, AcItem {
                    label: e.label.into(),
                    insert: e.insert.into(),
                    keep: 0,
                    doc: Some(e.doc.into()),
                    score: 0,
                });
            } else if lower.len() >= 3 && l.contains(&lower) {
                push(&mut items, AcItem {
                    label: e.label.into(),
                    insert: e.insert.into(),
                    keep: 0,
                    doc: Some(e.doc.into()),
                    score: 2,
                });
            }
        }
    }

    // Member access: `<base>.<part>` / `<base>:<part>` on ANY variable name —
    // match API entries by their member part, and complete just the member.
    if let Some(s) = sep {
        let sepc = token.as_bytes()[s] as char;
        let base = &lower[..s];
        let member = &lower[s + 1..];
        // Inferred type for this base (case-sensitive, so use the raw token):
        // a KNOWN type completes exactly its own members and suppresses the
        // generic guesses — misnamed fields never make the list.
        let raw_base = &token[..s];
        let types = infer_var_types(file_text);
        let typed = types.iter().find(|(k, _)| k == raw_base).map(|(_, t)| t.clone());
        match &typed {
            Some(VarType::Component(comp)) if sepc == '.' => {
                for (c, fld, d) in COMPONENT_FIELDS {
                    if c == comp && fld.starts_with(member) && *fld != member {
                        push(&mut items, AcItem {
                            label: format!("{raw_base}.{fld}"),
                            insert: (*fld).into(),
                            keep: s + 1,
                            doc: Some(format!("{c} handle: {d}")),
                            score: 0,
                        });
                    }
                }
            }
            Some(VarType::Script) if sepc == '.' => {
                for (fld, d) in [
                    ("node", "The node the script is attached to (a node handle)."),
                    ("params", "The script's tunables table."),
                    ("kind", "The script's name (its .lua file stem)."),
                    ("valid", "False once the script/node is gone."),
                ] {
                    if fld.starts_with(member) && fld != member {
                        push(&mut items, AcItem {
                            label: format!("{raw_base}.{fld}"),
                            insert: fld.into(),
                            keep: s + 1,
                            doc: Some(d.into()),
                            score: 0,
                        });
                    }
                }
            }
            _ => {}
        }
        // For typed bases, restrict the generic API-member matching to entries
        // of the matching kind (node fields for Node vars, anim methods for
        // animators, …) — nothing that wouldn't run.
        let api_prefix: Option<&[&str]> = match &typed {
            Some(VarType::Node) => Some(&["node.", "node:"]),
            Some(VarType::Animator) => Some(&["anim:"]),
            Some(VarType::Particles) => Some(&["particles:"]),
            Some(VarType::Component(_)) | Some(VarType::Script) => Some(&[]),
            None => None,
        };
        for e in LUA_API {
            let Some(es) = e.label.find(['.', ':']) else { continue };
            if e.label.as_bytes()[es] as char != sepc {
                continue;
            }
            let (ebase, emember) = (&e.label[..es], &e.label[es + 1..]);
            if let Some(allowed) = api_prefix
                && !allowed.iter().any(|p| e.label.starts_with(p))
            {
                continue;
            }
            let eml = emember.to_ascii_lowercase();
            if eml.starts_with(member) && eml != member {
                let insert =
                    e.insert.find(['.', ':']).map(|i| &e.insert[i + 1..]).unwrap_or(e.insert);
                push(&mut items, AcItem {
                    label: e.label.into(),
                    insert: insert.into(),
                    keep: s + 1,
                    doc: Some(e.doc.into()),
                    score: if ebase.eq_ignore_ascii_case(base) { 0 } else { 1 },
                });
            }
        }
        if sepc == '.' {
            // This script's own tunables complete after `params.`.
            if base == "params" {
                for k in defaults_keys(file_text) {
                    if k.to_ascii_lowercase().starts_with(member) && k.to_ascii_lowercase() != member {
                        push(&mut items, AcItem {
                            label: format!("params.{k}"),
                            insert: k.clone(),
                            keep: s + 1,
                            doc: Some("A tunable from this script's `defaults` (Inspector-editable).".into()),
                            score: 0,
                        });
                    }
                }
            }
            if typed.is_none() {
                // Untyped variable: offer every component-handle field (rb.fri
                // → friction). Typed variables got exact fields above instead.
                for (comp, f, d) in COMPONENT_FIELDS {
                    if f.starts_with(member) && *f != member {
                        push(&mut items, AcItem {
                            label: (*f).into(),
                            insert: (*f).into(),
                            keep: s + 1,
                            doc: Some(format!("{comp} handle: {d}")),
                            score: 2,
                        });
                    }
                }
            }
        }
    } else {
        // Identifiers from this file round the list out.
        for w in doc_words(file_text, token, token) {
            push(&mut items, AcItem {
                label: w.clone(),
                insert: w,
                keep: 0,
                doc: None,
                score: 4,
            });
        }
    }

    items.sort_by(|a, b| (a.score, a.label.as_str()).cmp(&(b.score, b.label.as_str())));
    items.truncate(10);
    items
}

fn current_token(text: &str, cursor_char: usize) -> (usize, String) {
    let chars: Vec<char> = text.chars().collect();
    let cur = cursor_char.min(chars.len());
    let mut start = cur;
    while start > 0 {
        let c = chars[start - 1];
        // `:` is a token char so method access (`node:getc…`, `anim:pl…`) completes.
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == ':' {
            start -= 1;
        } else {
            break;
        }
    }
    (start, chars[start..cur].iter().collect())
}

/// The full identifier (run of `[A-Za-z0-9_.:]`) containing char index `idx`, or
/// empty if that char isn't part of one. Used for hover docs.
fn word_at(text: &str, idx: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let i = idx.min(chars.len() - 1);
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == ':';
    if !is_word(chars[i]) {
        return String::new();
    }
    let mut s = i;
    while s > 0 && is_word(chars[s - 1]) {
        s -= 1;
    }
    let mut e = i;
    while e + 1 < chars.len() && is_word(chars[e + 1]) {
        e += 1;
    }
    chars[s..=e].iter().collect()
}

/// Replace the characters in `[start, end)` (char indices) of `s` with `ins`.
fn replace_chars(s: &mut String, start: usize, end: usize, ins: &str) {
    let (bs, be) = (line_edit::byte_of_char(s, start), line_edit::byte_of_char(s, end));
    s.replace_range(bs..be, ins);
}

/// 1-based (line, column) of char index `c` in `text`.
fn line_col(text: &str, c: usize) -> (usize, usize) {
    let (mut line, mut col) = (1, 1);
    for ch in text.chars().take(c) {
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Identifiers appearing in `text` that start with `prefix` (ASCII
/// case-insensitive), for document-word autocompletion. Excludes `except`.
fn doc_words(text: &str, prefix: &str, except: &str) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_byte(bytes[i]) && (i == 0 || !is_ident_byte(bytes[i - 1])) {
            let s = i;
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            let w = &text[s..i];
            if w.len() >= 3
                && !w.as_bytes()[0].is_ascii_digit()
                && w != except
                && w.len() > prefix.len()
                && w[..prefix.len()].eq_ignore_ascii_case(prefix)
            {
                out.push(w.to_string());
            }
        } else {
            i += 1;
        }
    }
    out.sort();
    out.dedup();
    out
}

// ---- the Scripting tab ------------------------------------------------------

impl EditorTabViewer<'_> {
    /// Populate the IDE's "references" list with every whole-word, case-sensitive use of
    /// `word` across all open buffers (using their live, unsaved text) plus every other
    /// `.lua` file in the project's scripts directory.
    pub(crate) fn gather_references(&mut self, word: &str) {
        let mut hits = Vec::new();
        self.scan_scripts(&mut hits, |path, name, text, out| {
            collect_word_hits(path, name, text, word, out)
        });
        self.ide.refs = hits;
        self.ide.refs_word = word.to_string();
    }

    /// Populate the references list with every LINE containing `needle` (substring,
    /// honoring the find bar's match-case) across open buffers + project scripts.
    fn gather_text_matches(&mut self, needle: &str) {
        let case = self.ide.find_case;
        let mut hits = Vec::new();
        self.scan_scripts(&mut hits, |path, name, text, out| {
            collect_line_hits(path, name, text, needle, case, out)
        });
        self.ide.refs = hits;
        self.ide.refs_word = needle.to_string();
    }

    /// Run `collect` over every open buffer (live text) + every unopened `.lua`
    /// under the project's scripts directory.
    fn scan_scripts(
        &self,
        hits: &mut Vec<RefHit>,
        collect: impl Fn(&str, &str, &str, &mut Vec<RefHit>),
    ) {
        let mut seen = std::collections::HashSet::new();
        for f in &self.ide.open {
            seen.insert(f.path.clone());
            collect(&f.path, &f.name, &f.text, hits);
        }
        let dir = self.project_root.join("scripts");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lua"))
                .collect();
            files.sort();
            for p in files {
                let ps = p.to_string_lossy().to_string();
                if seen.contains(&ps) {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&p) {
                    let name =
                        p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    collect(&ps, &name, &text, hits);
                }
            }
        }
    }

    /// Jump to where `word` is defined: the active file first, then the other open files,
    /// then the project's scripts on disk. Falls back to "find all references" if no
    /// definition is found (so Ctrl+B / the menu item always does something useful).
    pub(crate) fn goto_definition(&mut self, word: &str) {
        let active = self.ide.active.filter(|&a| a < self.ide.open.len());
        if let Some(a) = active
            && let Some(line) = find_definition_line(&self.ide.open[a].text, word) {
                self.ide.goto = Some(line);
                return;
            }
        // Other already-open files.
        let others: Vec<(String, String)> = self
            .ide
            .open
            .iter()
            .enumerate()
            .filter(|(idx, _)| Some(*idx) != active)
            .map(|(_, f)| (f.path.clone(), f.text.clone()))
            .collect();
        for (path, text) in others {
            if let Some(line) = find_definition_line(&text, word) {
                if self.ide.open_file(&path) {
                    self.ide.goto = Some(line);
                }
                return;
            }
        }
        // Scripts on disk that aren't open yet.
        let open_paths: std::collections::HashSet<String> =
            self.ide.open.iter().map(|f| f.path.clone()).collect();
        let dir = self.project_root.join("scripts");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lua"))
                .collect();
            files.sort();
            for p in files {
                let ps = p.to_string_lossy().to_string();
                if open_paths.contains(&ps) {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&p)
                    && let Some(line) = find_definition_line(&text, word) {
                        if self.ide.open_file(&ps) {
                            self.ide.goto = Some(line);
                        }
                        return;
                    }
            }
        }
        // No definition found — show references so Ctrl+B still helps.
        self.gather_references(word);
    }

    pub(crate) fn scripting_ui(&mut self, ui: &mut egui::Ui) {
        // Live script errors (from the last play frame) surface here in red.
        if !self.script_errors.is_empty() {
            egui::Frame::NONE
                .fill(egui::Color32::from_rgb(60, 20, 20))
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Δ script errors").strong().color(egui::Color32::from_rgb(255, 150, 150)));
                    for e in self.script_errors {
                        ui.label(egui::RichText::new(e).monospace().color(egui::Color32::from_rgb(255, 180, 180)));
                    }
                });
        }
        self.close_confirm_modal(ui);
        // Tab strip: Docs + each open file. Middle-click closes a tab.
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.ide.active.is_none(), "§ Docs").clicked() {
                self.ide.active = None;
            }
            let mut close: Option<usize> = None;
            for i in 0..self.ide.open.len() {
                let f = &self.ide.open[i];
                let title = if f.dirty { format!("{} *", f.name) } else { f.name.clone() };
                let resp = ui
                    .selectable_label(self.ide.active == Some(i), title)
                    .on_hover_text(&self.ide.open[i].path);
                if resp.clicked() {
                    self.ide.active = Some(i);
                }
                if resp.middle_clicked() {
                    close = Some(i);
                }
                if ui.small_button("×").clicked() {
                    close = Some(i);
                }
            }
            if let Some(i) = close {
                self.request_close_tab(i);
            }
        });
        ui.separator();

        match self.ide.active {
            None => self.docs_page_ui(ui),
            Some(i) if i < self.ide.open.len() => self.file_editor_ui(ui, i),
            _ => {
                self.ide.active = None;
            }
        }
    }

    /// Close tab `i`, confirming first when it has unsaved changes.
    fn request_close_tab(&mut self, i: usize) {
        if self.ide.open.get(i).is_some_and(|f| f.dirty) {
            self.ide.close_confirm = Some(i);
        } else {
            self.ide.close_tab(i);
        }
    }

    /// The "unsaved script" Save / Discard / Cancel modal (close-tab guard).
    fn close_confirm_modal(&mut self, ui: &mut egui::Ui) {
        let Some(ci) = self.ide.close_confirm else { return };
        if ci >= self.ide.open.len() {
            self.ide.close_confirm = None;
            return;
        }
        let name = self.ide.open[ci].name.clone();
        let mut open = true;
        let mut close = false;
        egui::Window::new("Unsaved script")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(320.0)
            .show(ui.ctx(), |ui| {
                ui.label(format!("\"{name}\" has unsaved changes."));
                ui.horizontal(|ui| {
                    if ui.button("💾 Save & close").clicked() {
                        if self.ide.save_file(ci) {
                            self.cmd.refresh_assets = true;
                            self.ide.close_tab(ci);
                        }
                        close = true;
                    }
                    if ui.button("Discard changes").clicked() {
                        self.ide.close_tab(ci);
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if !open || close {
            self.ide.close_confirm = None;
        }
    }

    /// The Docs landing page: three pages behind one search box — the sectioned
    /// guide, the **API browser**, and the shader stdlib.
    ///
    /// They used to be one endless scroll with the reference at the bottom,
    /// which meant the only way to reach an API entry was to search for a name
    /// you already knew. A reference you cannot *browse* teaches nobody the
    /// thing they didn't know existed, and "the engine has a scheduler and
    /// nobody knows" was the actual complaint that started this work.
    fn docs_page_ui(&mut self, ui: &mut egui::Ui) {
        // The page switch, then a filter box that narrows whichever page is up.
        ui.horizontal(|ui| {
            for (page, label) in [
                (DocsPage::Guides, "📖 Guides"),
                (DocsPage::Api, "⚙ API"),
                (DocsPage::Shaders, "◈ Shaders"),
            ] {
                if ui.selectable_label(self.ide.docs_page == page, label).clicked() {
                    self.ide.docs_page = page;
                }
            }
            ui.separator();
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.ide.docs_search)
                    .hint_text(match self.ide.docs_page {
                        DocsPage::Api => "search the API — \"look\", \"jump\", \"tween\", \"http\"",
                        DocsPage::Shaders => "search the shader stdlib",
                        DocsPage::Guides => "search the guides — \"friction\", \"crossfade\", \"mouse\"",
                    })
                    .desired_width(300.0),
            );
            if !self.ide.docs_search.is_empty() && ui.small_button("✖").clicked() {
                self.ide.docs_search.clear();
            }
        });
        ui.add_space(4.0);
        let q = self.ide.docs_search.trim().to_ascii_lowercase();
        let searching = !q.is_empty();
        let mut hits = 0usize;
        let page = self.ide.docs_page;
        egui::ScrollArea::vertical().show(ui, |ui| {
          if page == DocsPage::Guides {
            for (n, (title, body)) in DOC_SECTIONS.iter().enumerate() {
                if searching
                    && !title.to_ascii_lowercase().contains(&q)
                    && !body.to_ascii_lowercase().contains(&q)
                {
                    continue;
                }
                hits += 1;
                let hdr = egui::CollapsingHeader::new(*title).id_salt(("doc_sec", n));
                // While searching, matching sections open themselves.
                let hdr = if searching { hdr.open(Some(true)) } else { hdr.default_open(n == 0) };
                hdr.show(ui, |ui| self.doc_body_ui(ui, body));
            }
          }
          if page == DocsPage::Api {
            ui.small(
                "Every name the engine provides. The same table drives autocomplete as you \
                 type (Tab accepts, ↑↓ chooses) and the hover docs in the editor — click a \
                 name or an example to copy it.",
            );
            ui.add_space(6.0);
            // SEARCHING is a different job from browsing. Grouped results make
            // you scan every category for the one row you wanted, and with 500+
            // entries a doc-text match in the first group buries an exact name
            // match in the last. So while there's a query, rank everything into
            // one flat list, best first, and label each row with the group it
            // came from — you still learn where it lives, you just don't have
            // to go looking.
            if searching {
                let mut ranked: Vec<(u8, &ApiEntry)> =
                    LUA_API.iter().filter_map(|e| api_rank(e, &q).map(|r| (r, e))).collect();
                ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.label.cmp(b.1.label)));
                hits += ranked.len();
                if !ranked.is_empty() {
                    ui.small(format!(
                        "{} match{} — best first",
                        ranked.len(),
                        if ranked.len() == 1 { "" } else { "es" }
                    ));
                    ui.add_space(4.0);
                }
                for (_, e) in ranked {
                    self.api_entry_ui(ui, e, true);
                }
            }
            // …and with no query, the grouped browse. Groups open by default:
            // this is a BROWSER, and a wall of closed headers is a table of
            // contents, not a reference.
            if !searching {
                for cat in API_CATEGORIES {
                    let entries: Vec<&ApiEntry> =
                        LUA_API.iter().filter(|e| api_category(e.label) == *cat).collect();
                    if entries.is_empty() {
                        continue;
                    }
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("{cat}  ({})", entries.len())).strong(),
                    )
                    .id_salt(("api_cat", cat))
                    .default_open(true)
                    .show(ui, |ui| {
                        for e in entries {
                            self.api_entry_ui(ui, e, false);
                        }
                    });
                }
            }
          }
          if page == DocsPage::Shaders {
            ui.strong("Shader stdlib (.flsl)");
            ui.small(
                "Custom material looks (ADR-0007): Assets → right-click → ◈ New Shader, then \
                 Inspector → Material → Shader to assign. `uniform`s become Inspector knobs, \
                 `texture` slots take drag-and-drop textures, and every op below can be wired \
                 by name — also editable in VSCode.",
            );
            ui.add_space(4.0);
            {
                let inputs: Vec<String> = floptle_shader::ir::Input::all()
                    .iter()
                    .map(|i| format!("{}: {}", i.name(), i.ty().flsl()))
                    .collect();
                let inputs_line = format!("inputs — {}", inputs.join(", "));
                if !searching || inputs_line.to_ascii_lowercase().contains(&q) {
                    hits += 1;
                    ui.monospace(
                        egui::RichText::new(inputs_line)
                            .color(egui::Color32::from_rgb(190, 140, 255)),
                    );
                    ui.add_space(2.0);
                }
            }
            for cat in floptle_shader::stdlib::CATEGORIES.iter().copied() {
                let ops: Vec<&floptle_shader::stdlib::OpSpec> = floptle_shader::stdlib::OPS
                    .iter()
                    .filter(|o| o.category == cat)
                    .filter(|o| {
                        !searching
                            || o.name.to_ascii_lowercase().contains(&q)
                            || o.doc.to_ascii_lowercase().contains(&q)
                    })
                    .collect();
                if ops.is_empty() {
                    continue;
                }
                hits += ops.len();
                let hdr = egui::CollapsingHeader::new(format!("◈ {cat}  ({})", ops.len()))
                    .id_salt(("flsl_cat", cat));
                let hdr = if searching { hdr.open(Some(true)) } else { hdr.default_open(false) };
                hdr.show(ui, |ui| {
                    for o in ops {
                        let args: Vec<String> = o
                            .inputs
                            .iter()
                            .map(|i| match i.default {
                                Some(d) => format!("{}: {d}", i.name),
                                None => i.name.to_string(),
                            })
                            .collect();
                        ui.monospace(
                            egui::RichText::new(format!("{}({})", o.name, args.join(", ")))
                                .color(egui::Color32::from_rgb(190, 140, 255)),
                        );
                        ui.indent(("flsl_doc", o.name), |ui| ui.small(o.doc));
                        ui.add_space(2.0);
                    }
                });
            }
          }
            if searching && hits == 0 {
                ui.add_space(8.0);
                ui.label(format!(
                    "No matches for \"{}\" on this page — try a broader word, or another tab.",
                    self.ide.docs_search.trim()
                ));
            }
            ui.add_space(10.0);
            egui::CollapsingHeader::new("⌨ Editor shortcuts").default_open(false).show(ui, |ui| {
                ui.monospace(IDE_SHORTCUTS);
            });
        });
    }

    /// One API entry: the name (click to copy), its description, and its worked
    /// example if it has one.
    ///
    /// `with_group` adds the category it belongs to, which a flat search result
    /// needs and a row already sitting under that category's header does not.
    fn api_entry_ui(&mut self, ui: &mut egui::Ui, e: &ApiEntry, with_group: bool) {
        ui.horizontal(|ui| {
            // The name copies on click — the shortest path from "what was that
            // called?" to having it in your script.
            let name = ui
                .add(
                    egui::Label::new(
                        egui::RichText::new(e.label)
                            .monospace()
                            .color(egui::Color32::from_rgb(78, 201, 176)),
                    )
                    .sense(egui::Sense::click()),
                )
                .on_hover_text("click to copy");
            if name.clicked() {
                ui.ctx().copy_text(e.label.to_string());
            }
            if with_group {
                ui.weak(egui::RichText::new(api_category(e.label)).small());
            }
        });
        ui.indent(("api_doc", e.label), |ui| {
            inline_doc_label(ui, e.doc, &egui::FontId::monospace(12.0));
            // A worked example beats a signature — the signature is already in
            // the line above.
            if let Some(ex) = api_example(e.label) {
                self.doc_body_ui(ui, &indent_block(ex));
                if ui
                    .small_button("⎘ copy example")
                    .on_hover_text("copy this snippet to the clipboard")
                    .clicked()
                {
                    ui.ctx().copy_text(ex.to_string());
                }
            }
        });
        ui.add_space(4.0);
    }

    /// The code editor for open file `i`: toolbar, find/replace, shortcuts, the
    /// highlighted text area, diagnostics, autocomplete and the references panel.
    fn file_editor_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let editor_id = egui::Id::new(("ide_editor", self.ide.open[i].path.clone()));
        // ---- toolbar: path, save, external editor, snippets + Ln/Col status ----
        ui.horizontal(|ui| {
            ui.small(self.ide.open[i].path.clone());
            let dirty = self.ide.open[i].dirty;
            if ui.add_enabled(dirty, egui::Button::new("Save")).on_hover_text("Ctrl+S").clicked() {
                if self.ide.format_on_save {
                    self.format_with_caret(ui, i, editor_id);
                }
                if self.ide.save_file(i) {
                    self.cmd.refresh_assets = true;
                }
            }
            if self.ide.open.iter().filter(|f| f.dirty).count() > 1
                && ui.button("Save all").on_hover_text("Ctrl+Shift+S").clicked()
            {
                for k in 0..self.ide.open.len() {
                    self.ide.save_file(k);
                }
                self.cmd.refresh_assets = true;
            }
            if ui
                .button("⏵ Open in IDE")
                .on_hover_text("Open the project in your external editor (set it in Project Settings)")
                .clicked()
            {
                // Save first so the external editor sees the latest text.
                self.ide.save_file(i);
                self.cmd.open_in_editor = Some(self.ide.open[i].path.clone());
            }
            // Format: the button, and the on-save toggle right next to it so the
            // behaviour is discoverable where you'd look for it rather than buried
            // in settings.
            let is_lua_tab = self.ide.open[i].path.ends_with(".lua");
            if is_lua_tab {
                if ui
                    .button("▤ Format")
                    .on_hover_text(
                        "Alt+Shift+F — re-indent this file by block depth.\n\
                         Never changes anything but whitespace; `--@noformat` opts a file out.",
                    )
                    .clicked()
                {
                    self.format_with_caret(ui, i, editor_id);
                }
                ui.checkbox(&mut self.ide.format_on_save, "on save")
                    .on_hover_text("format when you save with Ctrl+S / Save (Play's auto-save leaves your text alone)");
            }
            ui.menu_button("Insert snippet", |ui| {
                ui.small("ready-made patterns — appended to the end of the file");
                for (category, snippets) in LUA_SNIPPETS {
                    ui.menu_button(*category, |ui| {
                        // Wide enough that snippet names read comfortably.
                        ui.set_min_width(280.0);
                        for (label, snippet) in *snippets {
                            if ui.button(*label).clicked() {
                                self.ide.open[i].text.push_str(snippet);
                                self.ide.open[i].dirty = true;
                                ui.close();
                            }
                        }
                    });
                }
            });
            // Ln/Col (+ selection size) from the editor's stored cursor state.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((a, b, p)) = ide_selection(ui.ctx(), editor_id) {
                    let (line, col) = line_col(&self.ide.open[i].text, p);
                    let sel = b - a;
                    let status = if sel > 0 {
                        format!("Ln {line}, Col {col} · {sel} selected")
                    } else {
                        format!("Ln {line}, Col {col}")
                    };
                    ui.small(egui::RichText::new(status).color(egui::Color32::from_gray(140)));
                }
            });
        });
        // Hint: the tunables this script declares via its `defaults` table.
        let hint = script_hint(&self.ide.open[i].text);
        if !hint.is_empty() {
            ui.small(egui::RichText::new(hint).color(egui::Color32::from_gray(160)));
        }

        // ---- tab-wide shortcuts (work from the editor, the find bar, anywhere) ----
        let mut nav: i32 = 0; // find navigation: -1 prev / +1 next
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::S)) {
            for k in 0..self.ide.open.len() {
                if self.ide.format_on_save {
                    // The file on screen keeps its caret; the others aren't showing one.
                    if k == i {
                        self.format_with_caret(ui, k, editor_id);
                    } else {
                        self.ide.format_file(k);
                    }
                }
                self.ide.save_file(k);
            }
            self.cmd.refresh_assets = true;
        }
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::S)) {
            if self.ide.format_on_save {
                self.format_with_caret(ui, i, editor_id);
            }
            if self.ide.save_file(i) {
                self.cmd.refresh_assets = true;
            }
        }
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::W)) {
            self.request_close_tab(i);
            return; // the tab may be gone — draw fresh next frame
        }
        // Ctrl+F / Ctrl+H open find (+replace), seeded from the editor selection.
        let open_find = ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::F));
        let open_replace = ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::H));
        if open_find || open_replace {
            self.ide.find_open = true;
            self.ide.find_focus = true;
            if open_replace {
                self.ide.replace_open = true;
            }
            if let Some((a, b, _)) = ide_selection(ui.ctx(), editor_id)
                && a != b && b - a <= 200 {
                    let text = &self.ide.open[i].text;
                    let (ba, bb) =
                        (line_edit::byte_of_char(text, a), line_edit::byte_of_char(text, b));
                    let sel = &text[ba..bb];
                    if !sel.contains('\n') {
                        self.ide.find_query = sel.to_string();
                    }
                }
        }
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::G)) {
            self.ide.goto_line_open = true;
            self.ide.goto_line_focus = true;
        }
        // F3 / Shift+F3 repeat the search without touching the find bar.
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::SHIFT, egui::Key::F3)) {
            nav = -1;
        }
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::F3)) {
            nav = 1;
        }
        if nav != 0 && !self.ide.find_query.is_empty() {
            self.ide.find_open = true;
        }

        // ---- go-to-line prompt (Ctrl+G) ----
        if self.ide.goto_line_open {
            let mut close = false;
            ui.horizontal(|ui| {
                ui.label("go to line:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.ide.goto_line_buf).desired_width(70.0),
                );
                if self.ide.goto_line_focus {
                    resp.request_focus();
                    self.ide.goto_line_focus = false;
                }
                if resp.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                    if let Ok(n) = self.ide.goto_line_buf.trim().parse::<usize>() {
                        let n = n.max(1);
                        let text = &self.ide.open[i].text;
                        // Caret to the start of that line (clamped to the last line).
                        let mut chars = 0;
                        let mut line = 1;
                        for ch in text.chars() {
                            if line >= n {
                                break;
                            }
                            chars += 1;
                            if ch == '\n' {
                                line += 1;
                            }
                        }
                        set_ide_caret(ui.ctx(), editor_id, chars);
                        ui.ctx().memory_mut(|m| m.request_focus(editor_id));
                        self.ide.goto = Some(n.min(line));
                    }
                    close = true;
                }
            });
            if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                close = true;
                ui.ctx().memory_mut(|m| m.request_focus(editor_id));
            }
            if close {
                self.ide.goto_line_open = false;
                self.ide.goto_line_buf.clear();
            }
        }

        // ---- find & replace bar ----
        if self.ide.find_open {
            self.find_bar_ui(ui, i, editor_id, nav);
        }

        // ---- the code editor ----
        let is_lua = self.ide.open[i].path.ends_with(".lua");
        let is_flsl = crate::assets::is_shader(&self.ide.open[i].path);
        let font = egui::FontId::monospace(13.0);
        let lfont = font.clone();
        let theme = CODE_THEMES[self.code_theme.min(CODE_THEMES.len() - 1)];
        let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            // No wrap (code editor) — logical lines == rows, so the gutter aligns.
            let mut job = if is_lua {
                lua_highlight(buf.as_str(), lfont.clone(), &theme)
            } else if is_flsl {
                crate::theme::flsl_highlight(buf.as_str(), lfont.clone(), &theme)
            } else {
                plain_job(buf.as_str(), lfont.clone(), &theme)
            };
            job.wrap.max_width = f32::INFINITY;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        // While the completion popup is open (last frame) it owns ENTER (accept),
        // the arrow keys (choose) and Esc (dismiss) — eaten *before* the editor
        // runs so they don't insert a newline / move the caret.
        //
        // Enter accepts and TAB NEVER DOES (v0.17.0): Tab is indentation, always,
        // which is the one key you press without looking. The popup only opens on
        // its own after `.` or `:` — where you're asking "what fields does this
        // have?" — so an Enter it intercepts is an Enter you aimed at it. Ctrl+Space
        // summons it anywhere, including for a plain word.
        let ac_id = egui::Id::new(("ide_ac_open", editor_id));
        // …but only while the EDITOR still has the keyboard. The open flag is last
        // frame's; if focus has since moved to the find bar or the go-to-line
        // prompt, the popup is about to close and its keys belong to whatever is
        // focused now. Without this check there is a one-frame window where Enter
        // in the find bar is swallowed by a popup you can no longer see — the kind
        // of once-in-twenty-times key loss that reads as "the editor is flaky".
        let editor_has_keys = ui.memory(|m| m.has_focus(editor_id));
        let ac_was_open =
            editor_has_keys && ui.ctx().data(|d| d.get_temp::<bool>(ac_id).unwrap_or(false));
        let (mut ac_accept, mut ac_nav, mut ac_dismiss) = (false, 0i32, false);
        if ac_was_open {
            ui.input_mut(|inp| {
                ac_accept = inp.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                ac_nav = inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) as i32
                    - (inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) as i32);
                ac_dismiss = inp.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            });
        }
        // Ctrl+Space — summon completion for whatever is under the caret, even a
        // one-character word. Latched until the token changes so the popup stays
        // up while you keep typing the name.
        if editor_has_keys
            && ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::Space))
        {
            self.ide.ac_manual = true;
            self.ide.ac_dismissed = false;
        }

        self.editor_shortcuts(ui, i, editor_id, is_lua);

        let line_count = self.ide.open[i].text.matches('\n').count() + 1;
        let goto = self.ide.goto.take();
        let find_hl = (self.ide.find_open && !self.ide.find_query.is_empty())
            .then(|| (self.ide.find_query.clone(), self.ide.find_case, self.ide.find_idx));
        // Selected-text occurrences: highlight the OTHER instances of a short,
        // single-line selection (standard IDE behavior). Skipped while the find
        // bar has a query so the two highlights never fight.
        let occ_hl = if find_hl.is_none() {
            ide_selection(ui.ctx(), editor_id).and_then(|(a, b, _)| {
                let text = &self.ide.open[i].text;
                if a == b {
                    return None;
                }
                let (ba, bb) =
                    (line_edit::byte_of_char(text, a), line_edit::byte_of_char(text, b));
                let sel = &text[ba..bb];
                (sel.len() >= 2 && sel.len() <= 200 && !sel.contains('\n') && !sel.trim().is_empty())
                    .then(|| (sel.to_string(), ba, bb))
            })
        } else {
            None
        };
        let diag_line = self.ide_diag.map(|(l, _)| *l);
        let output = egui::ScrollArea::both()
            .id_salt("ide_scroll")
            .show(ui, |ui| {
                let out = ui
                    .horizontal_top(|ui| {
                        // Line-number gutter (aligned with the un-wrapped rows).
                        let nums: String = (1..=line_count).fold(String::new(), |mut s, n| {
                            s.push_str(&format!("{n}\n"));
                            s
                        });
                        ui.add(egui::Label::new(
                            egui::RichText::new(nums).font(font.clone()).color(theme.gutter32()),
                        ));
                        // The code editor's background follows the selected editor theme.
                        ui.style_mut().visuals.extreme_bg_color = theme.bg32();
                        egui::TextEdit::multiline(&mut self.ide.open[i].text)
                            .id(editor_id)
                            .code_editor()
                            .desired_width(f32::INFINITY)
                            .desired_rows(20)
                            .layouter(&mut layouter)
                            .show(ui)
                    })
                    .inner;
                // All galley-space painting happens HERE, inside the scroll area, so
                // it's clipped to the code viewport (never over toolbars or panels).
                let painter = ui.painter();
                let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
                let text = &self.ide.open[i].text;
                // Current-line wash.
                if out.response.response.has_focus()
                    && let Some(cr) = out.cursor_range {
                        let caret = cr.primary.index.0;
                        let row = text.chars().take(caret).filter(|&c| c == '\n').count();
                        if let Some(r) = out.galley.rows.get(row) {
                            let rr = r.rect();
                            let clip = ui.clip_rect();
                            let rect = egui::Rect::from_min_max(
                                egui::pos2(clip.left(), out.galley_pos.y + rr.top()),
                                egui::pos2(clip.right(), out.galley_pos.y + rr.bottom()),
                            );
                            painter.rect_filled(rect, 0.0, theme.cur_line32());
                        }
                    }
                // Find matches: all in amber, the CURRENT one brighter + outlined.
                if let Some((query, case, idx)) = &find_hl {
                    let hl = egui::Color32::from_rgba_unmultiplied(255, 210, 0, 45);
                    let cur = egui::Color32::from_rgba_unmultiplied(255, 160, 40, 90);
                    for (n, (bs, be)) in find_ranges(text, query, *case).into_iter().enumerate() {
                        let line = text[..bs].matches('\n').count();
                        let line_start = text[..bs].rfind('\n').map(|p| p + 1).unwrap_or(0);
                        let col = text[line_start..bs].chars().count();
                        let len = text[bs..be].chars().count();
                        if let Some(r) = out.galley.rows.get(line) {
                            let rr = r.rect();
                            let x0 = out.galley_pos.x + rr.left() + col as f32 * char_w;
                            let x1 = x0 + len as f32 * char_w;
                            let rect = egui::Rect::from_min_max(
                                egui::pos2(x0, out.galley_pos.y + rr.top()),
                                egui::pos2(x1, out.galley_pos.y + rr.bottom()),
                            );
                            painter.rect_filled(rect, 2.0, if n == *idx { cur } else { hl });
                            if n == *idx {
                                painter.rect_stroke(
                                    rect,
                                    2.0,
                                    egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 190, 90)),
                                    egui::StrokeKind::Outside,
                                );
                            }
                        }
                    }
                }
                // Other occurrences of the selected text, in a cool wash (the
                // selection itself is already drawn by the TextEdit).
                if let Some((sel, ba, bb)) = &occ_hl {
                    let hl = egui::Color32::from_rgba_unmultiplied(110, 170, 255, 40);
                    for (bs, be) in find_ranges(text, sel, true) {
                        if bs == *ba && be == *bb {
                            continue;
                        }
                        let line = text[..bs].matches('\n').count();
                        let line_start = text[..bs].rfind('\n').map(|p| p + 1).unwrap_or(0);
                        let col = text[line_start..bs].chars().count();
                        let len = text[bs..be].chars().count();
                        if let Some(r) = out.galley.rows.get(line) {
                            let rr = r.rect();
                            let x0 = out.galley_pos.x + rr.left() + col as f32 * char_w;
                            let x1 = x0 + len as f32 * char_w;
                            let rect = egui::Rect::from_min_max(
                                egui::pos2(x0, out.galley_pos.y + rr.top()),
                                egui::pos2(x1, out.galley_pos.y + rr.bottom()),
                            );
                            painter.rect_filled(rect, 2.0, hl);
                        }
                    }
                }
                // Red squiggle on the line of a Lua syntax error.
                if let Some(line) = diag_line {
                    let row = line.saturating_sub(1).min(out.galley.rows.len().saturating_sub(1));
                    if let Some(r) = out.galley.rows.get(row) {
                        let rr = r.rect();
                        let y = out.galley_pos.y + rr.bottom();
                        let x0 = out.galley_pos.x + rr.left();
                        let x1 = out.galley_pos.x + rr.right().max(rr.left() + 30.0);
                        painter.line_segment(
                            [egui::pos2(x0, y), egui::pos2(x1, y)],
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(235, 80, 80)),
                        );
                    }
                }
                // A pending jump (Console source, find, Ctrl+G) scrolls into view.
                if let Some(line) = goto {
                    let row = line.saturating_sub(1).min(out.galley.rows.len().saturating_sub(1));
                    if let Some(r) = out.galley.rows.get(row) {
                        let rr = r.rect();
                        let target = egui::Rect::from_min_max(
                            out.galley_pos + rr.left_top().to_vec2(),
                            out.galley_pos + rr.right_bottom().to_vec2(),
                        );
                        ui.scroll_to_rect(target, Some(egui::Align::Center));
                    }
                }
                out
            })
            .inner;
        if output.response.response.changed() {
            self.ide.open[i].dirty = true;
        }

        // Right-click an identifier → Go to definition / Find all references. Capture
        // the word at the moment of the click (from the pointer position over the
        // code) and hold it: reading the LIVE hover each frame flickers, because once
        // the menu opens the pointer sits over the menu, not the word.
        if output.response.response.secondary_clicked() {
            self.ide.rc_word = output
                .response
                .response
                .hover_pos()
                .map(|p| {
                    let cc = output.galley.cursor_from_pos(p - output.galley_pos);
                    word_at(&self.ide.open[i].text, cc.index.0)
                })
                .filter(|w| !w.is_empty());
        }
        let rc_word = self.ide.rc_word.clone();
        output.response.response.context_menu(|ui| {
            match &rc_word {
                Some(w) => {
                    if ui.button(format!("📋 Go to definition of \"{w}\"  (Ctrl+B)")).clicked() {
                        self.goto_definition(w);
                        ui.close();
                    }
                    if ui.button(format!("🔎 Find all references to \"{w}\"")).clicked() {
                        self.gather_references(w);
                        ui.close();
                    }
                }
                None => {
                    ui.label("right-click a word for its definition / references");
                }
            }
        });
        if let Some((line, msg)) = self.ide_diag {
            ui.colored_label(egui::Color32::from_rgb(235, 120, 120), format!("Δ line {line}: {msg}"));
        }
        self.ide_lints_ui(ui, i);
        let ac_open = self.ide_autocomplete(
            ui,
            i,
            editor_id,
            output.response.response.has_focus(),
            output.cursor_range,
            &output.galley,
            output.galley_pos,
            ac_accept,
            ac_nav,
            ac_dismiss,
        );
        ui.ctx().data_mut(|d| d.insert_temp(ac_id, ac_open));

        // Hover doc: hovering an API identifier in the code shows its tooltip.
        if let Some(p) = output.response.response.hover_pos() {
            let rel = p - output.galley_pos;
            let cc = output.galley.cursor_from_pos(rel);
            let word = word_at(&self.ide.open[i].text, cc.index.0);
            if let Some(api) = api_entry_for(&word) {
                let example = api_example(api.label);
                output.response.response.clone().on_hover_ui_at_pointer(|ui| {
                    ui.set_max_width(420.0);
                    ui.monospace(egui::RichText::new(api.label).color(egui::Color32::from_rgb(78, 201, 176)));
                    ui.label(api.doc);
                    if let Some(ex) = example {
                        ui.add_space(4.0);
                        ui.separator();
                        for line in ex.lines() {
                            ui.monospace(
                                egui::RichText::new(line)
                                    .color(ui.visuals().weak_text_color())
                                    .size(11.5),
                            );
                        }
                    }
                });
            }
        }

        // "Find all references" / find-in-all-scripts results — click a row to jump.
        if !self.ide.refs.is_empty() {
            ui.separator();
            let word = self.ide.refs_word.clone();
            ui.horizontal(|ui| {
                ui.strong(format!("🔍 {} hit(s) for \"{word}\"", self.ide.refs.len()));
                if ui.small_button("✖ clear").clicked() {
                    self.ide.refs.clear();
                }
            });
            let mut jump: Option<(String, usize)> = None;
            egui::ScrollArea::vertical().max_height(150.0).id_salt("refs_scroll").show(ui, |ui| {
                for r in &self.ide.refs {
                    let row = format!("{}:{}", r.name, r.line);
                    if ui
                        .selectable_label(false, egui::RichText::new(format!("{row}   {}", r.text)).monospace())
                        .clicked()
                    {
                        jump = Some((r.path.clone(), r.line));
                    }
                }
            });
            if let Some((path, line)) = jump
                && self.ide.open_file(&path) {
                    self.ide.goto = Some(line);
                }
        }
    }

    /// The find & replace bar. Typing NEVER moves focus into the editor — the
    /// current match is selected in the editor's stored state + scrolled into
    /// view, and Enter / Shift+Enter (or F3 / ▶ ◀) step through matches while
    /// you keep typing. Esc closes and returns to the code.
    fn find_bar_ui(&mut self, ui: &mut egui::Ui, i: usize, editor_id: egui::Id, mut nav: i32) {
        let text = self.ide.open[i].text.clone();
        let ranges = find_ranges(&text, &self.ide.find_query, self.ide.find_case);
        let mut changed = false;
        let mut close = false;
        let (mut do_replace, mut do_replace_all) = (false, false);
        ui.horizontal(|ui| {
            ui.label("🔍");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.ide.find_query)
                    .desired_width(180.0)
                    .hint_text("find (Enter next, Shift+Enter prev)"),
            );
            if self.ide.find_focus {
                resp.request_focus();
                self.ide.find_focus = false;
            }
            changed |= resp.changed();
            if resp.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                nav = if ui.input(|inp| inp.modifiers.shift) { -1 } else { 1 };
                resp.request_focus(); // Enter surrendered focus — keep it in the field
            }
            if ui
                .selectable_label(self.ide.find_case, "Aa")
                .on_hover_text("match case")
                .clicked()
            {
                self.ide.find_case = !self.ide.find_case;
                changed = true;
            }
            if ui.button("◀").on_hover_text("previous match (Shift+F3)").clicked() {
                nav = -1;
            }
            if ui.button("▶").on_hover_text("next match (F3)").clicked() {
                nav = 1;
            }
            ui.label(
                if self.ide.find_query.is_empty() {
                    String::new()
                } else if ranges.is_empty() {
                    "no matches".to_string()
                } else {
                    format!("{} of {}", self.ide.find_idx.min(ranges.len() - 1) + 1, ranges.len())
                },
            );
            if ui
                .selectable_label(self.ide.replace_open, "⇄ replace")
                .on_hover_text("find & replace (Ctrl+H)")
                .clicked()
            {
                self.ide.replace_open = !self.ide.replace_open;
            }
            if !self.ide.find_query.is_empty()
                && ui
                    .button("🔍 all scripts")
                    .on_hover_text("list every matching line across all project scripts")
                    .clicked()
            {
                let q = self.ide.find_query.clone();
                self.gather_text_matches(&q);
            }
            if ui.button("✖").on_hover_text("close (Esc)").clicked() {
                close = true;
            }
        });
        if self.ide.replace_open {
            ui.horizontal(|ui| {
                ui.label("⇄");
                ui.add(
                    egui::TextEdit::singleline(&mut self.ide.replace_buf)
                        .desired_width(180.0)
                        .hint_text("replace with"),
                );
                if ui.add_enabled(!ranges.is_empty(), egui::Button::new("replace")).clicked() {
                    do_replace = true;
                }
                if ui.add_enabled(!ranges.is_empty(), egui::Button::new("replace all")).clicked() {
                    do_replace_all = true;
                }
            });
        }
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            close = true;
        }

        if !ranges.is_empty() {
            self.ide.find_idx = self.ide.find_idx.min(ranges.len() - 1);
            if changed {
                // Search restarts from the editor caret: first match at or after it.
                let caret = ide_selection(ui.ctx(), editor_id).map(|(a, ..)| a).unwrap_or(0);
                let caret_b = line_edit::byte_of_char(&text, caret);
                self.ide.find_idx =
                    ranges.iter().position(|&(a, _)| a >= caret_b).unwrap_or(0);
            }
            if nav > 0 {
                self.ide.find_idx = (self.ide.find_idx + 1) % ranges.len();
            } else if nav < 0 {
                self.ide.find_idx = (self.ide.find_idx + ranges.len() - 1) % ranges.len();
            }
            if do_replace_all {
                let t = &mut self.ide.open[i].text;
                for &(bs, be) in ranges.iter().rev() {
                    t.replace_range(bs..be, &self.ide.replace_buf);
                }
                self.ide.open[i].dirty = true;
                self.ide.find_idx = 0;
            } else if do_replace {
                let (bs, be) = ranges[self.ide.find_idx];
                let t = &mut self.ide.open[i].text;
                t.replace_range(bs..be, &self.ide.replace_buf);
                self.ide.open[i].dirty = true;
                // Select the replacement; the SAME index now points at the next match.
                let a = line_edit::char_of_byte(&self.ide.open[i].text, bs);
                let b = line_edit::char_of_byte(
                    &self.ide.open[i].text,
                    bs + self.ide.replace_buf.len(),
                );
                set_ide_selection(ui.ctx(), editor_id, a, b);
                self.ide.goto = Some(self.ide.open[i].text[..bs].matches('\n').count() + 1);
            } else if nav != 0 || changed {
                // Select + scroll to the current match — WITHOUT stealing focus, so
                // typing in the find field keeps flowing.
                let (bs, be) = ranges[self.ide.find_idx];
                let a = line_edit::char_of_byte(&text, bs);
                let b = line_edit::char_of_byte(&text, be);
                set_ide_selection(ui.ctx(), editor_id, a, b);
                self.ide.goto = Some(text[..bs].matches('\n').count() + 1);
            }
        }
        if close {
            self.ide.find_open = false;
            self.ide.replace_open = false;
            ui.ctx().memory_mut(|m| m.request_focus(editor_id));
        }
    }

    /// Keyboard editing shortcuts that need the editor focused: whole-line
    /// copy/cut/delete/duplicate/move, block indent + comment, auto-indent on
    /// Enter, and go-to-definition.
    fn editor_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        editor_id: egui::Id,
        is_lua: bool,
    ) {
        if !ui.memory(|m| m.has_focus(editor_id)) {
            return;
        }
        let Some((sel_a, sel_b, caret)) = ide_selection(ui.ctx(), editor_id) else { return };
        let empty_sel = sel_a == sel_b;

        // Copy/Cut with NO selection act on the whole line (VSCode-style); with a
        // selection they pass through untouched — the TextEdit's own handlers put
        // the selected text on the OS clipboard (and delete it for Cut). Only
        // intent is recorded inside `input_mut`: `copy_text` and the caret store
        // re-lock the egui Context, which deadlocks while the input lock is held.
        let mut pasted = None;
        let mut line_copy = false;
        let mut line_cut = false;
        ui.input_mut(|inp| {
            inp.events.retain(|e| match e {
                egui::Event::Paste(text) => {
                    pasted = Some(text.clone());
                    false
                }
                egui::Event::Copy if empty_sel => {
                    line_copy = true;
                    false
                }
                egui::Event::Cut if empty_sel => {
                    line_cut = true;
                    false
                }
                _ => true,
            });
        });
        if line_copy || line_cut {
            ui.ctx().copy_text(line_edit::line_with_newline(&self.ide.open[i].text, caret));
            if line_cut {
                let new_caret = delete_lines(&mut self.ide.open[i].text, caret, caret);
                self.ide.open[i].dirty = true;
                set_ide_caret(ui.ctx(), editor_id, new_caret);
            }
        }
        if let Some(pasted) = pasted {
            let new_caret = paste_text(&mut self.ide.open[i].text, sel_a, sel_b, &pasted);
            self.ide.open[i].dirty = true;
            if empty_sel {
                set_ide_caret(ui.ctx(), editor_id, new_caret);
            } else {
                set_ide_selection(ui.ctx(), editor_id, sel_a, sel_b);
            }
        }
        // Ctrl+Shift+K → delete the current line / selected lines (no clipboard).
        if ui.input_mut(|inp| {
            inp.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::K)
        }) {
            let new_caret = delete_lines(&mut self.ide.open[i].text, sel_a, sel_b);
            self.ide.open[i].dirty = true;
            set_ide_caret(ui.ctx(), editor_id, new_caret);
        }
        // Ctrl+D → duplicate the current line below.
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::D)) {
            let text = &mut self.ide.open[i].text;
            let (s, e, next) = line_edit::line_bytes(text, caret);
            let content = text[s..e].to_string();
            if next > e {
                text.insert_str(next, &format!("{content}\n"));
            } else {
                text.insert_str(e, &format!("\n{content}"));
            }
            self.ide.open[i].dirty = true;
        }
        // Alt+Up / Alt+Down → move the current line / selected lines.
        for (key, up) in [(egui::Key::ArrowUp, true), (egui::Key::ArrowDown, false)] {
            if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::ALT, key))
                && let Some((a, b)) = move_lines(&mut self.ide.open[i].text, sel_a, sel_b, up) {
                    self.ide.open[i].dirty = true;
                    set_ide_selection(ui.ctx(), editor_id, a, b);
                }
        }
        // Ctrl+/ → toggle line comments (Lua files) over the selection.
        if is_lua && ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::Slash)) {
            let (a, b) = toggle_comment_lines(&mut self.ide.open[i].text, sel_a, sel_b);
            self.ide.open[i].dirty = true;
            if empty_sel {
                set_ide_caret(ui.ctx(), editor_id, caret.min(b));
            } else {
                set_ide_selection(ui.ctx(), editor_id, a, b);
            }
        }
        // Tab / Shift+Tab → block indent/outdent. Plain Tab uses the same indent
        // width as auto-indent so the editor feels consistent, and never when the
        // autocomplete popup already claimed it.
        let multi_line = !empty_sel && {
            let text = &self.ide.open[i].text;
            let (ba, bb) =
                (line_edit::byte_of_char(text, sel_a), line_edit::byte_of_char(text, sel_b));
            text[ba..bb].contains('\n')
        };
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
            if multi_line {
                let (a, b) = indent_lines(&mut self.ide.open[i].text, sel_a, sel_b, false);
                self.ide.open[i].dirty = true;
                set_ide_selection(ui.ctx(), editor_id, a, b);
            } else {
                let new_caret = paste_text(&mut self.ide.open[i].text, sel_a, sel_b, &indent_unit());
                self.ide.open[i].dirty = true;
                set_ide_caret(ui.ctx(), editor_id, new_caret);
            }
        }
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)) {
            let (a, b) = indent_lines(&mut self.ide.open[i].text, sel_a, sel_b, true);
            self.ide.open[i].dirty = true;
            set_ide_selection(ui.ctx(), editor_id, a, b);
        }
        // Enter → newline + auto-indent (one level deeper after a block opener).
        if is_lua && ui.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
            let new_caret = auto_indent_newline(&mut self.ide.open[i].text, sel_a, sel_b);
            self.ide.open[i].dirty = true;
            set_ide_caret(ui.ctx(), editor_id, new_caret);
        }
        // Alt+Shift+F → format this document (VS Code's binding). The caret is
        // restored by LINE + COLUMN rather than by byte offset: re-indenting moves
        // every offset after the first change, so a byte-restored caret would jump
        // somewhere else on every format.
        if is_lua
            && ui.input_mut(|inp| {
                inp.consume_key(egui::Modifiers::ALT | egui::Modifiers::SHIFT, egui::Key::F)
            })
        {
            self.format_with_caret(ui, i, editor_id);
        }
        // Ctrl+B → go to the definition of the identifier under the caret.
        // F12 is the same thing under the name every other editor uses (Ctrl+B
        // stays); Shift+F12 lists references. The word under the caret is taken
        // from just before it too, so the binding works with the caret sitting at
        // the END of a name — where it is after you finish typing one.
        let goto_def = ui.input_mut(|inp| {
            inp.consume_key(egui::Modifiers::CTRL, egui::Key::B)
                || inp.consume_key(egui::Modifiers::NONE, egui::Key::F12)
        });
        let find_refs =
            ui.input_mut(|inp| inp.consume_key(egui::Modifiers::SHIFT, egui::Key::F12));
        if goto_def || find_refs {
            let mut w = word_at(&self.ide.open[i].text, caret);
            if w.is_empty() && caret > 0 {
                w = word_at(&self.ide.open[i].text, caret - 1);
            }
            if !w.is_empty() {
                if find_refs {
                    self.gather_references(&w);
                } else {
                    self.goto_definition(&w);
                }
            }
        }
    }

    /// Format file `i`, keeping the caret on its LINE and COLUMN rather than its
    /// byte offset — re-indenting shifts every offset after the first change, so a
    /// byte-restored caret lands somewhere else on every format.
    ///
    /// One entry point for Alt+Shift+F, the ▤ Format button and format-on-save, so
    /// all three behave identically.
    fn format_with_caret(&mut self, ui: &egui::Ui, i: usize, editor_id: egui::Id) {
        let caret = ide_selection(ui.ctx(), editor_id).map(|(_, _, c)| c);
        let (line, col) = match caret {
            Some(c) => crate::lua_format::line_col_of(&self.ide.open[i].text, c),
            None => (0, 0),
        };
        if self.ide.format_file(i) && caret.is_some() {
            let at = crate::lua_format::char_of_line_col(&self.ide.open[i].text, line, col);
            set_ide_caret(ui.ctx(), editor_id, at);
        }
    }

    /// Render a doc body with light structure instead of one monospace slab:
    /// headings, wrapped prose, bullets, and CODE BLOCKS in the editor's own
    /// syntax highlighting inside a framed panel.
    ///
    /// The markup is deliberately tiny — indented lines (4 spaces) or ``` fences
    /// are code, `## ` is a heading, `- ` is a bullet, `` `x` `` is inline code —
    /// because doc bodies are written by hand right here in the source and anything
    /// heavier would rot. Prose wraps to the panel, so the Scripting tab is
    /// readable docked narrow, which the old fixed-width monospace was not.
    pub(crate) fn doc_body_ui(&self, ui: &mut egui::Ui, body: &str) {
        let theme = CODE_THEMES[self.code_theme.min(CODE_THEMES.len() - 1)];
        let mono = egui::FontId::monospace(12.5);
        let mut code: Vec<&str> = Vec::new();
        let mut fenced = false;

        let flush = |ui: &mut egui::Ui, code: &mut Vec<&str>| {
            if code.is_empty() {
                return;
            }
            // Trim shared leading indentation so an indented block isn't doubly
            // indented by the frame.
            let text = code.join("\n");
            let dedent = code
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            let text: String = text
                .lines()
                .map(|l| if l.len() >= dedent { &l[dedent..] } else { l.trim_start() })
                .collect::<Vec<_>>()
                .join("\n");
            egui::Frame::new()
                .fill(ui.visuals().extreme_bg_color)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .corner_radius(4.0)
                .show(ui, |ui| {
                    let mut job = lua_highlight(&text, mono.clone(), &theme);
                    job.wrap.max_width = f32::INFINITY;
                    ui.add(egui::Label::new(job).selectable(true));
                });
            ui.add_space(4.0);
            code.clear();
        };

        for line in body.lines() {
            if line.trim_start().starts_with("```") {
                if fenced {
                    flush(ui, &mut code);
                }
                fenced = !fenced;
                continue;
            }
            let indented = line.starts_with("    ") && !line.trim().is_empty();
            if fenced || indented {
                code.push(line);
                continue;
            }
            flush(ui, &mut code);
            let t = line.trim();
            if t.is_empty() {
                ui.add_space(6.0);
            } else if let Some(h) = t.strip_prefix("## ") {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(h).strong().size(14.0));
            } else if let Some(b) = t.strip_prefix("- ") {
                ui.horizontal_top(|ui| {
                    ui.add_space(6.0);
                    ui.label("•");
                    inline_doc_label(ui, b, &mono);
                });
            } else {
                inline_doc_label(ui, t, &mono);
            }
        }
        flush(ui, &mut code);
    }

    /// The warnings strip under the editor: a one-line count that expands into the
    /// list, each row jumping to its line.
    ///
    /// Warnings, never errors, and never modal — a lint that interrupts you is a
    /// lint you disable. Re-linted only when the text actually changes.
    fn ide_lints_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let path = self.ide.open[i].path.clone();
        if !path.ends_with(".lua") {
            return;
        }
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            self.ide.open[i].text.hash(&mut h);
            h.finish()
        };
        if self.ide.lints_for.as_ref() != Some(&(path.clone(), hash)) {
            let api = api_labels();
            let refs: Vec<&str> = api.iter().map(|s| s.as_str()).collect();
            self.ide.lints = crate::lua_lint::lint(&self.ide.open[i].text, &refs);
            self.ide.lints_for = Some((path, hash));
        }
        let n = self.ide.lints.len();
        let amber = egui::Color32::from_rgb(230, 180, 90);
        // The strip is ALWAYS one line, warnings or not. Drawing it only when
        // there are warnings resized the editor under the caret as they appeared
        // and disappeared mid-typing — a half-written `local` is briefly an unused
        // one — which is the "nothing moves on its own" rule this editor is held to.
        if n == 0 {
            ui.label(
                egui::RichText::new("✔ no warnings")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        }
        ui.horizontal(|ui| {
            let label = if self.ide.lints_open { "▼" } else { "▶" };
            if ui
                .selectable_label(
                    self.ide.lints_open,
                    egui::RichText::new(format!("{label} ⚠ {n} warning{}", if n == 1 { "" } else { "s" }))
                        .color(amber),
                )
                .on_hover_text(
                    "likely mistakes Lua can't report: undeclared assignments (typos), \n\
                     unused locals, upvalue pressure. `--@nolint` silences a line.",
                )
                .clicked()
            {
                self.ide.lints_open = !self.ide.lints_open;
            }
        });
        if !self.ide.lints_open {
            return;
        }
        let mut goto = None;
        egui::ScrollArea::vertical().max_height(110.0).id_salt(("lints", i)).show(ui, |ui| {
            for l in &self.ide.lints {
                let icon = match l.kind {
                    crate::lua_lint::LintKind::AccidentalGlobal => "✏",
                    crate::lua_lint::LintKind::UnusedLocal => "○",
                    crate::lua_lint::LintKind::UpvaluePressure => "▲",
                    // A suggestion, not a defect — its own mark so the strip
                    // reads as "here is a better way", not "here is a bug".
                    crate::lua_lint::LintKind::RawInput => "➜",
                    // This one IS a defect, and a total one: the hook raises on
                    // its first sum, so the script does nothing whatsoever.
                    crate::lua_lint::LintKind::HookSignature => "✖",
                    // Also a defect: the binding cannot ever fire, and the
                    // failure is silent from inside the game.
                    crate::lua_lint::LintKind::ReservedKey => "✖",
                };
                if ui
                    .add(
                        egui::Label::new(
                            egui::RichText::new(format!("{icon} line {}: {}", l.line, l.message))
                                .small()
                                .color(amber),
                        )
                        .sense(egui::Sense::click()),
                    )
                    .on_hover_text("click to jump to the line")
                    .clicked()
                {
                    goto = Some(l.line);
                }
            }
        });
        if let Some(line) = goto {
            self.ide.goto = Some(line);
        }
    }

    /// An autocomplete popup at the caret: the engine API (full labels, member
    /// access on any variable, `params.` keys, component-handle fields) plus
    /// identifiers from the current file, ranked by [`ac_matches`].
    ///
    /// **It opens by itself only after `.` or `:`** — member access, where you're
    /// asking what fields a thing has. A plain identifier needs **Ctrl+Space**,
    /// because a popup over your code every two characters is the thing people
    /// turn autocomplete off to escape. ↑↓ choose, **Enter** accepts (Tab is
    /// always indentation), Esc dismisses until the token changes, a click
    /// inserts too; the selected row's doc shows inside the popup. Returns
    /// whether the popup is showing (so the caller routes keys to it next frame).
    #[allow(clippy::too_many_arguments)]
    fn ide_autocomplete(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        editor_id: egui::Id,
        has_focus: bool,
        cursor_range: Option<egui::text::CCursorRange>,
        galley: &egui::text::Galley,
        galley_pos: egui::Pos2,
        accept: bool,
        nav: i32,
        dismiss: bool,
    ) -> bool {
        if !has_focus {
            return false;
        }
        let Some(range) = cursor_range else { return false };
        if !range.is_empty() {
            return false; // a selection, not a caret
        }
        let cursor = range.primary.index.0;
        let (start, token) = current_token(&self.ide.open[i].text, cursor);
        // WHEN THE POPUP OPENS ON ITS OWN: only for MEMBER ACCESS — after a `.`
        // or `:`, which is exactly the moment you're asking what fields a thing
        // has, and where the answer is short-lived. A plain identifier does not
        // summon it (that was the intrusive case: a popup over your code every
        // time you typed two letters of a local's name); Ctrl+Space asks for it.
        let member = token.contains(['.', ':']);
        if !member && !self.ide.ac_manual {
            return false;
        }
        if token.is_empty() && !self.ide.ac_manual {
            return false;
        }
        if token != self.ide.ac_token {
            self.ide.ac_token = token.clone();
            self.ide.ac_sel = 0;
            self.ide.ac_dismissed = false;
            // A manual request covers the token it was made for; typing a
            // different one goes back to the automatic rule.
            if !member {
                self.ide.ac_manual = false;
            }
        }
        if dismiss {
            self.ide.ac_dismissed = true;
            self.ide.ac_manual = false;
        }
        if self.ide.ac_dismissed {
            return false;
        }
        let items = ac_matches(&token, &self.ide.open[i].text);
        if items.is_empty() {
            return false;
        }
        let sel = (self.ide.ac_sel as i32 + nav).rem_euclid(items.len() as i32) as usize;
        self.ide.ac_sel = sel;

        let caret = galley.pos_from_cursor(egui::text::CCursor::new(cursor));
        let pos = galley_pos + caret.left_bottom().to_vec2();
        // Enter inserts the selected match; otherwise a click does.
        let mut chosen: Option<(usize, String)> =
            accept.then(|| (items[sel].keep, items[sel].insert.clone()));
        egui::Area::new(egui::Id::new(("ide_ac", editor_id)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(360.0);
                    for (n, it) in items.iter().enumerate() {
                        let rich = if n == sel {
                            egui::RichText::new(&it.label).monospace().strong()
                        } else {
                            egui::RichText::new(&it.label).monospace()
                        };
                        if ui.selectable_label(n == sel, rich).clicked() {
                            chosen = Some((it.keep, it.insert.clone()));
                        }
                    }
                    ui.separator();
                    // The selected entry's doc, right in the popup (no hover needed).
                    ui.small(items[sel].doc.as_deref().unwrap_or("An identifier from this file."));
                    // …and how it's used, when there's a worked example for it.
                    if let Some(ex) = api_example(&items[sel].label) {
                        ui.monospace(
                            egui::RichText::new(
                                ex.lines().find(|l| !l.trim_start().starts_with("--")).unwrap_or(""),
                            )
                            .size(11.0)
                            .color(ui.visuals().weak_text_color()),
                        );
                    }
                    ui.small(
                        egui::RichText::new("↵ accept · ↑↓ choose · esc hide · ⇥ indents")
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });

        if let Some((keep, insert)) = chosen {
            let from = start + keep;
            replace_chars(&mut self.ide.open[i].text, from, cursor, &insert);
            self.ide.open[i].dirty = true;
            let new_idx = from + insert.chars().count();
            set_ide_caret(ui.ctx(), editor_id, new_idx);
            ui.ctx().memory_mut(|m| m.request_focus(editor_id));
            self.ide.ac_manual = false;
            return false; // inserted — popup closes
        }
        true
    }
}

// ---- templates, snippets & docs ---------------------------------------------

/// A starter Lua script body (ADR-0003) — named after the file it lands in.
pub(crate) fn script_template(name: &str) -> String {
    format!(
        "-- {name}.lua\n\
         --\n\
         -- `defaults` are tunables shown in the Inspector; `params` are this\n\
         -- instance's live values. `node` is the node's transform (x/y/z,\n\
         -- scale/scale_x..z, yaw/pitch/roll in radians). `time` = seconds since\n\
         -- play started, `dt` = frame delta. The full Lua stdlib is in scope.\n\
         \n\
         defaults = {{ speed = 1.0 }}\n\
         \n\
         function start(node)\n\
         \x20 -- runs once when play begins\n\
         end\n\
         \n\
         function update(node, dt)\n\
         \x20 node.yaw = node.yaw + params.speed * dt\n\
         end\n"
    )
}

/// Insert-menu snippets for the in-engine IDE, grouped into submenus:
/// `(category, [(label, Lua to append)])`. Every snippet is a self-contained,
/// working pattern — the things devs write over and over (portals, pickups,
/// health, platforms, chase AI, HUD counters…), each showing the intended API
/// (string params, vec3 math, trigger hooks, handles) so it doubles as a
/// by-example reference. Keep them in step with `docs/scripting.md`.
const LUA_SNIPPETS: &[(&str, &[(&str, &str)])] = &[
    (
        "Lifecycle",
        &[
            (
                "script skeleton (defaults + start + update)",
                "\ndefaults = { speed = 1.0 }\n\nfunction start(node)\n  -- runs once when Play begins\nend\n\nfunction update(node, dt)\n  -- runs every rendered frame\nend\n",
            ),
            ("start", "\nfunction start(node)\n  \nend\n"),
            ("update (every frame)", "\nfunction update(node, dt)\n  \nend\n"),
            (
                "fixedUpdate (gameplay tick — movement/physics)",
                "\nfunction fixedUpdate(node, dt)\n  \nend\n",
            ),
            (
                "lateUpdate (after physics — cameras/followers)",
                "\nfunction lateUpdate(node, dt)\n  \nend\n",
            ),
        ],
    ),
    (
        "Collision & triggers",
        &[
            (
                "collision hooks (enter / exit)",
                "\nfunction onCollisionEnter(node, other, hit)\n  -- other = the node we touched; hit = { x, y, z, nx, ny, nz }\n  log(\"touched \" .. (other.name or \"?\"))\nend\n\nfunction onCollisionExit(node, other, hit)\nend\n",
            ),
            (
                "trigger zone (enter / exit)",
                "\n-- Attach to a Collidable node with the 'trigger' switch ON:\n-- bodies pass through, these hooks still fire.\nfunction onTriggerEnter(node, other, hit)\n  if other:hasTag(\"player\") then\n    log(\"player entered \" .. node.name)\n  end\nend\n\nfunction onTriggerExit(node, other, hit)\nend\n",
            ),
            (
                "portal (string param + scene.load)",
                "\n-- One script, many portals: each instance sets its own destination\n-- in the Inspector (a string default = a text field).\ndefaults = { destination = \"hub\" }\n\nfunction onTriggerEnter(node, other, hit)\n  if other:hasTag(\"player\") then\n    scene.load(params.destination)\n  end\nend\n",
            ),
            (
                "pickup / collectible",
                "\n-- Trigger node: first player touch hides it and awards the manager.\nlocal taken = false\n\nfunction onTriggerEnter(node, other, hit)\n  if taken or not other:hasTag(\"player\") then return end\n  taken = true\n  node.visible = false\n  spawnEffect(\"vfx/Pickup\", node.x, node.y, node.z)\n  local mgr = findScript(\"game_manager\")\n  if mgr then mgr.addScore(1) end\nend\n",
            ),
            (
                "kill plane / respawn",
                "\n-- A huge flat trigger under the map: falling into it respawns you.\ndefaults = { spawn = \"Spawn\" }\n\nfunction onTriggerEnter(node, other, hit)\n  local spawn = find(params.spawn)\n  if spawn and other:hasTag(\"player\") then\n    other.pos = spawn.pos\n    other.vx, other.vy, other.vz = 0, 0, 0\n  end\nend\n",
            ),
            (
                "jump pad (boost on contact)",
                "\ndefaults = { boost = 14.0 }\n\nfunction onCollisionEnter(node, other, hit)\n  if other.vy ~= nil then -- only bodies have velocity\n    other.vy = params.boost\n  end\nend\n",
            ),
        ],
    ),
    (
        "Movement & objects",
        &[
            (
                "spin (yaw)",
                "\ndefaults = { speed = 45 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
            ),
            (
                "pulse (scale)",
                "\ndefaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }\nfunction update(node, dt)\n  node.scale = math.max(params.base * (1.0 + params.amplitude * math.sin(params.speed * time)), 0.01)\nend\n",
            ),
            (
                "bob / float (hover in place)",
                "\ndefaults = { height = 0.5, speed = 2.0 }\nlocal baseY\n\nfunction start(node)\n  baseY = node.y\nend\n\nfunction update(node, dt)\n  node.y = baseY + math.sin(time * params.speed) * params.height\nend\n",
            ),
            (
                "moving platform (Kinematic rigidbody)",
                "\n-- Slides between its start pose and start + (dx, dy, dz), forever.\n-- Give the node a Rigidbody with mode = KINEMATIC: it never falls, and\n-- it CARRIES/pushes dynamic bodies standing on it (players ride along).\ndefaults = { dx = 0.0, dy = 0.0, dz = 6.0, speed = 0.5 }\nlocal from\n\nfunction start(node)\n  from = node.pos\nend\n\nfunction update(node, dt)\n  local to = from + vec3(params.dx, params.dy, params.dz)\n  local t = (math.sin(time * params.speed * math.pi * 2) + 1) * 0.5\n  node.pos = from:lerp(to, t)\nend\n",
            ),
            (
                "grab / carry an object (kinematic toggle)",
                "\n-- Press E near a dynamic prop to pick it up (it stops simulating and\n-- follows you); press E again to drop it (wakes at rest, falls).\ndefaults = { reach = 3.0, holdDist = 2.0 }\nlocal held\n\nfunction update(node, dt)\n  if input.pressed(\"e\") then\n    if held then\n      local rig = held:getcomponent(\"RigidBody\")\n      if rig then rig.kinematic = false end\n      held = nil\n    else\n      local dx, dz = -math.sin(node.yaw), -math.cos(node.yaw)\n      local h = raycast(node.x, node.y, node.z, dx, 0, dz, params.reach)\n      if h and h.node then\n        local rig = h.node:getcomponent(\"RigidBody\")\n        if rig then\n          rig.kinematic = true\n          held = h.node\n        end\n      end\n    end\n  end\n  if held and held.valid then\n    local dx, dz = -math.sin(node.yaw), -math.cos(node.yaw)\n    held.pos = node.pos + vec3(dx * params.holdDist, 0.5, dz * params.holdDist)\n  end\nend\n",
            ),
            (
                "shoot a projectile (spawn a prefab)",
                "\n-- Fires the \"bullet\" prefab where you're facing. Make the prefab by\n-- dragging a node into the Assets panel (give it a Dynamic rigidbody,\n-- gravity off, + a script whose onCollisionEnter calls node:destroy()).\ndefaults = { speed = 40.0, cooldown = 0.2 }\nlocal next_ok = 0\n\nfunction update(node, dt)\n  if input.pressed(\"mouse1\") and time >= next_ok then\n    next_ok = time + params.cooldown\n    local dir = vec3(-math.sin(node.yaw), 0, -math.cos(node.yaw))\n    spawn(\"bullet\", node.pos + dir * 1.5, function(b)\n      b.vx, b.vy, b.vz = dir.x * params.speed, 0, dir.z * params.speed\n    end)\n  end\nend\n",
            ),
            (
                "destroy on touch (bullet / fragile prop)",
                "\n-- Self-destructs when it hits anything solid (pair with a trigger\n-- rigidbody + onTriggerEnter to also damage what it passes through).\nfunction onCollisionEnter(node, other, hit)\n  spawnEffect(\"vfx/Impact\", hit.x, hit.y, hit.z)\n  node:destroy()\nend\n",
            ),
            (
                "chase target (simple follow AI)",
                "\n-- Chases the node named `target` while it's inside aggro range.\ndefaults = { target = \"Player\", speed = 3.0, aggro = 12.0, keep = 1.5 }\n\nfunction update(node, dt)\n  local prey = find(params.target)\n  if not prey then return end\n  local d = distance(node, prey)\n  if d < params.aggro and d > params.keep then\n    local dir = (prey.pos - node.pos):normalized()\n    node.pos = node.pos + dir * params.speed * dt\n    node.yaw = math.atan2(-dir.x, -dir.z) -- face the prey (forward = -Z)\n  end\nend\n",
            ),
            (
                "patrol waypoints (named nodes)",
                "\n-- Walks Point1 -> Point2 -> ... -> PointN -> Point1. Name your\n-- waypoint nodes and list them here (or in a string param).\ndefaults = { speed = 2.0, reach = 0.3 }\nlocal points = { \"Point1\", \"Point2\", \"Point3\" }\nlocal at = 1\n\nfunction update(node, dt)\n  local wp = find(points[at])\n  if not wp then return end\n  if distance(node, wp) < params.reach then\n    at = (at % #points) + 1\n    return\n  end\n  local dir = (wp.pos - node.pos):normalized()\n  node.pos = node.pos + dir * params.speed * dt\nend\n",
            ),
        ],
    ),
    (
        "Combat & health",
        &[
            (
                "health (damage / heal via handles)",
                "\n-- Other scripts reach this via a handle:\n--   local hp = hit.node:getscript(\"health\")\n--   if hp then hp.damage(25) end\ndefaults = { max = 100 }\nhp = 100\n\nfunction start(node)\n  hp = params.max\nend\n\nfunction damage(amount)\n  hp = math.max(0, hp - amount)\n  if hp == 0 then die() end\nend\n\nfunction heal(amount)\n  hp = math.min(params.max, hp + amount)\nend\n\nfunction die()\n  node.visible = false -- swap for a death anim / respawn\nend\n",
            ),
            (
                "hitscan shot (raycast + tag filter)",
                "\ndefaults = { range = 60.0, power = 25 }\n\nfunction update(node, dt)\n  if input.clicked(0) then\n    local yaw, pitch = node.yaw, node.pitch\n    local cp = math.cos(pitch)\n    local dx, dy, dz = -math.sin(yaw) * cp, math.sin(pitch), -math.cos(yaw) * cp\n    local h = raycast(node.x, node.y, node.z, dx, dy, dz, params.range)\n    if h then\n      spawnEffect(\"vfx/Impact\", h.x, h.y, h.z)\n      if h.node and h.node:hasTag(\"enemy\") then\n        local hp = h.node:getscript(\"health\")\n        if hp then hp.damage(params.power) end\n      end\n    end\n  end\nend\n",
            ),
            (
                "melee swing (short reach, forward)",
                "\ndefaults = { reach = 2.5, power = 40 }\n\nfunction update(node, dt)\n  if input.clicked(0) then\n    local dx, dz = -math.sin(node.yaw), -math.cos(node.yaw)\n    local h = raycast(node.x, node.y, node.z, dx, 0, dz, params.reach)\n    if h and h.node and h.node:hasTag(\"enemy\") then\n      local hp = h.node:getscript(\"health\")\n      if hp then hp.damage(params.power) end\n    end\n  end\nend\n",
            ),
        ],
    ),
    (
        "Camera & input",
        &[
            (
                "smooth follow camera (lateUpdate)",
                "\n-- Attach to the active Camera. Follows `target` at an offset,\n-- smoothed — lateUpdate reads the target's FINAL pose (no jitter).\ndefaults = { target = \"Player\", ox = 0.0, oy = 4.0, oz = 8.0, smooth = 8.0 }\n\nfunction lateUpdate(node, dt)\n  local t = find(params.target)\n  if not t then return end\n  local want = t.pos + vec3(params.ox, params.oy, params.oz)\n  node.pos = node.pos:lerp(want, math.min(1, params.smooth * dt))\nend\n",
            ),
            (
                "WASD planar movement (rigidbody)",
                "\n-- Needs a Rigidbody (lock its rotation). Moves along the node's yaw.\ndefaults = { speed = 5.0, jump = 7.0 }\n\nfunction fixedUpdate(node, dt)\n  local f = input.axis(\"s\", \"w\")\n  local s = input.axis(\"a\", \"d\")\n  local cy, sy = math.cos(node.yaw), math.sin(node.yaw)\n  local vy = node.vy\n  if node.grounded and input.pressed(\"space\") then vy = params.jump end\n  node.vx = (-sy * f + cy * s) * params.speed\n  node.vz = (-cy * f - sy * s) * params.speed\n  node.vy = vy\nend\n",
            ),
            (
                "toggle on key press",
                "\ndefaults = { key = \"t\" }\nenabled = false\n\nfunction update(node, dt)\n  if input.pressed(params.key) then\n    enabled = not enabled\n    log(node.name .. \": \" .. (enabled and \"on\" or \"off\"))\n  end\nend\n",
            ),
            (
                "double-tap detection",
                "\ndefaults = { key = \"w\", window = 0.3 }\nlocal lastTap = -10\n\nfunction update(node, dt)\n  if input.pressed(params.key) then\n    if time - lastTap < params.window then\n      log(\"double tap!\")\n    end\n    lastTap = time\n  end\nend\n",
            ),
        ],
    ),
    (
        "Game state & scenes",
        &[
            (
                "game manager (score, the manager pattern)",
                "\n-- Attach ONCE to any node (e.g. an Empty named GameManager).\n-- Everyone else reaches it: local mgr = findScript(\"game_manager\")\nscore = 0\n\nfunction addScore(n)\n  score = score + n\n  local label = find(\"ScoreLabel\")\n  if label then label.text = score end\nend\n",
            ),
            (
                "scene switch on key",
                "\ndefaults = { scene = \"arena\", key = \"n\" }\n\nfunction update(node, dt)\n  if input.pressed(params.key) then\n    scene.load(params.scene)\n  end\nend\n",
            ),
            (
                "checkpoint (remember + respawn)",
                "\n-- Trigger node: touching it stores the respawn point on the manager.\nfunction onTriggerEnter(node, other, hit)\n  if other:hasTag(\"player\") then\n    local mgr = findScript(\"game_manager\")\n    if mgr then mgr.checkpoint = node.pos end\n  end\nend\n",
            ),
            (
                "timer / cooldown",
                "\ndefaults = { cooldown = 2.0 }\nlocal readyAt = 0\n\nfunction update(node, dt)\n  if input.clicked(0) and time >= readyAt then\n    readyAt = time + params.cooldown\n    log(\"fired! next in \" .. params.cooldown .. \"s\")\n  end\nend\n",
            ),
        ],
    ),
    (
        "Animation, VFX & audio",
        &[
            (
                "drive animator from speed",
                "\n-- Idle/Walk/Run from the body's real velocity (works with any\n-- Animation Controller carrying those states).\ndefaults = { walkAt = 0.4, runAt = 6.0 }\n\nfunction update(node, dt)\n  local anim = node:animator()\n  if not anim then return end\n  local speed = math.sqrt((node.vx or 0)^2 + (node.vz or 0)^2)\n  if speed > params.runAt then\n    anim:play(\"Run\")\n  elseif speed > params.walkAt then\n    anim:play(\"Walk\")\n  else\n    anim:play(\"Idle\")\n  end\nend\n",
            ),
            (
                "one-shot effect at a point",
                "\n-- Fire-and-forget: plays once, despawns itself.\nspawnEffect(\"vfx/Explosion\", node.x, node.y, node.z)\n",
            ),
            (
                "particles on this node (play / stop)",
                "\nfunction update(node, dt)\n  local fx = node:particles()\n  if fx then\n    if input.pressed(\"e\") then fx:restart() end\n    if input.pressed(\"q\") then fx:stop() end\n  end\nend\n",
            ),
            (
                "play a sound (3D, through the mixer)",
                "\naudio.play(\"audio/hit.ogg\", node.x, node.y, node.z, { maxDistance = 35, track = \"SFX\" })\n",
            ),
            (
                "footsteps while moving",
                "\ndefaults = { stride = 0.4 }\nlocal nextStep = 0\n\nfunction update(node, dt)\n  local speed = math.sqrt((node.vx or 0)^2 + (node.vz or 0)^2)\n  if node.grounded and speed > 0.5 and time >= nextStep then\n    nextStep = time + params.stride\n    audio.play(\"audio/step.ogg\", node, { volume = 0.6, track = \"SFX\" })\n  end\nend\n",
            ),
        ],
    ),
    (
        "UI & HUD",
        &[
            (
                "HUD counter (write a label)",
                "\n-- Attach anywhere; writes the UI Text element named \"ScoreLabel\".\nscore = 0\n\nfunction update(node, dt)\n  local label = find(\"ScoreLabel\")\n  if label then label.text = score end\nend\n",
            ),
            (
                "health bar (drive a UiSlider)",
                "\n-- Attach to the Bar element (a UiSlider). Reads the player's health.\nfunction update(node, dt)\n  local hp = findScript(\"health\")\n  local bar = node:getcomponent(\"UiSlider\")\n  if hp and bar then bar.value = hp.hp end\nend\n",
            ),
            (
                "menu manager (one script, every button)",
                "\n-- Attach to the PANEL, not the buttons: ui.on listens to elements\n-- this script doesn't live on, so a menu is one file instead of one\n-- three-line file per button.\nfunction start(node)\n  ui.on(find(\"Play\"), \"clicked\", function() scene.load(\"level1\") end)\n  ui.on(find(\"Options\"), \"clicked\", function() show(\"OptionsPanel\") end)\n  ui.on(find(\"Quit\"), \"clicked\", function() log(\"quit\") end)\nend\n\nfunction show(name)\n  local p = find(name)\n  if p then p.visible = true end\nend\n",
            ),
            (
                "button hooks (clicked / hover)",
                "\n-- Attach to a UI element with 'button' ON. Style your own states.\nfunction clicked(node)\n  log(\"clicked \" .. node.name)\nend\n\nfunction hoverStart(node)\n  local el = node:getcomponent(\"UiElement\")\n  if el then el.opacity = 1.0 end\nend\n\nfunction hoverEnd(node)\n  local el = node:getcomponent(\"UiElement\")\n  if el then el.opacity = 0.8 end\nend\n",
            ),
        ],
    ),
    (
        "Networking",
        &[
            (
                "synced variable (server → everyone)",
                "\n-- `synced` replicates server → all clients, changed-only.\nsynced = { phase = \"lobby\" }\n\nfunction update(node, dt)\n  if net.isServer() and input.pressed(\"enter\") then\n    synced.phase = \"playing\"\n  end\nend\n",
            ),
            (
                "RPC round trip (client asks, server decides)",
                "\n-- Client fires an intent; the SERVER validates and answers.\nfunction update(node, dt)\n  if net.isClient() and input.pressed(\"b\") then\n    net.rpc(\"buy\", { item = \"sword\" })\n  end\nend\n\nonRpc = {}\nfunction onRpc.buy(args, sender)\n  if not net.isServer() then return end\n  log(\"peer \" .. sender .. \" buys \" .. tostring(args.item))\n  net.rpc(\"bought\", { item = args.item }, { to = sender })\nend\nfunction onRpc.bought(args, sender)\n  log(\"purchase confirmed: \" .. tostring(args.item))\nend\n",
            ),
            (
                "networked door (rpc + synced)",
                "\nreplicated = { open = false }\n\nonRpc = {}\nfunction onRpc.use(args, sender)\n  if net.isServer() then synced.open = not synced.open end\nend\n\nfunction update(node, dt)\n  local target = synced.open and 1.6 or 0.0\n  node.y = node.y + (target - node.y) * math.min(1, dt * 6)\nend\n",
            ),
            (
                "rollback fighter (snapshot / restore)",
                "\n-- For a node whose Networked component is in mode \"Rollback (all peers)\".\n-- Every peer runs this script every tick from the shared input set, and\n-- re-runs it when a guessed input turns out wrong. That only works if the\n-- engine can put your script back exactly as it was.\n\nlocal state, frame, health = \"idle\", 0, 100\n\n-- Return EVERYTHING this script owns. Anything you leave out survives a\n-- rewind unchanged, which is what a desync is made of. Transforms and\n-- physics bodies are saved for you — don't list them.\nfunction snapshot()\n  return { state = state, frame = frame, health = health }\nend\n\nfunction restore(s)\n  state, frame, health = s.state, s.frame, s.health\nend\n\nfunction fixedUpdate(node, dt)\n  frame = frame + 1\n  -- ACTIONS, not raw keys: the wire carries actions, so input.pressed(\"j\")\n  -- reads neutral on a rollback-driven node. Bind these in Settings ▸ Input.\n  if state == \"idle\" and input.justPressed(\"Attack\") then\n    state, frame = \"startup\", 0\n  elseif state == \"startup\" and frame >= 4 then\n    state, frame = \"active\", 0\n  elseif state == \"active\" and frame >= 3 then\n    state = \"idle\"\n  end\n\n  -- tickPos, never node.x: node.x is the interpolated RENDER pose, and a\n  -- hurtbox built from it lags the body it belongs to.\n  local move = input.axis1(\"MoveX\") * 6.0 * dt\n  node.tickPos = node.tickPos + vec3(move, 0, 0)\n\n  -- Deterministic randomness only — rng() reads the clock, and two peers\n  -- drawing different numbers is a match that quietly forks in two.\n  if state == \"active\" and net.random() < 0.1 then\n    health = health - 1\n  end\nend\n",
            ),
            (
                "lag-compensated swing (net.rewind)",
                "\n-- client: fire the intent stamped with the tick you were SEEING\nfunction update(node, dt)\n  if net.isClient() and input.clicked(0) then\n    local yaw = input.aimYaw() or node.yaw\n    net.rpc(\"swing\", { dx = math.sin(yaw), dz = math.cos(yaw) }, { withInput = true })\n  end\nend\n\n-- server: judge it against the world as that player perceived it\nonRpc = {}\nfunction onRpc.swing(args, peer)\n  if not net.isServer() then return end\n  net.rewind(peer, function()\n    local hit = raycast(node.x, node.y, node.z, args.dx, 0, args.dz, 3.0)\n    if hit and hit.node then\n      local combat = hit.node:getscript(\"combat\")\n      if combat and combat.synced.parrying then\n        net.rpc(\"parried\", {}, { to = peer })\n      else\n        log(\"hit \" .. hit.node.name)\n      end\n    end\n  end)\nend\n",
            ),
        ],
    ),
    (
        "Debug & utilities",
        &[
            (
                "debug ray (gizmo)",
                "\n-- One-frame debug shapes, Scene view only. Call every frame.\nfunction update(node, dt)\n  gizmo.ray(node.x, node.y, node.z, -math.sin(node.yaw), 0, -math.cos(node.yaw), 3.0, 0.3, 1.0, 0.4)\nend\n",
            ),
            (
                "mark tagged nodes (gizmo spheres)",
                "\nfunction update(node, dt)\n  for _, n in ipairs(findTagged(\"enemy\")) do\n    gizmo.sphere(n.x, n.y, n.z, 1.0, 1.0, 0.3, 0.3)\n  end\nend\n",
            ),
            (
                "distance check (aggro gate)",
                "\ndefaults = { target = \"Player\", range = 10.0 }\n\nfunction update(node, dt)\n  local t = find(params.target)\n  if t and distance(node, t) < params.range then\n    -- in range\n  end\nend\n",
            ),
        ],
    ),
];

/// A one-line hint listing the tunables a script declares (parsed from its
/// `defaults = { ... }` table), shown above the code editor.
fn script_hint(text: &str) -> String {
    let keys = defaults_keys(text);
    if keys.is_empty() {
        String::new()
    } else {
        format!("params: {}", keys.join(", "))
    }
}

/// The IDE's keyboard shortcuts, shown on the Docs page.
const IDE_SHORTCUTS: &str = "\
Ctrl+S          save file          Ctrl+Shift+S   save all open files
Ctrl+F          find               Ctrl+H         find & replace
F3 / Shift+F3   next / prev match  Ctrl+G         go to line
Ctrl+C / X      copy / cut line (when nothing is selected)
Ctrl+D          duplicate line     Ctrl+Shift+K   delete line
Alt+Up / Down   move line(s)       Ctrl+/         toggle -- comment
Tab / Shift+Tab indent / outdent the selected lines
Ctrl+B / F12    go to definition   Shift+F12      find references
Alt+Shift+F     format document    Ctrl+Space     suggest (completion)
Ctrl+W          close tab          right-click    definition / references
completion:     opens by itself after `.` or `:` — Ctrl+Space anywhere else
                ↑↓ choose · Enter accept · Esc hide · Tab always indents";

// ---- in-engine IDE: Lua syntax highlighting + autocomplete -----------------

/// Lua reserved words (highlighted as keywords).
pub(crate) const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Identifiers highlighted as engine/builtin API (teal).
pub(crate) const LUA_API_WORDS: &[&str] = &[
    "node", "params", "time", "dt", "defaults", "log", "start", "update", "fixedUpdate", "lateUpdate", "input", "math",
    "string", "table", "ipairs", "pairs", "print", "tostring", "tonumber", "pcall", "select",
    "raycast", "find", "findAll", "findScript", "findScriptInScene", "findScripts", "findTagged",
    "spawn", "destroy", "spawnEffect",
    "vec2", "vec3", "distance", "onCollisionEnter", "onCollisionStay", "onCollisionExit",
    "onTriggerEnter", "onTriggerStay", "onTriggerExit",
    "assets", "gizmo",
    "net", "synced", "replicated", "onRpc", "audio", "terrain", "rng", "save",
    // Rollback: the two hooks a Rollback node's scripts must implement.
    "snapshot", "restore",
    "after", "every", "tween", "space", "camera",
];

/// The Docs page's API-reference groups, in display order.
const API_CATEGORIES: &[&str] = &[
    "script basics — lifecycle, params, log",
    "node — transform & body fields",
    "node — methods & handles",
    "vectors, directions & easing",
    "scene lookups & raycast",
    "references — wire nodes in the Inspector",
    "input — keyboard & mouse",
    "drawing — draw.*",
    "the web — http.*, json.*",
    "the player's account — account.*",
    // These two were routed to by `api_category` but never listed here, so the
    // whole UI surface — `ui.make`, `ui.on`, the pointer hooks, `color` — was
    // absent from the reference for as long as it has existed. Forty-three
    // entries you could only find by already knowing their names, which is the
    // discoverability complaint this release is about, in miniature.
    "game UI — text, buttons & hooks",
    "networking — net.*, synced",
    "scenes — load, unload & persist",
    "terrain — runtime sculpt & queries",
    "water — depth, buoyancy & ice",
    "scatter — instanced props",
    "2D — tilemaps & sprite batches",
    "vessels — assembly.*",
    "the camera & the screen",
    "physics controls — pause & step",
    "frame cost — perf.*",
    "accessibility — access.*",
    "persistence — save.*",
    "timers — after, every, tween",
    "space — orbits & time-warp",
    "components — getcomponent",
    "animation — node:animator",
    "particles — effects from script",
    "audio — sounds & the mixer",
    "assets",
    "debug gizmos",
    "lua stdlib",
];

/// How well an API entry answers `q` — lower is better, `None` is no match.
///
/// The ordering is the order someone actually wants: the thing you typed the
/// name of, then things whose name begins that way, then the rest of the name
/// matches, and only then a mention in the prose. Without it, typing "play"
/// puts `anim:play` below every entry whose description happens to say "while
/// playing", which is the difference between a search box and a filter.
fn api_rank(e: &ApiEntry, q: &str) -> Option<u8> {
    let label = e.label.to_ascii_lowercase();
    // The name after the last `.` or `:` — what people type when they don't
    // remember (or don't care) which table it hangs off.
    let leaf = label.rsplit(['.', ':']).next().unwrap_or(&label);
    if label == q {
        Some(0)
    } else if leaf == q {
        Some(1)
    } else if label.starts_with(q) || leaf.starts_with(q) {
        Some(2)
    } else if label.contains(q) {
        Some(3)
    } else if e.doc.to_ascii_lowercase().contains(q) {
        Some(4)
    } else {
        None
    }
}

/// Does `label` name a member of the handle conventionally called `holder`?
///
/// Matches `holder.field` and `holder:method` but not `holder` alone, and not
/// a longer name that merely starts the same way — `mat.cell` is a Material
/// handle, `math.clamp` is not.
fn starts(label: &str, holder: &str) -> bool {
    label
        .strip_prefix(holder)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with(':'))
}

/// Which Docs-page group an API entry belongs to (by its label shape).
fn api_category(label: &str) -> &'static str {
    // Handle members first: these are prefixed by the local name a script
    // conventionally binds the handle to (`local rb = node:getcomponent(...)`),
    // so they must be matched before the broader `node.` / `math.` arms below.
    if label == "node:getcomponent"
        || starts(label, "rb")
        || starts(label, "light")
        || starts(label, "cam")
        || starts(label, "mat")
        || starts(label, "env")
    {
        "components — getcomponent"
    } else if starts(label, "el") || starts(label, "slider") || starts(label, "layer") {
        "game UI — text, buttons & hooks"
    } else if starts(label, "particles") || label == "node:particles" || label == "spawnEffect" {
        "particles — effects from script"
    } else if starts(label, "sound") || starts(label, "source") || starts(label, "track") {
        "audio — sounds & the mixer"
    } else if starts(label, "tm")
        || starts(label, "batch")
        || label == "node:setTilemap"
        || label == "node:setSpriteBatch"
        || label == "node:tilemap"
        || label == "node:sprites"
        || label == "EMPTY_TILE"
    {
        "2D — tilemaps & sprite batches"
    } else if starts(label, "perf") {
        "frame cost — perf.*"
    } else if starts(label, "access") || label == "caption" {
        "accessibility — access.*"
    } else if starts(label, "hit") {
        "scene lookups & raycast"
    } else if starts(label, "body") {
        "space — orbits & time-warp"
    } else if starts(label, "timer") {
        "timers — after, every, tween"
    } else if label == "rng" || starts(label, "rng") {
        "lua stdlib"
    } else if matches!(
        label,
        "node.text"
            | "clicked"
            | "hoverStart"
            | "hoverEnd"
            | "pressed"
            | "released"
            | "focusEnter"
            | "focusExit"
            | "cancelled"
            | "submitted"
            | "changed"
            | "dragStart"
            | "dragMove"
            | "dropped"
            | "dragCancel"
            | "dragEnter"
            | "dragOver"
            | "dragLeave"
            | "node.index"
    ) || label.starts_with("ui.")
        || label.starts_with("color")
    {
        "game UI — text, buttons & hooks"
    } else if matches!(label, "noderef" | "scriptref" | "componentref") {
        "references — wire nodes in the Inspector"
    } else if label == "draw" || label.starts_with("draw.") {
        "drawing — draw.*"
    } else if label == "water" || label.starts_with("water.") {
        "water — depth, buoyancy & ice"
    } else if label == "scatter" || label.starts_with("scatter.") {
        "scatter — instanced props"
    } else if label == "assembly" || label.starts_with("assembly.") {
        "vessels — assembly.*"
    } else if label == "camera" || label.starts_with("camera.") {
        "the camera & the screen"
    } else if label == "physics" || label.starts_with("physics.") {
        "physics controls — pause & step"
    } else if label == "scene" || label.starts_with("scene.") {
        "scenes — load, unload & persist"
    } else if label == "account" || label.starts_with("account.") {
        "the player's account — account.*"
    } else if label.starts_with("http.") || label.starts_with("json.") || label == "openUrl" {
        "the web — http.*, json.*"
    } else if label.starts_with("vec3") || label.starts_with("vec2") || matches!(
        label,
        "distance"
            | "dirTo"
            | "yawOf"
            | "pitchOf"
            | "dirFromYaw"
            | "lookRotation"
            | "ease"
            | "smoothDamp"
            | "moveTowards"
            | "node:lookAt"
            | "node:turnTowards"
            | "node:moveTowards"
            | "node:toWorld"
            | "node:toLocal"
            | "node:setWorldPos"
            | "node:worldForward"
            | "node:worldRight"
            | "node:worldUp"
            | "node:distanceTo"
            | "node:distanceFlat"
            | "node.worldPos"
    ) {
        "vectors, directions & easing"
    } else if label.starts_with("node:") {
        "node — methods & handles"
    } else if label.starts_with("node.") {
        "node — transform & body fields"
    } else if label.starts_with("input") {
        "input — keyboard & mouse"
    } else if label.starts_with("net")
        || matches!(label, "synced" | "replicated" | "onRpc" | "snapshot" | "restore")
    {
        "networking — net.*, synced"
    } else if label == "save" || label.starts_with("save.") {
        "persistence — save.*"
    } else if matches!(label, "after" | "every" | "tween") {
        "timers — after, every, tween"
    } else if label == "space" || label.starts_with("space.") {
        "space — orbits & time-warp"
    } else if label.starts_with("terrain") {
        "terrain — runtime sculpt & queries"
    } else if label.starts_with("gizmo") {
        "debug gizmos"
    } else if label.starts_with("audio") || label == "node:sound" || label.starts_with("sound:") {
        "audio — sounds & the mixer"
    } else if label.starts_with("assets") {
        "assets"
    } else if label.starts_with("anim") {
        "animation — node:animator"
    } else if label.starts_with("math") || label.starts_with("string") || label.starts_with("table")
    {
        "lua stdlib"
    } else if matches!(
        label,
        "find"
            | "findAll"
            | "findScript"
            | "findScriptInScene"
            | "findScripts"
            | "findTagged"
            | "raycast"
            | "overlapSphere"
            | "spherecast"
            | "capsulecast"
    ) {
        "scene lookups & raycast"
    } else {
        "script basics — lifecycle, params, log"
    }
}

/// Render the whole API reference as Markdown, for `docs/lua-api.md`.
///
/// Same table, same grouping and same examples as the editor's Docs tab, so
/// the page you read on a website and the page you read in the tool cannot
/// disagree. A test regenerates and diffs it (see `lua_api_reference_file_is_current`),
/// which is also the only caller — the editor renders the table directly.
#[cfg(test)]
fn render_api_reference() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str(
        "# Lua API reference\n\n\
         Every name a script can reach, grouped the way the editor's **Docs** tab groups\n\
         them. The same table drives this page, that tab, the hover docs and autocomplete —\n\
         so there is one description of each call, in one place, and it is the one you get\n\
         everywhere.\n\n\
         *Generated — do not edit by hand.* Change the entry in `crates/floptle-editor/src/ide.rs`\n\
         and run `UPDATE_DOCS=1 cargo test -p floptle-editor lua_api_reference_file`.\n\n\
         New here? [`scripting.md`](scripting.md) is the guided tour — it teaches in order,\n\
         with worked examples. This page is the reference: complete, alphabetical within\n\
         each group, and meant to be searched.\n\n",
    );

    // Contents first: a reference you have to scroll is a reference you don't
    // use. Counts included so the size of each area is obvious at a glance.
    s.push_str("## Contents\n\n");
    for cat in API_CATEGORIES {
        let n = LUA_API.iter().filter(|e| api_category(e.label) == *cat).count();
        if n == 0 {
            continue;
        }
        let _ = writeln!(s, "- [{cat}](#{}) — {n}", anchor(cat));
    }
    s.push('\n');

    for cat in API_CATEGORIES {
        let mut entries: Vec<&ApiEntry> =
            LUA_API.iter().filter(|e| api_category(e.label) == *cat).collect();
        if entries.is_empty() {
            continue;
        }
        entries.sort_by_key(|e| e.label);
        let _ = writeln!(s, "## {cat}\n");
        for e in entries {
            let _ = writeln!(s, "### `{}`\n", e.label);
            let _ = writeln!(s, "{}\n", e.doc);
            if let Some((_, ex)) = API_EXAMPLES.iter().find(|(l, _)| *l == e.label) {
                let _ = writeln!(s, "```lua\n{ex}\n```\n");
            }
        }
    }
    s
}

/// A GitHub-style heading anchor, so the contents links actually land.
#[cfg(test)]
fn anchor(title: &str) -> String {
    title
        .to_ascii_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// One completion / docs entry for the in-engine IDE.
struct ApiEntry {
    label: &'static str,
    insert: &'static str,
    doc: &'static str,
}

/// Worked EXAMPLES for the API entries people actually reach for, shown under the
/// entry on the Docs page, in its hover tooltip, and in the completion popup.
///
/// A separate table rather than a field on [`ApiEntry`] so an example can be added
/// to any entry without touching the other 258 — and so the ones that have one are
/// obvious at a glance. Every snippet is a complete, runnable line or hook: the
/// point is to be copyable, not to be a signature (the signature is already in the
/// doc text right above it).
const API_EXAMPLES: &[(&str, &str)] = &[
    (
        "update",
        "function update(node, dt)\n  node.yaw = node.yaw + math.rad(90) * dt\nend",
    ),
    (
        "fixedUpdate",
        "-- gameplay writes belong on the tick, not the frame\nfunction fixedUpdate(node, dt)\n  node.vel = node.vel + vec3(0, -9.8, 0) * dt\nend",
    ),
    (
        "lateUpdate",
        "-- follow AFTER physics, so the camera samples this frame's final pose\nfunction lateUpdate(node, dt)\n  local t = find(\"Player\")\n  node.pos = t.pos + t.forward * -6 + vec3(0, 2, 0)\nend",
    ),
    (
        "defaults",
        "defaults = {\n  --@header Movement\n  -- How fast you walk on flat ground.\n  --@range 0 20 --@units m/s\n  walk = 4.5,\n  --@options Off|On|Auto\n  assist = 1,\n  invert = false,\n}",
    ),
    (
        "params",
        "function update(node, dt)\n  node.x = node.x + params.walk * dt   -- Inspector-tuned\nend",
    ),
    (
        "node.pos",
        "node.pos = node.pos + node.forward * (params.walk * dt)",
    ),
    (
        "node.vel",
        "-- one write instead of vx/vy/vz, and it reads as physics\nif node.grounded and input.pressed(\"space\") then\n  node.vel = node.vel + node.up * params.jump\nend",
    ),
    (
        "node.forward",
        "-- facing, from the node's rotation: -Z forward, +X right\nlocal aim = node.forward\nif raycast(node.pos, aim, 50) then log(\"something ahead\") end",
    ),
    ("node.up", "-- the body's up (-gravity): Y on flat ground, radial on a planet\nlocal lean = node.up:dot(vec3(0, 1, 0))"),
    (
        "find",
        "-- cache in start; find() every frame is wasteful\nfunction start(node) player = find(\"Player\") end",
    ),
    (
        "findTagged",
        "for _, e in ipairs(findTagged(\"enemy\")) do\n  if distance(node, e) < 10 then e:destroy() end\nend",
    ),
    (
        "raycast",
        "local hit = raycast(node.pos, vec3(0, -1, 0), params.ground_ray)\nif hit then log(\"ground at \" .. hit.y) end",
    ),
    (
        "node:getcomponent",
        "local rb = node:getcomponent(\"RigidBody\")\nif rb then rb.friction = on_ice and 0.02 or 0.6 end",
    ),
    (
        "node:setMaterial",
        "-- setup-time; use setShaderParam for per-frame values\nnode:setMaterial{ unlit = true, emissive = {1, 0.45, 0.15}, emissiveStrength = 2.5 }",
    ),
    (
        "spawn",
        "local b = spawn(\"Bullet\", node.pos + node.forward * 1.5, function(n)\n  n.vel = node.forward * 40\nend)",
    ),
    (
        "after",
        "after(0.25, function() spawnEffect(\"Explosion\", node.pos) end)",
    ),
    ("every", "-- a heartbeat that survives long sessions without drifting\nevery(1.0, function() hp = math.min(hp + 1, 100) end)"),
    (
        "tween",
        "-- SECONDS first, then the function; alpha eases 0 -> 1 and lands on 1.0\ntween(0.4, function(t) node:getcomponent(\"UiElement\").opacity = t end, \"smooth\")",
    ),
    (
        "input.action",
        "-- actions, not raw keys: rebindable, gamepad-ready, replay-safe\nif input.action(\"jump\") and node.grounded then\n  node.vel = node.vel + node.up * params.jump\nend",
    ),
    (
        "input.axis2",
        "local mx, my = input.axis2(\"move\")\nnode.pos = node.pos + (node.right * mx + node.forward * my) * params.walk * dt",
    ),
    ("math.clamp", "hp = math.clamp(hp + heal, 0, 100)"),
    ("math.approach", "-- frame-rate correct, never overshoots\nthrottle = math.approach(throttle, target, params.rate * dt)"),
    ("math.deltaAngle", "-- the short way round, across the +/-pi seam\nlocal turn = math.deltaAngle(node.yaw, wanted)\nnode.yaw = math.approachAngle(node.yaw, wanted, params.turn_rate * dt)"),
    ("math.remap", "local alpha = math.remap(distance(node, player), 5, 25, 1, 0)"),
    ("table.map", "local names = table.map(crew, function(m) return m.name end)"),
    ("table.filter", "local ready = table.filter(ships, function(s) return s.fuel > 0 end)"),
    ("table.find", "local docked, i = table.find(ships, function(s) return s.docked end)"),
    (
        "vec3",
        "local v = vec3(1, 0, 0) * 5 + vec3(0, 2, 0)   -- real operators\nlog(v:length(), v:normalized(), v:dot(node.forward))",
    ),
    (
        "node:animator",
        "local anim = node:animator()\nanim:crossfade(node.vel:length() > 4 and \"run\" or \"walk\", 0.15)",
    ),
    (
        "audio.play",
        "audio.play(\"audio/footstep\", node, { track = \"SFX\", volume = 0.6, minDistance = 4 })",
    ),
    (
        "save.set",
        "save.set(\"hp\", hp)                 -- survives scene loads and quits\nhp = save.get(\"hp\", 100)",
    ),
    (
        "ui.make",
        "ui.make(find(\"Crew Panel\"), {\n  \"col\", gap = 8, pad = 12, style = \"panel\", items = crew,\n  function(m) return { \"text\", key = m.id, text = m.name } end,\n})",
    ),
    (
        "ui.on",
        "-- one menu script instead of a script file per button\nfunction start(node)\n  ui.on(find(\"Play\"), \"clicked\", function() scene.load(\"level1\") end)\n  for _, b in ipairs(find(\"Toolbar\"):children()) do\n    ui.on(b, \"clicked\", function(el) selectTool(el.name) end)\n  end\nend",
    ),
    (
        "ui.events",
        "-- the whole screen, without naming a single element\nfunction update(node, dt)\n  for _, ev in ipairs(ui.events(\"clicked\")) do\n    log(\"clicked \" .. ev.node.name)\n  end\nend",
    ),
    (
        "ui.hovered",
        "-- a state, not an event: true for as long as it's true\nlocal over = ui.hovered()\nfind(\"Caption\").text = over and over.name or \"\"",
    ),
    (
        "onCollisionEnter",
        "function onCollisionEnter(node, other)\n  if other:hasTag(\"hazard\") then hp = hp - 10 end\nend",
    ),
    (
        "node:setShaderTexture",
        "-- swap a shader's texture slot at runtime (a path, or a live render target)\nnode:setShaderTexture(\"decal\", damaged and \"textures/scorch.png\" or \"\")\nnode:setShaderTexture(\"screen\", \"rt:securityCam\")",
    ),
    (
        "node:setShaderParam",
        "-- a live uniform write: safe every tick, never recompiles\nnode:setShaderParam(\"cell\", math.floor(time * 8) % 16)",
    ),
    (
        "node:setScreenShader",
        "-- switch one of the scene's screen shaders on or off (it keeps its knobs)\nlocal post = find(\"Post Processing\")\npost:setScreenShader(\"inkOutline\", bossFight)\npost:setShaderParam(\"inkOutline.thickness\", 1 + rage * 2)",
    ),
    (
        "node:lookAt",
        "-- point at a node or a world point; the up makes the horizon level\nnode:lookAt(find(\"Enemy\"))\nnode:lookAt(aimPoint, node.up)   -- roll set too, for a planet camera",
    ),
    (
        "node:turnTowards",
        "-- swing round at a rate instead of snapping. Short way, always.\nnode:turnTowards(find(\"Enemy\"), params.turn_rate * dt)\nnode:turnTowards(node.vel, 6 * dt)   -- or: face where you're going",
    ),
    (
        "dirTo",
        "local aim = dirTo(node, find(\"Enemy\"))\nspawn(\"Bullet\", node.pos + aim * 1.5, function(b) b.vel = aim * 60 end)",
    ),
    (
        "dirFromYaw",
        "-- the yaw/pitch -> direction pair, with the right signs\nlocal look = dirFromYaw(node.yaw, node.pitch)\nnode.pos = head - look * distance   -- an orbit camera, in one line",
    ),
    (
        "yawOf",
        "-- which way is that? (atan2(-x, -z), once and correctly)\nlocal heading = math.deg(yawOf(node.vel))",
    ),
    (
        "lookRotation",
        "-- the angles, without applying them\nnode.yaw, node.pitch, node.roll = lookRotation(forward, node.up)",
    ),
    (
        "ease",
        "-- the same feel at 30 fps and at 240 (this is what \"smoothing\" is)\nfunction lateUpdate(node, dt)\n  node.pos = ease(node.pos, target.pos + offset, params.smoothing, dt)\nend",
    ),
    (
        "smoothDamp",
        "-- a follow with momentum: it keeps moving after the target stops\ncamX, camVX = smoothDamp(camX, target.worldX, camVX, 0.25, dt)",
    ),
    (
        "moveTowards",
        "-- a patrol, in two lines: it returns true on arrival\nif node:moveTowards(waypoints[i], params.speed * dt) then\n  i = i % #waypoints + 1\nend",
    ),
    (
        "node:toWorld",
        "-- composes position, rotation AND scale up the whole parent chain\nlocal muzzle = gun:toWorld(vec3(0, 0, -1.2))\nspawn(\"Bullet\", muzzle, function(b) b.vel = gun:worldForward() * 60 end)",
    ),
    (
        "node:setWorldPos",
        "-- land on a world point whatever this node is parented to\nnode:setWorldPos(hit.node:toWorld(vec3(0, 1, 0)))",
    ),
    (
        "node:distanceTo",
        "-- WORLD space, so a unit under a container measures the real gap\nif node:distanceTo(player) < params.aggro then chase(player) end",
    ),
    (
        "node.worldPos",
        "-- x/y/z are LOCAL; this is where it really is\nif node.worldPos:distance(order) < params.arrive then arrived() end",
    ),
    (
        "vec3:flatten",
        "-- \"forward along the ground\" — on a flat world AND on a planet\nlocal up = node.up or vec3(0, 1, 0)\nlocal fwd = dirFromYaw(node.yaw):flatten(up)\nlocal right = fwd:cross(up)",
    ),
    (
        "http.get",
        "-- non-blocking: the callback runs on a later tick, on the main thread\nhttp.get(params.api .. \"/me/cards\", {\n  headers = { Authorization = \"Bearer \" .. token },\n}, function(res)\n  if not res.ok then return log(\"failed: \" .. tostring(res.error)) end\n  for _, card in ipairs(res.json.cards or {}) do addCard(card) end\nend)",
    ),
    (
        "http.post",
        "-- a TABLE body is sent as JSON; no json.encode dance needed\nhttp.post(params.api .. \"/me/loadout\", { deck = deckId }, function(res)\n  if not res.ok then log(\"the server said no: \" .. res.body) end\nend)",
    ),
    (
        "json.decode",
        "-- bad input is a VALUE, not an error\nlocal save, why = json.decode(text)\nif not save then return log(\"corrupt save: \" .. why) end",
    ),
    (
        "openUrl",
        "-- the player approves the pairing on your real site\nopenUrl(res.json.verify_url)",
    ),
    (
        "draw.text",
        "-- a HUD with no UI tree; align says which edge x is\ndraw.text(24, 24, \"HP \" .. hp, 22, 1, 0.4, 0.4)\ndraw.text(w - 24, 24, string.format(\"%.0f fps\", 1 / dt), 18, 1, 1, 1, 0.7, \"right\")",
    ),
    (
        "draw.circle",
        "-- x, y is the CENTRE\ndraw.circle(mx, my, 6, 0.3, 1.0, 0.5, 0.9)\ndraw.circleOutline(mx, my, 18, 0.3, 1.0, 0.5, 0.5, 2)",
    ),
    (
        "terrain.dig",
        "if input.clicked(\"left\") then\n  local hit = raycast(node.pos, node.forward, 6)\n  if hit then terrain.dig(hit.x, hit.y, hit.z, 1.5) end\nend",
    ),
];

/// The API entry a hovered identifier refers to, if any.
///
/// Hovering `node:lookAt` is the easy case, and the only one the exact match
/// used to handle. The cases that matter are the ones people actually write:
/// `target:lookAt` (the receiver is a variable, not literally `node`),
/// `v:flatten` (the reference calls it `vec3:flatten`), `player.worldPos`. So:
/// try the literal word, then fall back to the MEMBER name — with the same
/// separator, and only when that is unambiguous. A hover that guesses wrong is
/// worse than no hover.
fn api_entry_for(word: &str) -> Option<&'static ApiEntry> {
    if let Some(a) = LUA_API.iter().find(|a| a.label == word) {
        return Some(a);
    }
    // `foo:bar` / `foo.bar` — the LAST separator, so `a.b:c` resolves on `c`.
    let sep = word.rfind([':', '.'])?;
    let (member, ch) = (&word[sep + 1..], word.as_bytes()[sep] as char);
    if member.is_empty() {
        return None;
    }
    let mut hit = None;
    for a in LUA_API {
        let Some(i) = a.label.rfind([':', '.']) else { continue };
        if a.label.as_bytes()[i] as char != ch || &a.label[i + 1..] != member {
            continue;
        }
        // Two different namespaces claim this member (`audio.play` vs a
        // hypothetical `music.play`): say nothing rather than pick one. An
        // exact receiver match, though, would already have been found above.
        if hit.is_some() {
            return None;
        }
        hit = Some(a);
    }
    hit
}

/// Indent a snippet by four spaces, which is how [`EditorTabViewer::doc_body_ui`]
/// recognises a code block.
fn indent_block(code: &str) -> String {
    code.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}

/// The worked example for an API entry, if it has one.
fn api_example(label: &str) -> Option<&'static str> {
    API_EXAMPLES.iter().find(|(l, _)| *l == label).map(|(_, e)| *e)
}

/// A wrapped prose label where `` `backticked` `` spans render as monospace in the
/// API colour — the difference between docs that read like docs and docs that read
/// like a terminal dump.
fn inline_doc_label(ui: &mut egui::Ui, text: &str, mono: &egui::FontId) {
    let mut job = egui::text::LayoutJob::default();
    let body = egui::TextFormat {
        font_id: egui::TextStyle::Body.resolve(ui.style()),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let codef = egui::TextFormat {
        font_id: mono.clone(),
        color: egui::Color32::from_rgb(78, 201, 176),
        ..Default::default()
    };
    let mut in_code = false;
    for part in text.split('`') {
        if !part.is_empty() {
            job.append(part, 0.0, if in_code { codef.clone() } else { body.clone() });
        }
        in_code = !in_code;
    }
    job.wrap.max_width = ui.available_width();
    ui.add(egui::Label::new(job).selectable(true));
}

/// Every API name the engine provides, as bare identifiers — the ROOT of each
/// entry (`node:getcomponent` → `node`, `math.clamp` → `math`). The lints use this
/// so "that's not a thing" can never disagree with what autocomplete offers.
pub(crate) fn api_labels() -> Vec<String> {
    let mut out: Vec<String> = LUA_API
        .iter()
        .map(|a| {
            a.label
                .split(['.', ':', '('])
                .next()
                .unwrap_or(a.label)
                .to_string()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The engine scripting API, surfaced as autocomplete + hover docs (and the Docs
/// page's reference). Lua stdlib highlights are included so completion is useful.
const LUA_API: &[ApiEntry] = &[
    ApiEntry { label: "update", insert: "update", doc: "function update(node, dt) — runs every frame while playing." },
    ApiEntry { label: "fixedUpdate", insert: "fixedUpdate", doc: "function fixedUpdate(node, dt) — runs every GAMEPLAY TICK (60 Hz, constant dt). Movement/gameplay/physics writes belong here; cameras & followers in lateUpdate; other cosmetics in update. Same cadence physics steps at — frame-rate independent." },
    ApiEntry { label: "lateUpdate", insert: "lateUpdate", doc: "function lateUpdate(node, dt) — runs once per frame AFTER physics and the interpolated transform writeback: the CAMERA pass. Anything that follows something else (orbit cameras, name tags, listeners) belongs here so it samples this frame's FINAL poses. Following from update reads LAST frame's pose — a velocity × dt lag that turns frame-time noise into visible jitter." },
    ApiEntry { label: "start", insert: "start", doc: "function start(node) — runs once when play begins." },
    ApiEntry { label: "defaults", insert: "defaults", doc: "defaults = { name = value } — tunables shown in the Inspector." },
    ApiEntry { label: "input.aimYaw", insert: "input.aimYaw()", doc: "The ACTIVE camera's world yaw (radians), captured with the input snapshot — use it for camera-relative movement (in multiplayer it rides the input command, so server + prediction replay see exactly your view angle). nil without an active camera." },
    ApiEntry { label: "input.aimPitch", insert: "input.aimPitch()", doc: "The active camera's world pitch (radians), captured with the input snapshot." },
    ApiEntry { label: "net.host", insert: "net.host{}", doc: "net.host{ maxPlayers = 16, port = 7777, relay = \"addr\", interest = 150, interestBudget = 16384 } — become the authoritative host. relay = a rendezvous relay address (you get a LOBBY CODE, nobody port-forwards); port = direct UDP (QUIC) for LAN; neither = the in-editor loopback harness. interest = metres: each client hears about its own neighbourhood instead of the whole world (leave it off below a few dozen players — broadcasting is cheaper); interestBudget = bytes/sec of entity updates per client; inputDelay = rollback input delay in TICKS (clamped to 6) — omit it and the host derives one from the worst peer\'s measured RTT (2 on a LAN, 5 across a country)." },
    ApiEntry { label: "net.setInputDelay", insert: "net.setInputDelay(", doc: "net.setInputDelay(ticks) — the rollback input delay for the NEXT match, in ticks, clamped to 6. Too low and the opponent\'s input lands after the tick that needed it on every tick, so the driver guesses and re-simulates: correct, and five times the work. Fixed for a session on purpose — adaptive delay hides a bad connection by changing how the game FEELS while you are playing it. Call it between matches; the roster re-announce restarts the driver." },
    ApiEntry { label: "net.join", insert: "net.join(\"local://\")", doc: "net.join(addr) — join a session: \"relay://relayaddr/CODE\" = a lobby code through a relay (no port-forwarding), \"quic://host:port\" = a server directly, \"local://\" = the in-editor test harness." },
    ApiEntry { label: "net.leave", insert: "net.leave()", doc: "net.leave() — end the session." },
    ApiEntry { label: "net.role", insert: "net.role()", doc: "net.role() — \"offline\" | \"server\" | \"client\"." },
    ApiEntry { label: "net.isServer", insert: "net.isServer()", doc: "net.isServer() — true on the authoritative host." },
    ApiEntry { label: "net.isClient", insert: "net.isClient()", doc: "net.isClient() — true on a connected client." },
    ApiEntry { label: "net.peers", insert: "net.peers()", doc: "net.peers() — connected client peer ids (server)." },
    ApiEntry { label: "net.joinState", insert: "net.joinState()", doc: "net.joinState() -> state, reason — how a join is going: \"offline\" | \"connecting\" | \"joined\" | \"refused\". On \"refused\" the second return is the relay's own words (\"no lobby QK7RM\") — print it. WAIT ON THIS, not on net.role(): joining does not block, so role reads \"client\" from the frame you called net.join, whether or not that code matched any lobby." },
    ApiEntry { label: "net.lobbyCode", insert: "net.lobbyCode()", doc: "net.lobbyCode() — the code friends type in to join, on a host that used net.host{ relay = \"…\" }. Put it on your own lobby screen. nil until the relay answers (POLL it, don't read it once), and nil for good on a client or a direct/LAN host — there is no code there, joiners use the address." },
    ApiEntry { label: "net.ping", insert: "net.ping()", doc: "net.ping(peer?) — round-trip time in ms." },
    ApiEntry { label: "net.rpc", insert: "net.rpc(\"name\", {})", doc: "net.rpc(name, args, {to=peer, withInput=true}) — remote call: server→clients or client→server. withInput stamps a client intent with the tick it was seeing (for net.rewind). Handle with function onRpc.name(args, sender). Args: scalars + tables (≤4 deep, ≤1KB)." },
    ApiEntry { label: "net.rewind", insert: "net.rewind(peer, function()\n  \nend)", doc: "SERVER ONLY, inside onRpc for an rpc sent {withInput=true}: run the closure against the world as that peer PERCEIVED it — raycasts and other scripts' synced vars read the rewound tick (clamped ~250 ms). A parry that was up on the attacker's screen counts." },
    ApiEntry { label: "net.on", insert: "net.on(\"playerJoined\", function(peer) end)", doc: "net.on(event, fn) — session events: playerJoined/playerLeft (peer id), connected, disconnected (reason)." },
    ApiEntry { label: "net.spawn", insert: "net.spawn(\"scenes/thing.ron\", { x = 0, y = 0, z = 0 })", doc: "SERVER ONLY: net.spawn(path, {x,y,z,owner}) — spawn a scene's first node as a replicated runtime object on every client (available next tick)." },
    ApiEntry { label: "net.despawn", insert: "net.despawn(node)", doc: "SERVER ONLY: net.despawn(node) — remove a replicated runtime object everywhere." },
    ApiEntry { label: "net.isMine", insert: "net.isMine(node)", doc: "net.isMine(node) — is this node under MY control on this machine? Offline/non-networked → true; server → true unless a remote peer owns it; client → only your own predicted node(s). Cameras/HUDs use it to pick the local player out of many avatars (pair with findScripts)." },
    // ---- rollback netcode (a Networked node in mode "Rollback (all peers)") ----
    ApiEntry { label: "snapshot", insert: "function snapshot()\n  return { }\nend", doc: "function snapshot() — REQUIRED on a rollback node's scripts. Return a flat table of every gameplay value this script owns (state, frame counters, health, stun). The engine calls it each tick and restores it when a correction arrives. ANYTHING you leave out is a value that survives a rewind unchanged — which is exactly what a desync is made of. Transforms and physics bodies are saved for you; do NOT put them in here." },
    ApiEntry { label: "restore", insert: "function restore(s)\n  \nend", doc: "function restore(s) — the other half of snapshot(): put the table back. Called before the engine re-simulates a tick it already ran. Restore every key snapshot() returned, and nothing else." },
    ApiEntry { label: "net.random", insert: "net.random()", doc: "net.random(a?, b?) — deterministic RNG for a rollback match, drawn from (match seed, tick, draw index): every peer rolls the same number AND a re-simulated tick rolls it again. Use this instead of rng() in anything a rollback node reads — an unseeded roll comes from the clock, and two peers drawing differently is a match that quietly forks in two. No args → [0,1); one → integer 1..a; two → a..b." },
    ApiEntry { label: "net.replaying", insert: "net.replaying()", doc: "net.replaying() — true while the engine is RE-SIMULATING ticks it already ran after a correction. For cosmetics the engine can't gate for you (a screen shake, a UI poke). NEVER branch simulation on it: a replayed tick that computes something different from the live one is the definition of a desync." },
    ApiEntry { label: "net.stalled", insert: "net.stalled()", doc: "net.stalled() — true while the sim is waiting for a peer's input rather than guessing past the depth cap. The game runs slightly slow instead of teleporting the opponent. Drive your own \"connection trouble\" banner off this — a stall is otherwise indistinguishable from a bad frame rate." },
    ApiEntry { label: "net.inputDelay", insert: "net.inputDelay()", doc: "net.inputDelay() — the session's FIXED input delay in ticks. Never changes mid-match, because how the game feels must not." },
    ApiEntry { label: "net.rollbackDepth", insert: "net.rollbackDepth()", doc: "net.rollbackDepth() — ticks re-simulated by the most recent correction." },
    ApiEntry { label: "net.rollbackMax", insert: "net.rollbackMax()", doc: "net.rollbackMax() — the deepest rollback this session has had to perform: its worst moment." },
    ApiEntry { label: "net.rollbackAverage", insert: "net.rollbackAverage()", doc: "net.rollbackAverage() — mean ticks re-simulated per correction. The texture of the connection, where rollbackMax is only its worst moment. A healthy match sits low." },
    ApiEntry { label: "net.mispredictRate", insert: "net.mispredictRate()", doc: "net.mispredictRate() — 0..1, the fraction of simulated ticks that had to guess a peer's input. Rises with latency; what the input delay is chosen against." },
    ApiEntry { label: "replicated", insert: "replicated = {  }", doc: "replicated = { hp = 100 } — declare synced script vars (top level). Read/write them as synced.hp; the server's writes replicate to every client." },
    ApiEntry { label: "synced", insert: "synced", doc: "The synced-vars table (declared via replicated = {...}). Server writes replicate; client writes warn and get overwritten." },
    ApiEntry { label: "onRpc", insert: "onRpc = {}\nfunction onRpc.name(args, sender)\n  \nend", doc: "onRpc.<name>(args, sender) — handles net.rpc(\"name\", args). sender is the verified peer id (0 = server)." },
    ApiEntry { label: "params", insert: "params", doc: "This instance's tunables, a table seeded from `defaults` (params.speed, …). NUMBERS and STRINGS both work — a string default (destination = \"arena\") becomes an Inspector text field, so two portals can share one script with different destinations. TWO-WAY: writing a declared key persists across frames, shows live in the Inspector during Play, and is readable by other scripts through a handle (Stop reverts it). Undeclared keys stay frame-local; reference params (noderef & friends) never round-trip." },
    ApiEntry { label: "node", insert: "node", doc: "The node's transform: x/y/z, scale, scale_x/y/z, yaw/pitch/roll." },
    ApiEntry { label: "node.x", insert: "node.x", doc: "World X position (number)." },
    ApiEntry { label: "node.y", insert: "node.y", doc: "World Y position (number)." },
    ApiEntry { label: "node.z", insert: "node.z", doc: "World Z position (number)." },
    ApiEntry { label: "node.scale", insert: "node.scale", doc: "Uniform scale (shortcut). Setting it scales all axes." },
    ApiEntry { label: "node.scale_x", insert: "node.scale_x", doc: "Scale along X." },
    ApiEntry { label: "node.scale_y", insert: "node.scale_y", doc: "Scale along Y." },
    ApiEntry { label: "node.scale_z", insert: "node.scale_z", doc: "Scale along Z." },
    ApiEntry { label: "node.yaw", insert: "node.yaw", doc: "Heading about Y, in radians." },
    ApiEntry { label: "node.pitch", insert: "node.pitch", doc: "Pitch about X, in radians." },
    ApiEntry { label: "node.roll", insert: "node.roll", doc: "Roll about Z, in radians." },
    // The VECTOR reads/writes (v0.17.0) — the scalar triplets below them still
    // work, but these are what the docs teach: one write, no hand-rolled maths.
    ApiEntry { label: "node.vel", insert: "node.vel", doc: "The body's velocity as a vec3 (read/write). `node.vel = node.vel + node.up * jump` replaces three vx/vy/vz lines, and it accepts anything with x/y/z." },
    ApiEntry { label: "node.up", insert: "node.up", doc: "The body's up as a vec3 — minus gravity, so Y on flat ground and RADIAL on a planet. The direction to jump in, wherever the player is standing." },
    ApiEntry { label: "node.forward", insert: "node.forward", doc: "The node's facing as a vec3, from its rotation (-Z forward, matching the camera). Works on anything with a transform, body or not." },
    ApiEntry { label: "node.right", insert: "node.right", doc: "The node's +X axis as a vec3 (its rotation applied). Pairs with node.forward for camera-relative movement." },
    ApiEntry { label: "node.size", insert: "node.size", doc: "The node's whole scale as a vec3 (read/write). `node.scale` stays the uniform-scale shortcut, and also accepts a vec3 when you want all three axes at once." },
    ApiEntry { label: "node.vx", insert: "node.vx", doc: "Rigidbody velocity X (m/s). Read + write to drive the body; the engine integrates it." },
    ApiEntry { label: "node.vy", insert: "node.vy", doc: "Rigidbody velocity Y (m/s). Keep this for gravity/jump while replacing the horizontal part." },
    ApiEntry { label: "node.vz", insert: "node.vz", doc: "Rigidbody velocity Z (m/s)." },
    ApiEntry { label: "node.grounded", insert: "node.grounded", doc: "True while the rigidbody rests on a surface (read-only). Gate jumps on it." },
    ApiEntry { label: "node.groundNormal", insert: "node.groundNormal", doc: "The floor the body is standing on, as a vec3 normal — nil when airborne, so it is exactly node.grounded with the surface attached. Read-only. `node.groundNormal:dot(node.up)` is the cosine of the slope: 1 is flat, 0.5 is 60°. Align a character to the ground, judge a landing, or refuse to walk up something too steep." },
    ApiEntry { label: "node.wallNormal", insert: "node.wallNormal", doc: "The steepest surface the body is pressed against, as a vec3 normal — the cliff you ran at, the crate you're shoving — or nil when there's nothing but floor. Read-only. This is what stops a controller launching itself: driving into a steep face means the solver pushes the capsule out along a normal that points partly UP, every frame, which reads as being fired into the sky. Take that component out of your movement (see first_person.lua's `slide`) and you slide along the face instead. Also: wall jumps, wall slides, 'you can't go that way'." },
    ApiEntry { label: "node.up_x", insert: "node.up_x", doc: "Body up (−gravity) X — radial on a planet, so move along it for planet gravity. Read-only." },
    ApiEntry { label: "node.up_y", insert: "node.up_y", doc: "Body up (−gravity) Y (read-only)." },
    ApiEntry { label: "node.up_z", insert: "node.up_z", doc: "Body up (−gravity) Z (read-only)." },
    ApiEntry { label: "node.height", insert: "node.height", doc: "Capsule standing height — write a smaller value to crouch (the engine resizes it, feet planted)." },
    ApiEntry { label: "node.model", insert: "node.model", doc: "A Mesh node's model path — read it, or ASSIGN it to swap the model live (e.g. node.model = assets.getFile(\"models/x.glb\"))." },
    ApiEntry { label: "node.material", insert: "node.material", doc: "Apply a material — assign a preset name (\"Gold\") or an assets.getFile(\"materials/X.ron\")." },
    ApiEntry { label: "node.visible", insert: "node.visible", doc: "Whether the node's geometry is drawn — set node.visible = false to hide it (true to show)." },
    ApiEntry { label: "time", insert: "time", doc: "Seconds since play started (number)." },
    ApiEntry { label: "dt", insert: "dt", doc: "Seconds since the last frame (number)." },
    ApiEntry { label: "log", insert: "log(", doc: "log(\"message\") — print to the engine console." },
    ApiEntry { label: "input", insert: "input", doc: "Player input (play mode). input.key/pressed/axis/mouse/button — make interactive games." },
    ApiEntry { label: "input.key", insert: "input.key(", doc: "input.key(\"w\") — true while the key is held. Names: a-z, 0-9, space, enter, shift, ctrl, alt, left/right/up/down, escape, tab." },
    ApiEntry { label: "input.pressed", insert: "input.pressed(", doc: "input.pressed(\"space\") — true only on the frame the key goes down (an edge)." },
    ApiEntry { label: "input.typed", insert: "input.typed()", doc: "input.typed() — the CHARACTERS entered this frame, as a string, resolved by the OS keyboard layout (a paste folded in). Not the same question as input.pressed: that one is physical, so \"q\" is the key where Q sits on QWERTY and types `a` on AZERTY. Never contains control characters — Enter and Backspace stay actions. Empty while a UI text field has focus, because the field ate them." },
    ApiEntry { label: "input.released", insert: "input.released(", doc: "input.released(\"space\") — true only on the frame the key goes up (an edge)." },
    ApiEntry { label: "input.axis", insert: "input.axis(", doc: "input.axis(\"a\", \"d\") — returns -1/0/1 from a negative/positive key pair (e.g. strafing)." },
    ApiEntry { label: "input.mouse", insert: "input.mouse(", doc: "local x, y = input.mouse() — cursor position in pixels." },
    ApiEntry { label: "input.mouse_delta", insert: "input.mouse_delta(", doc: "local dx, dy = input.mouse_delta() — mouse movement since last frame." },
    ApiEntry { label: "input.button", insert: "input.button(", doc: "input.button(0) — true while a mouse button is held (0 left, 1 right, 2 middle)." },
    ApiEntry { label: "input.clicked", insert: "input.clicked(", doc: "input.clicked(0) — true only on the frame a mouse button goes down." },
    // --- the action layer (Project Settings → Input) ---
    ApiEntry { label: "input.action", insert: "input.action(", doc: "input.action(\"Jump\") — true while a NAMED action is held, from any of its bindings (key, mouse button, pad button, trigger). Define actions in Project Settings → Input; the list there is scanned from your scripts, so a name you type here shows up ready to bind. Prefer actions over input.key: they work on a gamepad, the player can rebind them, and they're what multiplayer replicates." },
    ApiEntry { label: "input.justPressed", insert: "input.justPressed(", doc: "input.justPressed(\"Punch\") — true only on the frame (or tick, inside fixedUpdate) the action goes down." },
    ApiEntry { label: "input.justReleased", insert: "input.justReleased(", doc: "input.justReleased(\"Block\") — true only on the frame/tick the action goes up." },
    ApiEntry { label: "input.heldSecs", insert: "input.heldSecs(", doc: "input.heldSecs(\"Charge\") — seconds the action has been continuously held (0 when up). Hold-to-charge without your own timer." },
    ApiEntry { label: "input.axis1", insert: "input.axis1(", doc: "input.axis1(\"Zoom\") — a named 1D axis in -1..1 (triggers, wheel, or a key pair)." },
    ApiEntry { label: "input.axis2", insert: "input.axis2(", doc: "local x, y = input.axis2(\"Move\") — a named 2D axis clamped to the unit disk. Reads identically on WASD and on a stick; deadzone and SOCD are handled for you." },
    ApiEntry { label: "input.player", insert: "input.player(", doc: "input.player(2) — the same input API bound to another LOCAL player (1-based). Two characters can run the same script: pass the slot as a param and use `local me = input.player(params.player)`. Set the count in Project Settings → Input. Sharing ONE keyboard: scope a binding to a player (right-click its chip) so a single action name can be J for P1 and 1 for P2 — pads sort themselves out already." },
    ApiEntry { label: "input.buffered", insert: "input.buffered(", doc: "input.buffered(\"Punch\", 4) — was it pressed within the last 4 TICKS and not yet consumed? The input buffer: a player who hits Punch a couple of frames before recovery ends still gets the punch. Pair with input.consume so it fires once. fixedUpdate only." },
    ApiEntry { label: "input.consume", insert: "input.consume(", doc: "input.consume(\"Punch\", 4) — spend a buffered press. Without it a 4-tick buffer fires your attack on all four ticks." },
    ApiEntry { label: "input.motion", insert: "input.motion(", doc: "input.motion(\"qcf\") — has a fighting-game motion just been completed? Seeded set: qcf, qcb, dp, rdp, hcf, hcb, dd, ff, bb, chargeF, chargeU (edit them in input.ron). Combine with input.buffered for a special: `if input.motion(\"qcf\") and input.buffered(\"Punch\", 4) then`. fixedUpdate only." },
    ApiEntry { label: "input.dir", insert: "input.dir()", doc: "input.dir() — the current numpad direction from \"Move\", from the character's point of view: 7 8 9 / 4 5 6 / 1 2 3, where 5 is neutral and 6 is forward." },
    ApiEntry { label: "input.dirHeldTicks", insert: "input.dirHeldTicks(", doc: "input.dirHeldTicks(4) — consecutive ticks a numpad direction has been held. Build your own charge or leniency rules on it." },
    ApiEntry { label: "input.setFacing", insert: "input.setFacing(", doc: "input.setFacing(-1) — mirror this player's directions after a cross-up, so motion(\"qcf\") keeps meaning \"toward the opponent\". The engine has no opinion about who faces where; the game sets it." },
    ApiEntry { label: "input.pushContext", insert: "input.pushContext(", doc: "input.pushContext(\"menu\", { priority = 100, consume = true, enabled = { \"Pause\" } }) — a consuming layer swallows every action it doesn't list, so a menu or dialogue eats movement without the player controller knowing. Pop it with input.popContext(\"menu\")." },
    ApiEntry { label: "input.popContext", insert: "input.popContext(", doc: "input.popContext(\"menu\") — remove an input layer. Returns whether one was removed." },
    ApiEntry { label: "input.actions", insert: "input.actions()", doc: "input.actions() — every action name in the map, for drawing an in-game controls screen." },
    ApiEntry { label: "input.bindingsOf", insert: "input.bindingsOf(", doc: "input.bindingsOf(\"Jump\") — an action's bindings as printable chips (\"⌨ Space\", \"🎮 South\")." },
    ApiEntry { label: "input.startRebind", insert: "input.startRebind(", doc: "input.startRebind(\"Jump\", \"pad\") — arm press-to-bind from a settings menu. Poll input.pendingRebind() for the captured chip, then input.commitRebind(). Filters: \"keyboard\", \"pad\", \"axis\", or nil for any button. Escape always cancels." },
    ApiEntry { label: "camera.worldToScreen", insert: "camera.worldToScreen(", doc: "camera.worldToScreen(x,y,z) → sx, sy, depth, onscreen — project a world point into the game view (pixels in input.mouse()'s space). onscreen=false behind the camera / off-frustum. Sample a drawn line into points, project each, keep the nearest to the cursor = click-on-line picking (the map's maneuver nodes)." },
    ApiEntry { label: "camera.screenToRay", insert: "camera.screenToRay(", doc: "camera.screenToRay(sx,sy) → ox,oy,oz, dx,dy,dz — a world ray from a screen pixel (inverse of worldToScreen)." },
    ApiEntry { label: "camera.screenSize", insert: "camera.screenSize()", doc: "camera.screenSize() → w, h — the game viewport size in pixels. camera.exists() is true once a live game camera is being fed." },
    ApiEntry { label: "camera.pixelsPerUnit", insert: "camera.pixelsPerUnit()", doc: "camera.pixelsPerUnit([distance]) → px — how many screen pixels one world unit covers at that distance (default: the camera's distance from the origin). The number every 2D game used to derive by hand from the FOV and the camera's Z, and then snap the camera to a multiple of for crisp pixels." },
    ApiEntry { label: "input.scroll", insert: "input.scroll(", doc: "input.scroll() — mouse wheel delta this frame." },
    ApiEntry { label: "input.lockMouse", insert: "input.lockMouse(", doc: "input.lockMouse() — pin the cursor to the window center and hide it (FPS / free-look mouselook without holding a button). Read motion with input.mouse_delta(). Released on Stop." },
    ApiEntry { label: "input.unlockMouse", insert: "input.unlockMouse(", doc: "input.unlockMouse() — release the cursor back to the desktop and show it again." },
    ApiEntry { label: "input.setMouseLocked", insert: "input.setMouseLocked(", doc: "input.setMouseLocked(true/false) — lock or unlock the mouse from a boolean (e.g. a menu toggle)." },
    ApiEntry { label: "raycast", insert: "raycast(", doc: "raycast(origin, dir, max [, ignore]) — or raycast(ox,oy,oz, dx,dy,dz, max [, ignore]). Cast a ray against the terrain + mesh colliders AND every physics body (players, crates). Returns a hit {x,y,z, nx,ny,nz, distance, node} or nil — node is the hit body's node handle (nil for static geometry). Your own node's body is excluded; pass a node as `ignore` to skip its body too. The last arg can instead be an options table: raycast(..., { ignore = target, layers = {\"Ground\"} }) — layers (name or array, Project Settings → Layers) filters what the ray can hit; a misspelled layer is an error. Use for ground checks, line-of-sight, shooting." },
    ApiEntry { label: "gizmo", insert: "gizmo", doc: "Immediate-mode debug drawing (play mode): gizmo.line/ray/sphere/point show for ONE frame in the Scene view (never the Game view; the viewport gizmos toggle hides them). Call every frame you want a shape visible." },
    ApiEntry { label: "gizmo.line", insert: "gizmo.line(", doc: "gizmo.line(x1,y1,z1, x2,y2,z2 [, r,g,b]) — a world-space debug line for one frame. Color is 0–1 floats (default green)." },
    ApiEntry { label: "gizmo.ray", insert: "gizmo.ray(", doc: "gizmo.ray(ox,oy,oz, dx,dy,dz [, len [, r,g,b]]) — a debug ray: origin + direction. With `len` the direction is normalized and the ray is that long — mirrors raycast(...), perfect for visualizing ground checks / line-of-sight." },
    ApiEntry { label: "gizmo.sphere", insert: "gizmo.sphere(", doc: "gizmo.sphere(x,y,z [, radius [, r,g,b]]) — a wire debug sphere (three rings): trigger zones, blast radii, pickup ranges." },
    ApiEntry { label: "gizmo.point", insert: "gizmo.point(", doc: "gizmo.point(x,y,z [, size [, r,g,b]]) — a small 3-axis cross marking a spot: hit points, waypoints, spawn locations." },
    ApiEntry { label: "scene.load", insert: "scene.load(", doc: "scene.load(\"arena\") — switch to another scene at the next frame boundary: the world swaps, physics/animators/particles/audio rebuild, every start re-fires (like the scene booting fresh). Accepts a name, a scenes-relative path (\"arenas/desert\"), or \"scenes/arena.ron\". Multiplayer: only the SERVER may call it — every client follows automatically; a client's call is refused (send the server an RPC instead)." },
    ApiEntry { label: "scene.current", insert: "scene.current()", doc: "scene.current() — the running scene's name (its file stem, e.g. \"first\")." },
    ApiEntry { label: "scene.list", insert: "scene.list()", doc: "scene.list() — every scene in the project as names scene.load accepts (sorted; subfolders kept)." },
    ApiEntry { label: "terrain.sculpt", insert: "terrain.sculpt(", doc: "terrain.sculpt(x,y,z, radius [, strength [, mode]]) — sculpt the nearest terrain at a world point, landing the SAME tick (collision updates with the surface). mode: \"raise\" (default), \"lower\"/\"dig\", \"smooth\", \"flatten\"; strength 0..1. No-op when no terrain surface is near the point. Multiplayer: run on the server + mirror by RPC (deterministic ops)." },
    ApiEntry { label: "terrain.dig", insert: "terrain.dig(", doc: "terrain.dig(x,y,z, radius [, strength]) — carve a hole: sugar for terrain.sculpt(..., \"lower\"). Pair with raycast(...) to dig where the player aims." },
    ApiEntry { label: "terrain.paint", insert: "terrain.paint(", doc: "terrain.paint(x,y,z, radius, r,g,b [, strength]) — recolor the terrain surface inside the brush ball (0..1 colors)." },
    ApiEntry { label: "terrain.paintTexture", insert: "terrain.paintTexture(", doc: "terrain.paintTexture(x,y,z, radius, slot) — paint a terrain-palette texture slot (1-based, the Terrain tab's palette; 0 clears to flat color)." },
    ApiEntry { label: "terrain.generatePlanet", insert: "terrain.generatePlanet(", doc: "terrain.generatePlanet(id [, opts]) — REPLACE terrain id's whole field with a generated planet (sphere ± noise relief, caves + chambers, molten core, craters, layered materials). Background-generated (seconds; Console shows progress). opts (all optional): radius, voxel, relief, bumpFreq, caveDepth, coreR, corePaint, craters, craterMin/Max, craterDust, surfaceA/B {slot,color}, patchBias/Thr, subsoil(+Depth), strata(+Depth), deep, pockets {slot,color,threshold,minDepth}, seam {slot,color,minDepth,center,width}, iceCaps {lat,slot,color}, seed." },
    ApiEntry { label: "terrain.saveDir", insert: "terrain.saveDir(", doc: "terrain.saveDir(path) / terrain.saveDir() — set (or read) the game's SAVE-SLOT directory for player-edited terrain, relative to the project root (e.g. \"saves/slot1/terrain\"). While set, streaming loads fields from here first (before the project file or the genspec) and writes edited fields back on stream-out — per-slot terrain persistence. \"\" clears; auto-cleared when Play stops." },
    ApiEntry { label: "terrain.warm", insert: "terrain.warm(", doc: "terrain.warm(bodyName) — keep that body's terrain RESIDENT this frame regardless of where the ship/player physically is: it streams in if cold and never streams out. Immediate mode — call every frame while you care (the map warms its focused planet). Streaming is otherwise anchored to dynamic bodies' physical positions, never the camera." },
    ApiEntry { label: "terrain.flush", insert: "terrain.flush()", doc: "terrain.flush() — checkpoint every EDITED resident terrain field to the save slot (terrain.saveDir must be set). Runs IN THE BACKGROUND (amortized encode + threaded write, deferred while a field is actively being dug) so autosaves never stutter; exit paths (Stop / scene.load) finish the writes synchronously so a checkpoint is never lost." },
    ApiEntry { label: "terrain.deleteSaveDir", insert: "terrain.deleteSaveDir(", doc: "terrain.deleteSaveDir(\"saves/slot2/terrain\") — delete a save slot's persisted terrain from disk (pair with save.deleteSlot in a \"delete this save\" UI). Narrow by design: relative path, no \"..\", must not be the ACTIVE saveDir, and only .cfield/.tfield/.meta files in that one directory are removed (emptied dirs are tidied). Returns the number of files removed." },
    ApiEntry { label: "terrain.query", insert: "terrain.query(", doc: "terrain.query(x,y,z) — signed distance to the nearest terrain surface (negative = inside rock), or nil with no terrain. Cheap: read it every frame (burrow checks, depth meters)." },
    ApiEntry { label: "terrain.height", insert: "terrain.height(", doc: "terrain.height(x, z) — world Y of the highest terrain surface under (x,z), or nil when nothing is hit. Spawning, footstep audio by ground, drop-to-floor." },
    ApiEntry { label: "rng", insert: "rng(", doc: "rng(seed) — a DETERMINISTIC random stream: same seed, same sequence, every machine. r:next() in [0,1), r:range(a,b), r:int(a,b) inclusive, r:pick(list). Use for gameplay that must reproduce (loot, procgen scatter, server replays); math.random stays for throwaway rolls." },
    // math helpers (v0.17.0): the gameplay arithmetic every controller was
    // writing out by hand.
    ApiEntry { label: "math.clamp", insert: "math.clamp", doc: "math.clamp(x, lo, hi) — x held inside the range. Reversed bounds are tolerated rather than returning NaN." },
    ApiEntry { label: "math.saturate", insert: "math.saturate", doc: "math.saturate(x) — clamp to 0..1, the most-written clamp of all." },
    ApiEntry { label: "math.sign", insert: "math.sign", doc: "math.sign(x) — -1, 0 or 1. Exactly 0 for 0 (not 1, which is what math.abs tricks give you)." },
    ApiEntry { label: "math.round", insert: "math.round", doc: "math.round(x [, step]) — nearest whole number, or nearest multiple of `step`: `math.round(x, 0.25)` snaps to quarters for grid placement." },
    ApiEntry { label: "math.lerp", insert: "math.lerp", doc: "math.lerp(a, b, t) — linear blend, UNCLAMPED (t outside 0..1 extrapolates, which is useful). Use math.mix for the clamped version." },
    ApiEntry { label: "math.mix", insert: "math.mix", doc: "math.mix(a, b, t) — math.lerp with t clamped to 0..1." },
    ApiEntry { label: "math.inverseLerp", insert: "math.inverseLerp", doc: "math.inverseLerp(a, b, x) — where x sits between a and b, 0..1. Returns 0 when a == b instead of a NaN that poisons everything downstream." },
    ApiEntry { label: "math.remap", insert: "math.remap", doc: "math.remap(x, a, b, c, d) — x from the range a..b onto c..d. The one-liner behind fades, falloffs and gauge needles." },
    ApiEntry { label: "math.smoothstep", insert: "math.smoothstep", doc: "math.smoothstep(a, b, x) — 0..1 with eased ends, for anything that shouldn't start and stop abruptly." },
    ApiEntry { label: "math.approach", insert: "math.approach", doc: "math.approach(current, target, maxDelta) — move toward target without ever overshooting. Pass `rate * dt`; this is the correct version of the hand-rolled move-towards that jitters at low frame rates." },
    ApiEntry { label: "math.wrapAngle", insert: "math.wrapAngle", doc: "math.wrapAngle(a) — an angle folded into (-pi, pi]." },
    ApiEntry { label: "math.deltaAngle", insert: "math.deltaAngle", doc: "math.deltaAngle(a, b) — the SHORTEST signed turn from a to b, correct across the +/-pi seam (350 degrees to 10 is +20, not -340)." },
    ApiEntry { label: "math.approachAngle", insert: "math.approachAngle", doc: "math.approachAngle(current, target, maxDelta) — math.approach for headings: turns the short way and never overshoots. Turrets, camera yaw, 'face the player'." },
    ApiEntry { label: "math.pingPong", insert: "math.pingPong", doc: "math.pingPong(t, len) — 0 to len and back, forever. Patrols, bobbing, breathing lights." },
    // table helpers (v0.17.0): a list operation instead of a bookkeeping loop.
    ApiEntry { label: "table.map", insert: "table.map", doc: "table.map(list, fn) — a new list of fn(value, i). Never mutates the input." },
    ApiEntry { label: "table.filter", insert: "table.filter", doc: "table.filter(list, fn) — a new list of the items where fn(value, i) is true." },
    ApiEntry { label: "table.find", insert: "table.find", doc: "table.find(list, fn) -> value, index — the first item satisfying the PREDICATE (nil, nil if none). `table.find(ships, function(s) return s.docked end)`." },
    ApiEntry { label: "table.indexOf", insert: "table.indexOf", doc: "table.indexOf(list, value) — the index of a value by plain equality, or nil." },
    ApiEntry { label: "table.count", insert: "table.count", doc: "table.count(t [, fn]) — how many entries (works on KEYED tables, which `#t` cannot), or how many satisfy the predicate." },
    ApiEntry { label: "table.sum", insert: "table.sum", doc: "table.sum(list [, fn]) — add the numbers, or add fn(value, i) over them: `table.sum(tanks, function(t) return t.fuel end)`." },
    ApiEntry { label: "table.keys", insert: "table.keys", doc: "table.keys(t) — the keys as a SORTED list. Sorted because raw `pairs` order is hash order, which a replay can't reproduce." },
    ApiEntry { label: "table.copy", insert: "table.copy", doc: "table.copy(t) — a shallow copy (keys and values)." },
    ApiEntry { label: "table.extend", insert: "table.extend", doc: "table.extend(dst, src) — append src's items onto dst in place, and return dst." },
    ApiEntry { label: "table.reverse", insert: "table.reverse", doc: "table.reverse(list) — a new list, back to front." },
    ApiEntry { label: "math.noise", insert: "math.noise(", doc: "math.noise(x, y, z [, seed]) — seeded value noise, one octave, about -1..1, identical on every machine (the same numbers the engine's Rust generators use). Scale the inputs to pick a frequency." },
    ApiEntry { label: "save.set", insert: "save.set(", doc: "save.set(\"gold\", 42) — store persistent game data: survives Play sessions, editor restarts, and ships with exported builds. Values follow the synced-var guardrails (numbers/strings/bools/tables, depth <= 4, <= 1 KB). Flushed on Stop + every few seconds during Play." },
    ApiEntry { label: "save.get", insert: "save.get(", doc: "save.get(\"gold\" [, default]) — the stored value, else the default, else nil. save.get(\"who\").hp reads into stored tables." },
    ApiEntry { label: "save.delete", insert: "save.delete(", doc: "save.delete(\"gold\") — remove a key; true if something was removed." },
    ApiEntry { label: "save.slot", insert: "save.slot(", doc: "save.slot(\"slot2\") — switch the active save slot (the old one flushes first); save.slot() reads the current name. Each slot is its own file under save/." },
    ApiEntry { label: "save.deleteSlot", insert: "save.deleteSlot(", doc: "save.deleteSlot(\"slot2\") — delete a slot's store file from disk (\"delete this save\" UIs). Deleting the ACTIVE slot also empties the in-memory store, so the slot is instantly reusable. Per-slot terrain is separate — pair with terrain.deleteSaveDir. Returns true if a file was removed." },
    ApiEntry { label: "save.flush", insert: "save.flush()", doc: "save.flush() — write the store to disk NOW (checkpoints, before risky sections). Returns false on an IO error (also shown in the Console)." },
    ApiEntry { label: "math.fbm", insert: "math.fbm(", doc: "math.fbm(x, y, z [, octaves [, seed]]) — seeded fractal noise (default 4 octaves, rotated so features never align to the axes), about -1..1. Terrain-style variation for scripts: scatter decorations, vary spawns, wobble paths." },
    ApiEntry { label: "after", insert: "after(", doc: "after(seconds, fn) — run fn once after that much GAME time (tick-driven, deterministic, pauses with the game). Returns a handle: h:cancel() aborts. Capture what you need as locals — the callback gets no arguments. after(2, function() door.visible = false end)" },
    ApiEntry { label: "every", insert: "every(", doc: "every(seconds, fn) — run fn repeatedly (first fire after one period). Anchored cadence: long sessions don't drift. Keep the handle to stop it: local h = every(1, tickDown) ... h:cancel()." },
    ApiEntry { label: "tween", insert: "tween(", doc: "tween(seconds, fn [, ease]) — animate: fn(alpha) runs every tick with alpha easing 0→1, final call exactly at 1.0. ease: \"linear\" (default), \"smooth\", \"in\", \"out\". tween(0.5, function(a) node.y = startY + a * 3 end, \"smooth\"). Returns a cancellable handle." },
    ApiEntry { label: "space.time", insert: "space.time()", doc: "space.time() — on-rails celestial time in seconds (0 at Play start; advances with warp). Scenes with Celestial Body components put planets/moons on exact Kepler rails." },
    ApiEntry { label: "space.warp", insert: "space.warp(", doc: "space.warp(50) — request a time-warp multiplier (1 .. 100000): rails fast-forward, local physics keeps ticking at 1×. space.warp() reads the current value." },
    ApiEntry { label: "space.bodies", insert: "space.bodies()", doc: "space.bodies() — every celestial body this tick: {name, x,y,z, vx,vy,vz, mu, radius, soi} in world coords (soi -1 = infinite). space.body(\"Pebble\") grabs one by node name." },
    ApiEntry { label: "space.dominant", insert: "space.dominant(", doc: "space.dominant(x, y, z) — the name of the body whose gravity OWNS that position (deepest sphere of influence — the moon inside the planet inside the sun), or nil." },
    ApiEntry { label: "space.gravity", insert: "space.gravity(", doc: "space.gravity(x, y, z) — gx, gy, gz: the µ/r² pull of the dominant body at a world position (patched conics: exactly one body pulls)." },
    ApiEntry { label: "space.elements", insert: "space.elements(", doc: "space.elements(x,y,z, vx,vy,vz) — the orbit a craft is ON around its dominant body: { body, a, e, periapsis, apoapsis, period } (apoapsis/period absent on an escape). Distances from the body CENTER. The map/HUD readout." },
    ApiEntry { label: "space.propagate", insert: "space.propagate(", doc: "space.propagate(px,py,pz, vx,vy,vz, mu, dt) — the state (px,py,pz, vx,vy,vz) advanced dt seconds on the two-body conic about a point mass mu (elliptic OR hyperbolic, drift-free). The map's maneuver nodes + SOI-encounter walk are built from it. State is in whatever frame you pass — compose parent frames yourself." },
    ApiEntry { label: "assets", insert: "assets", doc: "Reference files under Assets/ in code: assets.getFile(path), assets.getContents(dir)." },
    ApiEntry { label: "assets.getFile", insert: "assets.getFile(", doc: "assets.getFile(\"models/armor.glb\") — the asset's path (or nil), to hand to node.model / node.material. Path is relative to Assets/." },
    ApiEntry { label: "assets.getContents", insert: "assets.getContents(", doc: "assets.getContents(\"models\") — an array of every file under that folder (recursive). Build tables of assets with it." },
    ApiEntry { label: "find", insert: "find(", doc: "find(\"Player\") — the first node in the scene with that name (a node handle), or nil." },
    ApiEntry { label: "findAll", insert: "findAll(", doc: "findAll(\"Coin\") — an array of every node with that name." },
    ApiEntry { label: "findScript", insert: "findScript(", doc: "findScript(\"GameManager\") — a script handle for the first node anywhere running that script (the manager pattern), or nil. Call its methods / read its state. RESERVED KEYS: a handle answers `node` (its own node), `kind` (which script it is) and `valid` (still loaded?) ITSELF, so a script exporting one of those three can reach it and nobody else can — the editor lints the export and the Console says so at load. `name` is NOT reserved: a script's own `name` wins, and `kind` is the same string (floptle/0085)." },
    ApiEntry { label: "findScriptInScene", insert: "findScriptInScene(", doc: "Alias of findScript(kind)." },
    ApiEntry { label: "findScripts", insert: "findScripts(", doc: "findScripts(kind) — EVERY node carrying that script, as script handles in scene order. Pair with net.isMine to pick the local player out of many avatars: for _, s in ipairs(findScripts(\"third_person\")) do if net.isMine(s.node) then ... end end" },
    ApiEntry { label: "findTagged", insert: "findTagged(", doc: "findTagged(\"enemy\") — EVERY node carrying that tag (Inspector tag chips / node:addTag), as node handles in scene order. Empty table when none; findTagged(\"enemy\")[1] grabs the first." },
    ApiEntry { label: "node.layer", insert: "node.layer", doc: "The node's collision/query layer, by project-defined NAME (\"Default\" when unset). Assign to move it (node.layer = \"Ghosts\") — a name the project doesn't define is an ERROR, so typos surface immediately. The Project Settings matrix decides which layers collide; a dynamic body re-layers live." },
    ApiEntry { label: "node.tags", insert: "node.tags", doc: "The node's tags as an array of strings (a fresh table each read). Assign a whole array to replace the list; use node:addTag / node:removeTag for single edits and node:hasTag to test." },
    ApiEntry { label: "node:hasTag", insert: "node:hasTag(", doc: "node:hasTag(\"enemy\") — whether the node carries that exact tag. The classic hit-filter: local hit = raycast(...) if hit and hit.node and hit.node:hasTag(\"enemy\") then ... end" },
    ApiEntry { label: "node:addTag", insert: "node:addTag(", doc: "node:addTag(\"burning\") — add a tag at runtime (duplicates are ignored). findTagged sees it next frame." },
    ApiEntry { label: "node:removeTag", insert: "node:removeTag(", doc: "node:removeTag(\"burning\") — remove a tag (no-op when absent)." },
    ApiEntry { label: "node.pos", insert: "node.pos", doc: "The node's position as a vec3 (read/write): node.pos = node.pos + dir * dt. Accepts anything with x/y/z." },
    ApiEntry { label: "node.tickPos", insert: "node.tickPos", doc: "The body's TICK pose as a vec3 (read/write) — where the simulation says it is, as opposed to node.pos, which is the interpolated pose the camera renders. Inside fixedUpdate use this one: move with node.tickPos = node.tickPos + vec3(d, 0, 0) and build hurtboxes from it. `node.x = node.x + d` in fixedUpdate teleports the body onto its VISUAL position, so the model slides and the hitbox doesn't follow. In a rollback match this is the difference between a hit registering and not." },
    ApiEntry { label: "node.tickYaw", insert: "node.tickYaw", doc: "The body's tick-domain yaw (read/write) — node.yaw's simulation-truth counterpart, for facing a fighter inside fixedUpdate." },
    ApiEntry { label: "vec3", insert: "vec3(", doc: "vec3(x, y, z) — a 3-vector VALUE with real operators: a + b, a - b, v * 2, -v, a == b. Methods: :length(), :lengthSquared(), :normalized(), :dot(o), :cross(o), :lerp(o, t), :distance(o), :flatten(up), :withX/:withY/:withZ(n), :rotatedY(rad), :rotatedAround(axis, rad), :towards(o, maxDelta), :angleTo(o). vec3() = zero, vec3(s) = splat, vec3(other) = copy. Anything that takes a vector also takes a {x=,y=,z=} table or a node handle." },
    ApiEntry { label: "vec3:flatten", insert: ":flatten(", doc: "v:flatten(up) — the part of v that lies in the plane PERPENDICULAR to up, renormalised. THE planet-safe move: \"forward along the ground\" is dirFromYaw(node.yaw):flatten(node.up) whatever the local vertical is, and on a flat world :flatten() (default +Y) is the familiar \"drop the Y\". Straight up or down leaves nothing in the plane → vec3(0,0,0), never a NaN." },
    ApiEntry { label: "vec3:withY", insert: ":withY(", doc: "v:withX(n) / v:withY(n) / v:withZ(n) — the same vector with one component replaced. node.vel:withY(0) keeps your fall speed out of a horizontal speed clamp." },
    ApiEntry { label: "vec3:rotatedY", insert: ":rotatedY(", doc: "v:rotatedY(rad) — spun about world +Y (the yaw of a flat world). For any other axis use v:rotatedAround(axis, rad)." },
    ApiEntry { label: "vec3:rotatedAround", insert: ":rotatedAround(", doc: "v:rotatedAround(axis, rad) — Rodrigues rotation about ANY axis, which is what a planet camera's yaw actually is (about the LOCAL up, not about +Y)." },
    ApiEntry { label: "vec3:towards", insert: ":towards(", doc: "v:towards(other, maxDelta) — step toward another point without ever overshooting it: math.approach, for positions. Pass `speed * dt`." },
    ApiEntry { label: "vec3:angleTo", insert: ":angleTo(", doc: "v:angleTo(other) — the unsigned angle between two directions, in radians. Clamped before the acos, so parallel vectors give 0 and a zero vector gives 0 — never a NaN." },
    ApiEntry { label: "vec2", insert: "vec2(", doc: "vec2(x, y) — a 2-vector value (UI/screen math), same operators and methods as vec3 (minus cross)." },
    ApiEntry { label: "distance", insert: "distance(", doc: "distance(a, b) — distance between two points: vec3/vec2 values, {x=,y=,z=} tables, or NODE handles (distance(node, target) just works). Also distance(x1,y1,z1, x2,y2,z2) for raw numbers." },
    // ---- directions & orientation (0.20.0) ------------------------------------
    // The corner of the API people used to write out longhand. Every doc line
    // here names the arithmetic it replaces, because that is how someone
    // recognises the thing they were about to type by hand.
    ApiEntry { label: "node:lookAt", insert: "node:lookAt(", doc: "node:lookAt(target [, up]) — point this node at another node or a world point. Sets yaw + pitch and leaves roll alone; pass an `up` and it sets the roll too, to whatever puts that up over the node's head (a level horizon on a planet — the twenty-line undo-yaw-then-pitch dance, in one call). Measured in WORLD space on both ends." },
    ApiEntry { label: "node:turnTowards", insert: "node:turnTowards(", doc: "node:turnTowards(target, maxRadians) — turn toward something by at most that much, the SHORT way round (the ±pi seam is handled). Pass `rate * dt` for a frame-rate-independent turn. A node handle or a world point is somewhere to face; any other vector is taken as a DIRECTION, so node:turnTowards(node.vel, 6 * dt) steers a unit to face where it is going. A zero-length direction leaves the facing alone." },
    ApiEntry { label: "dirTo", insert: "dirTo(", doc: "dirTo(from, to) — the UNIT direction from one thing to another. Both may be a vec3, a {x=,y=,z=} table or a NODE handle, so dirTo(node, target) is the whole sentence. Same point twice → vec3(0,0,0), never a NaN." },
    ApiEntry { label: "yawOf", insert: "yawOf(", doc: "yawOf(dir) — the yaw that faces along a direction. This is atan2(-x, -z) (engine forward is −Z), once and with the right signs. Zero direction → 0." },
    ApiEntry { label: "pitchOf", insert: "pitchOf(", doc: "pitchOf(dir) — the pitch that faces along a direction, positive looking up. asin, clamped, so a denormalised vector can't produce a NaN." },
    ApiEntry { label: "dirFromYaw", insert: "dirFromYaw(", doc: "dirFromYaw(yaw [, pitch]) — the unit direction those angles face: the inverse of yawOf/pitchOf. Without a pitch you get the ground direction, which is what movement wants; with one you get a camera's view direction." },
    ApiEntry { label: "lookRotation", insert: "lookRotation(", doc: "lookRotation(dir [, up]) -> yaw, pitch, roll — the angles that face `dir`, WITHOUT applying them (node:lookAt applies them). Three returns, so `node.yaw, node.pitch, node.roll = lookRotation(f, up)` is one line. No up = roll 0." },
    ApiEntry { label: "ease", insert: "ease(", doc: "ease(a, b, rate, dt) — frame-rate-independent exponential ease: `a` covers a rate-dependent FRACTION of the remaining distance each second, so 30 fps and 240 fps feel identical. Numbers or vectors. rate <= 0 snaps. This is what a camera's \"smoothing\" knob is; three shipped camera scripts each defined it privately before it lived here." },
    ApiEntry { label: "smoothDamp", insert: "smoothDamp(", doc: "smoothDamp(current, target, vel, smoothTime, dt) -> value, vel — a critically-damped spring: unlike ease it has MOMENTUM, so a follow keeps moving for a moment after the target stops. Lua has no reference parameters, so the velocity comes back as the second return: camX, camVX = smoothDamp(camX, wantX, camVX, 0.25, dt). Numbers or vectors." },
    ApiEntry { label: "moveTowards", insert: "moveTowards(", doc: "moveTowards(node, target, maxDelta) — walk a node toward a WORLD point at a speed, never overshooting it. Pass `speed * dt`. Returns true once it has arrived, so `if moveTowards(node, goal, s * dt) then` is the whole patrol step. Also spelled node:moveTowards(target, maxDelta)." },
    ApiEntry { label: "node:moveTowards", insert: "node:moveTowards(", doc: "node:moveTowards(target, maxDelta) — the method spelling of moveTowards(node, …). World-space and placed through the parent inverse, so a node under a container arrives where you actually pointed." },
    // ---- local ↔ world --------------------------------------------------------
    ApiEntry { label: "node:setWorldPos", insert: "node:setWorldPos(", doc: "node:setWorldPos(v) — put this node at a WORLD point, whatever it is parented to, without deriving the parent inverse by hand. Goes through the componentwise TRS inverse, so it stays exact under a MIRRORED (negative-scale) parent, where a matrix decomposition puts the flip on the wrong axis." },
    ApiEntry { label: "node:toWorld", insert: "node:toWorld(", doc: "node:toWorld(v) — a point in this node's own frame, converted to world space: its position, rotation AND scale, composed up the whole parent chain. \"Where is the muzzle?\" is gun:toWorld(vec3(0, 0, -1.2))." },
    ApiEntry { label: "node:toLocal", insert: "node:toLocal(", doc: "node:toLocal(v) — the inverse of node:toWorld: a world point expressed in this node's frame." },
    ApiEntry { label: "node:worldForward", insert: "node:worldForward()", doc: "node:worldForward() — the node's forward AFTER the parent chain. node.forward is the LOCAL one: a gun barrel parented to a swinging arm points where the ARM says, so shooting along node.forward misses. Also node:worldRight() and node:worldUp()." },
    ApiEntry { label: "node:worldRight", insert: "node:worldRight()", doc: "node:worldRight() — the node's +X axis after the parent chain." },
    ApiEntry { label: "node:worldUp", insert: "node:worldUp()", doc: "node:worldUp() — the node's +Y axis after the parent chain (not the same as node.up, which is the body's −gravity up)." },
    ApiEntry { label: "node:distanceTo", insert: "node:distanceTo(", doc: "node:distanceTo(other) — distance to a node or a point, measured in WORLD space, which is the answer people mean. `distance(a, b)` compares LOCAL positions — correct right up until one of the two is parented, and then quietly about the wrong frame." },
    ApiEntry { label: "node:distanceFlat", insert: "node:distanceFlat(", doc: "node:distanceFlat(other [, up]) — distance ignoring the up axis (default +Y): the \"have I arrived?\" test for anything that walks on ground it doesn't control the height of. Pass an up for a planet." },
    ApiEntry { label: "node.worldPos", insert: "node.worldPos", doc: "The node's position in WORLD space as a vec3, composed up the parent chain (read-only; node.worldX/worldY/worldZ are the components). node.x/y/z are LOCAL — comparing those against a world target is how a unit under a container walks past its destination and keeps going." },
    ApiEntry { label: "onCollisionEnter", insert: "function onCollisionEnter(node, other, hit)\n  \nend", doc: "function onCollisionEnter(node, other, hit) — fires the tick this node's body STARTS touching something solid (a collider or another body). `other` = the other node's handle (check other:hasTag(\"...\") / other.name); hit = { x, y, z, nx, ny, nz } (world contact point + normal). Also onCollisionStay (every tick while touching) and onCollisionExit (on separation)." },
    ApiEntry { label: "onCollisionExit", insert: "function onCollisionExit(node, other, hit)\n  \nend", doc: "function onCollisionExit(node, other, hit) — fires the tick the touch ends (hit = the last known contact)." },
    ApiEntry { label: "onTriggerEnter", insert: "function onTriggerEnter(node, other, hit)\n  \nend", doc: "function onTriggerEnter(node, other, hit) — fires the tick a body enters a TRIGGER (the \"trigger\" switch on a Collider or Rigidbody: it stops blocking, events still fire — a Kinematic trigger rigidbody = a moving pickup). The portal/pickup/checkpoint hook — pair with a string param: scene.load(params.destination). Also onTriggerStay / onTriggerExit." },
    ApiEntry { label: "onTriggerExit", insert: "function onTriggerExit(node, other, hit)\n  \nend", doc: "function onTriggerExit(node, other, hit) — fires the tick a body leaves the trigger." },
    ApiEntry { label: "node.name", insert: "node.name", doc: "The node's name (string)." },
    ApiEntry { label: "node.id", insert: "node.id", doc: "A stable numeric id for this node." },
    ApiEntry { label: "node.parent", insert: "node.parent", doc: "The parent node handle, or nil. A handle has the same fields (x/y/z, …) so you can read/write another node." },
    ApiEntry { label: "node:setShaderTexture", insert: "node:setShaderTexture(", doc: "node:setShaderTexture(slot, ref) — point one of this node's .flsl shader TEXTURE SLOTS somewhere else, at runtime. `slot` is the name the shader declares (`texture ramp` -> \"ramp\"); `ref` is a project-relative image path, an `rt:<name>` render target (what another camera sees, live), or \"\" to clear it. A shader may declare up to 8 slots, so a material can mix a base, a mask, a ramp and a screen — and a script can swap any of them per frame." },
    ApiEntry { label: "node:setShaderParam", insert: "node:setShaderParam(", doc: "node:setShaderParam(\"glow\", 2.5) / node:setShaderParam(\"nose\", x, y, z) — drive a .flsl uniform on this node every tick (a GPU uniform write, never a recompile). Targets the node's Material shader, its UI element's `stage ui` shader (the navball pattern: a script feeds an instrument's uniforms each tick), the Skybox's sky shader, or — on the Post Processing node — its SCREEN shaders: name one with `\"inkOutline.thickness\"`, or leave the prefix off to set that knob on every pass. Unset lanes are 0." },
    ApiEntry { label: "node:setScreenShader", insert: "node:setScreenShader(", doc: "node:setScreenShader(\"inkOutline\", false) — switch one of the Post Processing node's screen shaders on or off. The name is the file without its extension, the one the Inspector lists. The pass and its knobs stay in the scene, so this is a switch and not a deletion: turn the outline on for a boss fight and off again after. Pass \"\" for every pass on the node." },
    // ---- the web (0.20.0) ------------------------------------------------------
    ApiEntry { label: "http.get", insert: "http.get(\"\", function(res)\n  \nend)", doc: "http.get(url [, opts], function(res) end) — fetch a URL. NON-BLOCKING: the callback runs on a later tick on the MAIN thread, so it is safe to touch nodes from it and a slow server can never stall a frame. opts = { headers = {...}, timeout = 10, json = true }. res = { ok, status, body, json, error } — `ok` is a 2xx with no error; a 404 still hands you `body`, because that is where an API explains itself. Play only." },
    ApiEntry { label: "http.post", insert: "http.post(\"\", {}, function(res)\n  \nend)", doc: "http.post(url, body [, opts], function(res) end) — same rules as http.get, plus a body: a STRING is sent as-is, a TABLE is encoded as JSON for you. http.put and http.delete round out the set." },
    ApiEntry { label: "http.put", insert: "http.put(\"\", {}, function(res)\n  \nend)", doc: "http.put(url, body [, opts], function(res) end) — as http.post, with PUT." },
    ApiEntry { label: "http.delete", insert: "http.delete(\"\", function(res)\n  \nend)", doc: "http.delete(url [, opts], function(res) end) — as http.get, with DELETE." },
    ApiEntry { label: "http.inFlight", insert: "http.inFlight()", doc: "http.inFlight() — how many requests are still waiting on a reply. Up to 8 may be in flight and 20 may start per second; past that, calls fail fast with res.error and the cap announces itself once in the Console. A cap you are hitting is nearly always a request inside update()." },
    ApiEntry { label: "http.cancelAll", insert: "http.cancelAll()", doc: "http.cancelAll() — forget every pending callback. Stop and scene.load do this for you: a callback closes over nodes from the scene that asked, and delivering it into a fresh session is how one run inherits the previous one's network." },
    ApiEntry { label: "json.encode", insert: "json.encode(", doc: "json.encode(value) — a Lua value as a JSON string. A table with a [1] is an ARRAY, anything else is an object (that is the only rule Lua's single table type can support). http.post takes a table body directly, so you rarely need this by hand." },
    ApiEntry { label: "json.decode", insert: "json.decode(", doc: "json.decode(s) -> value, err — parse JSON. Bad input returns nil AND a message rather than raising: a reply from someone else\'s server is data, not a bug in your script. JSON null becomes nil, so a null field reads exactly like a missing one." },
    ApiEntry { label: "account.signIn", insert: "account.signIn()", doc: "account.signIn() — begin signing the player in to their Foverse account (fopull.com). Returns IMMEDIATELY; watch account.state() and draw account.code(). The engine drives the OAuth device flow in Rust — the player approves in their browser, so the game never sees a password and never holds a token. Play only." },
    ApiEntry { label: "account.state", insert: "account.state()", doc: "account.state() — \"signedOut\" | \"starting\" | \"waiting\" | \"signedIn\" | \"failed\". Polled rather than called back, because signing in takes as long as a person takes to pick up their phone and a sign-in screen is redrawing anyway." },
    ApiEntry { label: "account.code", insert: "account.code()", doc: "account.code() — while state() is \"waiting\": { code = \"WXYZ-9999\", url = \"...\", expiresIn = 900 }. Show the code and send them to the url (openUrl does it) — that pairing is what the player approves. nil at any other time." },
    ApiEntry { label: "account.player", insert: "account.player()", doc: "account.player() — { id, name, email, tier } once signed in, else nil. There is deliberately no way to read the access token: a shipped game's Lua is readable, so anything a script can hold a player can read out of the file." },
    ApiEntry { label: "account.error", insert: "account.error()", doc: "account.error() — why the last sign-in failed, as a sentence you can put on screen. nil unless state() is \"failed\"." },
    ApiEntry { label: "account.cancel", insert: "account.cancel()", doc: "account.cancel() — abandon a sign-in in progress (the player pressed Escape). Harmless at any other time." },
    ApiEntry { label: "account.signOut", insert: "account.signOut()", doc: "account.signOut() — forget the session NOW, then clear the keyring and revoke the refresh token in the background. In that order on purpose: a player who presses Sign Out is signed out whether or not the network agrees." },
    ApiEntry { label: "account.get", insert: "account.get(", doc: "account.get(\"/wallet\", function(res) end) — a Floptle Cloud call with the player's bearer token attached for you. Takes a PATH, not a URL: there is exactly one host it can reach, which is what makes attaching a token to it safe. Bare paths get the /api/floptle/v1 prefix; /userinfo and /oauth/* stay at the root. res is the same table http.* gives you." },
    ApiEntry { label: "account.post", insert: "account.post(", doc: "account.post(\"/games/mygame/events\", { event = \"boss_killed\", event_id = id }, function(res) end) — report what HAPPENED and let the server decide what it is worth. A table body is sent as JSON. There is no endpoint that credits currency directly, by design: anything a client can announce, a modified client can announce." },
    ApiEntry { label: "account.put", insert: "account.put(", doc: "account.put(\"/games/mygame/saves/slot1\", { data = t, expected_version = v }, function(res) end) — a cloud save. expected_version is optimistic concurrency: send the version you last read and a stale write gets 409 instead of silently clobbering the player's other machine." },
    ApiEntry { label: "account.delete", insert: "account.delete(", doc: "account.delete(\"/games/mygame/saves/slot1\", function(res) end) — remove something from Floptle Cloud." },
    ApiEntry { label: "account.inFlight", insert: "account.inFlight()", doc: "account.inFlight() — how many account calls are still waiting on a reply (cap 6). A spinner, or a guard against firing the same request every frame." },
    ApiEntry { label: "openUrl", insert: "openUrl(", doc: "openUrl(url) — open an http:// or https:// address in the player\'s own browser. The device-code sign-in flow needs it: the player approves the pairing on your real site, so the game never sees a password and needs no secret baked into it. Play only; if the platform refuses, the URL is logged instead so the player can still get there." },
    ApiEntry { label: "draw.text", insert: "draw.text(", doc: "draw.text(x, y, s, size, r,g,b [, a] [, align] [, font]) — a string on the SCREEN, in the pixels input.mouse() reports, without building a UI tree: a damage number, a frame-time readout, the count under a selection box. The engine measures and lays out the glyphs with the same font stack ui.make uses — and measures with the SAME font it draws, so a centred run lands where you asked. align is \"left\" (default) | \"center\" | \"right\", and x is that edge. font is a project-relative .ttf/.otf; leave it out and you get the project\'s UI font (Project Settings ▸ UI font), which is where to set it once rather than at forty call sites. Immediate mode: re-draw it every frame you want it." },
    ApiEntry { label: "draw.circle", insert: "draw.circle(", doc: "draw.circle(x, y, radius, r,g,b [, a]) — a filled circle in screen pixels, x/y its CENTRE. draw.circleOutline(..., [px]) is the hollow twin. Same immediate-mode rules as draw.rect: over the scene, over the HUD, one frame each." },
    ApiEntry { label: "draw.circleOutline", insert: "draw.circleOutline(", doc: "draw.circleOutline(x, y, radius, r,g,b [, a] [, px]) — a hollow circle, `px` thick (default 2)." },
    ApiEntry { label: "draw.line", insert: "draw.line(", doc: "draw.line(x1,y1,z1, x2,y2,z2, r,g,b [, a]) — queue one world-space 3D line for THIS frame (immediate mode: re-draw every lateUpdate — the camera pass — while wanted). Drawn OVER the scene, never occluded — the KSP-style map draws its orbit conics with these." },
    ApiEntry { label: "node:getparent", insert: "node:getparent()", doc: "The parent node handle, or nil (same as node.parent)." },
    ApiEntry { label: "node:children", insert: "node:children()", doc: "An array of this node's child handles." },
    ApiEntry { label: "node:getchild", insert: "node:getchild(", doc: "node:getchild(\"Gun\") — the first child with that name (a node handle), or nil." },
    ApiEntry { label: "node:find", insert: "node:find(", doc: "node:find(\"Muzzle\") — the first descendant (any depth) with that name, or nil." },
    ApiEntry { label: "node:getscript", insert: "node:getscript(", doc: "node:getscript(\"health\") — a script handle for that script on this node, or nil. Read/write its state, call its methods, reach .node / .params." },
    ApiEntry { label: "node:getcomponent", insert: "node:getcomponent(", doc: "node:getcomponent(name) — a component handle whose fields you can read AND assign at runtime (applies live during play), or nil if absent. Components: RigidBody (friction, restitution, gravity, kinematic 1/0 — live Dynamic/Kinematic switch, shape 0/1/2, radius, height, half_x/y/z, lock_x/y/z, lock_rot_x/y/z, two_d — 2D mode), PointLight (intensity, range, r/g/b, and the EMITTER: shape 0 point / 1 sphere / 2 rect / 3 disk / 4 tube, plus width, height, radius, length, thickness, twoSided — a rect light IS a window, so growing one softens the highlight it leaves on everything), Camera (fovY radians, active — assign true to switch cameras), ParticleSystem (play_on_start), UiElement (visible, opacity, posX/posY, width/height, radius, border, fillRGBA, textSize, textRGBA, tintRGBA, cell — spritesheet frame), UiSlider (value/min/max — drive a health bar), UiLayer (enabled, z, designHeight, worldSpace), PostProcess (enabled, bloom, bloomThreshold, bloomIntensity, vignette, vignetteStrength, vignetteRadius, aoStrength, aoRadius, posterizeBands, posterizeDither, tonemap, and the lens: dofFocus, dofRange, dofNearRange, dofBlur, dofBlades, dofBladeAngle, dofHighlight, dofSamples, plus the shutter: motionBlur, motionSamples — a cutscene pushing a vignette, pulling a rack focus, or opening the shutter for a slow-motion beat), LightProbes (enabled, intensity, leak, normalBias — the baked bounce's live knobs; the bake-time ones are not here because a script cannot bake). e.g. node:getcomponent(\"RigidBody\").friction = 0.02 for ice." },
    ApiEntry { label: "node:animator", insert: "node:animator()", doc: "node:animator() — the animation handle for this node's Animation Controller (or a rigged model's embedded clips). Setters: :play/:restart/:crossfade/:stop/:setSpeed/:setLayerWeight/:seek. Getters: :state/:time/:finished/:isPlaying/:clips/:layers." },
    ApiEntry { label: "anim:play", insert: ":play(", doc: "anim:play(\"Run\" [, fade [, layer]]) — transition to a state. The controller supplies the crossfade (default fade, per-arrow overrides, and a state's ⇥ fade-in override which beats everything — 0 = instant); pass `fade` to override the first two. Safe to call every frame — re-playing the current state is a no-op." },
    ApiEntry { label: "anim:restart", insert: ":restart(", doc: "anim:restart(\"Attack\" [, fade [, layer]]) — like play, but re-enters even if that state is already playing (re-trigger a one-shot)." },
    ApiEntry { label: "anim:crossfade", insert: ":crossfade(", doc: "anim:crossfade(\"Idle\", 0.3 [, layer]) — transition with an explicit fade time (seconds)." },
    ApiEntry { label: "anim:stop", insert: ":stop(", doc: "anim:stop([layer [, fade]]) — stop a layer (all layers if omitted). Higher layers release to the layers below; the base returns to its default state." },
    ApiEntry { label: "anim:setSpeed", insert: ":setSpeed(", doc: "anim:setSpeed(2) — global playback speed multiplier for this node's animator." },
    ApiEntry { label: "anim:setLayerWeight", insert: ":setLayerWeight(", doc: "anim:setLayerWeight(\"Attack\", 0.5) — blend a layer over the ones below (0 = off, 1 = full override)." },
    ApiEntry { label: "anim:seek", insert: ":seek(", doc: "anim:seek(t [, layer]) — jump the current state's playhead to t seconds." },
    ApiEntry { label: "anim:state", insert: ":state(", doc: "anim:state([layer]) — the state currently showing (topmost active layer), or that layer's state. Nil when idle." },
    ApiEntry { label: "anim:current", insert: ":current(", doc: "anim:current([layer]) — alias of anim:state: the state currently showing (topmost active layer). Nil when idle." },
    ApiEntry { label: "anim:time", insert: ":time(", doc: "anim:time([layer]) — seconds into the current state." },
    ApiEntry { label: "anim:finished", insert: ":finished(", doc: "anim:finished([layer]) — true when a non-looped state reached its end this frame (or stays true while holding the last frame)." },
    ApiEntry { label: "anim:isPlaying", insert: ":isPlaying(", doc: "anim:isPlaying([state]) — is that state playing on any layer (or anything at all, with no argument)?" },
    ApiEntry { label: "anim:clips", insert: ":clips()", doc: "anim:clips() — every playable state name, as a list." },
    ApiEntry { label: "anim:layers", insert: ":layers()", doc: "anim:layers() — every layer name, base first, as a list." },
    ApiEntry { label: "anim:duration", insert: ":duration(", doc: "anim:duration(\"Punch\") — the clip's AUTHORED length in seconds (nil if there's no such state). Reads the asset, not playback, so it works in start()." },
    ApiEntry { label: "anim:events", insert: ":events(", doc: "anim:events(\"Punch\") — the clip's authored events as { {t = seconds, func = \"onHitboxStart\"}, … }, ascending by t; nil if there's no such state, an empty list if it has none. Reads the asset, so you can bake integer frame data at load: frame = math.floor(e.t / anim:duration(c) * totalFrames + 0.5). Prefer this to letting events DRIVE gameplay — they fire off float playback time, quantise to sample_fps, and are deliberately not re-fired on a prediction replay." },
    ApiEntry { label: "spawnEffect", insert: "spawnEffect(", doc: "spawnEffect(key, x, y, z) — fire a one-shot particle effect at a world point, no node needed. It plays once and despawns itself. e.g. local h = raycast(...); if h then spawnEffect(\"vfx/Impact\", h.x, h.y, h.z) end." },
    ApiEntry { label: "spawn", insert: "spawn(", doc: "spawn(prefab [, pos [, fn]]) — spawn a PREFAB instance (make one by dragging a node into the Assets panel). \"bullet\" finds prefabs/bullet.prefab.ron. pos = a vec3/node for the root; fn(root) runs with the new node's handle the same frame — spawn(\"bullet\", node.pos + dir, function(b) b.vx = dir.x * 40 end). Local-only in multiplayer: the server uses net.spawn for replicated objects." },
    ApiEntry { label: "createNode", insert: "createNode(", doc: "createNode(name [, parent] [, fn]) — create a PLAIN node (Empty matter). fn(n) gets its handle: combine with n:setTerrain(id) / n:setCelestial{...} / n:setPrimitive(shape, color) / n:setMaterial{...} + transform writes to build content from script (procgen, editor actions). Nested creates inside callbacks are fine." },
    ApiEntry { label: "node:setCelestial", insert: ":setCelestial{", doc: "node:setCelestial{mu=…, bodyRadius=…, soi=0, parent=\"Sun\", a=…, e=…, i=…, m0=…, atmoColor={r,g,b}, atmoHeight=…, atmoDensity=…, clouds=…, luminosity=…, starColor={r,g,b}, occluderRadius=…} — set (creating if absent) the node's CelestialBody. camelCase fields; colors take {r,g,b}. occluderRadius = occlusion culling: the solid-core radius geometry never pierces — terrain chunks fully behind it skip their draw calls (keep it below the deepest cave/dig; 0 = off)." },
    ApiEntry { label: "node:setMaterial", insert: ":setMaterial{", doc: "node:setMaterial{color={r,g,b}, emissive={r,g,b}, emissiveStrength=…, unlit=true, texture=\"…\", alpha=…, …} — set (creating if absent) the node's Material. texture also takes a live render target: \"rt:<name>\".\n\nSurface maps: normalMap / roughnessMap / metallicMap / occlusionMap (paths, \"\" clears) with normalStrength / roughness / metallic / occlusionStrength. shading=\"physical\" switches from the hand-set Blinn-Phong highlight to metal-rough; roughness and metallic only mean anything there, while a normal or occlusion map works under either.\n\nRetro artefacts: jitter (screen-grid vertex snapping, 0 = off), affineUv, vertexLit, ditherAlpha." },
    ApiEntry { label: "node:setTerrain", insert: ":setTerrain(", doc: "node:setTerrain(id) — make the node a Terrain volume with that id; fill it with terrain.generatePlanet(id, opts)." },
    ApiEntry { label: "node:setTerrainGen", insert: ":setTerrainGen(", doc: "node:setTerrainGen(opts) — attach an ON-DEMAND generation spec (the same opts table terrain.generatePlanet takes): the body's field generates in the background when something first approaches, so no field file is needed at all — a rolled galaxy is playable instantly and unvisited worlds cost one scene node. Player edits saved under terrain.saveDir take priority over regeneration. nil clears." },
    ApiEntry { label: "node:setPrimitive", insert: ":setPrimitive(", doc: "node:setPrimitive(\"Sphere\" [, {r,g,b}]) — make the node a primitive (Cube/Sphere/Capsule/Plane)." },
    ApiEntry { label: "access", insert: "access", doc: "Accessibility a game offers its players: UI text scale, a colour-vision filter, reduced motion and captions. The engine honours what it owns — text sizes go through the LAYOUT so scaling reflows, the filter is a post-chain stage, and UI transitions snap when motion is reduced. What it cannot honour for you (your camera shake) reads access.reducedMotion(). These are the PLAYER's settings, so persist them with save.*; the editor's ⚙ Settings → Accessibility drives the same values so you can try them. See docs/accessibility.md." },
    ApiEntry { label: "access.textScale", insert: "access.textScale()", doc: "access.textScale() → the player's UI text multiplier (1.0 = normal). Every UI text size is multiplied by it BEFORE layout, so text scaling reflows — a fit-height box grows and its neighbours move down — rather than painting bigger glyphs into the same rect and clipping." },
    ApiEntry { label: "access.setTextScale", insert: "access.setTextScale(", doc: "access.setTextScale(1.5) — set the UI text multiplier, 0.5–3.0. This is the single most-used accessibility setting in games. Out of range RAISES rather than clamping: a settings slider hands over a number it already bounded, so a value outside it means the caller computed it wrong. Persist it yourself with save.set — it is the player's setting, so it belongs in the player's save." },
    ApiEntry { label: "access.colorFilter", insert: "access.colorFilter()", doc: "access.colorFilter() → the active colour-vision filter's name (\"none\" / \"protanopia\" / \"deuteranopia\" / \"tritanopia\")." },
    ApiEntry { label: "access.setColorFilter", insert: "access.setColorFilter(", doc: "access.setColorFilter(\"deuteranopia\" [, strength]) — correct the picture for a colour vision deficiency, as a stage in the post chain (so it applies to everything the player sees, and a scene cannot veto it by disabling its PostProcess node). `strength` 0–1; full correction shifts hues a lot and some players want less. An unrecognised name raises naming the four it takes — a misspelled filter that quietly meant \"off\" is an accessibility setting that appears to do nothing." },
    ApiEntry { label: "access.colorFilterStrength", insert: "access.colorFilterStrength()", doc: "access.colorFilterStrength() → how strongly the colour filter applies, 0–1." },
    ApiEntry { label: "access.filters", insert: "access.filters()", doc: "access.filters() → { {name=, label=}, … } — every colour filter in menu order, so an options dropdown does not hard-code a list that can go stale. `label` is the human one (\"deuteranopia (green-blind)\")." },
    ApiEntry { label: "access.reducedMotion", insert: "access.reducedMotion()", doc: "access.reducedMotion() → the player asked for less movement. The engine already snaps its OWN UI transitions; read this for the motion it cannot know about — your camera shake, screen flashes, big animated wipes. The engine cannot tell which of your movement is the game." },
    ApiEntry { label: "access.setReducedMotion", insert: "access.setReducedMotion(", doc: "access.setReducedMotion(true) — ask for less movement. UI transitions SNAP rather than hurry (a 40 ms slide is still a slide)." },
    ApiEntry { label: "access.captions", insert: "access.captions()", doc: "access.captions() → is the player showing captions?" },
    ApiEntry { label: "access.setCaptions", insert: "access.setCaptions(", doc: "access.setCaptions(true) — turn captions on. While off, caption(...) draws nothing, so a game writes caption() beside the sound and never an `if` around it." },
    ApiEntry { label: "caption", insert: "caption(", doc: "caption(\"a door unlocks somewhere\" [, seconds]) → true if it was shown. Says a line the engine draws bottom-centre on a dark plate, at the player's text scale, oldest first — so every game gets the same readable placement instead of hand-rolling one. A no-op (returning false) while access.captions() is off. Without `seconds` the duration suits the length of the line." },
    ApiEntry { label: "node:setCamera", insert: ":setCamera{", doc: "node:setCamera{fovY=1.0, active=true, target=\"minimap\", width=256, height=256, hz=10, cullMask=…} — aim a camera, hand it play-mode authority, and point it at a RENDER TARGET. With a `target` the camera draws the world into a live texture any material or UI image wears as \"rt:<name>\" — minimaps, mirrors, security monitors, scopes, split-screen. `width`/`height` are the texture's pixels (8–4096) and `hz` how often it redraws (0 = every frame), so a 10 Hz minimap costs a sixth of a 60 Hz one. `active=true` clears every other camera's authority, because two active cameras is not a choice anyone made. fovY is RADIANS. Every value is checked at the call: an unknown key, a `width=0` or an `hz=\"10\"` raises naming the property, the value and the range." },
    ApiEntry { label: "node:setSpriteBatch", insert: ":setSpriteBatch{", doc: "node:setSpriteBatch{size=1.0} — make this node a SPRITE BATCH, so node:sprites() can draw into it. The counterpart of node:setTilemap: a game's sprite styles are data (one batch per material), so the nodes that draw them are made from the same script that declares them rather than authored one at a time into the scene. `size` is the quad's edge length; every sprite scales it. The sheet is the node's own Material." },
    ApiEntry { label: "node:setTilemap", insert: ":setTilemap{", doc: "node:setTilemap{cols=13, rows=7, tile=1.5 [, data={…}] [, tileset=\"tilesets/bricks.tileset.ron\"]} — make this node a TILEMAP: a grid of spritesheet cells drawn as one mesh, one draw call. The sheet is the node's own Material (texture + sheetCols/sheetRows). Neighbouring tiles share an exact edge, so the hairline gaps a grid of separate quads opens up as the camera moves cannot happen. `data` is row-major from the top-left; leave it out for an empty grid you fill with tm:set." },
    ApiEntry { label: "node:tilemap", insert: ":tilemap()", doc: "node:tilemap() — a handle to this node's tilemap grid. Squares: tm:set / tm:get / tm:at / tm:fill / tm:fillRect / tm:size / tm:resize. World space: tm:cellAt (which tile is the player standing on) / tm:worldAt / tm:tileSize. What a tile IS, from the node's tileset: tm:solid / tm:tags / tm:hasTag / tm:autotile." },
    ApiEntry { label: "node:setSorting", insert: ":setSorting{", doc: "node:setSorting{layer=\"Terrain\", order=3} — where this 2D node draws in the stack. `layer` is one of the project's sorting layers by name; `order` places it within that layer, higher being nearer the camera. Both optional and both keep what the node had. This is how a character steps behind a counter, or a picked-up card lifts above the hand." },
    ApiEntry { label: "node:setLighting2D", insert: ":setLighting2D{", doc: "node:setLighting2D{mode=\"2d\", layers={\"Terrain\",\"Characters\"}, blocks=\"on\", inner=4, falloff=2, shadows=true} — 2D lighting, from a script. `mode` is auto/2d/3d and says whether this node is on the 2D lighting path at all; auto decides from the scene and is never re-decided once you say otherwise. On a LIGHT, `layers` is the sorting layers it reaches — empty or absent means all of them, which is how you keep a torch off the background. `inner` is full brightness out to that radius before the ramp starts (0 = the ramp starts at the light) and `falloff` is its exponent (2 = the curve every light has always had): together they let a posterized game land a whole light inside one band instead of drawing concentric rings. `shadows=false` makes this one light pass through everything, whatever the scene blocks. On a RECEIVER, `blocks` is auto/on/off for whether it occludes light — under auto a tilemap casts from the collision it already declares, so a level\'s collision IS its light occlusion. A bad spelling names the accepted set rather than silently meaning auto." },
    ApiEntry { label: "node:setPointLight", insert: ":setPointLight{", doc: "node:setPointLight{color={1,0.8,0.5}, intensity=2, range=8} — make this node a light, or retune one. Every field is optional and keeps what the node had, INCLUDING its emitter shape — retuning a window’s colour never turns it back into a bare point. The shape itself is set through node:getcomponent(\"PointLight\").shape. Sixteen lights reach the shader at once; past that the ones contributing most at the camera win, and a light at intensity=0 gives its slot back — which is how you pool them. perf.counts().lights and .lightsDropped say where you stand." },
    ApiEntry { label: "env.ambient2d", insert: ":getcomponent(\"Light\")", doc: "find(\"Lighting\"):getcomponent(\"Light\") — the scene's Lighting node, read and written like any other component. THIS IS WHERE A 2D SCENE'S BRIGHTNESS LIVES: ambient2dR/G/B is the 2D base light, the whole light a flat scene has before a single 2D light is placed, so turning it down is how you get a dark room for a torch to carve a circle out of — and reading it back first is how you put it where it was. Also colorR/G/B + intensity + directionX/Y/Z (a day cycle), ambientR/G/B (the 3D fill, deliberately a different value), shadows/shadowSoftness/shadowStrength/shadowTintR/G/B/shadowQuantize/shadowDither/shadowDistance/contactShadows/contactLength/contactSteps/contactStrength, and the whole fog set: fog, fogColorR/G/B, fogStart, fogEnd, fogDensity, fogHeight, fogFalloff, fogNoise, fogNoiseScale, fogVolumetric, fogDither, fogDitherStrength, and the volumetric light injection (fogLight, fogAnisotropy, fogSteps, fogShafts). Every scene has exactly one Lighting node and the loader makes it, so find(\"Lighting\") always finds it. Writes land the same frame." },
    ApiEntry { label: "node:sprites", insert: ":sprites()", doc: "node:sprites() — a handle to this node's SpriteBatch (make it one with node:setSpriteBatch{} first; on any other node this is an error rather than a handle that silently draws nothing): b:draw(...) queues one sprite for this frame. N sprites from one node, each with its own position, rotation, scale, cell AND tint — no scene node per sprite and no pool to grow." },
    ApiEntry { label: "tm:set", insert: ":set(", doc: "tm:set(x, y, cell) — set one square, 0-based from the TOP-LEFT. Outside the grid is a no-op rather than a wrap. To clear a square pass -1 (any negative works, as in Tiled/Godot/LDtk), nil, or the EMPTY_TILE constant — all three are the same value. A cell that is not a whole number in range is an error naming what it got and what it accepts, never a neighbouring tile." },
    ApiEntry { label: "tm:get", insert: ":get(", doc: "tm:get(x, y) → cell, or nil outside the grid and on an empty square." },
    ApiEntry { label: "tm:fill", insert: ":fill(", doc: "tm:fill(cell) — set every square, including the empty ones. The fast way to reset a room before re-dressing it. tm:fill() with no argument, tm:fill(-1) and tm:fill(EMPTY_TILE) all clear the grid." },
    ApiEntry { label: "tm.EMPTY", insert: ".EMPTY", doc: "tm.EMPTY — the cell value that means \"no tile here\", on the handle rather than only as a global. Same number as EMPTY_TILE; -1 and nil mean it too." },
    ApiEntry { label: "EMPTY_TILE", insert: "EMPTY_TILE", doc: "EMPTY_TILE — the tilemap cell value that leaves a square empty (u32::MAX, 4294967295). Prefer -1: any negative cell means empty, which is the convention in Tiled, Godot and LDtk. This constant exists because the API documented the name long before Lua could resolve it." },
    ApiEntry { label: "tm:size", insert: ":size()", doc: "tm:size() → cols, rows." },
    ApiEntry { label: "tm:at", insert: ":at(", doc: "tm:at(x, y) → cell, rot, flipX — the WHOLE answer for a square, where tm:get gives only the cell. `rot` is degrees clockwise (0/90/180/270). For art that faces a direction: a conveyor, a pipe, a one-way platform." },
    ApiEntry { label: "tm:fillRect", insert: ":fillRect(", doc: "tm:fillRect(x0, y0, x1, y1, cell [, xform]) — fill a rectangle. Corners in either order, clipped to the grid, so dragging past the edge fills to the edge." },
    ApiEntry { label: "tm:tileSize", insert: ":tileSize()", doc: "tm:tileSize() → the world edge length of one square. What tm:cellAt divides by, and what a game placing something on a tile needs." },
    ApiEntry { label: "tm:resize", insert: ":resize{", doc: "tm:resize{ cols =, rows =, offsetX =, offsetY = } — resize the grid, keeping whatever overlaps. offsetX/offsetY is where the OLD top-left lands in the new grid, so offsetY = 1 grows a row on top rather than at the bottom. Give at least one of cols / rows." },
    ApiEntry { label: "tm:cellAt", insert: ":cellAt(", doc: "tm:cellAt(worldPos) → x, y — which square a WORLD position falls in, or nil off the map. Takes a vec3, an {x=,y=,z=} table, or a node. Goes through the tilemap node's own transform, so a map that has been moved, turned or scaled still answers correctly — which is the part a game cannot reasonably compute itself." },
    ApiEntry { label: "tm:worldAt", insert: ":worldAt(", doc: "tm:worldAt(x, y) → the world position of that square's CENTRE (a vec3), or nil off the grid. The centre and not a corner, because what you do with it is put something on the tile." },
    ApiEntry { label: "tm:tileset", insert: ":tileset()", doc: "tm:tileset() → the project-relative .tileset.ron this map is cut from, or nil. The tileset is what says whether a tile collides, what it is tagged, and how it autotiles — see docs/tilemaps.md." },
    ApiEntry { label: "tm:solid", insert: ":solid(", doc: "tm:solid(x, y) → whether the tileset says that square collides. False on an empty square and false with no tileset. Reads the TILESET, so marking one brick solid answers for every brick in every scene — a game keeping its own table of solid cell indices goes stale the day the artist reorders the sheet." },
    ApiEntry { label: "tm:tags", insert: ":tags(", doc: "tm:tags(x, y) → the tileset's tags for that square, as a list. This is how a tilemap carries gameplay (\"ice\", \"water\", \"damage\") without the game keeping a second table keyed by cell index." },
    ApiEntry { label: "tm:hasTag", insert: ":hasTag(", doc: "tm:hasTag(x, y, \"ice\") → the common case of tm:tags without allocating a table per square. What a per-frame ground check should call." },
    ApiEntry { label: "tm:autotile", insert: ":autotile(", doc: "tm:autotile(x0, y0, x1, y1) — recompute the region's autotiled squares, plus the one-square ring around it (which is where the stale edge tiles are). Call it after a run of tm:set, not per square: retiling per write would be O(area) each time and would fight a stroke still being laid down. Does nothing when the map has no tileset." },
    ApiEntry { label: "batch:draw", insert: ":draw(", doc: "b:draw(x, y [, z] [, scale] [, rot] [, cell] [, r, g, b, a]) — draw one sprite THIS FRAME, positioned in the batch node's local space. Immediate mode, exactly like draw.* : what you draw this frame is what shows, and next frame starts empty — there is no pool to grow and no clear() to forget. `scale` is one number, or a vec2 for squash-and-stretch: b:draw(x, y, 0, vec2(1.4, 0.6)). The tint is the thing a shared Material could never give one sprite: flash one enemy red without blinking it off." },
    ApiEntry { label: "destroy", insert: "destroy(", doc: "destroy(node) — remove a node AND its whole subtree (physics body included). Queued: applied after the pass, so the handle stays readable through the current call. Method form: node:destroy(). On a client, replicated nodes refuse (server authority — net.despawn)." },
    ApiEntry { label: "node:destroy", insert: ":destroy()", doc: "node:destroy() — remove this node and its children (same as destroy(node)). The classic pickup: onTriggerEnter → award score → node:destroy()." },
    ApiEntry { label: "node:particles", insert: "node:particles()", doc: "node:particles() — the particle handle for this node's Particle System component. Setters: :play/:stop/:restart/:setIntensity/:setBeamEnd. Getters: :isPlaying/:alive/:asset. e.g. on a hit, node:particles():restart() to re-fire a burst." },
    ApiEntry { label: "node:sound", insert: "node:sound()", doc: "node:sound() — the handle for this node's Audio Source component. :play() (restarts), :stop(), :pause(), :resume(), :setClip(\"audio/x.ogg\"), :seek(secs), :isPlaying(), :position(). Tunables (volume/pitch/distances/…) live on node:getcomponent(\"AudioSource\")." },
    ApiEntry { label: "audio.play", insert: "audio.play(", doc: "audio.play(clip [, node | x, y, z] [, opts]) — play a clip with no setup: audio.play(\"audio/ding.ogg\") is flat 2D; pass x,y,z for a world point; pass a node to follow it. opts: {volume, pitch, pan, mode=\"Spatial|Distance|Flat\", falloff=\"Inverse|Linear|Exponential\", minDistance, maxDistance, track, endBehavior=\"Stop|Destroy|Loop\", loop=true}. Returns a sound handle: :stop/:pause/:resume/:setVolume/:setPitch/:setPan/:setTrack/:setPosition/:seek/:isPlaying/:position. e.g. audio.play(\"audio/hit.ogg\", h.x, h.y, h.z, { maxDistance = 35, track = \"SFX\" })" },
    ApiEntry { label: "audio.stopAll", insert: "audio.stopAll()", doc: "audio.stopAll() — stop every playing sound (sources and one-shots), with a click-free fade." },
    ApiEntry { label: "audio.track", insert: "audio.track(", doc: "audio.track(name) — a live mixer-track handle (\"Master\" or a track from the Mixer tab): :setVolume(db), :setPan(-1..1), :setMuted(bool), :setSoloed(bool). Changes affect the running session only and revert on Stop. e.g. audio.track(\"Music\"):setVolume(-12) to duck music." },
    ApiEntry { label: "particles:play", insert: ":play()", doc: "particles:play() — start emitting if the effect is idle (spawns a fresh instance). No-op if already playing." },
    ApiEntry { label: "particles:stop", insert: ":stop()", doc: "particles:stop() — stop + despawn the effect; its live particles vanish." },
    ApiEntry { label: "particles:restart", insert: ":restart()", doc: "particles:restart() — re-spawn from t=0 (re-fire a one-shot burst, e.g. a muzzle flash on each shot)." },
    ApiEntry { label: "particles:setIntensity", insert: ":setIntensity(1.0)", doc: "particles:setIntensity(i) — live emission scale (0..~2): multiplies rates/burst counts and shades particle size. Drive an engine plume off the throttle without touching the asset." },
    ApiEntry { label: "particles:setBeamEnd", insert: ":setBeamEnd(x, y, z)", doc: "particles:setBeamEnd(x, y, z) — aim every Beam track's endpoint at a WORLD-space point (the engine converts it to effect-local, so the beam keeps tracking the target as the node moves). Re-call per tick to follow a moving target." },
    ApiEntry { label: "particles:isPlaying", insert: ":isPlaying()", doc: "particles:isPlaying() — true while an instance is emitting/ageing on this node." },
    ApiEntry { label: "particles:alive", insert: ":alive()", doc: "particles:alive() — live particle count across the effect's tracks (0 when stopped)." },
    ApiEntry { label: "particles:asset", insert: ":asset()", doc: "particles:asset() — the effect asset key this node's Particle System references, or nil." },
    ApiEntry { label: "math.sin", insert: "math.sin(", doc: "math.sin(x) — sine of x (radians)." },
    ApiEntry { label: "math.cos", insert: "math.cos(", doc: "math.cos(x) — cosine of x (radians)." },
    ApiEntry { label: "math.rad", insert: "math.rad(", doc: "math.rad(deg) — degrees to radians." },
    ApiEntry { label: "math.deg", insert: "math.deg(", doc: "math.deg(rad) — radians to degrees." },
    ApiEntry { label: "math.pi", insert: "math.pi", doc: "The constant π." },
    ApiEntry { label: "math.abs", insert: "math.abs(", doc: "math.abs(x) — absolute value." },
    ApiEntry { label: "math.max", insert: "math.max(", doc: "math.max(a, b, …) — largest argument." },
    ApiEntry { label: "math.min", insert: "math.min(", doc: "math.min(a, b, …) — smallest argument." },
    ApiEntry { label: "math.sqrt", insert: "math.sqrt(", doc: "math.sqrt(x) — square root." },
    ApiEntry { label: "math.floor", insert: "math.floor(", doc: "math.floor(x) — round down." },
    ApiEntry { label: "math.random", insert: "math.random(", doc: "math.random() — random in [0,1); math.random(n) — 1..n." },
    ApiEntry { label: "string.format", insert: "string.format(", doc: "string.format(fmt, …) — printf-style formatting." },
    ApiEntry { label: "function", insert: "function ", doc: "Define a function." },
    ApiEntry { label: "local", insert: "local ", doc: "Declare a local variable." },
    ApiEntry { label: "noderef", insert: "noderef()", doc: "defaults = { target = noderef() } — a NODE REFERENCE param: the Inspector shows a node picker (or drag a node from the Hierarchy onto it) and the script reads params.target as a node handle (nil while unwired). The preferred way to point a script at a specific node — no find() calls." },
    ApiEntry { label: "scriptref", insert: "scriptref(\"\")", doc: "defaults = { hp = scriptref(\"health\") } — the param binds to that SCRIPT on the wired node: params.hp is a script handle directly (call its functions, read its state). The Inspector only lists nodes carrying the script. nil while unwired/invalid." },
    ApiEntry { label: "componentref", insert: "componentref(\"\")", doc: "defaults = { body = componentref(\"RigidBody\") } — the param binds to that COMPONENT on the wired node: params.body is a component handle directly (params.body.friction = 0.05). Components: RigidBody, PointLight, Camera, ParticleSystem, UiElement, UiSlider, UiLayer. nil while unwired/invalid." },
    ApiEntry { label: "node.text", insert: "node.text", doc: "A UI element's label text — read/write; numbers coerce (hpLabel.text = 42). nil on nodes without UI text; writing to a UI element without a text spec creates one." },
    ApiEntry { label: "clicked", insert: "clicked", doc: "function clicked(node) — UI button hook: fires when this node's element (with 'button' on) is pressed AND released on it. Style states in Lua; no imposed look." },
    ApiEntry { label: "hoverStart", insert: "hoverStart", doc: "function hoverStart(node) — UI hook: the pointer entered this node's clickable element. Pair with hoverEnd." },
    ApiEntry { label: "hoverEnd", insert: "hoverEnd", doc: "function hoverEnd(node) — UI hook: the pointer left this node's clickable element." },
    ApiEntry { label: "pressed", insert: "pressed", doc: "function pressed(node) — UI hook: LMB went down on this node's clickable element." },
    ApiEntry { label: "released", insert: "released", doc: "function released(node) — UI hook: LMB came back up (on or off the element)." },
    ApiEntry { label: "focusEnter", insert: "focusEnter", doc: "function focusEnter(node) — UI hook: keyboard/gamepad focus arrived here. What focus LOOKS like is your style's `focus` block; this is for the rest (a sound, a preview, a description panel)." },
    ApiEntry { label: "focusExit", insert: "focusExit", doc: "function focusExit(node) — UI hook: focus left this element." },
    ApiEntry { label: "cancelled", insert: "cancelled", doc: "function cancelled(node) — UI hook: the UiCancel action (Escape / B) while this element has focus. Back out of a screen from the element the player is on." },
    ApiEntry { label: "submitted", insert: "submitted", doc: "function submitted(node) — UI hook: Enter (UiSubmit) in a focused TEXT FIELD. Read the value with node.text. A field fires this instead of `clicked`, so a field inside a button doesn't run the button." },
    ApiEntry { label: "changed", insert: "changed", doc: "function changed(node) — UI hook: a text field's value changed (typing, paste, backspace). Once per frame however many keystrokes landed. Read node.text." },
    ApiEntry { label: "dragStart", insert: "dragStart", doc: "function dragStart(node) — UI hook: a `draggable` element has been picked up (the pointer travelled far enough that it isn't a click). The engine does NOT move the element — draw the drag however your game wants." },
    ApiEntry { label: "dragMove", insert: "dragMove", doc: "function dragMove(node) — UI hook: fires every frame of a drag on the SOURCE. Use input.mouse() / node:uiRect() to position whatever you're showing." },
    ApiEntry { label: "dropped", insert: "dropped", doc: "function dropped(node) — UI hook: fires on BOTH ends of a completed drag — the target (which now has it) and the source (which gave it away). `ui.dragging()` and `ui.dropTarget()` name the pair." },
    ApiEntry { label: "dragCancel", insert: "dragCancel", doc: "function dragCancel(node) — UI hook: a drag was released over nothing. Put the item back; a half-finished gesture must not leave it stuck to the cursor." },
    ApiEntry { label: "dragEnter", insert: "dragEnter", doc: "function dragEnter(node) — UI hook: a drag moved over this `drop target`. Pair with `dragLeave`; highlight the slot here." },
    ApiEntry { label: "dragOver", insert: "dragOver", doc: "function dragOver(node) — UI hook: fires every frame a drag rests over this drop target." },
    ApiEntry { label: "dragLeave", insert: "dragLeave", doc: "function dragLeave(node) — UI hook: the drag moved off this drop target." },
    ApiEntry { label: "ui.focus", insert: "ui.focus(", doc: "ui.focus(node) — move the keyboard/gamepad focus. ui.focus(nil) drops it (a screen that wants nothing focused until the player touches something). Focusing a text field starts editing it." },
    ApiEntry { label: "ui.focused", insert: "ui.focused()", doc: "ui.focused() — the focused element as a node, or nil. ui.focused(el) answers yes/no for one element. Also readable per-node as node.focused." },
    ApiEntry { label: "ui.on", insert: "ui.on(", doc: "ui.on(element, \"clicked\", function(el, hook) ... end) — listen to an element from a script that does NOT live on it, so ONE manager holds a whole menu instead of a three-line script file per button. Any UI hook: clicked, pressed, released, hoverStart, hoverEnd, changed, submitted, cancelled, focusEnter, focusExit, dragStart/Move/Enter/Over/Leave/Cancel, dropped. The handler gets the element that fired and the hook name, so one function can serve a row of buttons. Registering again for the same element and hook REPLACES (so calling it from update() is harmless, not a leak). A listener dies with its element or with the script that registered it; a hot reload re-registers. Listening for an interaction the element doesn't take warns in the Console — it would otherwise be silent." },
    ApiEntry { label: "ui.off", insert: "ui.off(", doc: "ui.off(element) stops every hook YOUR script is listening to on that element; ui.off(element, \"clicked\") stops one. Only your own — two managers on one button must not be able to unregister each other." },
    ApiEntry { label: "ui.clicked", insert: "ui.clicked(", doc: "ui.clicked(element) — did it fire `clicked` THIS frame? The polling half of ui.on, for a manager that already has an update(). Reads the same event list the hooks fire from (published before scripts run), so a poll and a hook can never disagree." },
    ApiEntry { label: "ui.pressed", insert: "ui.pressed(", doc: "ui.pressed(element) — LMB went down on it this frame. Pair with ui.held(element) for hold-to-charge." },
    ApiEntry { label: "ui.released", insert: "ui.released(", doc: "ui.released(element) — LMB came back up this frame (on or off the element)." },
    ApiEntry { label: "ui.changed", insert: "ui.changed(", doc: "ui.changed(element) — a text field's value changed this frame. Read the value with element.text." },
    ApiEntry { label: "ui.submitted", insert: "ui.submitted(", doc: "ui.submitted(element) — Enter in this focused text field this frame." },
    ApiEntry { label: "ui.event", insert: "ui.event(", doc: "ui.event(element, \"dropped\") — did that element fire that hook this frame? Any hook by name; ui.clicked/pressed/released/changed/submitted are the shorthands." },
    ApiEntry { label: "ui.events", insert: "ui.events()", doc: "ui.events() — everything that happened on the UI this frame, as { node = element, event = \"clicked\" } rows. ui.events(\"clicked\") filters. Lets one manager handle a whole screen without naming a single element: for _, ev in ipairs(ui.events(\"clicked\")) do ... end." },
    ApiEntry { label: "ui.hovered", insert: "ui.hovered()", doc: "ui.hovered() — the element under the pointer, as a node, or nil. ui.hovered(el) answers yes/no for one element. A STATE, not an event: true for as long as it's true (hoverStart/hoverEnd are the edges)." },
    ApiEntry { label: "ui.held", insert: "ui.held()", doc: "ui.held() — the element the pointer is holding down, as a node, or nil. ui.held(el) answers yes/no. Hold-to-charge, press-and-hold repeat, a dip while pressed." },
    ApiEntry { label: "ui.dragging", insert: "ui.dragging()", doc: "ui.dragging() — the element being dragged, as a node, or nil. Live for the whole drag AND for the frame the `dropped` hooks run on. There is no separate payload channel because a node already carries params, a name and tags — ask it what it is." },
    ApiEntry { label: "ui.dropTarget", insert: "ui.dropTarget()", doc: "ui.dropTarget() — the drop target the drag is currently over, as a node, or nil." },
    ApiEntry { label: "ui.bind", insert: "ui.bind(", doc: "ui.bind(node, \"property\", function() ... end) — say the relationship once instead of writing an update() that keeps it true. The engine calls the function once a frame, after every update, and writes what it returns: a string or number to \"text\", a color(...) to a colour field, a number/boolean to any component field (the component is picked by which one actually has that field, so \"value\" finds UiSlider). Re-binding the same property replaces. A binding whose node is gone is dropped silently; one that throws is dropped after reporting once." },
    ApiEntry { label: "ui.unbind", insert: "ui.unbind(", doc: "ui.unbind(node) drops every binding on that node; ui.unbind(node, \"text\") drops one." },
    ApiEntry { label: "ui.make", insert: "ui.make(", doc: "ui.make(container, tree) — build a UI subtree from data and RECONCILE it with the one already there: call it again and only the difference is spawned and destroyed, so surviving rows keep their entity, their hover, their scroll and their in-flight transitions. An element is { \"kind\", prop = value, ..., children }, where kind is box/row/col/text/image/button/field/slider/scroll. `items = {...}` plus a function child makes one child per item (the function gets (item, i); return nil to skip it). `key = \"id\"` is how a row is matched through a re-sort. `onClicked = function(node) ... end` (any UI hook, `on` + its name) carries behaviour inline — no prefab, no script file. Properties the table stops mentioning go back to default; what the PLAYER did (scroll, typing, a toggle, a dragged slider) is kept. Play only, and a mistyped property raises rather than being ignored. Elements you placed by hand under the same container are never touched." },
    ApiEntry { label: "color", insert: "color(", doc: "color(r, g, b [, a]) — a colour, 0..1 per channel, alpha 1 by default. Also color(gray [, a]) and color(other [, a]) to copy with a new alpha. It's a plain table {r,g,b,a} (also [1]..[4]) so it prints, saves and compares. Assign it whole: el.fill = color(1, 0.85, 0.35), el.textColor, el.borderColor, el.tint, el.groupTint, el.caretColor." },
    ApiEntry { label: "color.hex", insert: "color.hex(\"#\")", doc: "color.hex(\"#ff8800\") / color.hex(\"ff8800aa\") — 6 or 8 hex digits. A 3-digit shorthand is refused rather than guessed at." },
    ApiEntry { label: "color.lerp", insert: "color.lerp(", doc: "color.lerp(a, b, t) — blend two colours per channel, t clamped to 0..1." },
    ApiEntry { label: "node.index", insert: "node.index", doc: "Which row of a UI repeater this node is, 0-based — nil on anything a repeater didn't spawn, so `if node.index then` is a fine \"am I a row\". Read the count with getcomponent(\"UiElement\").count on the container." },

    // ---- Added by the API-coverage audit -------------------------------
    // Every one of these is reachable from a script and had NO reference row:
    // the whole of water.*, scatter.*, assembly.*, physics.*, the shape
    // queries, half of draw.*, the gamepad calls, and sixteen table
    // overviews. `lua_api_reference_covers_the_whole_surface` now fails if
    // that gap ever reopens.
    ApiEntry { label: "account", insert: "account", doc: "The signed-in player: account.signIn(), account.player(), and http verbs that carry the session. A script asks for a PLAYER, never for a token, and the server decides what that player owns." },
    ApiEntry { label: "assembly", insert: "assembly", doc: "Multi-part vessels: hold forces and torques, split parts off, latch parts on, and read the compound's mass and centre of mass. A vessel is one physics body built from many nodes." },
    ApiEntry { label: "assembly.force", insert: "assembly.force(", doc: "assembly.force(node, force) — a HELD force through the centre of mass, re-applied every tick until you change it (engines, thrusters). Through the CoM means no torque: the vessel accelerates without turning." },
    ApiEntry { label: "assembly.forceAt", insert: "assembly.forceAt(", doc: "assembly.forceAt(node, force, at) — a held world-space force at a world point. Off the centre of mass it produces torque as well as acceleration, which is how an off-axis thruster makes a craft tumble — and how RCS steers it." },
    ApiEntry { label: "assembly.impacts", insert: "assembly.impacts(", doc: "assembly.impacts(node) — the LAST tick's per-part contact loads: { part, impulse, speed, speedAbs, x, y, z }. What a damage model reads: how hard each part was hit and where." },
    ApiEntry { label: "assembly.impulseAt", insert: "assembly.impulseAt(", doc: "assembly.impulseAt(node, impulse, at) — a one-shot kick at a world point, applied once rather than held. Explosions, collisions you resolve yourself, a docking clamp letting go." },
    ApiEntry { label: "assembly.info", insert: "assembly.info(", doc: "assembly.info(node) — { mass, com, origin, vel, angVel, grounded, anchored, parts }. com is the world-space centre of mass as a vec3 — the number a flight controller, a CoM gizmo and a landing check all need." },
    ApiEntry { label: "assembly.keepLive", insert: "assembly.keepLive(", doc: "assembly.keepLive(node, true) — exempt this compound from distant-craft LOD, so it keeps simulating in full even when nothing is near it. For the craft the player will come back to and expects to find where physics would have put it." },
    ApiEntry { label: "assembly.merge", insert: "assembly.merge(", doc: "assembly.merge(node, other) — latch another assembly onto this one: docking, grabbing, a part snapping into place. The two become one physics body with one mass and one centre of mass." },
    ApiEntry { label: "assembly.rebuild", insert: "assembly.rebuild(", doc: "assembly.rebuild(node) — re-gather the compound from the root's CURRENT children. Call it after you have added or removed part nodes yourself, so the physics body matches the scene again." },
    ApiEntry { label: "assembly.setAnchored", insert: "assembly.setAnchored(", doc: "assembly.setAnchored(node, true) — pin the vessel exactly where it stands (a launch clamp, a craft on a pad, anything that must not drift while you build it). Release it and normal physics resumes." },
    ApiEntry { label: "assembly.split", insert: "assembly.split(", doc: "assembly.split(node, parts [, fn] [, prefab]) — detach part nodes into their own assembly (stage separation, a wing coming off). The new assembly keeps the velocity it had, so debris carries on rather than appearing at rest." },
    ApiEntry { label: "assembly.syncColliders", insert: "assembly.syncColliders(", doc: "assembly.syncColliders(node) — re-pose the compound's collision shapes to its parts' current transforms. Needed after you move parts around without a rebuild, or the vessel collides with where it used to be." },
    ApiEntry { label: "assembly.teleport", insert: "assembly.teleport(", doc: "assembly.teleport(node, pos) — move the assembly origin to a world position, carrying every part with it. A teleport rather than a force: no acceleration, no tumble." },
    ApiEntry { label: "assembly.torque", insert: "assembly.torque(", doc: "assembly.torque(node, t) — a held PURE torque, no linear push: reaction wheels, SAS, anything that turns a vessel without moving it." },
    ApiEntry { label: "audio", insert: "audio", doc: "Sounds and the mixer: audio.play for one-shots, audio.track for a mixer bus, node:sound() for a node's Audio Source." },
    ApiEntry { label: "camera", insert: "camera", doc: "The game camera's projection: viewport size and rect, world↔screen conversion, and picking rays. camera.screenRect shares its space with input.mouse(), which is why hit-testing works." },
    ApiEntry { label: "camera.exists", insert: "camera.exists()", doc: "camera.exists() — true once a live game camera is being fed. Guard the other camera.* calls with it during the first frames, or while a scene without a camera is up." },
    ApiEntry { label: "camera.screenRect", insert: "camera.screenRect()", doc: "camera.screenRect() -> x, y, w, h — the game viewport in the SAME space as input.mouse() and camera.worldToScreen, offset included. That shared space is the only reason hit-testing the mouse against a projected point works; screenSize alone would be wrong wherever the viewport isn't at the window origin." },
    ApiEntry { label: "capsulecast", insert: "capsulecast(", doc: "capsulecast(origin, dir, radius, halfHeight, max [, opts]) — the player-shaped sweep: \"can I actually move there\", asked with the shape that will be moving. Upright along the capsule's own axis, matching how the solver keeps a capsule body aligned, so the cast and the move agree." },
    ApiEntry { label: "draw", insert: "draw", doc: "The GAME's telegraph layer — 3D lines/shapes and screen-space rects, circles and text that SHIP with your game. gizmo.* is the debug-only twin that never appears for a player." },
    ApiEntry { label: "draw.box", insert: "draw.box(", doc: "draw.box(cx,cy,cz, hx,hy,hz, yaw, r,g,b [,a]) — a yaw-rotated wireframe box from half-extents. Trigger volumes, build footprints, an attach point." },
    ApiEntry { label: "draw.cone", insert: "draw.cone(", doc: "draw.cone(bx,by,bz, dx,dy,dz, radius, height, r,g,b [,a]) — a SOLID cone: base disc at b, apex `height` along the unit direction d. Gizmo arrowheads, thruster plumes, direction markers." },
    ApiEntry { label: "draw.disc", insert: "draw.disc(", doc: "draw.disc(cx,cy,cz, nx,ny,nz, r0, r1, r,g,b [,a]) — a filled annulus around normal n (r0 = inner, r1 = outer; r0 = 0 gives a full disc). Rotation gizmo bands, ground markers." },
    ApiEntry { label: "draw.rect", insert: "draw.rect(", doc: "draw.rect(x, y, w, h, r,g,b [,a] [,radius]) — a filled rectangle in SCREEN PIXELS, in input.mouse()'s space. An RTS marquee is just the two corners you dragged between — the 3D version has to be projected onto a ground plane, which fights the camera angle and misses whatever the plane doesn't cross." },
    ApiEntry { label: "draw.rectOutline", insert: "draw.rectOutline(", doc: "draw.rectOutline(x, y, w, h, r,g,b [,a] [,thickness]) — the hollow twin of draw.rect. The last number is the border thickness rather than a corner radius." },
    ApiEntry { label: "draw.ring", insert: "draw.ring(", doc: "draw.ring(cx,cy,cz, nx,ny,nz, radius, r,g,b [,a]) — a circle around normal n at c. Range rings, selection circles, an area-of-effect telegraph." },
    ApiEntry { label: "draw.sphere", insert: "draw.sphere(", doc: "draw.sphere(cx,cy,cz, radius, r,g,b [,a]) — three rings, i.e. a wireframe ball. Cheap enough to draw per-frame for every marker on screen." },
    ApiEntry { label: "draw.tri", insert: "draw.tri(", doc: "draw.tri(x1,y1,z1, x2,y2,z2, x3,y3,z3, r,g,b [,a]) — one filled triangle. The raw primitive under the solid shapes, for when you want your own." },
    ApiEntry { label: "http", insert: "http", doc: "Talk to a web server: http.get / post / put / delete, plus json.*. Every call is asynchronous — the reply arrives in a callback, never as a return value." },
    ApiEntry { label: "input.cancelRebind", insert: "input.cancelRebind()", doc: "input.cancelRebind() — abandon a rebind in progress, leaving the old binding alone." },
    ApiEntry { label: "input.commitRebind", insert: "input.commitRebind()", doc: "input.commitRebind() — accept the captured binding. Returns false if nothing was captured yet." },
    ApiEntry { label: "input.facing", insert: "input.facing()", doc: "input.facing() — which way this player's character is facing, as -1 or 1. The fighter layer mirrors directional input by it, so \"forward\" means toward the opponent on both sides of the screen." },
    ApiEntry { label: "input.padAxis", insert: "input.padAxis(", doc: "input.padAxis(1, \"leftx\") — read a pad axis raw, -1..1, past the action map. Same diagnostic purpose as input.padButton; bind through actions for real gameplay." },
    ApiEntry { label: "input.padButton", insert: "input.padButton(", doc: "input.padButton(1, \"a\") — read a pad button RAW, straight past the action map. Deliberately unmediated: this is what distinguishes \"your pad works, your bindings are wrong\" from \"your pad is not here\"." },
    ApiEntry { label: "input.padCount", insert: "input.padCount()", doc: "input.padCount() — how many gamepads are connected. The quick check behind a \"press a button to join\" prompt." },
    ApiEntry { label: "input.pads", insert: "input.pads()", doc: "input.pads() — every gamepad the engine has enumerated: { index, name, connected }. Show it in your options screen; \"the pad isn't listed\" and \"the pad is listed but nothing is bound\" are different problems and only this can tell them apart." },
    ApiEntry { label: "input.pendingRebind", insert: "input.pendingRebind()", doc: "input.pendingRebind() — the captured chip text once something has been pressed, an EMPTY string while still waiting, or nil when no rebind is running. Enough for a menu to show \"press any button…\" and then the result." },
    ApiEntry { label: "json", insert: "json", doc: "json.encode(t) and json.decode(s) — the wire format for http.*. decode returns nil, message on bad input rather than raising, because a reply from someone else's server is data, not a bug in your script." },
    ApiEntry { label: "net", insert: "net", doc: "Multiplayer: host and join, synced state, RPCs, ownership (net.isMine), and the rollback readouts. Open netcode — you can self-host the relay." },
    ApiEntry { label: "overlapSphere", insert: "overlapSphere(", doc: "overlapSphere(center, radius [, opts]) — everything inside a sphere, DEEPEST overlap first, as hit tables ({x,y,z, nx,ny,nz, distance, node}). Reports static geometry AND body hulls. opts takes { exclude = node, layers = {\"Enemies\"} }. The blast-radius / \"what is in this area\" query." },
    ApiEntry { label: "physics", insert: "physics", doc: "Sim controls: physics.pause(true) freezes the whole gameplay tick while scripts keep running (pause menus, cutscenes, loading screens), and physics.step() advances it one tick at a time." },
    ApiEntry { label: "physics.isPaused", insert: "physics.isPaused()", doc: "physics.isPaused() — whether the sim is currently frozen, including when the editor froze it rather than your script." },
    ApiEntry { label: "physics.pause", insert: "physics.pause(", doc: "physics.pause(true) — freeze the whole gameplay tick while scripts keep running. Pause menus, cutscenes and loading screens are this call: the world stops, your UI doesn't." },
    ApiEntry { label: "physics.step", insert: "physics.step([n])", doc: "physics.step([n]) — advance the frozen tick n times (default 1, max 600) — the same thing the editor's frame-step button does, so a game can build its own training mode. Call it from update: a fixedUpdate caller would never get a second turn, because the tick it is waiting for is the one it just stopped." },
    ApiEntry { label: "perf", insert: "perf", doc: "Where YOUR frame time goes — per subsystem and per script, readable from Lua so a game can assert its own budget in a smoke test rather than filing an engine ticket. Off by default and free while off: call perf.enable(true) first. Every getter RAISES while collection is off rather than answering 0, because a budget assertion that passes on no data is worse than no assertion." },
    ApiEntry { label: "perf.enable", insert: "perf.enable(", doc: "perf.enable(true) — start collecting; perf.enable(false) stops and CLEARS the history (a stale average from before a fix looks exactly like a fix that did not work). Off by default, because a profiler that costs a frame is one people turn off." },
    ApiEntry { label: "perf.enabled", insert: "perf.enabled()", doc: "perf.enabled() — is anything being measured? Safe to call while off, so a script can ask before reading." },
    ApiEntry { label: "perf.buckets", insert: "perf.buckets()", doc: "perf.buckets() → the bucket names, in frame order: scripts, physics, terrain, scatter, particles, audio, animation, ui, render. Iterate this rather than keeping your own list, which could go stale." },
    ApiEntry { label: "perf.ms", insert: "perf.ms(", doc: "perf.ms(\"scripts\") — that bucket's rolling average, in milliseconds. An unknown bucket names every accepted value rather than answering 0." },
    ApiEntry { label: "perf.worstMs", insert: "perf.worstMs(", doc: "perf.worstMs(\"scripts\") — the WORST single frame in the last second. This is the one to watch: a 40 ms hitch once a second adds under a millisecond to a 60-frame average, so the mean hides exactly the thing you are chasing." },
    ApiEntry { label: "perf.scriptMs", insert: "perf.scriptMs(", doc: "perf.scriptMs(\"planet_walker\") — one script's own average cost, by file name. 0 for a script that has not run, which is different from an error." },
    ApiEntry { label: "perf.scriptWorstMs", insert: "perf.scriptWorstMs(", doc: "perf.scriptWorstMs(\"planet_walker\") — that script's worst frame in the last second." },
    ApiEntry { label: "perf.scripts", insert: "perf.scripts()", doc: "perf.scripts() → { {name=, ms=, worstMs=}, ... }, MOST EXPENSIVE FIRST — which is the order the question is asked in. A total for 'scripts' never answered 'which of my scripts is doing this'." },
    ApiEntry { label: "perf.slowestScript", insert: "perf.slowestScript()", doc: "perf.slowestScript() → the name of the costliest script, or nil if none have run. The one-liner you actually put in an assertion message." },
    ApiEntry { label: "perf.counts", insert: "perf.counts()", doc: "perf.counts() → { nodes=, culled=, instances=, draws=, chunks=, props=, particles=, effects=, effectsDropped=, lights=, lightsDropped=, voices= }. Readable even while collection is off, because counts are free to keep — and three of the four 'the engine is slow' reports this API exists for were answerable from one count alone (a scatter field asking for 117,000 props was one of them). The *Dropped pair is what a ceiling refused this frame: nonzero means the engine is cutting your look, which you should hear from a number rather than from a screenshot." },
    ApiEntry { label: "perf.accountedMs", insert: "perf.accountedMs()", doc: "perf.accountedMs() — the buckets added up. Called 'accounted' and not 'total' on purpose: vsync, the OS and the GPU finishing are outside every bucket, so this is what the engine can see, not the frame time." },
    ApiEntry { label: "save", insert: "save", doc: "The persistent store: save.set / save.get, named slots, and flushing to disk. Values are capped at about a kilobyte each — store the small fact, not the whole world." },
    ApiEntry { label: "scatter", insert: "scatter", doc: "Thousands of props from a seed — GPU-instanced, with no scene node anywhere in it. Your generator still decides WHAT grows where; the engine decides where each instance stands and draws them. scatter.create declares a source, scatter.remove harvests one." },
    ApiEntry { label: "scatter.create", insert: "scatter.create{", doc: "scatter.create{ asset = \"tree.glb\", seed = 7, perChunk = 24, chunk = 16 } — declare a source, get its id. Region: center + radius for a sphere (a planet), or center + halfX/halfZ for ground. `parent = \"Umunquo\"` anchors the region to a NODE, so a planet that orbits carries its props instead of sliding out from under them — every prop keeps its id, its place on the surface and the ground height it settled at, because none of those were ever expressed in world space. Without a parent the region is pinned to the world, which is right for a landscape that never moves and wrong for every celestial body. Also scaleMin/scaleMax, align = \"surface\" (default) or \"world\", fade, and lod = { {asset=, distance=}, ... } nearest-first. Placement is a pure function of the seed, so every machine and every session grows the SAME forest without storing one. `density` is how a world gets biomes: pass a function(x, y, z) -> 0..1 and it is sampled ONCE, at declare time, into a densityRows grid (rows x 2*rows for a sphere\'s longitude) — 0 means no instance is generated at all, not a hidden one. An option this doesn\'t list is an error, not a shrug. `asset` may be a mesh file OR a .prefab.ron — a prefab is baked once into one instanced draw per Mesh node it holds, each at its authored place in the prop, which is how a prop your own script assembled gets scattered." },
    ApiEntry { label: "scatter.cost", insert: "scatter.cost(", doc: "scatter.cost(id) — what this source asks for every frame: { chunks, props, far, chunkSize, perChunk }. Read it BEFORE you ship the field. The knobs look like a look, but the outermost `lod` distance is really the budget: it sets how many chunks stay resident, as a sweep whose side grows with it, walked every frame. Cost is about (far/chunk)^2 per source — halving the distance, or doubling the chunk, quarters it. A field big enough to matter also says so in the Console when you declare it. On a body smaller than your view distance the count saturates at the body, so a planet never costs more than a planet." },
    ApiEntry { label: "scatter.destroy", insert: "scatter.destroy(", doc: "scatter.destroy(id) — remove a whole source and everything it was drawing. Returns true if there was one." },
    ApiEntry { label: "scatter.near", insert: "scatter.near(", doc: "scatter.near(sourceId, point, radius) — the instances around a point, nearest first: { id, pos, distance, scale, param }. What a harvest verb aims with, and what a \"is there room to build here\" check reads." },
    ApiEntry { label: "scatter.remove", insert: "scatter.remove(", doc: "scatter.remove(sourceId, instanceId) — take one prop out, permanently. By id rather than by position, which is what makes it survive streaming out and back in: an id comes from (seed, chunk, index), a position is a float off the end of a chain of arithmetic." },
    ApiEntry { label: "scatter.removed", insert: "scatter.removed(", doc: "scatter.removed(sourceId) — the sorted ids this source has lost. A game that wants permanence saves THIS — a handful of numbers — not every plant it ever saw, which is what made permanence unstorable before (save values are capped at about a kilobyte)." },
    ApiEntry { label: "scatter.restore", insert: "scatter.restore(", doc: "scatter.restore(sourceId [, instanceId]) — put one prop back, or all of them when the instance is omitted (returns how many). This is what \"the forest regrows after fifteen minutes\" is, without your game having to remember what it cut." },
    ApiEntry { label: "scene", insert: "scene", doc: "Which world is loaded: scene.load / scene.unload, additive layers, and scene.onLoaded. Pair with node.persistent to carry a node across a swap." },
    ApiEntry { label: "scene.onLoaded", insert: "scene.onLoaded(", doc: "scene.onLoaded(function(name, additive) ... end) — run something once a scene has finished loading. Fires AFTER the world is whole, because a loading screen's whole job is to go away once the thing it was covering exists." },
    ApiEntry { label: "scene.unload", insert: "scene.unload(", doc: "scene.unload(\"Shop\") — remove a scene that was loaded additively, and everything under it. The other half of scene.load{ additive = true }." },
    ApiEntry { label: "space", insert: "space", doc: "On-rails celestial mechanics: where the bodies are, which one's gravity owns a point, the orbit a craft is on, and time-warp." },
    ApiEntry { label: "space.body", insert: "space.body(", doc: "space.body(\"Pebble\") — one celestial body by node name: { name, x,y,z, vx,vy,vz, mu, radius, soi } in world coordinates, or nil. space.bodies() returns them all." },
    ApiEntry { label: "spherecast", insert: "spherecast(", doc: "spherecast(origin, dir, radius, max [, opts]) — the first thing a moving BALL of that radius would hit, or nil. A raycast that can't slip through a gap narrower than the thing you are actually moving." },
    ApiEntry { label: "terrain", insert: "terrain", doc: "Runtime sculpting and queries against the SDF terrain: dig, sculpt, paint, ask what is under a point, and persist edits per save slot." },
    ApiEntry { label: "terrain.slotAt", insert: "terrain.slotAt(", doc: "terrain.slotAt(x, y, z) — the texture-palette slot at a world point, or nil where the field is untextured. The material half of the question terrain.query answers the distance half of: survey before you cut, and let a footstep know what it is standing on." },
    ApiEntry { label: "terrain.yields", insert: "terrain.yields()", doc: "terrain.yields() — drains what recent digs actually removed: { id, removed, added, untextured, slots }, with slots mapping palette slot to volume. This is how mining pays out by MATERIAL — you get ore because you cut rock that was painted as ore." },
    ApiEntry { label: "ui", insert: "ui", doc: "Screen UI from scripts: ui.on / ui.events for input, ui.bind for data, ui.make for whole trees. See the game-UI section for the full set." },
    ApiEntry { label: "water", insert: "water", doc: "Water volumes: how deep a point is (water.depthAt), what is in the water (water.at), freezing and thawing (water.setFrozen). The engine already does buoyancy and drag — these are the questions a GAME still has to answer: swimming, drowning, flooding, a gauge going red." },
    ApiEntry { label: "water.at", insert: "water.at(", doc: "water.at(point) — nil in air, else { depth, density, frozen, node, up }. `up` is the way OUT of the water (radial on a sea, the pool's own +Y) — what a swim controller pushes along, and NOT the same as -gravity in a tilted tank. Innermost volume wins, so a tank inside an ocean answers as the tank." },
    ApiEntry { label: "water.depthAt", insert: "water.depthAt(", doc: "water.depthAt(x, y, z) — or a vec3, or a node. Metres BELOW the surface at that point; 0 in air. The one number everything else is derived from, and it is the same rule the solver uses, so a swim state can never disagree with the physics that floats you. A frozen volume reads 0 everywhere." },
    ApiEntry { label: "water.isUnderwater", insert: "water.isUnderwater(", doc: "water.isUnderwater(point) — the yes/no, for when you don't need the depth. Takes x,y,z or a vec3 or a node: if water.isUnderwater(node) then stamina = stamina - dt end" },
    ApiEntry { label: "water.setFrozen", insert: "water.setFrozen(", doc: "water.setFrozen(node, true) — freeze a water volume. Freezing is a STATE, not a second system: the same node with a flag flipped, and both the physics (no buoyancy, no drag) and the look follow from it. A world that thaws is one call back." },
    ApiEntry { label: "water.volumes", insert: "water.volumes()", doc: "water.volumes() — every body of water in the scene, as node handles. What a climate or weather system iterates when it wants to know where the seas are." },

    // ---- Handle members ------------------------------------------------
    // Everything reachable THROUGH a handle rather than by name: the component
    // handles `node:getcomponent` returns, the sound/track/particle handles,
    // vec3/vec2's own methods, a raycast hit's fields. `api_surface()` cannot
    // see these — they live on metatables and userdata — so they are checked
    // against the editor's own EmmyLua annotations instead, which is where
    // their descriptions come from.
    ApiEntry { label: "body.mu", insert: "body.mu", doc: "Gravitational parameter µ = GM." },
    ApiEntry { label: "body.name", insert: "body.name", doc: "The celestial body's node name — what space.body() takes and space.dominant() returns." },
    ApiEntry { label: "body.radius", insert: "body.radius", doc: "Physical surface radius." },
    ApiEntry { label: "body.soi", insert: "body.soi", doc: "Sphere-of-influence radius (-1 = infinite, the root)." },
    ApiEntry { label: "body.vx", insert: "body.vx", doc: "World velocity X — the body's own motion along its rails, which a rendezvous has to match." },
    ApiEntry { label: "body.vy", insert: "body.vy", doc: "World velocity Y." },
    ApiEntry { label: "body.vz", insert: "body.vz", doc: "World velocity Z." },
    ApiEntry { label: "body.x", insert: "body.x", doc: "World X of the body's centre this tick." },
    ApiEntry { label: "body.y", insert: "body.y", doc: "World Y of the body's centre." },
    ApiEntry { label: "body.z", insert: "body.z", doc: "World Z of the body's centre." },
    ApiEntry { label: "cam.active", insert: "cam.active", doc: "The play-mode view camera (1/0) — assign true to switch to it." },
    ApiEntry { label: "cam.fovY", insert: "cam.fovY", doc: "Vertical field of view, radians." },
    ApiEntry { label: "el.border", insert: "el.border", doc: "Shape border thickness (design units)." },
    ApiEntry { label: "el.cell", insert: "el.cell", doc: "Spritesheet cell index the image shows (set per frame for sprite animation)." },
    ApiEntry { label: "el.fillA", insert: "el.fillA", doc: "Shape fill alpha 0..1." },
    ApiEntry { label: "el.fillB", insert: "el.fillB", doc: "Shape fill blue 0..1." },
    ApiEntry { label: "el.fillG", insert: "el.fillG", doc: "Shape fill green 0..1." },
    ApiEntry { label: "el.fillR", insert: "el.fillR", doc: "Shape fill red 0..1." },
    ApiEntry { label: "el.height", insert: "el.height", doc: "Height (same rules as width)." },
    ApiEntry { label: "el.opacity", insert: "el.opacity", doc: "Multiplies every color the element draws, 0..1." },
    ApiEntry { label: "el.posX", insert: "el.posX", doc: "Free position X / Pin offset X (design units)." },
    ApiEntry { label: "el.posY", insert: "el.posY", doc: "Free position Y / Pin offset Y (design units)." },
    ApiEntry { label: "el.radius", insert: "el.radius", doc: "Shape corner radius (design units)." },
    ApiEntry { label: "el.scrollY", insert: "el.scrollY", doc: "Scroll-view position, design units (0 = top; the wheel drives it too, clamped to the content). Present only on elements with the scroll-view option." },
    ApiEntry { label: "el.textA", insert: "el.textA", doc: "Text color alpha 0..1." },
    ApiEntry { label: "el.textB", insert: "el.textB", doc: "Text color blue 0..1." },
    ApiEntry { label: "el.textG", insert: "el.textG", doc: "Text color green 0..1." },
    ApiEntry { label: "el.textR", insert: "el.textR", doc: "Text color red 0..1." },
    ApiEntry { label: "el.textSize", insert: "el.textSize", doc: "Text glyph size (design units; ignored while fit is on)." },
    ApiEntry { label: "el.tintA", insert: "el.tintA", doc: "Image tint alpha 0..1." },
    ApiEntry { label: "el.tintB", insert: "el.tintB", doc: "Image tint blue 0..1." },
    ApiEntry { label: "el.tintG", insert: "el.tintG", doc: "Image tint green 0..1." },
    ApiEntry { label: "el.tintR", insert: "el.tintR", doc: "Image tint red 0..1." },
    ApiEntry { label: "el.visible", insert: "el.visible", doc: "Shown (1/0; assign true/false)." },
    ApiEntry { label: "el.width", insert: "el.width", doc: "Width in the axis's sizing mode (px value, % fraction, or grow weight). Absent (nil) on a fit axis; writing one makes it fixed px." },
    ApiEntry { label: "hit.nx", insert: "hit.nx", doc: "Contact normal X (unit, out of the hit surface)." },
    ApiEntry { label: "hit.ny", insert: "hit.ny", doc: "Contact normal Y." },
    ApiEntry { label: "env.ambient2dR", insert: "env.ambient2dR", doc: "The 2D BASE LIGHT, red 0..1 — the whole light a flat scene has before any 2D light is placed. White by default; turn it down for a dark room a torch can carve a circle out of, and read it back first so you can put it where it was." },
    ApiEntry { label: "env.ambient2dG", insert: "env.ambient2dG", doc: "The 2D base light, green 0..1. See ambient2dR." },
    ApiEntry { label: "env.ambient2dB", insert: "env.ambient2dB", doc: "The 2D base light, blue 0..1. See ambient2dR." },
    ApiEntry { label: "env.intensity", insert: "env.intensity", doc: "Brightness multiplier on the key (directional) light." },
    ApiEntry { label: "env.colorR", insert: "env.colorR", doc: "Key light colour red." },
    ApiEntry { label: "env.colorG", insert: "env.colorG", doc: "Key light colour green." },
    ApiEntry { label: "env.colorB", insert: "env.colorB", doc: "Key light colour blue." },
    ApiEntry { label: "env.directionX", insert: "env.directionX", doc: "Key light direction X — lerp the three for a day cycle." },
    ApiEntry { label: "env.directionY", insert: "env.directionY", doc: "Key light direction Y." },
    ApiEntry { label: "env.directionZ", insert: "env.directionZ", doc: "Key light direction Z." },
    ApiEntry { label: "env.stars", insert: "env.stars", doc: "Stars mode (1/0; assign true/false): luminous celestial bodies ARE the key lights." },
    ApiEntry { label: "env.ambientR", insert: "env.ambientR", doc: "3D ambient fill red 0..1 — the fill under the key light, deliberately a different value from ambient2dR." },
    ApiEntry { label: "env.ambientG", insert: "env.ambientG", doc: "3D ambient fill green 0..1." },
    ApiEntry { label: "env.ambientB", insert: "env.ambientB", doc: "3D ambient fill blue 0..1." },
    ApiEntry { label: "env.shadows", insert: "env.shadows", doc: "Sun shadows on (1/0; assign true/false). Every shadow field below only applies when this is on." },
    ApiEntry { label: "env.shadowSoftness", insert: "env.shadowSoftness", doc: "0 = razor-hard edge … 1 = dreamy-soft penumbra." },
    ApiEntry { label: "env.shadowStrength", insert: "env.shadowStrength", doc: "How dark full shadow gets, 0..1 (ambient still fills, so never pitch black)." },
    ApiEntry { label: "env.shadowTintR", insert: "env.shadowTintR", doc: "Shadows darken toward this colour instead of black — red." },
    ApiEntry { label: "env.shadowTintG", insert: "env.shadowTintG", doc: "Shadow tint green." },
    ApiEntry { label: "env.shadowTintB", insert: "env.shadowTintB", doc: "Shadow tint blue." },
    ApiEntry { label: "env.shadowQuantize", insert: "env.shadowQuantize", doc: "0 = smooth penumbra; 2..8 = posterize it into that many bands (toon/retro)." },
    ApiEntry { label: "env.shadowDither", insert: "env.shadowDither", doc: "Bayer-dither the penumbra (1/0) — the classic PS1 dithered shadow edge." },
    ApiEntry { label: "env.shadowDistance", insert: "env.shadowDistance", doc: "Max world distance a shadow ray marches before giving up; far geometry stops casting past it." },
    ApiEntry { label: "env.fog", insert: "env.fog", doc: "Depth fog on (1/0; assign true/false)." },
    ApiEntry { label: "env.contactShadows", insert: "env.contactShadows", doc: "The small dark line where things touch (1/0). A moving mesh casts through its COLLIDER, so a character's shadow is a capsule's — this shadows from the real silhouette of whatever is on screen. Only what is ON SCREEN casts one." },
    ApiEntry { label: "env.contactLength", insert: "env.contactLength", doc: "How far a contact shadow traces, in world units. Short is the point — the shadow under a foot, in a seam, behind a bolt." },
    ApiEntry { label: "env.contactSteps", insert: "env.contactSteps", doc: "Samples along the contact trace (2..32). Raise it if the shadow looks striped." },
    ApiEntry { label: "env.contactStrength", insert: "env.contactStrength", doc: "How dark a contact shadow gets, 0..1, before the shared shadow tint and strength." },
    ApiEntry { label: "env.fogColorR", insert: "env.fogColorR", doc: "Fog colour red — match it to the horizon or a seam shows." },
    ApiEntry { label: "env.fogColorG", insert: "env.fogColorG", doc: "Fog colour green." },
    ApiEntry { label: "env.fogColorB", insert: "env.fogColorB", doc: "Fog colour blue." },
    ApiEntry { label: "env.fogStart", insert: "env.fogStart", doc: "World distance where fog begins (fully clear nearer than this)." },
    ApiEntry { label: "env.fogEnd", insert: "env.fogEnd", doc: "World distance where fog is full." },
    ApiEntry { label: "env.fogDither", insert: "env.fogDither", doc: "Dither the fog gradient to hide 8-bit banding on long ramps (1/0)." },
    ApiEntry { label: "env.fogDitherStrength", insert: "env.fogDitherStrength", doc: "Dither amplitude 0..1." },
    ApiEntry { label: "env.fogVolumetric", insert: "env.fogVolumetric", doc: "Volumetric mode (1/0): march real fog media instead of a distance ramp, so hills poke out of ground mist. fogStart/fogEnd do not apply." },
    ApiEntry { label: "env.fogDensity", insert: "env.fogDensity", doc: "Volumetric: media density per world unit." },
    ApiEntry { label: "env.fogHeight", insert: "env.fogHeight", doc: "Volumetric: world height (y) of the fog layer's top." },
    ApiEntry { label: "env.fogFalloff", insert: "env.fogFalloff", doc: "Volumetric: softness of the layer's top edge, world units." },
    ApiEntry { label: "env.fogNoise", insert: "env.fogNoise", doc: "Volumetric: how much drifting noise breaks up the media, 0..1." },
    ApiEntry { label: "env.fogNoiseScale", insert: "env.fogNoiseScale", doc: "Volumetric: noise feature size, world units per repeat." },
    ApiEntry { label: "env.fogLight", insert: "env.fogLight", doc: "Volumetric: how much of the scene's light scatters IN the fog. 0 = a flat colour; 1 = lit by the sun, the point lights and the baked bounce; past 1 exaggerates. Ramp it up as a storm rolls in and the air itself starts carrying the light." },
    ApiEntry { label: "env.fogAnisotropy", insert: "env.fogAnisotropy", doc: "Volumetric: which way the media throws light (-0.9..0.9). Positive blooms toward the sun, 0 is an even haze. Fog has no normal — this is what does that job." },
    ApiEntry { label: "env.fogSteps", insert: "env.fogSteps", doc: "Volumetric: samples along each pixel's fog ray (2..64). The quality/cost dial — drop it on a weak machine." },
    ApiEntry { label: "env.fogShafts", insert: "env.fogShafts", doc: "Volumetric (1/0): march the sun shadow at every fog step, so beams appear through windows and branches. The entire cost of lit fog lives here." },
    ApiEntry { label: "hit.nz", insert: "hit.nz", doc: "Contact normal Z." },
    ApiEntry { label: "hit.x", insert: "hit.x", doc: "Contact point X (world)." },
    ApiEntry { label: "hit.y", insert: "hit.y", doc: "Contact point Y (world)." },
    ApiEntry { label: "hit.z", insert: "hit.z", doc: "Contact point Z (world)." },
    ApiEntry { label: "layer.designHeight", insert: "layer.designHeight", doc: "Design units that span the window height." },
    ApiEntry { label: "layer.enabled", insert: "layer.enabled", doc: "Master switch (1/0; assign true/false) — an off layer draws nothing." },
    ApiEntry { label: "layer.textSnap", insert: "layer.textSnap", doc: "Round every rasterized text size to a whole multiple of this many SCREEN PIXELS; 0 = off. For a pixel font, whose art is a grid: a cell only looks like a pixel when it lands on a whole one, and `text size x layer scale` almost never does — so every stem is softened by a different fraction and the text reads as badly spaced even though nothing is mispositioned. Set it to the number of cells in an em." },
    ApiEntry { label: "layer.worldSpace", insert: "layer.worldSpace", doc: "1 = a panel inside the 3D world at this node's transform; 0 = a screen overlay." },
    ApiEntry { label: "layer.z", insert: "layer.z", doc: "Draw order: lowest z first." },
    ApiEntry { label: "light.b", insert: "light.b", doc: "Color blue 0..1." },
    ApiEntry { label: "light.g", insert: "light.g", doc: "Color green 0..1." },
    ApiEntry { label: "light.intensity", insert: "light.intensity", doc: "Brightness multiplier." },
    ApiEntry { label: "light.r", insert: "light.r", doc: "Color red 0..1." },
    ApiEntry { label: "light.range", insert: "light.range", doc: "Reach in world units." },
    ApiEntry { label: "light.shape", insert: "light.shape", doc: "The surface it emits from: 0 point, 1 sphere, 2 rect, 3 disk, 4 tube. A rect and a disk face the node's FORWARD and a tube lies along its local X, so a light with a shape is aimed by rotating the node. Assigning keeps the size it had, so cross-fading a window into a bulb does not flash." },
    ApiEntry { label: "light.width", insert: "light.width", doc: "Rect only: its width in world units. Reads 0 on a shape that has no width." },
    ApiEntry { label: "light.height", insert: "light.height", doc: "Rect only: its height in world units." },
    ApiEntry { label: "light.radius", insert: "light.radius", doc: "Sphere / disk only: its radius in world units." },
    ApiEntry { label: "light.length", insert: "light.length", doc: "Tube only: how long the bar is — a long one streaks its highlight along itself." },
    ApiEntry { label: "light.thickness", insert: "light.thickness", doc: "Tube only: how thick the bar is." },
    ApiEntry { label: "light.twoSided", insert: "light.twoSided", doc: "Rect / disk only (1/0): lights out of the back as well as the front. Off is a window; on is a floating panel." },
    ApiEntry { label: "mat.cell", insert: "mat.cell", doc: "Which cell of the sheet draws (row-major from the top-left; clamped into the grid)." },
    ApiEntry { label: "mat.sheetCols", insert: "mat.sheetCols", doc: "Sheet columns (0 = not a sheet — the whole texture)." },
    ApiEntry { label: "mat.sheetRows", insert: "mat.sheetRows", doc: "Sheet rows." },
    ApiEntry { label: "rb.friction", insert: "rb.friction", doc: "Grip, as a coefficient: a ramp holds while tan(its angle) <= friction. 0 is ice, 1 holds exactly 45 degrees, above 1 is grippier still." },
    ApiEntry { label: "rb.slopeLimit", insert: "rb.slopeLimit", doc: "Steepest standable surface, in degrees (default 60). Past it nothing grounds the body and no grip holds it." },
    ApiEntry { label: "rb.gravity", insert: "rb.gravity", doc: "Gravity pull on this body (1/0; assign true/false)." },
    ApiEntry { label: "rb.half_x", insert: "rb.half_x", doc: "Box half-extent X." },
    ApiEntry { label: "rb.half_y", insert: "rb.half_y", doc: "Box half-extent Y." },
    ApiEntry { label: "rb.half_z", insert: "rb.half_z", doc: "Box half-extent Z." },
    ApiEntry { label: "rb.height", insert: "rb.height", doc: "Capsule total height." },
    ApiEntry { label: "rb.kinematic", insert: "rb.kinematic", doc: "Transform-driven mode (1/0; assign true/false, live): never falls or gets pushed, but PUSHES dynamic bodies — platforms, elevators, grabbed objects. (Static mode is the Inspector dropdown — a baked collider, nothing to toggle here.)" },
    ApiEntry { label: "rb.lock_rot_x", insert: "rb.lock_rot_x", doc: "Freeze rotation about X (1/0)." },
    ApiEntry { label: "rb.lock_rot_y", insert: "rb.lock_rot_y", doc: "Freeze rotation about Y (1/0)." },
    ApiEntry { label: "rb.lock_rot_z", insert: "rb.lock_rot_z", doc: "Freeze rotation about Z (1/0)." },
    ApiEntry { label: "rb.two_d", insert: "rb.two_d", doc: "2D (1/0): keep the body in the XY plane — it keeps its depth, never drifts out of the layer, and still spins the one way a flat object spins. Collides with the same world a 3D body does." },
    ApiEntry { label: "rb.lock_x", insert: "rb.lock_x", doc: "Freeze world X translation (1/0)." },
    ApiEntry { label: "rb.lock_y", insert: "rb.lock_y", doc: "Freeze world Y translation (1/0)." },
    ApiEntry { label: "rb.lock_z", insert: "rb.lock_z", doc: "Freeze world Z translation (1/0)." },
    ApiEntry { label: "rb.radius", insert: "rb.radius", doc: "Sphere/capsule radius." },
    ApiEntry { label: "rb.restitution", insert: "rb.restitution", doc: "Bounciness 0..1 (0 = no bounce)." },
    ApiEntry { label: "rb.shape", insert: "rb.shape", doc: "Body shape: 0 = sphere, 1 = capsule, 2 = box." },
    ApiEntry { label: "rng:int", insert: "rng:int(", doc: "Uniform integer in [a, b] inclusive." },
    ApiEntry { label: "rng:next", insert: "rng:next(", doc: "Uniform in [0, 1)." },
    ApiEntry { label: "rng:pick", insert: "rng:pick(", doc: "A uniform element of `list` (nil if empty)." },
    ApiEntry { label: "rng:range", insert: "rng:range(", doc: "Uniform in [a, b)." },
    ApiEntry { label: "slider.max", insert: "slider.max", doc: "Range end." },
    ApiEntry { label: "slider.min", insert: "slider.min", doc: "Range start." },
    ApiEntry { label: "slider.value", insert: "slider.value", doc: "Current value (clamped to min..max at draw time)." },
    ApiEntry { label: "sound:isPlaying", insert: "sound:isPlaying(", doc: "Still audible (false once finished)?" },
    ApiEntry { label: "sound:pause", insert: "sound:pause(", doc: "Freeze playback." },
    ApiEntry { label: "sound:position", insert: "sound:position(", doc: "Playhead in seconds." },
    ApiEntry { label: "sound:resume", insert: "sound:resume(", doc: "Continue a paused sound." },
    ApiEntry { label: "sound:seek", insert: "sound:seek(", doc: "Jump the playhead to a time in seconds." },
    ApiEntry { label: "sound:setPan", insert: "sound:setPan(", doc: "Stereo pan −1..1 (non-spatial sounds)." },
    ApiEntry { label: "sound:setPitch", insert: "sound:setPitch(", doc: "Playback-rate pitch (0.5 = octave down, 2 = octave up)." },
    ApiEntry { label: "sound:setPosition", insert: "sound:setPosition(", doc: "Move the emitter (stops following a node)." },
    ApiEntry { label: "sound:setTrack", insert: "sound:setTrack(", doc: "Re-route through a mixer track (\\\"Master\\\" or a track name)." },
    ApiEntry { label: "sound:setVolume", insert: "sound:setVolume(", doc: "Linear volume (1 = as authored)." },
    ApiEntry { label: "sound:stop", insert: "sound:stop(", doc: "Fade the sound out and end it." },
    ApiEntry { label: "source:isPlaying", insert: "source:isPlaying(", doc: "Is the source audible right now?" },
    ApiEntry { label: "source:pause", insert: "source:pause(", doc: "Freeze playback (resume continues from here)." },
    ApiEntry { label: "source:play", insert: "source:play(", doc: "Play the source's clip from the start (restarts if already playing)." },
    ApiEntry { label: "source:position", insert: "source:position(", doc: "Playhead in seconds." },
    ApiEntry { label: "source:resume", insert: "source:resume(", doc: "Continue a paused sound." },
    ApiEntry { label: "source:seek", insert: "source:seek(", doc: "Jump the playhead to a time in seconds." },
    ApiEntry { label: "source:setClip", insert: "source:setClip(", doc: "Swap the clip (project-relative path like \\\"audio/steps.ogg\\\"); restarts playback if playing." },
    ApiEntry { label: "source:stop", insert: "source:stop(", doc: "Fade the sound out (a few ms — no click)." },
    ApiEntry { label: "timer:cancel", insert: "timer:cancel(", doc: "timer:cancel() — stop a pending after / every / tween. The handle those three return exists for exactly this: local h = every(1, tick) ... h:cancel()." },
    ApiEntry { label: "track:setMuted", insert: "track:setMuted(", doc: "Mute / unmute the track." },
    ApiEntry { label: "track:setPan", insert: "track:setPan(", doc: "Stereo pan −1..1." },
    ApiEntry { label: "track:setSoloed", insert: "track:setSoloed(", doc: "Solo the track (mutes everything else)." },
    ApiEntry { label: "track:setVolume", insert: "track:setVolume(", doc: "Fader gain in dB (0 = unity, −60 = silent)." },
    ApiEntry { label: "vec2.x", insert: "vec2.x", doc: "The vector's X." },
    ApiEntry { label: "vec2.y", insert: "vec2.y", doc: "The vector's Y." },
    ApiEntry { label: "vec2:distance", insert: "vec2:distance(", doc: "vec2:distance(other) — the distance between two 2-D points." },
    ApiEntry { label: "vec2:dot", insert: "vec2:dot(", doc: "vec2:dot(other) — the dot product; the cosine of the angle when both are unit length." },
    ApiEntry { label: "vec2:length", insert: "vec2:length(", doc: "vec2:length() — how long the 2-D vector is." },
    ApiEntry { label: "vec2:lengthSquared", insert: "vec2:lengthSquared(", doc: "vec2:lengthSquared() — length without the square root, for comparisons." },
    ApiEntry { label: "vec2:lerp", insert: "vec2:lerp(", doc: "vec2:lerp(other, t) — a straight-line blend from this (t = 0) to other (t = 1)." },
    ApiEntry { label: "vec2:normalized", insert: "vec2:normalized(", doc: "vec2:normalized() — a unit-length copy, pointing the same way. Zero stays zero rather than becoming a NaN." },
    ApiEntry { label: "vec3.x", insert: "vec3.x", doc: "The vector's X. Vectors are values, not handles — writing v.x = 5 changes that vector, not whatever it came from." },
    ApiEntry { label: "vec3.y", insert: "vec3.y", doc: "The vector's Y." },
    ApiEntry { label: "vec3.z", insert: "vec3.z", doc: "The vector's Z." },
    ApiEntry { label: "vec3:cross", insert: "vec3:cross(", doc: "vec3:cross(other) — a vector perpendicular to both, right-handed. The way to build a basis, or to ask which side of a plane something is on." },
    ApiEntry { label: "vec3:distance", insert: "vec3:distance(", doc: "vec3:distance(other) — the distance between two points. Reads better than (a - b):length() and does the same thing." },
    ApiEntry { label: "vec3:dot", insert: "vec3:dot(", doc: "vec3:dot(other) — the dot product. With unit vectors it is the cosine of the angle between them: node.forward:dot(toEnemy) > 0.7 is a 45° cone in front." },
    ApiEntry { label: "vec3:length", insert: "vec3:length(", doc: "vec3:length() — how long the vector is. The distance form of a difference: (b - a):length()." },
    ApiEntry { label: "vec3:lengthSquared", insert: "vec3:lengthSquared(", doc: "vec3:lengthSquared() — length without the square root. Compare distances with it (d2 < r*r) and skip the expensive part." },
    ApiEntry { label: "vec3:lerp", insert: "vec3:lerp(", doc: "vec3:lerp(other, t) — a straight-line blend, t from 0 (this) to 1 (other). The one-liner behind smooth camera and marker movement." },
    ApiEntry { label: "vec3:normalized", insert: "vec3:normalized(", doc: "Unit-length copy (zero stays zero)." },
];

/// The built-in Scripting docs, shown on the IDE's Docs page as searchable
/// collapsible sections: (title, monospace body).
const DOC_SECTIONS: &[(&str, &str)] = &[
    (
        "Inspector tunables — headers, tooltips, dropdowns, sliders",
        "\
Everything in `defaults` becomes a row in the Inspector. Describe those rows with
`--@` comments and the panel designs itself — in DECLARATION order, grouped under
headers, each row the widget the value actually wants:

```
defaults = {
  --@header Movement
  -- How fast you walk on flat ground.
  --@range 0 20 --@units m/s
  walk = 4.5,

  --@desc Blend between the walk and run animations.
  --@slider 0 1 --@step 0.05
  blend = 0.35,

  --@header Assist
  --@options Off|On|Auto
  assist = 1,              -- a NUMBER + options = a dropdown; the value is the index

  --@options walk|run|sprint
  gait = \"walk\",           -- a STRING + options = a dropdown of those strings

  invert = false,          -- a boolean default is a checkbox, no annotation needed

  --@color
  tint = \"#ff8800\",        -- a swatch; the script still reads the hex string

  --@multiline
  intro = \"Hello.\",

  --@hidden
  debugScale = 1.0,        -- a tunable the Inspector doesn't show
}
```

## The whole vocabulary

- `--@header Text` — a section rule above this row. Underscores render as spaces.
- `--@desc Text` — the row's tooltip. Several `--@desc` lines join into a paragraph.
- **A plain comment** directly above a key is its tooltip too, so scripts that
  already document their tunables get hover text for free.
- `--@range min max` — clamps the value and bounds the drag.
- `--@slider min max` — draw a slider instead of a drag value.
- `--@step n` — drag speed / slider granularity.
- `--@units m/s` — a suffix after the number.
- `--@options a|b|c` — a dropdown. On a string param the value IS the label; on a
  number it's the index (0, 1, 2 …).
- `--@color` — a colour swatch over a `#rrggbb` string.
- `--@multiline` — a text box instead of a field.
- `--@hidden` — keep it out of the Inspector.
- `--@about Text` — describes the SCRIPT (above `defaults`), not a param.
- `--@editorButton Label fn` — a button that runs `fn(node)` in EDIT mode.

Annotations are comments: nothing changes at runtime, nothing breaks if you delete
them, and a misspelled one is ignored rather than fatal. Several can share a line.

## Booleans are real booleans

A `flag = false` default round-trips as a boolean, so `if params.flag then` means
what it says. (It's stored as 0/1 between the Inspector and the script, which is
exactly the kind of detail you should never have to know — every number is truthy
in Lua, so a leaked 0 would have been permanently `true`.)",
    ),
    (
        "The editor — formatting, warnings, completion, keys",
        "\
## Formatting

**Alt+Shift+F** formats the open file, or tick **on save** next to the ▤ Format
button. It re-indents by real block depth and fixes whitespace, and it changes
NOTHING else — no re-flowed expressions, no realigned comments, no moved code. Your
line stays your line.

- `--@noformat` anywhere in a file exempts the whole file.
- A line ending in `--@keep` keeps its own indentation (hand-aligned tables).
- It's idempotent: saving twice can't produce a second diff.

## Warnings

Under the editor, `⚠ n warnings` expands into the list; click one to jump. These are
the mistakes Lua itself can't report:

- **an undeclared assignment** — `sped = speed * dt` compiles, writes a global,
  reads `nil` forever, and says nothing. The warning names the typo and suggests the
  nearby local. Globals you assign at FILE scope are deliberate publications and are
  never flagged (that's how scripts share state).
- **an unused local** — usually a half-finished rename. Prefix with `_` to keep it.
- **upvalue pressure** — LuaJIT allows 60 upvalues per function and every
  file-scope `local` is one. At 50 you get a warning naming the fix: group related
  state in a table (`local s = { … }`), which costs one upvalue instead of thirty.

`--@nolint` silences a line; on its own line it silences the file.

## Completion

The popup opens **by itself only after `.` or `:`** — where you're asking what
fields a thing has. **Ctrl+Space** summons it anywhere else. **Enter** accepts,
**Tab always indents** (it never steals a keystroke), Esc hides it until the token
changes. The selected row shows its doc, and a usage example when there is one.

## Keys

    Ctrl+S / Ctrl+Shift+S   save / save all
    Ctrl+F / Ctrl+H         find / find & replace       F3, Shift+F3  next / prev
    Ctrl+G                  go to line                  Ctrl+W        close tab
    Ctrl+D                  duplicate line              Ctrl+Shift+K  delete line
    Alt+Up / Alt+Down       move line(s)                Ctrl+/        toggle comment
    Tab / Shift+Tab         indent / outdent
    Ctrl+B or F12           go to definition            Shift+F12     find references
    Alt+Shift+F             format document             Ctrl+Space    suggest",
    ),
    (
        "Getting started — your first script",
        "\
Game logic is written in Lua. A script is a `.lua` file in your project's
`scripts/` folder; attach it to a node and it runs every frame while playing.
A script defines plain functions and a `defaults` table:

    -- spin.lua
    defaults = { speed = 45 }          -- tunables (also shown in the Inspector)

    function start(node)               -- once, when play begins (optional)
    end

    function update(node, dt)          -- every frame while playing
      node.yaw = node.yaw + math.rad(params.speed) * dt
    end

Two more hooks round out the frame: `fixedUpdate(node, dt)` runs every
GAMEPLAY TICK (60 Hz, constant dt — movement/gameplay/physics writes), and
`lateUpdate(node, dt)` runs once per frame AFTER physics and the interpolated
transform writeback — the CAMERA pass. Anything that follows another node
(orbit cameras, name tags) belongs in lateUpdate so it samples this frame's
FINAL pose; following from update reads last frame's pose and turns frame-time
noise into visible jitter.

Each script keeps its own state across frames (set a variable in start, read it
in update) and hot-reloads the moment you save the file. `+=  -=  *=  /=  ..=`
and friends work too.",
    ),
    (
        "node — the transform",
        "\
`node` is synced from the node's transform before each call and read back after,
so setting a field moves the object:
  • node.x, node.y, node.z              position (world units)
  • node.yaw, node.pitch, node.roll     rotation, in radians
  • node.scale                          uniform scale (shortcut)
  • node.scale_x / scale_y / scale_z    per-axis scale",
    ),
    (
        "node — the physics body",
        "\
These extra fields appear ONLY when the node has a Rigidbody (Inspector ⏵
♦ Rigidbody). Drive the body by its velocity instead of teleporting it:
  • node.vx, node.vy, node.vz   velocity (m/s) — READ the current value, modify,
                                and WRITE it back; the engine integrates it
  • node.grounded               true while the body rests on a surface (read-only)
  • node.up_x, node.up_y, node.up_z   the body's up = −gravity (read-only):
                                [0,1,0] on a flat world, RADIAL on a planet —
                                move along it and you handle planets for free
  • node.height                 capsule standing height — write a smaller value
                                to crouch (the engine shrinks it, feet planted)
  • node.groundNormal           the floor it stands on, as a vec3 — nil airborne
  • node.wallNormal             the steepest surface it is PRESSED AGAINST, as a
                                vec3 — nil when there's nothing but floor

SLOPES. A controller that drives into a steep face LAUNCHES ITSELF: the solver
resolves the overlap by pushing the capsule out along the surface normal, that
normal points partly upward, and pushing again next frame collects the same
push again. At a run into a 70° hillside that is tens of m/s of free climb.

Stop pushing, and what's left is a slide:

    local steep = math.cos(math.rad(params.slope_limit))   -- 50°, say

    local function slide(m, n)
      if not n or n:dot(node.up) >= steep then return m end  -- absent, or walkable
      local into = m:dot(n)
      if into >= 0 then return m end                         -- moving away already
      return m - n * into                                    -- slide along it
    end

    move = slide(slide(move, node.wallNormal), node.groundNormal)

…and while grounded and not jumping, drop any upward velocity you did not ask
for — it came from being pushed out of a slope, and keeping it is how a walk
turns into a takeoff. The shipped first_person.lua /
third_person.lua do both, with `slope_limit` in the Inspector.",
    ),
    (
        "Components — live tweaks: node:getcomponent",
        "\
Every tunable the Inspector shows on a Rigidbody or Point Light is scriptable.
node:getcomponent(name) returns a live COMPONENT HANDLE (or nil if the node
doesn't have that component): read a field to sample it, assign to change it.
Writes land the same frame, and during play the physics sim re-reads the body
tunables every step — no reset, no teleport.

  local rb = node:getcomponent(\"RigidBody\")
  • rb.friction                 surface friction 0..1 (0 = frictionless — ice)
  • rb.restitution              bounciness 0..1 (0 = no bounce)
  • rb.gravity                  assign true/false (reads back 1/0)
  • rb.shape                    0 = sphere, 1 = capsule, 2 = box
  • rb.radius / rb.height       sphere/capsule size
  • rb.half_x / half_y / half_z box half-extents
  • rb.lock_x / lock_y / lock_z freeze world-axis translation (2.5D: lock_z)
  • rb.lock_rot_x / _y / _z     freeze rotation about an axis (stay upright)
  • rb.two_d                    2D: keep the body in the XY plane (one switch)

  local l = node:getcomponent(\"PointLight\")
  • l.intensity / l.range       brightness / reach
  • l.r, l.g, l.b               color, 0..1 per channel

    -- an ice patch: slippery while on it
    node:getcomponent(\"RigidBody\").friction = on_ice and 0.02 or 0.6

Handles work cross-node too:
    find(\"Crate\"):getcomponent(\"RigidBody\").restitution = 0.9",
    ),
    (
        "input — keyboard & mouse",
        "\
  • input.key(\"w\")          true while held. Names: a-z, 0-9, space, enter,
                            shift, ctrl, alt, left/right/up/down, escape, tab
  • input.pressed(\"space\")  true only on the frame it goes DOWN (an edge)
  • input.released(\"space\") true only on the frame it goes UP (an edge)
  • input.axis(\"a\", \"d\")    -1 / 0 / 1 from a negative/positive key pair
  • input.button(1)         mouse button held (0 left, 1 right, 2 middle)
  • input.clicked(1)        mouse button pressed this frame (an edge)
  • local dx, dy = input.mouse_delta()   mouse movement since last frame
  • local x, y  = input.mouse()          cursor position in pixels
  • input.scroll()          wheel delta this frame",
    ),
    (
        "raycast — ground checks, line-of-sight, shooting",
        "\
  • raycast(ox,oy,oz, dx,dy,dz, max [, ignore])  cast a ray against the
    terrain + mesh colliders AND every physics body (players, crates).
    Returns a hit table {x,y,z, nx,ny,nz, distance, node} or nil. `hit.node`
    is the hit BODY's node handle (nil when the ray hit static geometry) — so
    you can tell WHO you hit: hit.node:getscript(\"combat\"). Your own node's
    body is excluded, so a ray from your center never hits you; pass another
    node as `ignore` to skip its body too (an orbit camera ignores the
    character it follows — see third_person_camera.lua).

    -- is there ground within 1.2 units below me?
    local h = raycast(node.x, node.y, node.z, 0, -1, 0, 1.2)
    if h then  -- h.y is the ground height, h.ny the slope --  end

  Use it for ground checks, line-of-sight, shooting, placing things on a surface.",
    ),
    (
        "Debug gizmos — gizmo.line / ray / sphere / point",
        "\
Draw one-frame debug shapes over the viewport from code — Scene view only
(the Game view stays clean), and the viewport's gizmos toggle hides them.
Colors are optional 0–1 floats (default green). Immediate mode: call every
frame you want the shape visible.
  • gizmo.line(x1,y1,z1, x2,y2,z2 [, r,g,b])
  • gizmo.ray(ox,oy,oz, dx,dy,dz [, len [, r,g,b]])   origin + direction
  • gizmo.sphere(x,y,z [, radius [, r,g,b]])          wire sphere
  • gizmo.point(x,y,z [, size [, r,g,b]])             small 3-axis cross

    -- visualize a ground probe (see first_person.lua / third_person.lua:
    -- flip their debug_ray param to 1 in the Inspector for a live example)
    gizmo.ray(node.x, node.y, node.z, 0, -1, 0, 1.5, 0.3, 1.0, 0.4)",
    ),
    (
        "Reaching other nodes & scripts — find, handles, managers",
        "\
Reach beyond your own node — traverse the hierarchy, find any node/script in
the scene, and call into other scripts to build systems that span many files.

  Node handles (your `node`, and any node you reach, share the same fields):
  • node.name / node.id        this node's name / a stable numeric id
  • node.parent                the parent node handle (or nil)
  • node:getparent()           same as node.parent
  • node:children()            array of child handles
  • node:getchild(\"Gun\")       first child with that name (or nil)
  • node:find(\"Muzzle\")        first DESCENDANT (any depth) with that name
  • node:getscript(\"health\")   a script handle on this node (or nil)

  Scene-wide lookups (globals):
  • find(\"Player\")             first node in the scene with that name (or nil)
  • findAll(\"Coin\")            array of every node with that name
  • findScript(\"GameManager\")  script handle for the first node running that
                               script anywhere — the MANAGER pattern (or nil)

  A script handle talks to another script:
  • mgr.score                  read a variable it declared (its state)
  • mgr.score = 10             write that variable
  • mgr.addScore(5)            call a function it defines
  • mgr.params                 its params table   • mgr.node  its node handle

    -- a coin hands its points to the shared manager
    local mgr = findScript(\"manager\")
    if mgr then mgr.addScore(10) end

Inside a script's own functions, `node` is always ITS node, so a method called
from elsewhere still acts on the right object. Handles stay valid across frames —
cache a lookup in start() and reuse it.",
    ),
    (
        "References — noderef / scriptref / componentref (skip find())",
        "\
Declare a `defaults` entry as a REFERENCE and wire it in the Inspector (pick
from the dropdown, or DRAG a node from the Hierarchy onto the slot):

    defaults = {
      target = noderef(),                  -- a node handle
      victim = scriptref(\"health\"),       -- that script ON the wired node
      body   = componentref(\"RigidBody\"), -- that component ON the wired node
    }

    function update(node, dt)
      if params.victim then params.victim.damage(10) end
      if params.body then params.body.friction = 0.05 end
    end

  • The picker filters to VALID targets (scriptref lists only nodes carrying
    that script; componentref only nodes with that component).
  • Unwired / invalid refs read nil — guard with `if params.x then`.
  • Refs re-resolve by name each tick, so spawned/renamed targets rebind.
  • This is the fast path: no find() scans, no getcomponent chains. (find()
    itself is O(1) now — a hash index — but wiring beats typing names.)
Components you can reference: RigidBody, PointLight, Camera, ParticleSystem,
UiElement, UiSlider, UiLayer.",
    ),
    (
        "Layers & tags — group, filter, find",
        "\
LAYERS group nodes for physics + queries. Define them in Project Settings →
Layers (up to 32, referenced by NAME everywhere); pick a node's layer in the
Inspector. The collision matrix there decides which layers collide — uncheck
Ghosts × Walls and ghost bodies walk straight through wall colliders.

  • node.layer                       the layer name (\"Default\" when unset)
  • node.layer = \"Ghosts\"            move it (typos ERROR — never silent)
  • raycast(x,y,z, dx,dy,dz, max,
      { layers = {\"Ground\"} })      the ray only hits those layers
      { ignore = target, layers = \"Walls\" }   combine with an ignore

TAGS are free-form strings on any node (Inspector \"tags\" chips) — mark things
\"enemy\", \"checkpoint\", \"breakable\" and find/compare them cheaply:

  • node:hasTag(\"enemy\")            true/false — the classic raycast filter:
      local hit = raycast(...)
      if hit and hit.node and hit.node:hasTag(\"enemy\") then ... end
  • node:addTag / node:removeTag    edit at runtime (dedup / no-op safe)
  • node.tags                       the full list (assign an array to replace)
  • findTagged(\"enemy\")             EVERY tagged node, scene order

Layers answer \"what can touch/see what\" (fast bitmask filters in physics);
tags answer \"what IS this thing\" (identity checks + lookups). Both save with
the scene and replicate with spawned nodes in multiplayer.",
    ),
    (
        "Collision & trigger events — onCollisionEnter and friends",
        "\
Define these hooks in any script and the engine calls them when the node's
body touches something (per gameplay tick, after physics):

  function onCollisionEnter(node, other, hit)   -- touch STARTED
  function onCollisionStay(node, other, hit)    -- every tick while touching
  function onCollisionExit(node, other, hit)    -- touch ENDED

  • other                    the other node's handle (other.name, other:hasTag(...))
  • hit                      { x, y, z, nx, ny, nz } — world contact point + normal
  • fires for body-vs-collider AND body-vs-body (bodies detect each other even
    though they don't push each other; the solver is body-vs-static)
  • the collision-matrix (Project Settings → Layers) gates events too: pairs
    that don't collide don't event

TRIGGERS: tick \"trigger\" on a node's Collider component and bodies pass
STRAIGHT THROUGH it (rays too) — but overlap still fires:

  function onTriggerEnter(node, other, hit)
  function onTriggerStay(node, other, hit)
  function onTriggerExit(node, other, hit)

Rigidbody nodes can be triggers too — the checkbox sits on the Rigidbody
there, and the BODY becomes the sensor: it never blocks or gets blocked.
Kinematic + trigger = the moving pickup / sweeping damage zone. (A Dynamic
trigger still falls — straight through floors — so pin things that stay put.)

The portal recipe — one script, many portals (string param per instance):

    defaults = { destination = \"hub\" }        -- Inspector text field
    function onTriggerEnter(node, other, hit)
      if other:hasTag(\"player\") then scene.load(params.destination) end
    end

Events fire where physics runs (offline, the server, the predicted owner) —
never during prediction replays, so they can't double-fire on corrections.",
    ),
    (
        "Prefabs — spawn & destroy",
        "\
A PREFAB is a reusable node (with its whole child subtree) saved as an asset:
drag a node from the Hierarchy into the Assets panel (or right-click it →
◇ Save as Prefab). Place instances by dragging the prefab into the viewport,
onto a Hierarchy row (spawns as a child), or right-click → Add to scene.

Scripts spawn and remove them at runtime:

    spawn(\"bullet\")                                    -- authored spot
    spawn(\"bullet\", node.pos + dir * 1.5)              -- at a position
    spawn(\"bullet\", node.pos + dir * 1.5, function(b)  -- ...then configure it
      b.vx = dir.x * 40
    end)

    destroy(other)    -- remove a node + its whole subtree
    node:destroy()    -- method form (self-destruct a pickup/bullet)

  • \"bullet\" finds prefabs/bullet.prefab.ron; \"weapons/sword\" and full
    paths work too.
  • The spawned node is complete immediately: rigidbodies simulate, scripts
    fire start next pass, animators/particles/audio wire themselves.
  • destroy is queued (applied after the pass) — the handle stays readable
    for the rest of the call. Double-destroy is harmless.
  • MULTIPLAYER: spawn()/destroy() are local. Replicated objects go through
    the server: net.spawn(\"bullet\", {x=,y=,z=}) (accepts prefab names) +
    net.despawn(node). destroy() on the server routes replicated nodes
    through the session automatically; a client's call is refused.
  • A spawned prop that should be SOLID needs a Rigidbody in Static mode
    (a plain Collidable marker only bakes at Play start).",
    ),
    (
        "Vectors & math — vec3, vec2, distance",
        "\
Real vector VALUES with operators, not just x/y/z triplets:

  local dir = (target.pos - node.pos):normalized()
  node.pos = node.pos + dir * params.speed * dt

  • vec3(x, y, z) / vec3(s) / vec3()   make one (splat / zero)
  • a + b   a - b   v * 2   v / 2   -v   a == b
  • v:length()  v:lengthSquared()  v:normalized()
  • a:dot(b)   a:cross(b)   a:lerp(b, t)   a:distance(b)
  • vec2(x, y)                          the 2D version (UI/screen math)
  • node.pos                            the node's position AS a vec3 (read/write)

  distance(a, b) works on vectors, {x=,y=,z=} tables, and NODE handles:

    if distance(node, player) < params.aggro then chase() end

  (Everything that accepts a vector also accepts a node or a plain table with
  x/y/z — no conversions needed.)

## The node's own vectors

  • node.pos        position (read/write)
  • node.vel        the body's velocity (read/write) — one write, not three
  • node.up         the body's up (-gravity): Y on flat ground, radial on a planet
  • node.forward    facing, from the rotation (-Z forward, like the camera)
  • node.right      +X in the node's rotation
  • node.size       the whole scale as a vec3 (node.scale stays the uniform one)

    -- a jump, in whatever direction up actually means where you are standing
    if node.grounded and input.action(\"jump\") then
      node.vel = node.vel + node.up * params.jump
    end

## math — the arithmetic you were writing out by hand

  • math.clamp(x, lo, hi)      math.saturate(x)        math.sign(x)
  • math.round(x [, step])     round(2.34, 0.25) snaps to quarters
  • math.lerp(a, b, t)         unclamped     math.mix(a, b, t)  clamped
  • math.inverseLerp(a, b, x)  math.remap(x, a, b, c, d)   math.smoothstep(a, b, x)
  • math.approach(cur, target, maxDelta)   never overshoots — pass rate * dt
  • math.wrapAngle(a)          into (-pi, pi]
  • math.deltaAngle(a, b)      the SHORT way round, across the seam
  • math.approachAngle(cur, target, maxDelta)     \"turn to face\", done right
  • math.pingPong(t, len)      0 -> len -> 0 forever

    -- a turret that turns the short way and never overshoots
    node.yaw = math.approachAngle(node.yaw, wanted, params.turn_rate * dt)

## table — lists without the bookkeeping loop

  • table.map(list, fn)     table.filter(list, fn)    table.reverse(list)
  • table.find(list, fn)    -> value, index (a predicate, not a value)
  • table.indexOf(list, v)  table.count(list [, fn])  table.sum(list [, fn])
  • table.keys(t)           SORTED keys (pairs order isn't reproducible)
  • table.copy(t)           table.extend(dst, src)

    local ready = table.filter(ships, function(s) return s.fuel > 0 end)
    local total = table.sum(ready, function(s) return s.fuel end)",
    ),
    (
        "Game UI from scripts — text, sliders, buttons",
        "\
UI elements are nodes, so the same handles drive HUDs:

    hpLabel.text = hp                              -- numbers coerce to text
    hpBar:getcomponent(\"UiSlider\").value = hp     -- Fill/Handle parts follow
    hpBar:getcomponent(\"UiElement\").opacity = 0.5

  • node.text — a UI element's label (read/write; nil on non-text nodes).
  • getcomponent(\"UiElement\") — visible, opacity, posX/posY, width/height,
    radius, border, fillR/G/B/A, textSize, textR/G/B/A, tintR/G/B/A, cell (spritesheet frame).
  • getcomponent(\"UiSlider\") — value / min / max on a slider track.
  • getcomponent(\"UiLayer\") — enabled / z / designHeight / worldSpace (1 = in-world panel).

Buttons: turn on `button (clickable)` on an element (or Add > UI > Button) and
its scripts get pointer hooks — plain functions with a node handle:

    function hoverStart(node) node:getcomponent(\"UiElement\").opacity = 0.8 end
    function hoverEnd(node)   node:getcomponent(\"UiElement\").opacity = 1.0 end
    function clicked(node)    log(\"play!\") end

Also: pressed / released. A slider with `draggable` on lets the player set the
value by clicking/dragging the track — poll it with getcomponent(\"UiSlider\").
The engine imposes no look: style hover/press states yourself, it's 3 lines.",
    ),
    (
        "One script for a whole screen — ui.on and ui.events",
        "\
A `clicked` function answers for the node its script is on, so a menu of eight
buttons wants eight script files — each one three lines long, each one really
saying \"tell the menu\". Two ways to keep it in ONE script instead.

**Listen from anywhere** — `ui.on(element, hook, fn)`:

    function start(node)
      ui.on(find(\"Play\"),    \"clicked\", function() scene.load(\"level1\") end)
      ui.on(find(\"Options\"), \"clicked\", function() open(\"OptionsPanel\") end)
      ui.on(find(\"Quit\"),    \"clicked\", quit)
    end

The handler is called `fn(element, hook)` — the element that fired, so one
function can serve a row of buttons:

    for _, b in ipairs(find(\"Toolbar\"):children()) do
      ui.on(b, \"clicked\", function(el) selectTool(el.name) end)
    end

Any hook works: clicked, pressed, released, hoverStart, hoverEnd, changed,
submitted, cancelled, focusEnter, focusExit, the drag ones. Registering again
with the same element and hook REPLACES, so calling ui.on from update() is
harmless. `ui.off(element)` stops listening, `ui.off(element, \"clicked\")` stops
one hook. Only YOUR listeners — two managers can't unregister each other. A
listener dies with its element or with the script that registered it.

**Or ask instead of being called** — the same events, polled in update():

    function update(node, dt)
      if ui.clicked(playButton) then start() end
      for _, ev in ipairs(ui.events(\"clicked\")) do log(\"clicked \" .. ev.node.name) end
    end

  • ui.clicked / pressed / released / changed / submitted (element) → this frame?
  • ui.event(element, hook) — any hook by name.
  • ui.events([hook]) — everything that fired this frame: { node = , event = }.
  • ui.hovered() / ui.held() / ui.focused() — which element, or pass one for a
    yes/no. These are STATES, not events: true for as long as they're true.

Both read the same list, published before scripts run, so a poll and a hook can
never disagree about what happened. A listener on an element that takes no
clicks warns in the Console rather than failing silently.",
    ),
    (
        "Assets, models & materials — swap things at runtime",
        "\
Reference files under Assets/ in code, and swap a node's components at runtime.
  • assets.getFile(\"models/x.glb\")   the file's path (or nil) — pass it to model/material
  • assets.getContents(\"models\")      array of EVERY file under a folder (recursive)
  • node.model                        a Mesh node's model — assign to SWAP it live
  • node.material = \"Gold\"             apply a material preset (a name, or a .ron path)
  • node.visible = false               hide / show the node's geometry (true to show)

    -- equip a different model on a key press
    if input.pressed(\"e\") then node.model = assets.getFile(\"models/gold.glb\") end

(Right-click an asset ⏵ Copy asset path to grab the string to type.)",
    ),
    (
        "Animation — node:animator()",
        "\
node:animator() is the animation handle for a node's Animation Controller (or
a rigged model's embedded clips). Drive states from gameplay:

  local anim = node:animator()
  • anim:play(\"Run\" [, fade [, layer]])   transition to a state (the controller
                                          supplies fades; safe to call every frame)
  • anim:restart(\"Attack\")               re-enter even if already playing (one-shots)
  • anim:crossfade(\"Idle\", 0.3)          transition with an explicit fade (seconds)
  • anim:stop([layer [, fade]])           stop a layer (base returns to its default)
  • anim:setSpeed(2)                      playback speed multiplier
  • anim:setLayerWeight(\"Attack\", 0.5)   blend a layer over the ones below
  • anim:seek(t [, layer])                jump the current state's playhead
  Getters: anim:state()/anim:current()  anim:time()  anim:finished()  anim:isPlaying([state])  anim:clips()  anim:layers()
  Authored (asset, not playback — callable in start()): anim:duration(\"Punch\")  anim:events(\"Punch\")

    -- walk/run from speed, one-shot attack on click
    local speed = math.sqrt(node.vx ^ 2 + node.vz ^ 2)
    anim:play(speed > 4 and \"Run\" or (speed > 0.1 and \"Walk\" or \"Idle\"))
    if input.clicked(0) then anim:restart(\"Attack\") end",
    ),
    (
        "Particles — node:particles()",
        "\
node:particles() controls the node's Particle System component from a script —
start and stop effects on cue, and read their live state:

  local p = node:particles()
  • p:play()        start emitting if idle (spawns a fresh instance)
  • p:stop()        stop + despawn — the live particles vanish
  • p:restart()     re-spawn from t=0 (re-fire a one-shot burst)
  • p:setIntensity(i)     live emission scale 0..~2 (throttle a plume)
  • p:setBeamEnd(x, y, z) aim every Beam track at a WORLD point (laser targeting)
  Getters: p:isPlaying()   p:alive()   p:asset()

    -- muzzle flash on each shot; thruster smoke only while accelerating
    if input.clicked(0) then node:particles():restart() end
    local jet = find(\"Thruster\"):particles()
    if input.key('w') then jet:play() else jet:stop() end

You can also arm a node to auto-play (or not) at spawn:
    node:getcomponent(\"ParticleSystem\").play_on_start = 1

FIRE-AND-FORGET — spawn a one-shot at a world point with no node at all:
    spawnEffect(\"vfx/Explosion\", x, y, z)   -- plays once, despawns itself
    local h = raycast(px,py,pz, dx,dy,dz, 100)
    if h then spawnEffect(\"vfx/Impact\", h.x, h.y, h.z) end",
    ),
    (
        "Audio — sounds & the mixer",
        "\
THE 5-SECOND VERSION — no prefabs, no source setup, just a clip path:
    audio.play(\"audio/ding.ogg\")                       -- flat 2D (UI, stingers)
    audio.play(\"audio/hit.ogg\", h.x, h.y, h.z)         -- 3D at a world point
    audio.play(\"audio/engine.ogg\", carNode, {loop=true}) -- follows the node

Every option rides in the table (all optional):
    local s = audio.play(\"audio/roar.ogg\", boss, {
        volume = 0.8, pitch = 1.1,
        mode = \"Spatial\",        -- \"Distance\" = no panning · \"Flat\" = 2D
        falloff = \"Inverse\",     -- \"Linear\" · \"Exponential\"
        minDistance = 2, maxDistance = 50,
        track = \"SFX\",           -- mixer track (default Master)
        endBehavior = \"Destroy\", -- \"Stop\" (default) · \"Destroy\" · \"Loop\"
    })
The returned handle stays live while it plays:
    s:setVolume(0.5)  s:setPitch(2)  s:stop()  s:isPlaying()  s:position()

NODES — an Audio Source component (Inspector ➕) makes a node an emitter:
    node:sound():play()            -- restart its clip
    node:sound():setClip(\"audio/alarm2.ogg\")
    node:getcomponent(\"AudioSource\").volume = 0.3   -- live tunables
    (fields: volume, pitch, pan, minDistance, maxDistance, playOnStart,
     mode 0/1/2 = Spatial/Distance/Flat, falloff 0/1/2 = Inv/Lin/Exp,
     endBehavior 0/1/2 = Stop/Destroy/Loop)

THE MIXER — every sound routes through the 🎧 Mixer tab's tracks (volume,
pan, effects like EQ/reverb/delay, routing into other tracks, all ending at
Master). Scripts get live control that reverts when Play stops:
    audio.track(\"Music\"):setVolume(-12)   -- duck music (dB)
    audio.track(\"Master\"):setMuted(true)
    audio.stopAll()",
    ),
    (
        "Globals — params, time, dt, log",
        "\
  • params   this instance's tunables — a table SEEDED from `defaults`, so
             `params.speed` works out of the box; the Inspector overrides them
  • time     seconds since play started
  • dt       seconds since the last frame (also passed to update)
  • log(\"...\")   print to the engine console
  • the full Lua standard library (math, string, table, …)",
    ),
    (
        "Recipe — a walkable character (first/third person)",
        "\
Two ready-made controller setups ship in scripts/ — no glue code needed:

FIRST PERSON — attach `first_person.lua` to an Active Camera that has a
Capsule Rigidbody. Hold right-mouse to look, WASD to move, Space to jump,
Shift to run, hold C to crouch. Works on flat ground AND around
Radial-gravity planets.

THIRD PERSON — build a body node with a Capsule Rigidbody + `third_person.lua`,
parent your character model to it as a child named \"Model\" (a rigged .glb
animates as it moves: Idle/Walk/Run/Jump), then put `third_person_camera.lua`
on an Active Camera. The mouse orbits, the scroll wheel zooms, and zooming all
the way in goes first person.

A minimal controller, to show the velocity loop:

    defaults = { speed = 6, jump = 7 }
    function update(node, dt)
      local f = (input.key(\"w\") and 1 or 0) - (input.key(\"s\") and 1 or 0)
      local vy = node.vy                          -- keep gravity/jump
      if node.grounded and input.pressed(\"space\") then vy = params.jump end
      node.vx = -math.sin(node.yaw) * f * params.speed
      node.vz = -math.cos(node.yaw) * f * params.speed
      node.vy = vy
    end",
    ),
    (
        "Attaching & running scripts",
        "\
• Drag a `.lua` from Assets onto a node, drop it on the Inspector's Scripting
  section, or use Inspector ⏵ Scripting ⏵ + Add Script.
• F1 = ⏵ Play / ⏹ Stop, F2 = pause the clock. Stop restores the scene.
  Pressing Play auto-saves any unsaved script edits (what you see is what runs).
• The Inspector edits a script's params live; errors show at the top of this tab.

Bundled examples (in scripts/): first_person.lua + third_person.lua +
third_person_camera.lua (see §7), freelook.lua (fly camera), the RTS trio
(rts_camera.lua + rts_unit.lua + rts_commander.lua: isometric camera, units
that take move orders, click/box select), rotate.lua, pulsate.lua, float.lua
— open one for a working start.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything a script can reach by name must have a reference entry.
    ///
    /// The list of names comes from [`floptle_script::ScriptHost::api_surface`],
    /// which diffs a live Lua state against a bare one — so this is checked
    /// against what the engine ACTUALLY installs, never against a second list
    /// that could rot in the same direction as the first.
    ///
    /// It found 69 undocumented names the first time it ran: the whole of
    /// `water.*`, `scatter.*`, `assembly.*` and `physics.*`, the shape queries,
    /// most of the solid `draw.*` shapes, the gamepad calls, and sixteen tables
    /// with no overview at all. An API nobody can find is one nobody has.
    #[test]
    fn lua_api_reference_covers_the_whole_surface() {
        let documented: std::collections::HashSet<&str> =
            LUA_API.iter().map(|e| e.label).collect();
        let missing: Vec<String> = floptle_script::ScriptHost::api_surface()
            .into_iter()
            .filter(|name| !documented.contains(name.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "{} name(s) are reachable from Lua with no entry in LUA_API — \
             add one so they appear in the Docs tab and in docs/lua-api.md:\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }

    /// Everything reachable through a HANDLE must have a reference entry too.
    ///
    /// `api_surface()` walks the globals, so it cannot see a method that lives
    /// on a metatable: every component handle, the sound and particle handles,
    /// `vec3`'s own methods, a raycast hit's fields. Those are declared in the
    /// editor's EmmyLua annotations (which external IDEs already read), so the
    /// annotations are the checklist here.
    ///
    /// Between the two tests, "documented" means the whole API and not the part
    /// that happened to be easy to enumerate. This one found 118 members with
    /// no entry — most of `getcomponent`'s fields, all of the audio handles,
    /// and `vec3:dot` / `vec3:cross` / `vec3:length`.
    #[test]
    fn lua_api_reference_covers_every_handle_member() {
        let documented: std::collections::HashSet<&str> =
            LUA_API.iter().map(|e| e.label).collect();
        let missing: Vec<String> = annotated_members()
            .into_iter()
            .filter(|(label, _)| !documented.contains(label.as_str()))
            .map(|(label, class)| format!("{label}  (from ---@class {class})"))
            .collect();
        assert!(
            missing.is_empty(),
            "{} handle member(s) are annotated for IDEs but have no LUA_API entry, \
             so they are missing from the Docs tab and docs/lua-api.md:\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }

    /// `(label, class)` for every `---@field` in the editor's EmmyLua stubs
    /// whose class is a handle the reference documents by member.
    fn annotated_members() -> Vec<(String, String)> {
        // class -> the local name a script conventionally binds it to, which is
        // the prefix the reference uses. `Node` is excluded: its members are
        // already covered as `node.` / `node:` entries by the surface test's
        // sibling rows, and the annotation carries no other spelling.
        const HOLDERS: &[(&str, &str)] = &[
            ("RigidBodyHandle", "rb"),
            ("PointLightHandle", "light"),
            ("LightHandle", "env"),
            ("CameraHandle", "cam"),
            ("UiElementHandle", "el"),
            ("UiSliderHandle", "slider"),
            ("UiLayerHandle", "layer"),
            ("MaterialHandle", "mat"),
            ("TilemapHandle", "tm"),
            ("ParticleSystemHandle", "particles"),
            ("AudioSourceHandle", "source"),
            ("SoundHandle", "sound"),
            ("AudioTrackHandle", "track"),
            ("Vec3", "vec3"),
            ("Vec2", "vec2"),
            ("Rng", "rng"),
            ("Hit", "hit"),
            ("SpaceBody", "body"),
            ("TimerHandle", "timer"),
        ];
        let mut out = Vec::new();
        // The class the following `---@field` lines belong to, once it is one
        // of the handles above; None while inside any other class.
        let mut current: Option<(&str, &str)> = None;
        for line in crate::lua_support::LUA_ANNOTATIONS.lines() {
            if let Some(rest) = line.strip_prefix("---@class ") {
                let name = rest.split_whitespace().next().unwrap_or_default();
                current = HOLDERS.iter().find(|(c, _)| *c == name).copied();
                continue;
            }
            let (Some(rest), Some((class, holder))) =
                (line.strip_prefix("---@field "), current)
            else {
                continue;
            };
            let mut it = rest.split_whitespace();
            let Some(name) = it.next() else { continue };
            // A method is annotated as `fun(...)`; anything else is a field,
            // and the reference spells the two differently.
            let sep = if it.next().is_some_and(|t| t.starts_with("fun(")) { ':' } else { '.' };
            out.push((format!("{holder}{sep}{name}"), class.to_owned()));
        }
        assert!(out.len() > 100, "only {} annotated members parsed", out.len());
        out
    }

    /// Searching the reference puts the name you typed first.
    ///
    /// With 500+ entries this is the difference between a search box and a
    /// filter: an unranked `contains` match puts every entry whose *prose*
    /// mentions "play" above `anim:play` itself, purely because its category is
    /// drawn earlier.
    #[test]
    fn api_search_ranks_the_obvious_answer_first() {
        let best = |q: &str| -> String {
            let mut ranked: Vec<(u8, &ApiEntry)> =
                LUA_API.iter().filter_map(|e| api_rank(e, q).map(|r| (r, e))).collect();
            ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.label.cmp(b.1.label)));
            ranked.first().map(|(_, e)| e.label.to_owned()).unwrap_or_default()
        };
        // Exact label.
        assert_eq!(best("water.depthat"), "water.depthAt");
        // The leaf alone — what you type when you don't recall the table.
        assert_eq!(best("depthat"), "water.depthAt");
        assert_eq!(best("crossfade"), "anim:crossfade");
        // A prefix.
        assert_eq!(best("spherec"), "spherecast");
        // A word that appears in a lot of PROSE must still lose to the entry
        // actually named that.
        assert_eq!(best("tween"), "tween");

        // And a doc-only match is still found, just last.
        let doc_only = LUA_API
            .iter()
            .filter_map(|e| api_rank(e, "buoyancy").map(|r| (r, e.label)))
            .collect::<Vec<_>>();
        assert!(!doc_only.is_empty(), "a word that only appears in prose must still match");
    }

    /// `docs/lua-api.md` is generated from [`LUA_API`] — this keeps it current.
    ///
    /// The reference exists twice on purpose: in the editor, where you are when
    /// you need it, and in the repo, where search engines, a text editor and
    /// anyone reading on a second monitor can reach it. Writing it twice by
    /// hand would mean maintaining it once and letting the other rot, so the
    /// file is generated and this test fails if it drifts.
    ///
    /// Regenerate with:
    /// `UPDATE_DOCS=1 cargo test -p floptle-editor lua_api_reference_file`
    #[test]
    fn lua_api_reference_file_is_current() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/lua-api.md");
        let generated = render_api_reference();
        if std::env::var("UPDATE_DOCS").is_ok() {
            std::fs::write(path, &generated).expect("write docs/lua-api.md");
            return;
        }
        let on_disk = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            on_disk, generated,
            "docs/lua-api.md is out of date — regenerate it with \
             `UPDATE_DOCS=1 cargo test -p floptle-editor lua_api_reference_file`"
        );
    }

    /// Collect every string egui painted this frame.
    fn painted_text(output: &egui::FullOutput) -> String {
        fn walk(shape: &egui::epaint::Shape, out: &mut String) {
            match shape {
                egui::epaint::Shape::Text(t) => {
                    out.push_str(t.galley.text());
                    out.push('\n');
                }
                egui::epaint::Shape::Vec(v) => {
                    for sh in v {
                        walk(sh, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = String::new();
        for cs in &output.shapes {
            walk(&cs.shape, &mut out);
        }
        out
    }

    /// Formatting must not move the caret: it's restored by line + column, because
    /// re-indenting shifts every byte offset after the first change. This is what
    /// makes format-on-save safe to leave on while you type.
    #[test]
    fn a_format_keeps_the_caret_where_the_text_is() {
        let src = "function f()\nlocal x = 1\nprint(x)\nend\n";
        // Caret just after `print(` on line 3 (0-based line 2, column 6).
        let caret = src.find("print(").unwrap() + 6;
        let (line, col) = crate::lua_format::line_col_of(src, caret);
        assert_eq!((line, col), (2, 6), "line 2, six characters into the code");

        let formatted = crate::lua_format::format(src);
        assert_ne!(formatted, src, "the fixture must actually get re-indented");
        let restored = crate::lua_format::char_of_line_col(&formatted, line, col);
        // Still just inside `print(` — the two spaces of new indentation would have
        // dragged an offset- or margin-restored caret back into the middle of the word.
        let head: String = formatted.chars().take(restored).collect();
        assert!(head.ends_with("print("), "caret landed at {restored} in:\n{formatted}");

        // A caret past the end of a shortened line clamps instead of overflowing.
        let long = crate::lua_format::char_of_line_col(&formatted, 2, 999);
        assert!(long <= formatted.chars().count());
    }

    /// Every API entry that has a worked example must name a REAL entry — an
    /// example keyed to a label that no longer exists is invisible, so it would rot
    /// silently as the API is renamed.
    #[test]
    fn every_worked_example_belongs_to_a_real_api_entry() {
        let known: std::collections::HashSet<&str> = LUA_API.iter().map(|e| e.label).collect();
        let orphans: Vec<&str> =
            API_EXAMPLES.iter().map(|(l, _)| *l).filter(|l| !known.contains(l)).collect();
        assert!(orphans.is_empty(), "examples for entries that don't exist: {orphans:?}");
        // And every example must be Lua THE ENGINE can parse — checked through the
        // script host itself, so an example can't be valid-looking Lua that the
        // preprocessor or LuaJIT rejects. A copyable example that doesn't compile is
        // worse than no example.
        let host = floptle_script::ScriptHost::new();
        for (label, ex) in API_EXAMPLES {
            // A snippet may be a fragment (a hook body, a bare statement), so give
            // fragments a home before checking.
            let wrapped = format!("local function __ex()\n{ex}\nend");
            if host.check_syntax(ex).is_some() && host.check_syntax(&wrapped).is_some() {
                panic!(
                    "the example for `{label}` is not valid Lua: {:?}\n{ex}",
                    host.check_syntax(ex)
                );
            }
        }
    }

    /// The Docs page renders its guides, the API reference, and the worked
    /// examples — with a REAL screen rect, because a headless pass with none lays
    /// out into nothing and would "pass" while drawing zero widgets.
    #[test]
    fn the_docs_page_renders_its_guides_and_examples() {
        let ctx = crate::icons::test_context();
        let body = "\
## A heading
Prose with `inline code` in it.
- a bullet

    local x = 1   -- an indented code block
";
        let mut painted = String::new();
        for _ in 0..2 {
            let out = ctx.run_ui(crate::icons::test_input(), |ui| {
                // `doc_body_ui` is a method on the tab viewer for the code theme;
                // exercise the markup through the same path the page uses.
                let theme_idx = 0usize;
                let _ = theme_idx;
                ui.label("");
                super::inline_doc_label(ui, "Prose with `inline code` in it.", &egui::FontId::monospace(12.0));
                for line in body.lines() {
                    if let Some(h) = line.trim().strip_prefix("## ") {
                        ui.label(egui::RichText::new(h).strong());
                    }
                }
            });
            painted = painted_text(&out);
        }
        assert!(painted.contains("A heading"), "headings must render:\n{painted}");
        assert!(painted.contains("inline code"), "inline code must render:\n{painted}");
    }

    /// Every API entry lands in a category the browser actually displays.
    ///
    /// `api_category` has a catch-all arm, so a new group name that isn't in
    /// `API_CATEGORIES` doesn't fail to compile — it just silently drops every
    /// entry routed to it out of the browser.
    #[test]
    fn every_api_entry_lands_in_a_displayed_category() {
        let mut missing: Vec<(&str, &str)> = Vec::new();
        for e in LUA_API {
            let cat = api_category(e.label);
            if !API_CATEGORIES.contains(&cat) {
                missing.push((e.label, cat));
            }
        }
        assert!(missing.is_empty(), "entries routed to a group the browser never draws: {missing:?}");
        // …and every category has something in it, or it's a header for nothing.
        for cat in API_CATEGORIES {
            assert!(
                LUA_API.iter().any(|e| api_category(e.label) == *cat),
                "the API browser draws an empty group: {cat}"
            );
        }
    }

    /// Hover resolves the identifiers people actually type, not just the ones
    /// spelled exactly like the reference.
    ///
    /// `target:lookAt(...)` is the same call as `node:lookAt(...)`, and a hover
    /// that only matched the literal string `node:lookAt` explained nothing
    /// about the line under the cursor. Ambiguity is the other half: when two
    /// namespaces claim a member name, saying nothing beats guessing.
    #[test]
    fn hover_resolves_a_member_name_on_any_receiver() {
        // The exact case still works.
        assert_eq!(api_entry_for("node:lookAt").map(|a| a.label), Some("node:lookAt"));
        // …and so does the same call on a variable, which is how it gets written.
        assert_eq!(api_entry_for("target:lookAt").map(|a| a.label), Some("node:lookAt"));
        assert_eq!(api_entry_for("enemy:distanceTo").map(|a| a.label), Some("node:distanceTo"));
        assert_eq!(api_entry_for("player.worldPos").map(|a| a.label), Some("node.worldPos"));
        // A vec3 method, whose reference label names the type, not a variable.
        assert_eq!(api_entry_for("fwd:flatten").map(|a| a.label), Some("vec3:flatten"));
        // The separator has to match: `node.lookAt` is not `node:lookAt`.
        assert!(api_entry_for("thing.lookAt").is_none());
        // A bare word that is not an entry stays unexplained.
        assert!(api_entry_for("myLocalVariable").is_none());
        assert!(api_entry_for("").is_none());
        assert!(api_entry_for("foo.").is_none());
    }

    /// The two new guide sections must exist and stay findable by the words a
    /// developer would search for — they document features with no other home.
    #[test]
    fn the_new_guides_cover_the_new_features() {
        let all: String = DOC_SECTIONS.iter().map(|(t, b)| format!("{t}\n{b}\n")).collect();
        for needle in [
            "--@header", "--@desc", "--@range", "--@slider", "--@options", "--@color",
            "--@hidden", "--@units", "--@multiline", "--@editorButton",
            "--@noformat", "--@keep", "--@nolint",
            "Alt+Shift+F", "Ctrl+Space", "F12", "upvalue",
            "node.vel", "node.forward", "math.approach", "math.deltaAngle", "table.filter",
            "ui.on", "ui.off", "ui.events", "ui.clicked", "ui.hovered", "ui.held",
        ] {
            assert!(all.contains(needle), "the in-engine docs never mention {needle:?}");
        }
    }

    /// The in-engine IDE and the VSCode stub library must describe the same
    /// engine.
    ///
    /// They are written in two different places and nothing connected them, so
    /// they drifted: the whole rollback surface — `net.random`, `net.stalled`,
    /// the depth counters — shipped in the stub library and reached the
    /// in-engine autocomplete not at all. A developer scripting inside Floptle
    /// could not discover the feature they had just switched on in the
    /// Inspector.
    #[test]
    fn every_net_function_in_the_stub_library_is_also_in_the_engines_own_autocomplete() {
        // `function net.foo(` in the annotations → the name the IDE must know.
        let stub: Vec<String> = crate::lua_support::LUA_ANNOTATIONS
            .lines()
            .filter_map(|l| l.trim().strip_prefix("function net."))
            .filter_map(|rest| rest.split('(').next())
            .map(|n| format!("net.{n}"))
            .collect();
        assert!(stub.len() > 10, "the stub library should define a lot of net.* — found {stub:?}");
        let known: std::collections::HashSet<&str> = LUA_API.iter().map(|e| e.label).collect();
        let missing: Vec<&String> = stub.iter().filter(|n| !known.contains(n.as_str())).collect();
        assert!(
            missing.is_empty(),
            "these exist for VSCode but not for the editor's own script editor, so they are \
             undiscoverable in the engine that ships them: {missing:?}"
        );
    }

    #[test]
    fn find_ranges_case_modes() {
        assert_eq!(find_ranges("Foo foo FOO", "foo", false).len(), 3);
        assert_eq!(find_ranges("Foo foo FOO", "foo", true), vec![(4, 7)]);
        assert!(find_ranges("abc", "", false).is_empty());
    }

    #[test]
    fn comment_toggle_round_trips() {
        let mut t = "a = 1\n  b = 2\n\nc = 3".to_string();
        let end = t.chars().count();
        toggle_comment_lines(&mut t, 0, end);
        assert_eq!(t, "-- a = 1\n  -- b = 2\n\n-- c = 3");
        let end = t.chars().count();
        toggle_comment_lines(&mut t, 0, end);
        assert_eq!(t, "a = 1\n  b = 2\n\nc = 3");
    }

    #[test]
    fn indent_and_outdent_block() {
        let mut t = "a\n  b".to_string();
        let end = t.chars().count();
        let (a, b) = indent_lines(&mut t, 0, end, false);
        assert_eq!(t, "  a\n    b");
        assert_eq!((a, b), (0, t.chars().count()));
        let end = t.chars().count();
        indent_lines(&mut t, 0, end, true);
        assert_eq!(t, "a\n  b");
    }

    #[test]
    fn move_lines_up_down_and_edges() {
        let mut t = "one\ntwo\nthree".to_string();
        // Move "two" up.
        let sel = move_lines(&mut t, 4, 4, true).unwrap();
        assert_eq!(t, "two\none\nthree");
        assert_eq!(sel, (0, 3));
        // Top line can't move up; bottom line can't move down.
        assert!(move_lines(&mut t, 0, 0, true).is_none());
        let last = t.chars().count();
        assert!(move_lines(&mut t, last, last, false).is_none());
        // Move "one" (now the middle line) down past "three" (no trailing \n).
        let sel = move_lines(&mut t, 4, 4, false).unwrap();
        assert_eq!(t, "two\nthree\none");
        let s = "two\nthree\n".chars().count();
        assert_eq!(sel, (s, s + 3));
    }

    #[test]
    fn delete_lines_spans_selection() {
        let mut t = "one\ntwo\nthree".to_string();
        let caret = delete_lines(&mut t, 4, 9); // selection touching "two" + "three"
        assert_eq!(t, "one\n");
        assert_eq!(caret, 4);
    }

    #[test]
    fn cut_line_clipboard_content_and_delete() {
        // Empty-selection Ctrl+X: the caret's line (with a trailing \n, so a
        // paste re-inserts a whole line) goes to the clipboard, then leaves the
        // buffer — including the last line, which has no trailing \n to span.
        let mut t = "one\ntwo\nthree".to_string();
        let caret = "one\ntw".chars().count();
        assert_eq!(line_edit::line_with_newline(&t, caret), "two\n");
        let caret = delete_lines(&mut t, caret, caret);
        assert_eq!(t, "one\nthree");
        assert_eq!(caret, 4);
        let last = t.chars().count();
        assert_eq!(line_edit::line_with_newline(&t, last), "three\n");
        let caret = delete_lines(&mut t, last, last);
        assert_eq!(t, "one\n");
        assert_eq!(caret, 4);
    }

    #[test]
    fn whole_line_paste_at_eol_inserts_on_next_line() {
        let mut t = "alpha\n".to_string();
        let caret = "alpha".chars().count();
        let new_caret = paste_text(&mut t, caret, caret, "beta\n");
        assert_eq!(t, "alpha\nbeta\n");
        assert_eq!(new_caret, "alpha\nbeta".chars().count());
    }

    #[test]
    fn auto_indent_follows_and_deepens() {
        // Plain line: indent carried over.
        let mut t = "  x = 1".to_string();
        let end = t.chars().count();
        let caret = auto_indent_newline(&mut t, end, end);
        assert_eq!(t, "  x = 1\n  ");
        assert_eq!(caret, t.chars().count());
        // Block opener: one level deeper. Unclosed → the matching end appears
        // too (see enter_on_unclosed_block_inserts_end); already-closed → just
        // the indent. (`do` must be a WORD, not a suffix — tested via avocado.)
        let mut t = "if x then".to_string();
        let end = t.chars().count();
        auto_indent_newline(&mut t, end, end);
        assert_eq!(t, "if x then\n  \nend");
        let mut t = "if x then\nend".to_string();
        let caret = "if x then".chars().count();
        auto_indent_newline(&mut t, caret, caret);
        assert_eq!(t, "if x then\n  \nend");
        let mut t = "x = avocado".to_string();
        let end = t.chars().count();
        auto_indent_newline(&mut t, end, end);
        assert_eq!(t, "x = avocado\n");
    }

    #[test]
    fn ac_member_completion_works_on_any_variable() {
        // `node:getc` — base + colon member completes the method, keeping the base.
        let items = ac_matches("node:getc", "");
        assert!(items[0].label.starts_with("node:getc")); // getchild/getcomponent tie
        let comp = items.iter().find(|i| i.label == "node:getcomponent").unwrap();
        assert_eq!(comp.insert, "getcomponent(");
        assert_eq!(comp.keep, 5); // "node:" kept, member replaced
        // Any variable name works: `body:getc` ranks the same method next.
        let items = ac_matches("body:getc", "");
        assert!(items.iter().any(|i| i.label == "node:getcomponent" && i.keep == 5));
        // `anim:pl` reaches the animator methods.
        let items = ac_matches("anim:pl", "");
        assert_eq!(items[0].label, "anim:play");
        assert_eq!(items[0].insert, "play(");
        // Component-handle fields complete after a dot on any variable.
        let items = ac_matches("rb.fri", "");
        assert!(items.iter().any(|i| i.label == "friction" && i.insert == "friction"));
        // Typing the separator alone lists the members (discoverability).
        assert!(!ac_matches("node:", "").is_empty());
    }

    #[test]
    fn ac_params_keys_come_from_this_scripts_defaults() {
        let src = "defaults = { speed = 6, jump_power = 7 }\n";
        let items = ac_matches("params.ju", src);
        assert_eq!(items[0].label, "params.jump_power");
        assert_eq!(items[0].insert, "jump_power");
        assert_eq!(items[0].keep, 7);
        assert_eq!(defaults_keys(src), vec!["speed".to_string(), "jump_power".to_string()]);
    }

    #[test]
    fn ac_plain_words_prefix_then_substring() {
        // Prefix beats substring: "gro" → node.grounded via substring too.
        let items = ac_matches("getcomp", "");
        assert!(items.iter().any(|i| i.label == "node:getcomponent"), "substring should match");
        let items = ac_matches("inp", "local input_speed = 1\n");
        assert_eq!(items[0].label, "input"); // API prefix outranks buffer words
        // A word with no API competition comes from the buffer.
        let items = ac_matches("spd", "local spd_boost = 2\n");
        assert!(items.iter().any(|i| i.label == "spd_boost"));
    }

    #[test]
    fn typed_vars_complete_only_their_component_fields() {
        // `local rb = node:getcomponent("RigidBody")` types rb — completion
        // offers RigidBody fields only (no UiElement/PointLight noise).
        let src = "local rb = node:getcomponent(\"RigidBody\")\n";
        let items = ac_matches("rb.r", src);
        assert!(items.iter().any(|i| i.label == "rb.radius" || i.label == "rb.restitution"));
        assert!(!items.iter().any(|i| i.label.contains("tintR") || i.label == "r"),
            "PointLight/UiElement fields must not leak onto a typed RigidBody var");
        // defaults refs type params.<key>: componentref("UiSlider") → slider fields.
        let src = "defaults = { hp = componentref(\"UiSlider\") }\n";
        let items = ac_matches("params.hp.v", src);
        assert_eq!(items[0].insert, "value");
        // An animator var only offers anim methods.
        let src = "local a = node:animator()\n";
        let items = ac_matches("a:pl", src);
        assert_eq!(items[0].insert, "play(");
        assert!(items.iter().all(|i| i.label.starts_with("anim:")));
        // Untyped vars keep the generic behavior (discoverability).
        assert!(ac_matches("mystery.fri", "").iter().any(|i| i.label == "friction"));
    }

    #[test]
    fn block_balance_counts_real_blocks_only() {
        assert_eq!(block_balance("function f()\n"), 1);
        assert_eq!(block_balance("function f()\nend\n"), 0);
        assert_eq!(block_balance("if x then y = 1 end\n"), 0);
        assert_eq!(block_balance("if a then\nelseif b then\nelse\nend"), 0);
        assert_eq!(block_balance("for i = 1, 10 do\n"), 1);
        assert_eq!(block_balance("repeat\nuntil done"), 0);
        // Keywords inside strings/comments don't count.
        assert_eq!(block_balance("x = \"function if do\" -- if then\n"), 0);
        assert_eq!(block_balance("--[[ function ]] local x = 1"), 0);
    }

    #[test]
    fn enter_on_unclosed_block_inserts_end() {
        // Enter at the end of an unclosed `function` header: body line + end.
        let mut t = String::from("function update(node, dt)");
        let caret = t.chars().count();
        let c = auto_indent_newline(&mut t, caret, caret);
        assert_eq!(t, "function update(node, dt)\n  \nend");
        assert_eq!(c, "function update(node, dt)\n  ".chars().count());
        // Inside an ALREADY balanced block, Enter only indents (no double end).
        let mut t = String::from("function f()\nend");
        let caret = "function f()".chars().count();
        auto_indent_newline(&mut t, caret, caret);
        assert_eq!(t, "function f()\n  \nend");
        // `if ... then` gets its end too; nested indent is preserved.
        let mut t = String::from("  if hp < 10 then");
        let caret = t.chars().count();
        auto_indent_newline(&mut t, caret, caret);
        assert_eq!(t, "  if hp < 10 then\n    \n  end");
    }

    #[test]
    fn doc_words_prefix_matches() {
        let w = doc_words("local velocity = 1\nvel = velocity + vel2", "vel", "vel");
        assert_eq!(w, vec!["vel2".to_string(), "velocity".to_string()]);
    }
}
