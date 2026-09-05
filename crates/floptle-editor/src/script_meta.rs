//! SCRIPT METADATA — what a `.lua` script tells the Inspector about its own
//! tunables, read straight from the source.
//!
//! A script's `defaults` table already declares its params; this module reads the
//! **comments around them** so the Inspector can draw a designed panel instead of
//! a stack of anonymous drag values:
//!
//! ```lua
//! defaults = {
//!   --@header Movement
//!   -- How fast you walk on flat ground.      <- a plain comment is the tooltip
//!   --@range 0 20 --@units m/s
//!   walk = 4.5,
//!
//!   --@desc Blend between the walk and run animations.
//!   --@slider 0 1
//!   blend = 0.35,
//!
//!   --@options Off|On|Auto                    <- a numeric param becomes a dropdown
//!   sas = 0,
//!
//!   --@options walk|run|sprint                <- a string param, values as written
//!   gait = "walk",
//!
//!   invert = false,                           <- a bool default: a checkbox, no annotation
//!
//!   --@color
//!   tint = "#ff8800",
//!
//!   --@hidden
//!   debugScale = 1.0,
//! }
//! ```
//!
//! The annotation vocabulary is the same `--@` convention `--@editorButton`
//! established, and every part of it is optional — an un-annotated script renders
//! exactly as it did before, in DECLARATION order rather than alphabetically.
//!
//! Parsing is source-level (never executing the file) and cached per
//! `(path, mtime)`, because the Inspector asks every frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How the Inspector should draw one `defaults` entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ParamMeta {
    pub(crate) name: String,
    /// Section header to draw ABOVE this row (`--@header`).
    pub(crate) header: Option<String>,
    /// Tooltip: `--@desc`, or the plain `--` comment lines directly above the key.
    pub(crate) desc: Option<String>,
    /// `--@range min max` — clamps the value and bounds the drag.
    pub(crate) range: Option<(f32, f32)>,
    /// `--@slider min max` — draw a slider (implies a range).
    pub(crate) slider: bool,
    /// `--@step n` — drag speed / slider granularity.
    pub(crate) step: Option<f32>,
    /// `--@options a|b|c` — a dropdown. On a STRING param the labels are the
    /// values; on a NUMBER param they're indices 0..n-1.
    pub(crate) options: Vec<String>,
    /// A checkbox: `--@bool`, or inferred from a `true` / `false` default.
    pub(crate) boolean: bool,
    /// `--@color` — a colour picker over a `#rrggbb` string param.
    pub(crate) color: bool,
    /// `--@multiline` — a multi-line text box for a string param.
    pub(crate) multiline: bool,
    /// `--@units m/s` — suffix shown after the number.
    pub(crate) units: Option<String>,
    /// `--@hidden` — a tunable the Inspector shouldn't show at all.
    pub(crate) hidden: bool,
    /// The value the SCRIPT declares, as it is written in the file.
    ///
    /// Kept so the Inspector can tell a row that is overriding it from a row
    /// that is merely showing it. A scene's stored param wins over the script's
    /// default, silently and forever — so a number you edit in the script does
    /// nothing, and there is nothing on screen to say why (`floptle/0068`).
    pub(crate) default: Option<String>,
}

impl ParamMeta {
    /// The drag/slider bounds, defaulting wide open.
    pub(crate) fn bounds(&self) -> (f32, f32) {
        self.range.unwrap_or((f32::MIN, f32::MAX))
    }
}

/// Everything the Inspector reads out of a script's source.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ScriptMeta {
    /// `defaults` entries in DECLARATION order — the order they render in.
    pub(crate) params: Vec<ParamMeta>,
    /// `--@editorButton <Label> [fn]` — edit-mode actions.
    pub(crate) buttons: Vec<(String, String)>,
    /// `--@about <text>` above the `defaults` table: what this script does, shown
    /// as the script component's own tooltip.
    pub(crate) about: Option<String>,
}

