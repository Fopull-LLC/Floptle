//! `floptle cloud collections`: a game's Cloud collections, kept in the project.
//!
//! `cloud_collections.ron` beside `project.ron` lists what the game stores on
//! fopull.com:
//!
//! ```ron
//! [
//!     (name: "replays", kind: blobs, access: public, cap_kb: 512),
//!     (name: "time", kind: rank, keep: min),
//!     (name: "installs", kind: counter, mode: unique),
//! ]
//! ```
//!
//! The verb compares that with what the server has and prints the difference;
//! `--apply` makes the server match. Reading uses the project's game key, so a
//! check needs nobody signed in. Applying is the owner's, as the developer
//! signed in to the Hub.
//!
//! It is its own file rather than a block in `project.ron` because an older
//! Hub rewrites `project.ron` through its own idea of the fields and would drop
//! a block it does not know. Nothing rewrites this file.
//!
//! A collection only on the server is reported and left alone. Deleting one
//! deletes everything in it, and that stays a click on the game's page.

use std::path::Path;

use serde::Deserialize;

/// The file, beside `project.ron`.
pub(crate) const FILE: &str = "cloud_collections.ron";

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Docs,
    Blobs,
    Rank,
    Counter,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Access {
    Private,
    Public,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Authority {
    Player,
    Server,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Keep {
    Max,
    Min,
    Latest,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Total,
    Unique,
}

/// One line of the file, as written.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    kind: Kind,
    #[serde(default)]
    access: Option<Access>,
    #[serde(default)]
    authority: Option<Authority>,
    #[serde(default)]
    cap_kb: Option<u64>,
    #[serde(default)]
    keep: Option<Keep>,
    #[serde(default)]
    mode: Option<Mode>,
}

fn word<T: std::fmt::Debug>(v: T) -> String {
    format!("{v:?}").to_ascii_lowercase()
}

/// One collection as the server should have it: every field the file decides,
/// as the wire spells it. `cap_kb` is absent when the file leaves it to the
/// server.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Want {
    pub(crate) name: String,
    fields: Vec<(&'static str, serde_json::Value)>,
}

/// Read and check the file. Every mistake is reported, not just the first.
pub(crate) fn parse(text: &str) -> Result<Vec<Want>, Vec<String>> {
    let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
    let entries: Vec<Entry> = opts.from_str(text).map_err(|e| vec![format!("{FILE}: {e}")])?;
    let mut errors = Vec::new();
    let mut out: Vec<Want> = Vec::new();
    for e in entries {
        let n = &e.name;
        let valid_name = !n.is_empty()
            && n.len() <= 32
            && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !valid_name {
            errors.push(format!("'{n}' is not a collection name: 1 to 32 of a-z, 0-9, - and _"));
        }
        if out.iter().any(|w| &w.name == n) {
            errors.push(format!("{n} is listed twice"));
        }
        let shared = matches!(e.kind, Kind::Rank | Kind::Counter);
        if shared && e.access == Some(Access::Private) {
            errors.push(format!("{n}: a {} collection is always public", word(e.kind)));
        }
        if e.keep.is_some() && e.kind != Kind::Rank {
            errors.push(format!("{n}: keep is for rank collections only"));
        }
        if e.mode.is_some() && e.kind != Kind::Counter {
            errors.push(format!("{n}: mode is for counter collections only"));
        }
        if e.kind == Kind::Rank && e.keep.is_none() {
            // The server's old silent default was max, which keeps a racer's
            // slowest time. Saying it is the whole point of declaring.
            errors.push(format!("{n}: a rank collection says what it keeps: keep: max (points), min (times) or latest"));
        }
        if e.cap_kb == Some(0) {
            errors.push(format!("{n}: cap_kb is at least 1"));
        }
        let access = e.access.unwrap_or(if shared { Access::Public } else { Access::Private });
        let mut fields = vec![
            ("kind", word(e.kind).into()),
            ("access", word(access).into()),
            ("authority", word(e.authority.unwrap_or(Authority::Player)).into()),
        ];
        if let Some(c) = e.cap_kb {
            fields.push(("cap_kb", c.into()));
        }
        if let Some(k) = e.keep {
            fields.push(("keep", word(k).into()));
        }
        if e.kind == Kind::Counter {
            fields.push(("mode", word(e.mode.unwrap_or(Mode::Total)).into()));
        }
        out.push(Want { name: e.name, fields });
    }
    if errors.is_empty() { Ok(out) } else { Err(errors) }
}

/// What one collection needs.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    /// The server has it exactly.
    Same,
    /// The server does not have it.
    Create,
    /// The server has it differently: (field, server's, file's).
    Change(Vec<(&'static str, serde_json::Value, serde_json::Value)>),
}

/// Each wanted collection's step, with the body `--apply` would send, and the
/// names only the server has.
pub(crate) struct Plan {
    pub(crate) steps: Vec<(String, Step, serde_json::Value)>,
    pub(crate) server_only: Vec<String>,
}

impl Plan {
    fn pending(&self) -> usize {
        self.steps.iter().filter(|(_, s, _)| *s != Step::Same).count()
    }
}

pub(crate) fn plan(wants: &[Want], server: &[serde_json::Value]) -> Plan {
    let named = |n: &str| server.iter().find(|s| s.get("name").and_then(|v| v.as_str()) == Some(n));
    let mut steps = Vec::new();
    for w in wants {
        let mut body = serde_json::Map::new();
        for (k, v) in &w.fields {
            body.insert((*k).to_string(), v.clone());
        }
        let step = match named(&w.name) {
            None => Step::Create,
            Some(s) => {
                // A cap the file leaves to the server stays what it is: a PUT
                // without one could put it back to the default.
                if let Some(c) = s.get("cap_kb").filter(|c| !c.is_null() && !body.contains_key("cap_kb")) {
                    body.insert("cap_kb".into(), c.clone());
                }
                let mut diffs: Vec<_> = w
                    .fields
                    .iter()
                    .filter(|(k, v)| s.get(*k) != Some(v))
                    .map(|(k, v)| (*k, s.get(*k).cloned().unwrap_or(serde_json::Value::Null), v.clone()))
                    .collect();
                // Made by a write before declaring was required: the fields may
                // all match, and it still is not declared until someone says so.
                if s.get("declared") == Some(&serde_json::Value::Bool(false)) {
                    diffs.push(("declared", false.into(), true.into()));
                }
                if diffs.is_empty() { Step::Same } else { Step::Change(diffs) }
            }
        };
        steps.push((w.name.clone(), step, serde_json::Value::Object(body)));
    }
    let server_only = server
        .iter()
        .filter_map(|s| s.get("name").and_then(|v| v.as_str()))
        .filter(|n| !wants.iter().any(|w| w.name == *n))
        .map(str::to_string)
        .collect();
    Plan { steps, server_only }
}

fn show(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "unset".into(),
        other => other.to_string(),
    }
}

fn summary(body: &serde_json::Value) -> String {
    ["kind", "access", "authority", "keep", "mode"]
        .iter()
        .filter_map(|k| body.get(*k).map(show))
        .chain(body.get("cap_kb").map(|c| format!("{} KB", show(c))))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The whole verb: 0 in step (or applied), 1 out of step (or a declaration
/// refused), 4 no usable file, 3 no game or nobody signed in.
pub(crate) fn run(project: &Path, apply: bool, json: bool) -> i32 {
    let base = std::env::var("FLOPTLE_ACCOUNT_BASE").unwrap_or_else(|_| floptle_account::DEFAULT_BASE.to_string());
    let account = apply.then(|| floptle_account::Account::new(base.clone()));
    run_with(project, apply, json, &base, &mut |game, name, body| {
        let a = account.as_ref().expect("made when applying");
        let waited = std::time::Instant::now();
        while a.is_restoring() && waited.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !a.is_signed_in() {
            return Err(NOT_SIGNED_IN.into());
        }
        a.declare_collection(game, name, body)
    })
}

const NOT_SIGNED_IN: &str = "nobody is signed in: sign in to Floptle in the Hub (or the editor) as the game's owner and run this again";

/// Declares one collection: `(game, name, body)`.
type Declare<'a> = dyn FnMut(&str, &str, &serde_json::Value) -> Result<u16, String> + 'a;

fn run_with(project: &Path, apply: bool, json: bool, base: &str, declare: &mut Declare) -> i32 {
    let say_json = |v: serde_json::Value| floptle_say::say!("{v}");
    let fail = |code: i32, errors: Vec<String>| -> i32 {
        if json {
            say_json(serde_json::json!({ "ok": false, "errors": errors }));
        } else {
            for e in errors {
                floptle_say::say_err!("{e}");
            }
        }
        code
    };
    let cfg = floptle_scene::load_project(&project.join("project.ron"));
    let Some(cloud) = cfg.cloud.filter(|c| c.is_connected() && !c.game.trim().is_empty()) else {
        return fail(
            3,
            vec!["this project is not connected to a game on Floptle Cloud; connect it in ⚙ Settings ▸ Networked".into()],
        );
    };
    let file = project.join(FILE);
    let Ok(text) = std::fs::read_to_string(&file) else {
        return fail(
            4,
            vec![format!(
                "{} is missing. List the game's collections there, one per line: \
                 (name: \"time\", kind: rank, keep: min)",
                file.display()
            )],
        );
    };
    let wants = match parse(&text) {
        Ok(w) => w,
        Err(errors) => return fail(4, errors),
    };
    let server = match floptle_account::collections::read_declared(base, &cloud.game, &cloud.key) {
        Ok(s) => s,
        Err(e) => return fail(1, vec![e]),
    };
    let plan = plan(&wants, &server);
    let game = cloud.game;

    // Applying: one PUT per collection that needs one, carrying on past a
    // refusal so one full collection does not hold up the rest.
    let mut results: Vec<(String, Result<u16, String>)> = Vec::new();
    if apply {
        for (name, step, body) in &plan.steps {
            if *step != Step::Same {
                let r = declare(&game, name, body);
                let stop = matches!(&r, Err(e) if e == NOT_SIGNED_IN);
                results.push((name.clone(), r));
                if stop {
                    break;
                }
            }
        }
    }
    let refused = results.iter().any(|(_, r)| r.is_err());
    let signed_out = results.iter().any(|(_, r)| matches!(r, Err(e) if e == NOT_SIGNED_IN));

    if json {
        let steps: Vec<_> = plan
            .steps
            .iter()
            .map(|(name, step, body)| {
                let mut v = serde_json::json!({ "name": name, "declaration": body });
                match step {
                    Step::Same => v["step"] = "same".into(),
                    Step::Create => v["step"] = "create".into(),
                    Step::Change(d) => {
                        v["step"] = "change".into();
                        v["changes"] = d
                            .iter()
                            .map(|(k, from, to)| serde_json::json!({ "field": k, "server": from, "file": to }))
                            .collect();
                    }
                }
                if let Some((_, r)) = results.iter().find(|(n, _)| n == name) {
                    match r {
                        Ok(status) => v["applied"] = (*status).into(),
                        Err(e) => v["error"] = e.clone().into(),
                    }
                }
                v
            })
            .collect();
        let in_step = if apply { !refused } else { plan.pending() == 0 };
        say_json(serde_json::json!({
            "ok": in_step, "game": game, "applied": apply, "collections": steps, "server_only": plan.server_only,
        }));
    } else {
        floptle_say::say!("{game} on fopull.com, against {FILE}:");
        for (name, step, body) in &plan.steps {
            let line = match step {
                Step::Same => format!("  = {name:<14} {}", summary(body)),
                Step::Create => format!("  + {name:<14} new: {}", summary(body)),
                Step::Change(d) => format!(
                    "  ~ {name:<14} {}",
                    d.iter().map(|(k, from, to)| format!("{k} {} -> {}", show(from), show(to))).collect::<Vec<_>>().join(", ")
                ),
            };
            floptle_say::say!("{line}");
            match results.iter().find(|(n, _)| n == name) {
                Some((_, Ok(201))) => floptle_say::say!("      declared"),
                Some((_, Ok(_))) => floptle_say::say!("      changed"),
                Some((_, Err(e))) if e == NOT_SIGNED_IN => {}
                Some((_, Err(e))) => floptle_say::say_err!("      {name}: {e}"),
                None => {}
            }
        }
        for n in &plan.server_only {
            floptle_say::say!("  ? {n:<14} only on the server; left alone (deleting it deletes its data: do that on the game's page)");
        }
        if signed_out {
            floptle_say::say_err!("{NOT_SIGNED_IN}");
        } else if apply && refused {
            floptle_say::say_err!("fopull.com refused some of it; the rest is applied");
        } else if apply {
            floptle_say::say!("fopull.com matches {FILE}");
        } else if plan.pending() == 0 {
            floptle_say::say!("in step: nothing to apply");
        } else {
            floptle_say::say!("{} to apply: run it again with --apply to make fopull.com match", plan.pending());
        }
    }
    match () {
        _ if signed_out => 3,
        _ if apply => i32::from(refused),
        _ => i32::from(plan.pending() > 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server() -> Vec<serde_json::Value> {
        vec![
            json!({"name":"installs","kind":"counter","access":"public","authority":"player","cap_kb":1,"mode":"unique","declared":true}),
            json!({"name":"replays","kind":"blobs","access":"public","authority":"player","cap_kb":512,"declared":true}),
            json!({"name":"time","kind":"rank","access":"public","authority":"player","cap_kb":4,"keep":"max","declared":true}),
            json!({"name":"old","kind":"docs","access":"private","authority":"player","cap_kb":64,"declared":false}),
        ]
    }

    #[test]
    fn the_file_reads_as_its_author_wrote_it_and_defaults_are_the_servers() {
        let w = parse(
            r#"[
                (name: "replays", kind: blobs, access: public, cap_kb: 512),
                (name: "time", kind: rank, keep: min),
                (name: "installs", kind: counter, mode: unique),
                (name: "saves", kind: docs),
            ]"#,
        )
        .unwrap();
        let body = |i: usize| w[i].fields.iter().map(|(k, v)| (*k, v.clone())).collect::<Vec<_>>();
        assert_eq!(
            body(1),
            vec![("kind", json!("rank")), ("access", json!("public")), ("authority", json!("player")), ("keep", json!("min"))]
        );
        assert_eq!(body(2).last().unwrap(), &("mode", json!("unique")));
        assert_eq!(body(3)[1], ("access", json!("private")), "docs default to private, as the server does");
    }

    #[test]
    fn every_mistake_is_named_at_once() {
        let errs = parse(
            r#"[
                (name: "Laps", kind: rank),
                (name: "laps", kind: counter, access: private, keep: max),
                (name: "laps", kind: docs, mode: unique, cap_kb: 0),
            ]"#,
        )
        .unwrap_err();
        let all = errs.join("\n");
        for want in [
            "'Laps' is not a collection name",
            "Laps: a rank collection says what it keeps",
            "laps: a counter collection is always public",
            "laps: keep is for rank collections only",
            "laps is listed twice",
            "laps: mode is for counter collections only",
            "laps: cap_kb is at least 1",
        ] {
            assert!(all.contains(want), "missing {want:?} in:\n{all}");
        }
        let e = parse("[(name: \"x\", kind: docs, colour: red)]").unwrap_err();
        assert!(e[0].contains("colour"), "{e:?}");
    }

    #[test]
    fn the_plan_creates_changes_and_leaves_the_rest() {
        let w = parse(
            r#"[
                (name: "replays", kind: blobs, access: public, cap_kb: 512),
                (name: "time", kind: rank, keep: min),
                (name: "laps", kind: rank, keep: min),
                (name: "old", kind: docs),
            ]"#,
        )
        .unwrap();
        let p = plan(&w, &server());
        let step = |n: &str| p.steps.iter().find(|(m, ..)| m == n).unwrap();
        assert_eq!(step("replays").1, Step::Same);
        assert_eq!(step("laps").1, Step::Create);
        assert_eq!(step("time").1, Step::Change(vec![("keep", json!("max"), json!("min"))]));
        // The file left the cap to the server, so the PUT keeps the server's.
        assert_eq!(step("time").2["cap_kb"], json!(4));
        assert_eq!(step("old").1, Step::Change(vec![("declared", json!(false), json!(true))]));
        assert_eq!(p.server_only, vec!["installs".to_string()]);
        assert_eq!(p.pending(), 3);
    }

    /// A scratch project directory, removed when the test ends.
    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("floptle-collections-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A project on disk connected to a game, with the server played by a
    /// loopback listener that answers the collection list.
    fn project(tag: &str, file: &str) -> (Dir, String) {
        let d = Dir::new(tag);
        std::fs::write(d.0.join("project.ron"), "(cloud: Some((game: \"freeflier\", key: \"fk_live_T\")))").unwrap();
        std::fs::write(d.0.join(FILE), file).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let list = json!({ "game": "freeflier", "collections": server() }).to_string();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for mut c in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf);
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{list}",
                    list.len()
                );
            }
        });
        (d, base)
    }

    #[test]
    fn a_check_never_declares_and_apply_declares_only_what_differs() {
        let file = r#"[(name: "replays", kind: blobs, access: public, cap_kb: 512), (name: "time", kind: rank, keep: min), (name: "laps", kind: rank, keep: min)]"#;
        let (d, base) = project("apply", file);
        let mut sent: Vec<(String, serde_json::Value)> = Vec::new();
        let code = run_with(&d.0, false, true, &base, &mut |_, n, b| {
            sent.push((n.into(), b.clone()));
            Ok(200)
        });
        assert_eq!(code, 1, "out of step is exit 1, for a build to stop on");
        assert!(sent.is_empty(), "a check declared something: {sent:?}");

        let code = run_with(&d.0, true, true, &base, &mut |g, n, b| {
            assert_eq!(g, "freeflier");
            sent.push((n.into(), b.clone()));
            Ok(if n == "laps" { 201 } else { 200 })
        });
        assert_eq!(code, 0);
        let names: Vec<_> = sent.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["time", "laps"], "only the two that differ are sent");

        // A refusal is reported and the rest still goes.
        let mut tried = Vec::new();
        let code = run_with(&d.0, true, true, &base, &mut |_, n, _| {
            tried.push(n.to_string());
            if n == "time" { Err("fopull.com answered 409: not empty".into()) } else { Ok(201) }
        });
        assert_eq!(code, 1);
        assert_eq!(tried, ["time", "laps"]);
    }

    #[test]
    fn a_project_without_a_game_or_a_file_says_what_is_missing() {
        let d = Dir::new("missing");
        std::fs::write(d.0.join("project.ron"), "()").unwrap();
        assert_eq!(run_with(&d.0, false, true, "http://127.0.0.1:9", &mut |_, _, _| unreachable!()), 3);
        std::fs::write(d.0.join("project.ron"), "(cloud: Some((game: \"g\", key: \"fk_live_T\")))").unwrap();
        assert_eq!(run_with(&d.0, false, true, "http://127.0.0.1:9", &mut |_, _, _| unreachable!()), 4);
    }
}
