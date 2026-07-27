//! Scan the project's Lua for the actions it actually references.
//!
//! The Input settings list every action a script names — deduped, with the
//! first `file:line` that mentions it — so the binding list is driven by the
//! code rather than by whatever someone remembered to type into a settings
//! screen. An action used but unbound is the single most useful thing this can
//! surface: it's a control that silently does nothing.
//!
//! Deliberately a plain text scan, not a Lua parse: it must work on a script
//! that doesn't currently compile (which is exactly when you're editing it).
//! The cost of that choice is that a name built at runtime
//! (`input.action("Attack" .. n)`) can't be seen; those are rare, and a
//! hand-added action covers them.

use std::collections::BTreeMap;
use std::path::Path;

/// What kind of map entry a call site implies.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum UsageKind {
    Action,
    Axis1,
    Axis2,
    Motion,
    /// A legacy raw-key/mouse poll. Not a map entry — surfaced so the migration
    /// from raw polling to actions has a visible worklist.
    RawKey,
}

impl UsageKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            UsageKind::Action => "action",
            UsageKind::Axis1 => "axis1",
            UsageKind::Axis2 => "axis2",
            UsageKind::Motion => "motion",
            UsageKind::RawKey => "raw key",
        }
    }
}

/// One deduped reference, with where it was first seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) name: String,
    pub(crate) kind: UsageKind,
    /// Project-relative path.
    pub(crate) file: String,
    pub(crate) line: u32,
    /// How many call sites mention it (all files).
    pub(crate) count: u32,
}

/// The functions whose first string argument names a map entry.
const CALLS: &[(&str, UsageKind)] = &[
    ("action", UsageKind::Action),
    ("justPressed", UsageKind::Action),
    ("justReleased", UsageKind::Action),
    ("heldSecs", UsageKind::Action),
    ("buffered", UsageKind::Action),
    ("consume", UsageKind::Action),
    ("axis1", UsageKind::Axis1),
    ("axis2", UsageKind::Axis2),
    ("motion", UsageKind::Motion),
];

/// Legacy raw-device polls, matched as `input.<fn>(`.
const RAW_CALLS: &[&str] = &["key", "pressed", "released", "button", "clicked", "axis"];

/// Results of a scan, plus the stamp they were taken at.
#[derive(Default)]
pub(crate) struct InputScan {
    pub(crate) usages: Vec<Usage>,
    /// (file count, newest mtime) when last scanned — a cheap change detector.
    stamp: Option<(usize, std::time::SystemTime)>,
    /// Throttle: seconds of editor time at the last stamp check.
    last_check: f32,
}

impl InputScan {
    /// Named map entries only (raw-key call sites filtered out).
    pub(crate) fn entries(&self) -> impl Iterator<Item = &Usage> {
        self.usages.iter().filter(|u| u.kind != UsageKind::RawKey)
    }

    pub(crate) fn raw_key_uses(&self) -> impl Iterator<Item = &Usage> {
        self.usages.iter().filter(|u| u.kind == UsageKind::RawKey)
    }

    /// Rescan unconditionally.
    pub(crate) fn rescan(&mut self, scripts_dir: &Path) {
        self.usages = scan_dir(scripts_dir);
        self.stamp = dir_stamp(scripts_dir);
    }

    /// Rescan only if the scripts changed, and at most once a second — the
    /// window can be open for a long time while someone edits in another app.
    pub(crate) fn poll(&mut self, scripts_dir: &Path, now: f32) {
        if now - self.last_check < 1.0 && self.stamp.is_some() {
            return;
        }
        self.last_check = now;
        let stamp = dir_stamp(scripts_dir);
        if stamp != self.stamp || self.stamp.is_none() {
            self.usages = scan_dir(scripts_dir);
            self.stamp = stamp;
        }
    }
}

/// (file count, newest mtime) across the script tree.
fn dir_stamp(dir: &Path) -> Option<(usize, std::time::SystemTime)> {
    let mut count = 0;
    let mut newest = std::time::SystemTime::UNIX_EPOCH;
    for path in lua_files(dir) {
        count += 1;
        if let Ok(m) = std::fs::metadata(&path).and_then(|m| m.modified()) {
            newest = newest.max(m);
        }
    }
    (count > 0).then_some((count, newest))
}