impl ScriptMeta {
    pub(crate) fn param(&self, name: &str) -> Option<&ParamMeta> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// mtime-keyed cache: the Inspector asks for this every frame, per selected node.
#[derive(Default)]
pub(crate) struct ScriptMetaCache {
    /// The stamp is `None` where the platform has none (a browser bundle) or
    /// the file is missing; either way it is a key, not a "re-parse every call".
    entries: HashMap<PathBuf, (Option<floptle_core::time::SystemTime>, ScriptMeta)>,
}

impl ScriptMetaCache {
    /// The metadata for `scripts/<kind>.lua`, re-parsed only when the file changes.
    /// A missing or unreadable script yields empty metadata (every caller then
    /// falls back to the plain, un-annotated rendering).
    pub(crate) fn get(&mut self, project_root: &Path, kind: &str) -> &ScriptMeta {
        let path = project_root.join("scripts").format_lua(kind);
        let mtime = floptle_vfs::modified(&path);
        let stale = self.entries.get(&path).is_none_or(|(seen, _)| *seen != mtime);
        if stale {
            let src = floptle_vfs::read_to_string(&path).unwrap_or_default();
            let meta = parse(&src);
            self.entries.insert(path.clone(), (mtime, meta));
        }
        &self.entries.get(&path).expect("just inserted").1
    }
}

/// `scripts/` + `<kind>.lua` — a named helper so the cache key and the parse path
/// can never disagree.
trait ScriptPath {
    fn format_lua(self, kind: &str) -> PathBuf;
}
impl ScriptPath for PathBuf {
    fn format_lua(self, kind: &str) -> PathBuf {
        self.join(format!("{kind}.lua"))
    }
}

/// Split a line into its code part and its `--` comment part, ignoring `--`
/// inside a string literal (`sep = "--"` is not a comment).
fn split_comment(line: &str) -> (&str, Option<&str>) {
    let b = line.as_bytes();
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
            }
            None => {
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
                    return (&line[..i], Some(line[i + 2..].trim()));
                }
            }
        }
        i += 1;
    }
    (line, None)
}

/// Parse a script source's metadata. Line-oriented and forgiving: an annotation
/// it doesn't recognise is ignored rather than fatal, so a typo can never stop a
/// script from being editable.
pub(crate) fn parse(src: &str) -> ScriptMeta {
    let mut meta = ScriptMeta::default();
    // Pending annotations/comments, consumed by the next `key =` line.
    let mut pending = ParamMeta::default();
    let mut comment_lines: Vec<String> = Vec::new();
    let mut about: Vec<String> = Vec::new();
    // Brace depth INSIDE the defaults table (0 = not in it yet / done).
    let mut depth = 0usize;
    let mut in_defaults = false;

    for raw in src.lines() {
        let (code, comment) = split_comment(raw);
        let code_t = code.trim();

        if let Some(c) = comment {
            if let Some(rest) = c.strip_prefix('@') {
                // Several annotations may share a line: `--@range 0 20 --@units m/s`
                for part in rest.split("--@") {
                    apply_annotation(part.trim(), &mut pending, &mut meta, &mut about);
                }
            } else if !c.is_empty() && !c.starts_with('-') {
                // A plain comment: a tooltip candidate for the next key (or, above
                // `defaults`, part of the script's own description).
                comment_lines.push(c.to_string());
            }
            if code_t.is_empty() {
                continue; // a comment-only line never resets the pending block
            }
        }

        // Entering / leaving the defaults table.
        if !in_defaults {
            if code_t.starts_with("defaults") && code_t.contains('=') && code_t.contains('{') {
                in_defaults = true;
                depth = 1;
                // Comments above `defaults` describe the SCRIPT, not a param.
                if meta.about.is_none() && !comment_lines.is_empty() {
                    about.append(&mut comment_lines);
                }
                comment_lines.clear();
                pending = ParamMeta::default();
            } else if !code_t.is_empty() {
                comment_lines.clear(); // ordinary code: those comments weren't ours
            }
            continue;
        }

        // Inside `defaults`: track nesting so a sub-table's keys aren't params.
        let opens = code_t.matches('{').count();
        let closes = code_t.matches('}').count();
        let at_top = depth == 1;
        if at_top && let Some(name) = key_of(code_t) {
            pending.name = name;
            pending.default = literal_of(code_t);
            // `= true` / `= false` is a checkbox without saying so.
            if is_bool_literal(code_t) {
                pending.boolean = true;
            }
            if pending.desc.is_none() && !comment_lines.is_empty() {
                pending.desc = Some(comment_lines.join(" "));
            }
            meta.params.push(std::mem::take(&mut pending));
            comment_lines.clear();
        } else if !code_t.is_empty() && !code_t.starts_with('}') {
            comment_lines.clear();
        }
        depth = depth + opens - closes.min(depth);
        if depth == 0 {
            in_defaults = false;
            comment_lines.clear();
            pending = ParamMeta::default();
        }
    }
    if !about.is_empty() {
        meta.about = Some(about.join(" "));
    }
    meta
}

