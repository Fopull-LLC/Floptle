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
    /// The Docs page's filter box.
    pub(crate) docs_search: String,
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
    fn save_file(&mut self, i: usize) -> bool {
        let Some(f) = self.open.get_mut(i) else { return false };
        if std::fs::write(&f.path, &f.text).is_ok() {
            f.dirty = false;
            return true;
        }
        false
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
    let new: Vec<String> = block
        .split('\n')
        .map(|l| {
            if outdent {
                l.strip_prefix("  ")
                    .or_else(|| l.strip_prefix('\t'))
                    .or_else(|| l.strip_prefix(' '))
                    .unwrap_or(l)
                    .to_string()
            } else if l.trim().is_empty() {
                l.to_string()
            } else {
                format!("  {l}")
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
    let t = before_caret.trim_end();
    let opener = ends_with_word(t, "then")
        || ends_with_word(t, "do")
        || ends_with_word(t, "else")
        || ends_with_word(t, "repeat")
        || t.ends_with('{')
        || t.ends_with('(')
        || (t.ends_with(')') && t.contains("function"));
    let ins = if opener { format!("\n{indent}  ") } else { format!("\n{indent}") };
    text.replace_range(ba..bb, &ins);
    line_edit::char_of_byte(text, ba + ins.len())
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

/// Fields of the live component handles (`node:getcomponent(…)`), offered when
/// completing after a `.` on any variable — `rb.fri` → `friction`.
const HANDLE_FIELDS: &[(&str, &str)] = &[
    ("friction", "RigidBody handle: surface friction 0..1 (0 = frictionless — ice)."),
    ("restitution", "RigidBody handle: bounciness 0..1 (0 = no bounce)."),
    ("gravity", "RigidBody handle: gravity pull on this body — assign true/false (reads back 1/0)."),
    ("shape", "RigidBody handle: body shape — 0 sphere, 1 capsule, 2 box."),
    ("radius", "RigidBody handle: sphere/capsule radius."),
    ("height", "RigidBody handle: capsule total height."),
    ("half_x", "RigidBody handle: box half-extent X."),
    ("half_y", "RigidBody handle: box half-extent Y."),
    ("half_z", "RigidBody handle: box half-extent Z."),
    ("lock_x", "RigidBody handle: freeze world X translation (assign true/false)."),
    ("lock_y", "RigidBody handle: freeze world Y translation."),
    ("lock_z", "RigidBody handle: freeze world Z translation (e.g. for 2.5D)."),
    ("lock_rot_x", "RigidBody handle: freeze rotation about X (keeps a body upright)."),
    ("lock_rot_y", "RigidBody handle: freeze rotation about Y."),
    ("lock_rot_z", "RigidBody handle: freeze rotation about Z."),
    ("intensity", "PointLight handle: brightness multiplier."),
    ("range", "PointLight handle: reach in world units."),
    ("r", "PointLight handle: color red 0..1."),
    ("g", "PointLight handle: color green 0..1."),
    ("b", "PointLight handle: color blue 0..1."),
];

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
        for e in LUA_API {
            let Some(es) = e.label.find(['.', ':']) else { continue };
            if e.label.as_bytes()[es] as char != sepc {
                continue;
            }
            let (ebase, emember) = (&e.label[..es], &e.label[es + 1..]);
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
            // Component-handle fields on any variable: rb.fri → friction.
            for (f, d) in HANDLE_FIELDS {
                if f.starts_with(member) && *f != member {
                    push(&mut items, AcItem {
                        label: (*f).into(),
                        insert: (*f).into(),
                        keep: s + 1,
                        doc: Some((*d).into()),
                        score: 2,
                    });
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

    /// The Docs landing page: a search box over the sectioned scripting guide +
    /// the categorized API reference + IDE shortcuts.
    fn docs_page_ui(&mut self, ui: &mut egui::Ui) {
        // A filter box up top: the guides and the API reference both narrow to
        // matching entries, so devs can comb for exactly what they need.
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.ide.docs_search)
                    .hint_text("search the docs — try \"friction\", \"jump\", \"crossfade\", \"mouse\"")
                    .desired_width(320.0),
            );
            if !self.ide.docs_search.is_empty() && ui.small_button("✖").clicked() {
                self.ide.docs_search.clear();
            }
        });
        ui.add_space(4.0);
        let q = self.ide.docs_search.trim().to_ascii_lowercase();
        let searching = !q.is_empty();
        let mut hits = 0usize;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.strong("Guides");
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
                hdr.show(ui, |ui| ui.monospace(*body));
            }
            ui.add_space(10.0);
            ui.separator();
            ui.strong("API reference");
            ui.small("Everything here also pops up as you type in a script (Tab accepts, ↑↓ chooses).");
            ui.add_space(4.0);
            for cat in API_CATEGORIES {
                let entries: Vec<&ApiEntry> = LUA_API
                    .iter()
                    .filter(|e| api_category(e.label) == *cat)
                    .filter(|e| {
                        !searching
                            || e.label.to_ascii_lowercase().contains(&q)
                            || e.doc.to_ascii_lowercase().contains(&q)
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                hits += entries.len();
                let hdr = egui::CollapsingHeader::new(format!("{cat}  ({})", entries.len()))
                    .id_salt(("api_cat", cat));
                let hdr = if searching { hdr.open(Some(true)) } else { hdr.default_open(false) };
                hdr.show(ui, |ui| {
                    for e in entries {
                        ui.monospace(
                            egui::RichText::new(e.label)
                                .color(egui::Color32::from_rgb(78, 201, 176)),
                        );
                        ui.indent(("api_doc", e.label), |ui| ui.small(e.doc));
                        ui.add_space(2.0);
                    }
                });
            }
            if searching && hits == 0 {
                ui.add_space(8.0);
                ui.label(format!(
                    "No matches for \"{}\" — try a broader word (e.g. \"move\", \"light\", \"spin\").",
                    self.ide.docs_search.trim()
                ));
            }
            ui.add_space(10.0);
            egui::CollapsingHeader::new("⌨ Editor shortcuts").default_open(false).show(ui, |ui| {
                ui.monospace(IDE_SHORTCUTS);
            });
        });
    }

    /// The code editor for open file `i`: toolbar, find/replace, shortcuts, the
    /// highlighted text area, diagnostics, autocomplete and the references panel.
    fn file_editor_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let editor_id = egui::Id::new(("ide_editor", self.ide.open[i].path.clone()));
        // ---- toolbar: path, save, external editor, snippets + Ln/Col status ----
        ui.horizontal(|ui| {
            ui.small(self.ide.open[i].path.clone());
            let dirty = self.ide.open[i].dirty;
            if ui.add_enabled(dirty, egui::Button::new("Save")).on_hover_text("Ctrl+S").clicked()
                && self.ide.save_file(i)
            {
                self.cmd.refresh_assets = true;
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
            ui.menu_button("Insert snippet", |ui| {
                for (label, snippet) in LUA_SNIPPETS {
                    if ui.button(*label).clicked() {
                        self.ide.open[i].text.push_str(snippet);
                        self.ide.open[i].dirty = true;
                        ui.close();
                    }
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
                self.ide.save_file(k);
            }
            self.cmd.refresh_assets = true;
        }
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::S))
            && self.ide.save_file(i) {
                self.cmd.refresh_assets = true;
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
        let font = egui::FontId::monospace(13.0);
        let lfont = font.clone();
        let theme = CODE_THEMES[self.code_theme.min(CODE_THEMES.len() - 1)];
        let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            // No wrap (code editor) — logical lines == rows, so the gutter aligns.
            let mut job = if is_lua {
                lua_highlight(buf.as_str(), lfont.clone(), &theme)
            } else {
                plain_job(buf.as_str(), lfont.clone(), &theme)
            };
            job.wrap.max_width = f32::INFINITY;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        // While the completion popup is open (last frame), it owns Tab (accept),
        // the arrow keys (choose) and Esc (dismiss) — eat them *before* the
        // editor runs so they don't indent / move the caret. Enter is
        // deliberately NOT an accept key: it stays a plain newline, so typing
        // `else` + Enter never turns into an unwanted `elseif`.
        let ac_id = egui::Id::new(("ide_ac_open", editor_id));
        let ac_was_open = ui.ctx().data(|d| d.get_temp::<bool>(ac_id).unwrap_or(false));
        let (mut ac_accept, mut ac_nav, mut ac_dismiss) = (false, 0i32, false);
        if ac_was_open {
            ui.input_mut(|inp| {
                ac_accept = inp.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
                ac_nav = inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) as i32
                    - (inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) as i32);
                ac_dismiss = inp.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            });
        }

        self.editor_shortcuts(ui, i, editor_id, is_lua, ac_accept);

        let line_count = self.ide.open[i].text.matches('\n').count() + 1;
        let goto = self.ide.goto.take();
        let find_hl = (self.ide.find_open && !self.ide.find_query.is_empty())
            .then(|| (self.ide.find_query.clone(), self.ide.find_case, self.ide.find_idx));
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
            if let Some(api) = LUA_API.iter().find(|a| a.label == word) {
                output.response.response.clone().on_hover_ui_at_pointer(|ui| {
                    ui.set_max_width(360.0);
                    ui.monospace(egui::RichText::new(api.label).color(egui::Color32::from_rgb(78, 201, 176)));
                    ui.label(api.doc);
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
        tab_accept: bool,
    ) {
        if !ui.memory(|m| m.has_focus(editor_id)) {
            return;
        }
        let Some((sel_a, sel_b, caret)) = ide_selection(ui.ctx(), editor_id) else { return };
        let empty_sel = sel_a == sel_b;

        // Ctrl+C / Ctrl+X with no selection → whole current line. egui-winit turns
        // those chords into Copy/Cut EVENTS (they never arrive as key presses), so
        // intercept the events; with a selection they pass through to the editor.
        if empty_sel {
            let (mut do_copy, mut do_cut) = (false, false);
            ui.input_mut(|inp| {
                inp.events.retain(|e| match e {
                    egui::Event::Copy => {
                        do_copy = true;
                        false
                    }
                    egui::Event::Cut => {
                        do_cut = true;
                        false
                    }
                    _ => true,
                });
            });
            if do_copy {
                ui.ctx().copy_text(line_edit::line_with_newline(&self.ide.open[i].text, caret));
            }
            if do_cut {
                let clip = line_edit::line_with_newline(&self.ide.open[i].text, caret);
                ui.ctx().copy_text(clip);
                let new_caret = delete_lines(&mut self.ide.open[i].text, caret, caret);
                self.ide.open[i].dirty = true;
                set_ide_caret(ui.ctx(), editor_id, new_caret);
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
        // Tab / Shift+Tab → block indent/outdent. Plain Tab only when the selection
        // spans lines (a caret Tab should still insert an indent), and never when
        // the autocomplete popup already claimed it.
        let multi_line = !empty_sel && {
            let text = &self.ide.open[i].text;
            let (ba, bb) =
                (line_edit::byte_of_char(text, sel_a), line_edit::byte_of_char(text, sel_b));
            text[ba..bb].contains('\n')
        };
        if !tab_accept
            && multi_line
            && ui.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
        {
            let (a, b) = indent_lines(&mut self.ide.open[i].text, sel_a, sel_b, false);
            self.ide.open[i].dirty = true;
            set_ide_selection(ui.ctx(), editor_id, a, b);
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
        // Ctrl+B → go to the definition of the identifier under the caret.
        if ui.input_mut(|inp| inp.consume_key(egui::Modifiers::CTRL, egui::Key::B)) {
            let mut w = word_at(&self.ide.open[i].text, caret);
            if w.is_empty() && caret > 0 {
                w = word_at(&self.ide.open[i].text, caret - 1);
            }
            if !w.is_empty() {
                self.goto_definition(&w);
            }
        }
    }

    /// An autocomplete popup at the caret: the engine API (full labels, member
    /// access on any variable, `params.` keys, component-handle fields) plus
    /// identifiers from the current file, ranked by [`ac_matches`]. ↑↓ choose,
    /// Tab accepts (Enter stays a newline), Esc dismisses (until the token
    /// changes), a click
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
        // Pop only on a real prefix: ≥2 chars for a plain word, or any member access.
        if token.len() < 2 && !token.contains(['.', ':']) {
            return false;
        }
        if token != self.ide.ac_token {
            self.ide.ac_token = token.clone();
            self.ide.ac_sel = 0;
            self.ide.ac_dismissed = false;
        }
        if dismiss {
            self.ide.ac_dismissed = true;
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
        // Tab inserts the selected match; otherwise a click does.
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
                    ui.small(
                        egui::RichText::new("⇥ accept · ↑↓ choose · esc hide")
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

/// Insert-menu snippets for the in-engine IDE: (label, Lua to append).
const LUA_SNIPPETS: &[(&str, &str)] = &[
    (
        "update",
        "\nfunction update(node, dt)\n  \nend\n",
    ),
    (
        "start",
        "\nfunction start(node)\n  \nend\n",
    ),
    (
        "fixedUpdate",
        "\nfunction fixedUpdate(node, dt)\n  \nend\n",
    ),
    (
        "networked door (rpc + synced)",
        "\nreplicated = { open = false }\n\nonRpc = {}\nfunction onRpc.use(args, sender)\n  if net.isServer() then synced.open = not synced.open end\nend\n\nfunction update(node, dt)\n  local target = synced.open and 1.6 or 0.0\n  node.y = node.y + (target - node.y) * math.min(1, dt * 6)\nend\n",
    ),
    (
        "lag-compensated swing (net.rewind)",
        "\n-- client: fire the intent stamped with the tick you were SEEING\nfunction update(node, dt)\n  if net.isClient() and input.clicked(0) then\n    local yaw = input.aimYaw() or node.yaw\n    net.rpc(\"swing\", { dx = math.sin(yaw), dz = math.cos(yaw) }, { withInput = true })\n  end\nend\n\n-- server: judge it against the world as that player perceived it\nonRpc = {}\nfunction onRpc.swing(args, peer)\n  if not net.isServer() then return end\n  net.rewind(peer, function()\n    local hit = raycast(node.x, node.y, node.z, args.dx, 0, args.dz, 3.0)\n    if hit and hit.node then\n      local combat = hit.node:getscript(\"combat\")\n      if combat and combat.synced.parrying then\n        net.rpc(\"parried\", {}, { to = peer })\n      else\n        log(\"hit \" .. hit.node.name)\n      end\n    end\n  end)\nend\n",
    ),
    (
        "spin (yaw)",
        "\ndefaults = { speed = 45 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
    ),
    (
        "pulse (scale)",
        "\ndefaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }\nfunction update(node, dt)\n  node.scale = math.max(params.base * (1.0 + params.amplitude * math.sin(params.speed * time)), 0.01)\nend\n",
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
Ctrl+B          go to definition   right-click    definition / references
completion:     ↑↓ choose · Tab accept (Enter = newline) · Esc hide
Ctrl+W          close tab          Tab            accept completion";

// ---- in-engine IDE: Lua syntax highlighting + autocomplete -----------------

/// Lua reserved words (highlighted as keywords).
pub(crate) const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Identifiers highlighted as engine/builtin API (teal).
pub(crate) const LUA_API_WORDS: &[&str] = &[
    "node", "params", "time", "dt", "defaults", "log", "start", "update", "fixedUpdate", "input", "math",
    "string", "table", "ipairs", "pairs", "print", "tostring", "tonumber", "pcall", "select",
    "raycast", "find", "findAll", "findScript", "findScriptInScene", "findScripts", "assets", "gizmo",
    "net", "synced", "replicated", "onRpc",
];

/// The Docs page's API-reference groups, in display order.
const API_CATEGORIES: &[&str] = &[
    "script basics — lifecycle, params, log",
    "node — transform & body fields",
    "node — methods & handles",
    "scene lookups & raycast",
    "input — keyboard & mouse",
    "networking — net.*, synced",
    "components — getcomponent",
    "animation — node:animator",
    "assets",
    "debug gizmos",
    "lua stdlib",
];

/// Which Docs-page group an API entry belongs to (by its label shape).
fn api_category(label: &str) -> &'static str {
    if label == "node:getcomponent" {
        "components — getcomponent"
    } else if label.starts_with("node:") {
        "node — methods & handles"
    } else if label.starts_with("node.") {
        "node — transform & body fields"
    } else if label.starts_with("input") {
        "input — keyboard & mouse"
    } else if label.starts_with("net")
        || matches!(label, "synced" | "replicated" | "onRpc")
    {
        "networking — net.*, synced"
    } else if label.starts_with("gizmo") {
        "debug gizmos"
    } else if label.starts_with("assets") {
        "assets"
    } else if label.starts_with("anim") {
        "animation — node:animator"
    } else if label.starts_with("math") || label.starts_with("string") {
        "lua stdlib"
    } else if matches!(label, "find" | "findAll" | "findScript" | "findScriptInScene" | "findScripts" | "raycast") {
        "scene lookups & raycast"
    } else {
        "script basics — lifecycle, params, log"
    }
}

/// One completion / docs entry for the in-engine IDE.
struct ApiEntry {
    label: &'static str,
    insert: &'static str,
    doc: &'static str,
}

/// The engine scripting API, surfaced as autocomplete + hover docs (and the Docs
/// page's reference). Lua stdlib highlights are included so completion is useful.
const LUA_API: &[ApiEntry] = &[
    ApiEntry { label: "update", insert: "update", doc: "function update(node, dt) — runs every frame while playing." },
    ApiEntry { label: "fixedUpdate", insert: "fixedUpdate", doc: "function fixedUpdate(node, dt) — runs every GAMEPLAY TICK (60 Hz, constant dt). Movement/gameplay/physics writes belong here; cameras & cosmetics in update. Same cadence physics steps at — frame-rate independent." },
    ApiEntry { label: "start", insert: "start", doc: "function start(node) — runs once when play begins." },
    ApiEntry { label: "defaults", insert: "defaults", doc: "defaults = { name = value } — tunables shown in the Inspector." },
    ApiEntry { label: "input.aimYaw", insert: "input.aimYaw()", doc: "The ACTIVE camera's world yaw (radians), captured with the input snapshot — use it for camera-relative movement (in multiplayer it rides the input command, so server + prediction replay see exactly your view angle). nil without an active camera." },
    ApiEntry { label: "input.aimPitch", insert: "input.aimPitch()", doc: "The active camera's world pitch (radians), captured with the input snapshot." },
    ApiEntry { label: "net.host", insert: "net.host{}", doc: "net.host{ maxPlayers = 16, port = 7777 } — become the authoritative host. With a port, host a REAL session on UDP (QUIC) that other machines join; without one, the in-editor loopback harness." },
    ApiEntry { label: "net.join", insert: "net.join(\"local://\")", doc: "net.join(addr) — join a session: \"quic://host:port\" = a real server over the network, \"local://\" = the in-editor test harness. relay:// lobby codes arrive with floptle-relay." },
    ApiEntry { label: "net.leave", insert: "net.leave()", doc: "net.leave() — end the session." },
    ApiEntry { label: "net.role", insert: "net.role()", doc: "net.role() — \"offline\" | \"server\" | \"client\"." },
    ApiEntry { label: "net.isServer", insert: "net.isServer()", doc: "net.isServer() — true on the authoritative host." },
    ApiEntry { label: "net.isClient", insert: "net.isClient()", doc: "net.isClient() — true on a connected client." },
    ApiEntry { label: "net.peers", insert: "net.peers()", doc: "net.peers() — connected client peer ids (server)." },
    ApiEntry { label: "net.ping", insert: "net.ping()", doc: "net.ping(peer?) — round-trip time in ms." },
    ApiEntry { label: "net.rpc", insert: "net.rpc(\"name\", {})", doc: "net.rpc(name, args, {to=peer, withInput=true}) — remote call: server→clients or client→server. withInput stamps a client intent with the tick it was seeing (for net.rewind). Handle with function onRpc.name(args, sender). Args: scalars + tables (≤4 deep, ≤1KB)." },
    ApiEntry { label: "net.rewind", insert: "net.rewind(peer, function()\n  \nend)", doc: "SERVER ONLY, inside onRpc for an rpc sent {withInput=true}: run the closure against the world as that peer PERCEIVED it — raycasts and other scripts' synced vars read the rewound tick (clamped ~250 ms). A parry that was up on the attacker's screen counts." },
    ApiEntry { label: "net.on", insert: "net.on(\"playerJoined\", function(peer) end)", doc: "net.on(event, fn) — session events: playerJoined/playerLeft (peer id), connected, disconnected (reason)." },
    ApiEntry { label: "net.spawn", insert: "net.spawn(\"scenes/thing.ron\", { x = 0, y = 0, z = 0 })", doc: "SERVER ONLY: net.spawn(path, {x,y,z,owner}) — spawn a scene's first node as a replicated runtime object on every client (available next tick)." },
    ApiEntry { label: "net.despawn", insert: "net.despawn(node)", doc: "SERVER ONLY: net.despawn(node) — remove a replicated runtime object everywhere." },
    ApiEntry { label: "net.isMine", insert: "net.isMine(node)", doc: "net.isMine(node) — is this node under MY control on this machine? Offline/non-networked → true; server → true unless a remote peer owns it; client → only your own predicted node(s). Cameras/HUDs use it to pick the local player out of many avatars (pair with findScripts)." },
    ApiEntry { label: "replicated", insert: "replicated = {  }", doc: "replicated = { hp = 100 } — declare synced script vars (top level). Read/write them as synced.hp; the server's writes replicate to every client." },
    ApiEntry { label: "synced", insert: "synced", doc: "The synced-vars table (declared via replicated = {...}). Server writes replicate; client writes warn and get overwritten." },
    ApiEntry { label: "onRpc", insert: "onRpc = {}\nfunction onRpc.name(args, sender)\n  \nend", doc: "onRpc.<name>(args, sender) — handles net.rpc(\"name\", args). sender is the verified peer id (0 = server)." },
    ApiEntry { label: "params", insert: "params", doc: "This instance's tunables, a table seeded from `defaults` (params.speed, …)." },
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
    ApiEntry { label: "node.vx", insert: "node.vx", doc: "Rigidbody velocity X (m/s). Read + write to drive the body; the engine integrates it." },
    ApiEntry { label: "node.vy", insert: "node.vy", doc: "Rigidbody velocity Y (m/s). Keep this for gravity/jump while replacing the horizontal part." },
    ApiEntry { label: "node.vz", insert: "node.vz", doc: "Rigidbody velocity Z (m/s)." },
    ApiEntry { label: "node.grounded", insert: "node.grounded", doc: "True while the rigidbody rests on a surface (read-only). Gate jumps on it." },
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
    ApiEntry { label: "input.released", insert: "input.released(", doc: "input.released(\"space\") — true only on the frame the key goes up (an edge)." },
    ApiEntry { label: "input.axis", insert: "input.axis(", doc: "input.axis(\"a\", \"d\") — returns -1/0/1 from a negative/positive key pair (e.g. strafing)." },
    ApiEntry { label: "input.mouse", insert: "input.mouse(", doc: "local x, y = input.mouse() — cursor position in pixels." },
    ApiEntry { label: "input.mouse_delta", insert: "input.mouse_delta(", doc: "local dx, dy = input.mouse_delta() — mouse movement since last frame." },
    ApiEntry { label: "input.button", insert: "input.button(", doc: "input.button(0) — true while a mouse button is held (0 left, 1 right, 2 middle)." },
    ApiEntry { label: "input.clicked", insert: "input.clicked(", doc: "input.clicked(0) — true only on the frame a mouse button goes down." },
    ApiEntry { label: "input.scroll", insert: "input.scroll(", doc: "input.scroll() — mouse wheel delta this frame." },
    ApiEntry { label: "input.lockMouse", insert: "input.lockMouse(", doc: "input.lockMouse() — pin the cursor to the window center and hide it (FPS / free-look mouselook without holding a button). Read motion with input.mouse_delta(). Released on Stop." },
    ApiEntry { label: "input.unlockMouse", insert: "input.unlockMouse(", doc: "input.unlockMouse() — release the cursor back to the desktop and show it again." },
    ApiEntry { label: "input.setMouseLocked", insert: "input.setMouseLocked(", doc: "input.setMouseLocked(true/false) — lock or unlock the mouse from a boolean (e.g. a menu toggle)." },
    ApiEntry { label: "raycast", insert: "raycast(", doc: "raycast(ox,oy,oz, dx,dy,dz, max [, ignore]) — cast a ray against the terrain + mesh colliders AND every physics body (players, crates). Returns a hit {x,y,z, nx,ny,nz, distance, node} or nil — node is the hit body's node handle (nil for static geometry). Your own node's body is excluded; pass a node as `ignore` to skip its body too. Use for ground checks, line-of-sight, shooting." },
    ApiEntry { label: "gizmo", insert: "gizmo", doc: "Immediate-mode debug drawing (play mode): gizmo.line/ray/sphere/point show for ONE frame in the Scene view (never the Game view; the viewport gizmos toggle hides them). Call every frame you want a shape visible." },
    ApiEntry { label: "gizmo.line", insert: "gizmo.line(", doc: "gizmo.line(x1,y1,z1, x2,y2,z2 [, r,g,b]) — a world-space debug line for one frame. Color is 0–1 floats (default green)." },
    ApiEntry { label: "gizmo.ray", insert: "gizmo.ray(", doc: "gizmo.ray(ox,oy,oz, dx,dy,dz [, len [, r,g,b]]) — a debug ray: origin + direction. With `len` the direction is normalized and the ray is that long — mirrors raycast(...), perfect for visualizing ground checks / line-of-sight." },
    ApiEntry { label: "gizmo.sphere", insert: "gizmo.sphere(", doc: "gizmo.sphere(x,y,z [, radius [, r,g,b]]) — a wire debug sphere (three rings): trigger zones, blast radii, pickup ranges." },
    ApiEntry { label: "gizmo.point", insert: "gizmo.point(", doc: "gizmo.point(x,y,z [, size [, r,g,b]]) — a small 3-axis cross marking a spot: hit points, waypoints, spawn locations." },
    ApiEntry { label: "assets", insert: "assets", doc: "Reference files under Assets/ in code: assets.getFile(path), assets.getContents(dir)." },
    ApiEntry { label: "assets.getFile", insert: "assets.getFile(", doc: "assets.getFile(\"models/armor.glb\") — the asset's path (or nil), to hand to node.model / node.material. Path is relative to Assets/." },
    ApiEntry { label: "assets.getContents", insert: "assets.getContents(", doc: "assets.getContents(\"models\") — an array of every file under that folder (recursive). Build tables of assets with it." },
    ApiEntry { label: "find", insert: "find(", doc: "find(\"Player\") — the first node in the scene with that name (a node handle), or nil." },
    ApiEntry { label: "findAll", insert: "findAll(", doc: "findAll(\"Coin\") — an array of every node with that name." },
    ApiEntry { label: "findScript", insert: "findScript(", doc: "findScript(\"GameManager\") — a script handle for the first node anywhere running that script (the manager pattern), or nil. Call its methods / read its state." },
    ApiEntry { label: "findScriptInScene", insert: "findScriptInScene(", doc: "Alias of findScript(kind)." },
    ApiEntry { label: "findScripts", insert: "findScripts(", doc: "findScripts(kind) — EVERY node carrying that script, as script handles in scene order. Pair with net.isMine to pick the local player out of many avatars: for _, s in ipairs(findScripts(\"third_person\")) do if net.isMine(s.node) then ... end end" },
    ApiEntry { label: "node.name", insert: "node.name", doc: "The node's name (string)." },
    ApiEntry { label: "node.id", insert: "node.id", doc: "A stable numeric id for this node." },
    ApiEntry { label: "node.parent", insert: "node.parent", doc: "The parent node handle, or nil. A handle has the same fields (x/y/z, …) so you can read/write another node." },
    ApiEntry { label: "node:getparent", insert: "node:getparent()", doc: "The parent node handle, or nil (same as node.parent)." },
    ApiEntry { label: "node:children", insert: "node:children()", doc: "An array of this node's child handles." },
    ApiEntry { label: "node:getchild", insert: "node:getchild(", doc: "node:getchild(\"Gun\") — the first child with that name (a node handle), or nil." },
    ApiEntry { label: "node:find", insert: "node:find(", doc: "node:find(\"Muzzle\") — the first descendant (any depth) with that name, or nil." },
    ApiEntry { label: "node:getscript", insert: "node:getscript(", doc: "node:getscript(\"health\") — a script handle for that script on this node, or nil. Read/write its state, call its methods, reach .node / .params." },
    ApiEntry { label: "node:getcomponent", insert: "node:getcomponent(", doc: "node:getcomponent(\"RigidBody\" | \"PointLight\") — a component handle whose fields you can read AND assign at runtime (applies live during play), or nil if absent. RigidBody (every Inspector tunable): friction, restitution, gravity (true/false, reads 1/0), shape (0 sphere / 1 capsule / 2 box), radius, height, half_x/y/z (box half-extents), lock_x/y/z + lock_rot_x/y/z (axis freezes). PointLight: intensity, range, r, g, b. ParticleSystem: play_on_start (1/0). e.g. node:getcomponent(\"RigidBody\").friction = 0.02 for ice. (To play/stop effects at runtime use node:particles().)" },
    ApiEntry { label: "node:animator", insert: "node:animator()", doc: "node:animator() — the animation handle for this node's Animation Controller (or a rigged model's embedded clips). Setters: :play/:restart/:crossfade/:stop/:setSpeed/:setLayerWeight/:seek. Getters: :state/:time/:finished/:isPlaying/:clips/:layers." },
    ApiEntry { label: "anim:play", insert: ":play(", doc: "anim:play(\"Run\" [, fade [, layer]]) — transition to a state. The controller supplies the crossfade (default fade, per-arrow overrides, and a state's ⇥ fade-in override which beats everything — 0 = instant); pass `fade` to override the first two. Safe to call every frame — re-playing the current state is a no-op." },
    ApiEntry { label: "anim:restart", insert: ":restart(", doc: "anim:restart(\"Attack\" [, fade [, layer]]) — like play, but re-enters even if that state is already playing (re-trigger a one-shot)." },
    ApiEntry { label: "anim:crossfade", insert: ":crossfade(", doc: "anim:crossfade(\"Idle\", 0.3 [, layer]) — transition with an explicit fade time (seconds)." },
    ApiEntry { label: "anim:stop", insert: ":stop(", doc: "anim:stop([layer [, fade]]) — stop a layer (all layers if omitted). Higher layers release to the layers below; the base returns to its default state." },
    ApiEntry { label: "anim:setSpeed", insert: ":setSpeed(", doc: "anim:setSpeed(2) — global playback speed multiplier for this node's animator." },
    ApiEntry { label: "anim:setLayerWeight", insert: ":setLayerWeight(", doc: "anim:setLayerWeight(\"Attack\", 0.5) — blend a layer over the ones below (0 = off, 1 = full override)." },
    ApiEntry { label: "anim:seek", insert: ":seek(", doc: "anim:seek(t [, layer]) — jump the current state's playhead to t seconds." },
    ApiEntry { label: "anim:state", insert: ":state(", doc: "anim:state([layer]) — the state currently showing (topmost active layer), or that layer's state. Nil when idle." },
    ApiEntry { label: "anim:time", insert: ":time(", doc: "anim:time([layer]) — seconds into the current state." },
    ApiEntry { label: "anim:finished", insert: ":finished(", doc: "anim:finished([layer]) — true when a non-looped state reached its end this frame (or stays true while holding the last frame)." },
    ApiEntry { label: "anim:isPlaying", insert: ":isPlaying(", doc: "anim:isPlaying([state]) — is that state playing on any layer (or anything at all, with no argument)?" },
    ApiEntry { label: "anim:clips", insert: ":clips()", doc: "anim:clips() — every playable state name, as a list." },
    ApiEntry { label: "spawnEffect", insert: "spawnEffect(", doc: "spawnEffect(key, x, y, z) — fire a one-shot particle effect at a world point, no node needed. It plays once and despawns itself. e.g. local h = raycast(...); if h then spawnEffect(\"vfx/Impact\", h.x, h.y, h.z) end." },
    ApiEntry { label: "node:particles", insert: "node:particles()", doc: "node:particles() — the particle handle for this node's Particle System component. Setters: :play/:stop/:restart. Getters: :isPlaying/:alive/:asset. e.g. on a hit, node:particles():restart() to re-fire a burst." },
    ApiEntry { label: "particles:play", insert: ":play()", doc: "particles:play() — start emitting if the effect is idle (spawns a fresh instance). No-op if already playing." },
    ApiEntry { label: "particles:stop", insert: ":stop()", doc: "particles:stop() — stop + despawn the effect; its live particles vanish." },
    ApiEntry { label: "particles:restart", insert: ":restart()", doc: "particles:restart() — re-spawn from t=0 (re-fire a one-shot burst, e.g. a muzzle flash on each shot)." },
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
];

/// The built-in Scripting docs, shown on the IDE's Docs page as searchable
/// collapsible sections: (title, monospace body).
const DOC_SECTIONS: &[(&str, &str)] = &[
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
                                to crouch (the engine shrinks it, feet planted)",
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
  Getters: anim:state()  anim:time()  anim:finished()  anim:isPlaying([state])
           anim:clips()  anim:layers()

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
third_person_camera.lua (see §7), freelook.lua (fly camera), rotate.lua,
pulsate.lua, float.lua — open one for a working start.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

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
    fn auto_indent_follows_and_deepens() {
        // Plain line: indent carried over.
        let mut t = "  x = 1".to_string();
        let end = t.chars().count();
        let caret = auto_indent_newline(&mut t, end, end);
        assert_eq!(t, "  x = 1\n  ");
        assert_eq!(caret, t.chars().count());
        // Block opener: one level deeper (and `do` must be a WORD, not a suffix).
        let mut t = "if x then".to_string();
        let end = t.chars().count();
        auto_indent_newline(&mut t, end, end);
        assert_eq!(t, "if x then\n  ");
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
    fn doc_words_prefix_matches() {
        let w = doc_words("local velocity = 1\nvel = velocity + vel2", "vel", "vel");
        assert_eq!(w, vec!["vel2".to_string(), "velocity".to_string()]);
    }
}