/// Every `.lua` under `dir`, recursively (scripts may live in sub-folders).
fn lua_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "lua") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn scan_dir(dir: &Path) -> Vec<Usage> {
    // Keyed by (kind, name) so an action and a motion of the same name stay
    // distinct; BTreeMap keeps the list stable between scans.
    let mut found: BTreeMap<(UsageKind, String), Usage> = BTreeMap::new();
    for path in lua_files(dir) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        for (line_no, line) in text.lines().enumerate() {
            let code = strip_comment(line);
            for (name, kind) in scan_line(code) {
                let e = found.entry((kind, name.clone())).or_insert_with(|| Usage {
                    name,
                    kind,
                    file: rel.clone(),
                    line: line_no as u32 + 1,
                    count: 0,
                });
                e.count += 1;
            }
        }
    }
    found.into_values().collect()
}

/// Drop a trailing `--` comment, respecting quotes so a `--` inside a string
/// literal doesn't truncate the line.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut quote, mut i) = (None::<u8>, 0usize);
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                } else if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
                    return &line[..i];
                }
            }
        }
        i += 1;
    }
    line
}

/// Every `(name, kind)` a single line of code references.
fn scan_line(line: &str) -> Vec<(String, UsageKind)> {
    let mut out = Vec::new();
    for (call, kind) in CALLS {
        collect_calls(line, call, *kind, &mut out);
    }
    // Raw polls are matched only when qualified by `input.`, because `key` and
    // `pressed` are ordinary words a script might use for its own functions.
    for call in RAW_CALLS {
        let qualified = format!("input.{call}");
        collect_calls(line, &qualified, UsageKind::RawKey, &mut out);
    }
    out
}