/// The literal a `key = value,` line declares, as written — `"4.0"`, `"true"`,
/// `"\"walk\""`. Only for showing back to a person, so a value this cannot make
/// sense of (a table, a call, a `noderef()`) is simply absent rather than
/// guessed at.
fn literal_of(code: &str) -> Option<String> {
    let (_, rhs) = code.split_once('=')?;
    let v = rhs.trim().trim_end_matches(',').trim();
    if v.is_empty() || v.starts_with('{') || v.contains('(') {
        return None;
    }
    Some(v.to_string())
}

/// One `--@name args` annotation.
fn apply_annotation(
    text: &str,
    pending: &mut ParamMeta,
    meta: &mut ScriptMeta,
    about: &mut Vec<String>,
) {
    let mut it = text.splitn(2, char::is_whitespace);
    let Some(tag) = it.next() else { return };
    let arg = it.next().unwrap_or("").trim();
    let nums = |s: &str| -> Vec<f32> { s.split_whitespace().filter_map(|n| n.parse().ok()).collect() };
    match tag {
        "header" => pending.header = Some(arg.replace('_', " ")),
        "desc" | "description" | "tooltip" => {
            // Consecutive --@desc lines join into a paragraph.
            pending.desc = Some(match pending.desc.take() {
                Some(prev) => format!("{prev} {arg}"),
                None => arg.to_string(),
            });
        }
        "about" => about.push(arg.to_string()),
        "range" | "slider" => {
            let n = nums(arg);
            if n.len() >= 2 {
                pending.range = Some((n[0], n[1]));
            }
            pending.slider |= tag == "slider";
        }
        "step" => pending.step = nums(arg).first().copied(),
        "options" | "enum" => {
            pending.options = arg
                .split(['|', ','])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        "bool" | "toggle" => pending.boolean = true,
        "color" | "colour" => pending.color = true,
        "multiline" | "text" => pending.multiline = true,
        "units" | "unit" | "suffix" => pending.units = Some(arg.to_string()),
        "hidden" | "hide" => pending.hidden = true,
        "editorButton" => {
            let mut parts = arg.split_whitespace();
            if let Some(label) = parts.next() {
                let func = parts.next().unwrap_or(label).to_string();
                meta.buttons.push((label.replace('_', " "), func));
            }
        }
        _ => {}
    }
}

/// The param name a `defaults` line declares (`walk = 4.5,` → `walk`), or None if
/// the line isn't a plain `name =` assignment (`["a"] = 1`, a bare array item…).
fn key_of(code: &str) -> Option<String> {
    let (name, _) = code.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.ends_with(['<', '>', '!', '~', '=']) {
        return None; // a comparison, not an assignment
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    (name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| name.to_string())
}

/// Does this `key = value` line assign a boolean literal?
fn is_bool_literal(code: &str) -> bool {
    let Some((_, v)) = code.split_once('=') else { return false };
    let v = v.trim().trim_end_matches(',').trim();
    v == "true" || v == "false"
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r##"
-- Planet-surface character controller.
-- Second line of the blurb.
defaults = {
  --@header Movement
  -- How fast you walk on flat ground.
  --@range 0 20 --@units m/s
  walk = 4.5,
  run = 8.0,

  --@header Animation
  --@desc Blend between walk and run.
  --@slider 0 1
  --@step 0.05
  blend = 0.35,
  --@options Off|On|Auto
  sas = 0,
  --@options walk|run|sprint
  gait = "walk",
  invert = false,
  --@color
  tint = "#ff8800",
  --@hidden
  debugScale = 1.0,
  -- a nested table's keys are not params
  curve = { a = 1, b = 2 },
  after = 3.0,
}

--@editorButton Generate_roll roll
function roll(node) end
"##;

    #[test]
    fn declaration_order_and_annotations_come_through() {
        let m = parse(SRC);
        let names: Vec<&str> = m.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["walk", "run", "blend", "sas", "gait", "invert", "tint", "debugScale", "curve", "after"],
            "params render in declaration order, and a nested table's keys are not params"
        );

        let walk = m.param("walk").unwrap();
        assert_eq!(walk.header.as_deref(), Some("Movement"));
        assert_eq!(walk.desc.as_deref(), Some("How fast you walk on flat ground."));
        assert_eq!(walk.range, Some((0.0, 20.0)));
        assert_eq!(walk.units.as_deref(), Some("m/s"), "two annotations can share a line");

        // Annotations never leak onto the NEXT param.
        let run = m.param("run").unwrap();
        assert_eq!(run.header, None);
        assert_eq!(run.desc, None);
        assert_eq!(run.range, None);

        let blend = m.param("blend").unwrap();
        assert_eq!(blend.header.as_deref(), Some("Animation"));
        assert!(blend.slider);
        assert_eq!(blend.range, Some((0.0, 1.0)));
        assert_eq!(blend.step, Some(0.05));

        assert_eq!(m.param("sas").unwrap().options, ["Off", "On", "Auto"]);
        assert_eq!(m.param("gait").unwrap().options, ["walk", "run", "sprint"]);
        assert!(m.param("invert").unwrap().boolean, "a `= false` default is a checkbox");
        assert!(m.param("tint").unwrap().color);
        assert!(m.param("debugScale").unwrap().hidden);
        // A comment describing a nested table doesn't become the NEXT param's tooltip.
        assert_eq!(m.param("after").unwrap().desc, None);

        assert_eq!(m.buttons, [("Generate roll".to_string(), "roll".to_string())]);
        assert_eq!(
            m.about.as_deref(),
            Some("Planet-surface character controller. Second line of the blurb.")
        );
    }

    /// A script with no annotations at all must parse to plain params in order —
    /// this is every existing script in both projects.
    #[test]
    fn an_unannotated_script_still_yields_ordered_params() {
        let m = parse("defaults = {\n  walk = 4.5,\n  run = 8.0,\n}\n");
        let names: Vec<&str> = m.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["walk", "run"]);
        assert_eq!(
            m.params[0],
            ParamMeta { name: "walk".into(), default: Some("4.5".into()), ..Default::default() }
        );
    }

    /// The declared literal, kept so the Inspector can say "this scene is
    /// overriding the script" (`floptle/0068`). Values it cannot make sense of
    /// are absent rather than guessed at — a wrong answer here would put a
    /// "you are overriding this" badge on a row that is not.
    #[test]
    fn the_declared_default_is_captured_as_written() {
        let m = parse(
            "defaults = {\n  walk = 4.5,\n  invert = false,\n  clip = \"footstep\",\n\
             \x20 target = noderef(),\n  curve = { 1, 2 },\n}\n",
        );
        assert_eq!(m.param("walk").unwrap().default.as_deref(), Some("4.5"));
        assert_eq!(m.param("invert").unwrap().default.as_deref(), Some("false"));
        assert_eq!(m.param("clip").unwrap().default.as_deref(), Some("\"footstep\""));
        assert_eq!(m.param("target").unwrap().default, None, "a call is not a literal");
        assert_eq!(m.param("curve").unwrap().default, None, "nor is a table");
    }

    /// A `--` inside a string is not a comment, and a script with no `defaults`
    /// table at all is not a parse error.
    #[test]
    fn strings_and_missing_defaults_are_handled() {
        let m = parse("defaults = {\n  sep = \"--@header Nope\",\n}\n");
        assert_eq!(m.params.len(), 1);
        assert_eq!(m.params[0].name, "sep");
        assert_eq!(m.params[0].header, None, "an annotation inside a string is text, not markup");

        let none = parse("function update(node, dt) end\n");
        assert!(none.params.is_empty() && none.buttons.is_empty());
    }

    /// `--@editorButton` keeps working exactly as it did (label, optional fn,
    /// underscores as spaces) — this replaced its old standalone parser.
    #[test]
    fn editor_buttons_match_the_old_parser() {
        let m = parse("--@editorButton Generate\nfunction Generate(n) end\n");
        assert_eq!(m.buttons, [("Generate".to_string(), "Generate".to_string())]);
        let m = parse("--@editorButton Regrow_flora regrow\n");
        assert_eq!(m.buttons, [("Regrow flora".to_string(), "regrow".to_string())]);
    }
}