/// Find `call ( "literal"` occurrences, at a word boundary.
fn collect_calls(line: &str, call: &str, kind: UsageKind, out: &mut Vec<(String, UsageKind)>) {
    let mut from = 0;
    while let Some(rel) = line[from..].find(call) {
        let at = from + rel;
        from = at + call.len();
        // Word boundary before: `justPressed` must not match inside
        // `myJustPressed`, and the bare `action` must not match `reaction`.
        // A `.` before is fine — that IS the method-call form.
        if at > 0 {
            let prev = line.as_bytes()[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let rest = line[from..].trim_start();
        let Some(rest) = rest.strip_prefix('(') else { continue };
        let rest = rest.trim_start();
        let Some(q) = rest.as_bytes().first().copied() else { continue };
        if q != b'"' && q != b'\'' {
            continue; // a variable, not a literal — nothing to harvest
        }
        let body = &rest[1..];
        let Some(end) = body.find(q as char) else { continue };
        let name = &body[..end];
        if !name.is_empty() {
            out.push((name.to_string(), kind));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str, kind: UsageKind) -> Vec<String> {
        let mut v: Vec<String> = src
            .lines()
            .flat_map(|l| scan_line(strip_comment(l)))
            .filter(|(_, k)| *k == kind)
            .map(|(n, _)| n)
            .collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn finds_every_action_call_form() {
        let src = r#"
            if input.action("Jump") then end
            if input.justPressed("Punch") then end
            if input.justReleased("Block") then end
            local t = input.heldSecs("Charge")
            if input.buffered("Kick", 4) then end
            input.consume("Kick")
        "#;
        assert_eq!(
            names(src, UsageKind::Action),
            vec!["Block", "Charge", "Jump", "Kick", "Punch"]
        );
    }

    #[test]
    fn finds_calls_through_a_player_handle() {
        // `input.player(2).justPressed("Punch")` — the receiver is irrelevant,
        // which is exactly why the scan matches on the method name.
        let src = r#"local p2 = input.player(2)
            if p2.justPressed("Punch") then end
            if input.player(1).action("Jump") then end"#;
        assert_eq!(names(src, UsageKind::Action), vec!["Jump", "Punch"]);
    }

    #[test]
    fn separates_axes_and_motions_from_actions() {
        let src = r#"
            local x, y = input.axis2("Move")
            local z = input.axis1("Zoom")
            if input.motion("qcf") then end
        "#;
        assert_eq!(names(src, UsageKind::Axis2), vec!["Move"]);
        assert_eq!(names(src, UsageKind::Axis1), vec!["Zoom"]);
        assert_eq!(names(src, UsageKind::Motion), vec!["qcf"]);
        assert!(names(src, UsageKind::Action).is_empty());
    }

    #[test]
    fn word_boundaries_stop_false_matches() {
        // `reaction`/`myAction` are not `action`; a user function named
        // `consumeItem` is not `consume`.
        let src = r#"
            local reaction = compute("Nope")
            obj.myaction("Nope2")
            inventory.consumeItem("Potion")
        "#;
        assert!(names(src, UsageKind::Action).is_empty(), "{:?}", names(src, UsageKind::Action));
    }

    #[test]
    fn comments_are_ignored() {
        let src = r#"
            -- input.action("Ghost") is only a note
            if input.action("Real") then end   -- input.action("AlsoGhost")
        "#;
        assert_eq!(names(src, UsageKind::Action), vec!["Real"]);
    }

    #[test]
    fn a_double_dash_inside_a_string_does_not_truncate_the_line() {
        let src = r#"log("a--b") if input.action("Real") then end"#;
        assert_eq!(names(src, UsageKind::Action), vec!["Real"]);
    }

    #[test]
    fn single_quotes_work_too() {
        let src = "if input.action('Jump') then end";
        assert_eq!(names(src, UsageKind::Action), vec!["Jump"]);
    }

    #[test]
    fn whitespace_between_the_call_and_its_argument_is_tolerated() {
        let src = "if input.action ( \"Jump\" ) then end";
        assert_eq!(names(src, UsageKind::Action), vec!["Jump"]);
    }

    #[test]
    fn a_computed_name_is_skipped_rather_than_guessed() {
        // Better to miss it than to invent an action called `attackName`.
        let src = r#"if input.action(attackName) then end"#;
        assert!(names(src, UsageKind::Action).is_empty());
    }

    #[test]
    fn raw_polls_are_flagged_only_when_qualified() {
        let src = r#"
            if input.key("w") then end
            if input.pressed("space") then end
            if input.clicked(0) then end
            local mine = { key = "not a poll" }
            if mydevice.key("w") then end
        "#;
        // `input.clicked(0)` has no string literal, so only the two named ones
        // land — and `mydevice.key` is somebody else's function.
        assert_eq!(names(src, UsageKind::RawKey), vec!["space", "w"]);
    }

    #[test]
    fn multiple_calls_on_one_line_are_all_found() {
        let src = r#"if input.action("A") and input.action("B") then end"#;
        assert_eq!(names(src, UsageKind::Action), vec!["A", "B"]);
    }

    #[test]
    fn scanning_a_tree_dedupes_and_counts() {
        let dir = std::env::temp_dir().join("floptle_input_scan_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(
            dir.join("a.lua"),
            "if input.action(\"Jump\") then end\nif input.action(\"Jump\") then end\n",
        )
        .unwrap();
        std::fs::write(dir.join("nested/b.lua"), "\n\nif input.action(\"Jump\") then end\n")
            .unwrap();

        let found = scan_dir(&dir);
        assert_eq!(found.len(), 1, "one deduped entry, not three: {found:?}");
        assert_eq!(found[0].name, "Jump");
        assert_eq!(found[0].count, 3, "every call site counted");
        assert_eq!(found[0].file, "a.lua", "reported at its FIRST occurrence");
        assert_eq!(found[0].line, 1);
    }

    /// Every input name the SHIPPED default scripts reference must exist in the
    /// starter map.
    ///
    /// This is the conversion's guard rail. A fresh project seeds both, and its
    /// camera has `freelook` attached — so a name drifting out of sync here is
    /// a brand-new project whose camera silently cannot move, which is about
    /// the worst first impression the engine could make.
    #[test]
    fn the_default_scripts_only_use_actions_the_starter_map_defines() {
        use floptle_input::InputMap;

        let dir = std::env::temp_dir().join("floptle_default_script_actions");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in crate::lua_support::DEFAULT_SCRIPTS {
            std::fs::write(dir.join(name), body).unwrap();
        }

        let starter = InputMap::starter();
        // `fighter.lua` is the one deliberate exception: it documents that you
        // add Punch/Kick/Block yourself, and the settings flag them for you.
        let fighter_only = ["Punch", "Kick", "Block"];

        let mut missing = Vec::new();
        for u in scan_dir(&dir) {
            if fighter_only.contains(&u.name.as_str()) {
                continue;
            }
            let defined = match u.kind {
                UsageKind::Action => starter.action_index(&u.name).is_some(),
                UsageKind::Axis1 => starter.axis1_index(&u.name).is_some(),
                UsageKind::Axis2 => starter.axis2_index(&u.name).is_some(),
                UsageKind::Motion => starter.motion(&u.name).is_some(),
                UsageKind::RawKey => continue,
            };
            if !defined {
                missing.push(format!("{} ({}) at {}:{}", u.name, u.kind.label(), u.file, u.line));
            }
        }
        assert!(missing.is_empty(), "not in the starter map:\n  {}", missing.join("\n  "));
    }

    /// The converted defaults must not poll raw devices any more: raw polls
    /// can't be rebound, don't work on a pad, and read neutral on a Predicted
    /// node in multiplayer.
    #[test]
    fn the_default_scripts_no_longer_poll_raw_keys() {
        let dir = std::env::temp_dir().join("floptle_default_script_rawkeys");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in crate::lua_support::DEFAULT_SCRIPTS {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let raw: Vec<String> = scan_dir(&dir)
            .into_iter()
            .filter(|u| u.kind == UsageKind::RawKey)
            .map(|u| format!("{} at {}:{}", u.name, u.file, u.line))
            .collect();
        assert!(raw.is_empty(), "still polling raw devices:\n  {}", raw.join("\n  "));
    }

    /// Seeding must be idempotent and must never overwrite a customised map.
    ///
    /// `seed_input_map` runs on every project open, on `--new` and on
    /// `--migrate`, so a version of it that replaced rather than topped up
    /// would quietly destroy a developer's bindings on a routine upgrade.
    #[test]
    fn seeding_the_input_map_tops_up_without_clobbering() {
        use floptle_input::{Binding, InputMap, Key, Source};

        let dir = std::env::temp_dir().join("floptle_seed_input_map");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A project with one custom action and a deliberately rebound Jump.
        let mut mine = InputMap::default();
        mine.actions.push(floptle_input::Action {
            name: "Grapple".into(),
            bindings: vec![Binding::new(Source::Key(Key::KeyQ))],
        });
        mine.actions.push(floptle_input::Action {
            name: "Jump".into(),
            bindings: vec![Binding::new(Source::Key(Key::KeyZ))],
        });
        floptle_input::save_map(&mine, &dir).unwrap();

        let ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        ed.seed_input_map();
        let after = floptle_input::load_map(&dir).unwrap().unwrap();

        assert!(after.action_index("Grapple").is_some(), "a custom action survives");
        let jump = &after.actions[after.action_index("Jump").unwrap()];
        assert!(
            jump.bindings.iter().any(|b| b.source == Source::Key(Key::KeyZ)),
            "the developer's own Jump binding survives"
        );
        assert!(after.axis2_index("Move").is_some(), "and the starter entries arrive");

        // Running it again changes nothing at all.
        let bytes = std::fs::read(dir.join(floptle_input::MAP_FILE)).unwrap();
        ed.seed_input_map();
        assert_eq!(
            std::fs::read(dir.join(floptle_input::MAP_FILE)).unwrap(),
            bytes,
            "seeding is idempotent — a routine upgrade must not churn the file"
        );
    }

    /// A map that won't parse must be left strictly alone. Rewriting it would
    /// turn a fixable typo into lost work.
    #[test]
    fn seeding_refuses_to_touch_an_unparseable_map() {
        let dir = std::env::temp_dir().join("floptle_seed_input_map_broken");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let broken = "InputMap( actions: [ this isn't RON";
        std::fs::write(dir.join(floptle_input::MAP_FILE), broken).unwrap();

        let ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        ed.seed_input_map();
        assert_eq!(
            std::fs::read_to_string(dir.join(floptle_input::MAP_FILE)).unwrap(),
            broken,
            "the developer's file is theirs to fix"
        );
    }

    #[test]
    fn an_empty_or_missing_scripts_dir_is_fine() {
        let dir = std::env::temp_dir().join("floptle_input_scan_missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(scan_dir(&dir).is_empty());
        assert_eq!(dir_stamp(&dir), None);
    }
}
