//! Option-table checking, shared by every Lua call that takes one.
//!
//! **Why this module exists.** 32 of the 74 bugs ever filed against this engine
//! — 43% — are one shape: the engine answered something it did not understand
//! instead of refusing it (`floptle/0082`). A `collide` option parsed, stored and
//! read by nothing, for two releases. A `pin = "topCenter"` that meant top-LEFT,
//! silently, forever. A typo'd `perchunk` that took the default and said
//! nothing. Every one was found by somebody playing, and every one was fixed on
//! its own.
//!
//! So the fix is not another patch per site. It is that there is exactly one
//! place that answers *"is this key one we read?"* and one place that answers
//! *"is this string one of the values?"*, both of which produce an error naming
//! **the property, the value received, and what is accepted** — and a test
//! ([`crate::opts::tests`]) that walks the registry so a new option table cannot
//! be added without appearing here.

use mlua::{Table, Value};

/// One Lua-facing option table: the call that takes it and every key it reads.
///
/// Registering a table here is what makes it checkable — and
/// `every_registered_table_refuses_an_unknown_key` walks this list, so an entry
/// with no validation in the real code fails the build.
pub struct OptTable {
    /// The call as a script writes it, e.g. `scatter.create`.
    pub call: &'static str,
    /// Every key the engine reads from the table.
    pub keys: &'static [&'static str],
}

/// Every Lua-facing option table in the engine, and the keys it reads.
///
/// This is the list `every_registered_option_table_refuses_an_unknown_key` walks:
/// it pushes a bogus key through each call **for real, through Lua**, and fails
/// if the call accepts it. So an entry here is a promise the code has to keep,
/// and the companion source-scan test
/// (`no_option_table_escapes_the_registry`) fails when a NEW option table
/// appears that is neither registered nor deliberately excused.
pub const TABLES: &[OptTable] = &[
    OptTable { call: "scatter.create", keys: crate::scatter_api::CREATE_KEYS },
    OptTable { call: "node:setCamera", keys: crate::api::CAMERA_KEYS },
    OptTable { call: "node:setMaterial", keys: crate::api::MATERIAL_KEYS },
    OptTable { call: "node:setCelestial", keys: crate::api::CELESTIAL_KEYS },
    OptTable { call: "node:setTilemap", keys: crate::api::TILEMAP_KEYS },
    OptTable { call: "node:setSpriteBatch", keys: crate::api::SPRITE_BATCH_KEYS },
    OptTable { call: "terrain.generatePlanet", keys: crate::terrain_api::PLANET_KEYS },
    OptTable { call: "audio.play", keys: crate::audio_api::PLAY_KEYS },
    OptTable { call: "net.host", keys: crate::net_api::HOST_KEYS },
    OptTable { call: "net.rpc", keys: crate::net_api::RPC_KEYS },
    OptTable { call: "net.spawn", keys: crate::net_api::SPAWN_KEYS },
    OptTable { call: "input.pushContext", keys: crate::input_api::CONTEXT_KEYS },
    OptTable { call: "scene.load", keys: crate::host::SCENE_LOAD_KEYS },
    OptTable { call: "http.get", keys: crate::http_api::OPT_KEYS },
    OptTable { call: "raycast", keys: crate::shape_api::QUERY_KEYS },
    OptTable { call: "nav.agent", keys: crate::nav_api::AGENT_KEYS },
    OptTable { call: "agent:set", keys: crate::nav_api::AGENT_KEYS },
    OptTable {
        call: "steam.findOrCreateLeaderboard",
        keys: crate::steam_api::CREATE_KEYS,
    },
    OptTable { call: "steam.uploadScore", keys: crate::steam_api::UPLOAD_KEYS },
    OptTable { call: "steam.downloadScores", keys: crate::steam_api::DOWNLOAD_KEYS },
    OptTable { call: "steam.createLobby", keys: crate::steam_api::LOBBY_CREATE_KEYS },
    OptTable { call: "steam.findLobbies", keys: crate::steam_api::LOBBY_FIND_KEYS },
];

/// Refuse an options table containing anything the engine does not read.
///
/// Names the key, and suggests the nearest real one — a rejected typo that
/// doesn't say what you meant is only half an error message.
pub fn check_keys(opts: &Table, known: &[&str], call: &str) -> mlua::Result<()> {
    let mut positional = 0usize;
    let mut unknown: Option<String> = None;
    for pair in opts.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        // A LIST where a keyed table belongs — `node:setSprite{ 8, 1, true }`,
        // which is what anybody who reads the call as taking arguments in order
        // writes. Every option is looked up BY NAME, so such a table sets
        // nothing whatsoever: the call returns, the value is unchanged, and the
        // script's own `print` still says the thing it meant to write. That cost
        // a real project a debugging session on a sprite that would not flip.
        if matches!(k, Value::Integer(_)) {
            positional += 1;
            continue;
        }
        let Value::String(k) = k else { continue };
        let key = k.to_str()?.to_string();
        if known.contains(&key.as_str()) || unknown.is_some() {
            continue;
        }
        unknown = Some(key);
    }
    // The shape first: a table that is a list is wrong in a way that makes the
    // spelling of its other keys beside the point.
    if positional > 0 {
        let example = known.first().copied().unwrap_or("key");
        return Err(mlua::Error::RuntimeError(format!(
            "{call}: {positional} value(s) passed by position, and nothing reads those — every \
             option is named, as in {call}{{ {example} = ... }}. It reads: {}",
            known.join(", ")
        )));
    }
    if let Some(key) = unknown {
        return Err(mlua::Error::RuntimeError(format!(
            "{call}: no option called `{key}`{}",
            near_miss_hint(&key, known)
        )));
    }
    Ok(())
}

/// `" (did you mean `perChunk`?)"`, or the full list when nothing is close.
///
/// The list is the fallback rather than the default because a twelve-name list
/// buries the answer when the answer is one letter of case.
pub fn near_miss_hint(key: &str, known: &[&str]) -> String {
    // Case-insensitive near-miss first (`perchunk` for `perChunk`), then a
    // shared prefix, which covers most of the rest.
    let near = known.iter().find(|n| n.eq_ignore_ascii_case(key)).or_else(|| {
        known.iter().find(|n| {
            let (a, b) = (n.to_ascii_lowercase(), key.to_ascii_lowercase());
            a.len() >= 3 && b.len() >= 3 && a[..3] == b[..3]
        })
    });
    match near {
        Some(n) => format!(" (did you mean `{n}`?)"),
        None => format!(" (it reads: {})", known.join(", ")),
    }
}

/// Resolve an enumerated string value, or refuse it naming what is accepted.
///
/// `parse` is the SAME parser the engine uses to act on the value — that is the
/// whole point of the shape (`floptle/0072`): a check that reimplements the list
/// drifts from it, and the drift is invisible until a player types a name the
/// check allows and the parser doesn't.
pub fn parse_enum<T>(
    call: &str,
    property: &str,
    value: &str,
    accepted: &[&str],
    parse: impl Fn(&str) -> Option<T>,
) -> mlua::Result<T> {
    match parse(value) {
        Some(v) => Ok(v),
        None => Err(mlua::Error::RuntimeError(format!(
            "{call}: `{property} = \"{value}\"` is not a name I know — it takes {}",
            accepted.join(", ")
        ))),
    }
}

/// A number that must sit inside a range: refuse rather than clamp when the
/// value is so far out that the caller clearly meant something else.
///
/// Clamping is right for a value a player drags (a slider) and wrong for one a
/// script states, because a clamped `width = 0` is a texture that renders
/// nothing at a size nobody asked for.
pub fn require_range(
    call: &str,
    property: &str,
    v: f64,
    lo: f64,
    hi: f64,
) -> mlua::Result<f64> {
    if v.is_finite() && v >= lo && v <= hi {
        return Ok(v);
    }
    Err(mlua::Error::RuntimeError(format!(
        "{call}: `{property} = {v}` is outside {lo} – {hi}"
    )))
}

/// Read an option as a boolean, refusing anything else.
///
/// Lua truthiness would take `active = 0` for `true` and `active = "no"` for
/// `true`, both of which are the opposite of what was written.
pub fn opt_bool(t: &Table, call: &str, key: &str) -> mlua::Result<Option<bool>> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Boolean(b) => Ok(Some(b)),
        other => Err(mlua::Error::RuntimeError(format!(
            "{call}: `{key}` takes true or false, got {}",
            other.type_name()
        ))),
    }
}

/// Read an option as a number inside `lo..=hi`, refusing anything else.
pub fn opt_num(
    t: &Table,
    call: &str,
    key: &str,
    lo: f64,
    hi: f64,
) -> mlua::Result<Option<f64>> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Integer(i) => require_range(call, key, i as f64, lo, hi).map(Some),
        Value::Number(n) => require_range(call, key, n, lo, hi).map(Some),
        other => Err(mlua::Error::RuntimeError(format!(
            "{call}: `{key}` takes a number between {lo} and {hi}, got {}",
            other.type_name()
        ))),
    }
}

/// Read an option as a string, refusing anything else. A number is refused
/// deliberately: Lua would coerce `42` to `"42"`, and a numeric asset path or
/// target name is a mistake worth seeing.
pub fn opt_str(t: &Table, call: &str, key: &str) -> mlua::Result<Option<String>> {
    match t.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        other => Err(mlua::Error::RuntimeError(format!(
            "{call}: `{key}` takes a string, got {}",
            other.type_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, Value)]) -> (mlua::Lua, Table) {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        for (k, v) in pairs {
            t.set(*k, v.clone()).unwrap();
        }
        (lua, t)
    }

    #[test]
    fn a_number_where_a_boolean_belongs_is_refused_not_read_as_true() {
        // Lua truthiness makes `0` true, so a coerced read would turn
        // `active = 0` into "yes, active" — the opposite of what was written.
        let (_l, t) = table(&[("active", Value::Integer(0))]);
        let err = opt_bool(&t, "node:setCamera", "active").unwrap_err().to_string();
        assert!(err.contains("active") && err.contains("true or false"), "{err}");
    }

    #[test]
    fn a_boolean_reads_as_itself() {
        let (_l, t) = table(&[("active", Value::Boolean(false))]);
        assert_eq!(opt_bool(&t, "c", "active").unwrap(), Some(false));
        let (_l, t) = table(&[]);
        assert_eq!(opt_bool(&t, "c", "active").unwrap(), None, "absent is not false");
    }

    #[test]
    fn a_string_where_a_number_belongs_names_the_type_it_got() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("width", "big").unwrap();
        let err = opt_num(&t, "node:setCamera", "width", 8.0, 4096.0).unwrap_err().to_string();
        assert!(err.contains("width") && err.contains("string"), "{err}");
    }

    #[test]
    fn a_number_where_a_string_belongs_is_refused_rather_than_stringified() {
        let (_l, t) = table(&[("target", Value::Integer(42))]);
        let err = opt_str(&t, "node:setCamera", "target").unwrap_err().to_string();
        assert!(err.contains("target") && err.contains("integer"), "{err}");
    }

    #[test]
    fn an_unknown_key_names_itself_and_the_nearest_real_one() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("perchunk", 4).unwrap();
        let err = check_keys(&t, &["perChunk", "asset"], "scatter.create").unwrap_err().to_string();
        assert!(err.contains("perchunk"), "the value received: {err}");
        assert!(err.contains("did you mean `perChunk`"), "the nearest real name: {err}");
    }

    #[test]
    fn an_unknown_key_with_no_near_miss_lists_what_is_accepted() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("bogus", 1).unwrap();
        let err = check_keys(&t, &["asset", "range"], "scatter.create").unwrap_err().to_string();
        assert!(err.contains("asset, range"), "what is accepted: {err}");
    }

    #[test]
    fn an_enum_value_error_names_the_property_the_value_and_the_list() {
        let err = parse_enum("ui.make", "pin", "topCentre", &["topLeft", "topCenter"], |s| {
            (s == "topLeft" || s == "topCenter").then_some(())
        })
        .unwrap_err()
        .to_string();
        for want in ["pin", "topCentre", "topLeft", "topCenter"] {
            assert!(err.contains(want), "missing {want}: {err}");
        }
    }

    #[test]
    fn a_range_error_states_the_range() {
        let err = require_range("node:setCamera", "width", 0.0, 8.0, 4096.0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("width") && err.contains('8') && err.contains("4096"), "{err}");
        assert!(require_range("c", "w", 256.0, 8.0, 4096.0).is_ok());
        assert!(
            require_range("c", "w", f64::NAN, 8.0, 4096.0).is_err(),
            "a NaN passes every comparison and must still be refused"
        );
    }

    /// One line of Lua that calls each registered option table, with a key the
    /// engine does not read. A registry entry with no snippet fails the test
    /// below — which is what makes adding an option table cost writing this
    /// down.
    ///
    /// `nonesuch` is the bogus key everywhere, so the assertion is uniform.
    fn bogus_call(call: &str) -> &'static str {
        match call {
            "scatter.create" => "scatter.create{ asset = 'a.glb', nonesuch = 1 }",
            "node:setCamera" => "node:setCamera{ nonesuch = 1 }",
            "node:setMaterial" => "node:setMaterial{ nonesuch = 1 }",
            "node:setCelestial" => "node:setCelestial{ nonesuch = 1 }",
            "node:setTilemap" => "node:setTilemap{ cols = 2, rows = 2, nonesuch = 1 }",
            "node:setSpriteBatch" => "node:setSpriteBatch{ nonesuch = 1 }",
            "terrain.generatePlanet" => "terrain.generatePlanet(1, { nonesuch = 1 })",
            "audio.play" => "audio.play('a.ogg', { nonesuch = 1 })",
            "net.host" => "net.host{ nonesuch = 1 }",
            "net.rpc" => "net.rpc('ping', nil, { nonesuch = 1 })",
            "net.spawn" => "net.spawn('p.prefab.ron', { nonesuch = 1 })",
            "input.pushContext" => "input.pushContext('menu', { nonesuch = 1 })",
            "scene.load" => "scene.load('next', { nonesuch = 1 })",
            "http.get" => "http.get('/x', { nonesuch = 1 }, function() end)",
            "raycast" => "raycast(0,0,0, 0,-1,0, 10, { nonesuch = 1 })",
            "nav.agent" => "nav.agent(node, { nonesuch = 1 })",
            "agent:set" => "local a = nav.agent(node) a:set{ nonesuch = 1 }",
            "steam.findOrCreateLeaderboard" => {
                "steam.findOrCreateLeaderboard('HI', { nonesuch = 1 }, function() end)"
            }
            "steam.uploadScore" => {
                "steam.uploadScore('1', 5, { nonesuch = 1 }, function() end)"
            }
            "steam.downloadScores" => {
                "steam.downloadScores('1', { nonesuch = 1 }, function() end)"
            }
            "steam.createLobby" => "steam.createLobby({ nonesuch = 1 }, function() end)",
            "steam.findLobbies" => "steam.findLobbies({ nonesuch = 1 }, function() end)",
            other => panic!(
                "opts::TABLES lists `{other}` but no test calls it — add a line to \
                 `bogus_call` so the registry entry is a promise the code has to keep"
            ),
        }
    }

    /// Every registered option table refuses a key the engine does not read —
    /// checked by CALLING IT, from Lua, through the real host (`floptle/0082`).
    ///
    /// 32 of the 74 bugs filed against this engine were one shape: the engine
    /// answered something it did not understand. Every one was fixed on its own,
    /// after a player hit it. A registry with no test behind it would be the
    /// same thing again — a list that says the checks exist.
    #[test]
    fn every_registered_option_table_refuses_an_unknown_key() {
        for t in TABLES {
            let src = bogus_call(t.call);
            let err = crate::ScriptHost::eval_for_test(src).expect_err(&format!(
                "`{}` accepted `nonesuch` silently — the whole class of bug this \
                 registry exists to end: {src}",
                t.call
            ));
            assert!(
                err.contains("nonesuch"),
                "`{}` refused, but not because of the key it was given — the message \
                 has to name the value received: {err}",
                t.call
            );
            assert!(
                !t.keys.is_empty(),
                "`{}` is registered with no keys at all",
                t.call
            );
        }
    }

    /// No option table escapes the registry (`floptle/0082`).
    ///
    /// This is the half that stops the audit decaying. It scans this crate's own
    /// source for the shape of an options table — a Lua closure taking a
    /// `Table`/`Option<Table>` that is not a handle — and fails when one appears
    /// that neither calls [`check_keys`] nor is excused below with a reason.
    ///
    /// A source scan is a blunt instrument, and the right one here: the
    /// alternative is trusting that whoever adds the next `terrain.sculpt{...}`
    /// remembers a convention, and 32 bugs say that does not happen.
    #[test]
    fn no_option_table_escapes_the_registry() {
        // Closures that take a table which is NOT a bag of options, with why.
        const NOT_OPTIONS: &[(&str, &str)] = &[
            ("math_api.rs", "list helpers (map/filter/sort) take DATA, not options"),
            ("http_api.rs", "the reply table is built by the engine and read by the game"),
            ("ui_make.rs", "ui.make validates every property AND value itself (floptle/0072)"),
            ("api.rs", "handle metatables and the construction calls, all checked at the call"),
            ("audio_api.rs", "sound/track handles; the one options table is audio.play"),
            ("net_api.rs", "node handles and the synced store's __index/__newindex"),
            ("assembly_api.rs", "vector arguments accepted as {x,y,z} tables"),
            ("shape_api.rs", "query options go through query_opts, which checks them"),
            ("terrain_api.rs", "the one options table is the planet fill"),
            ("input_api.rs", "the one options table is input.pushContext"),
            ("scatter_api.rs", "the one options table is scatter.create"),
            ("host.rs", "scene.load; the rest are handle/registry tables"),
            ("env.rs", "the script environment itself"),
            ("save_api.rs", "save data is the GAME's table, arbitrary by design"),
            ("rollback_api.rs", "snapshot/restore tables are the game's own state"),
            ("account_api.rs", "the account/mission reply tables are engine-built"),
            ("perf_api.rs", "no option tables"),
            ("space_api.rs", "no option tables"),
            ("view_api.rs", "no option tables"),
            ("water_api.rs", "no option tables"),
            ("sched_api.rs", "no option tables"),
            ("math_api.rs", "vector and list helpers"),
            ("preprocess.rs", "no Lua closures"),
            ("opts.rs", "this module"),
            ("lib.rs", "tests and type declarations"),
        ];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut unexcused: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("read src/").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            let src = std::fs::read_to_string(&path).expect("read source");
            // Both spellings — `nav_api.rs` wrote `Option<mlua::Table>` and
            // sailed straight past the narrower scan for a release.
            let takes_table = src.contains("Option<Table>")
                || src.contains("(Table, Table)")
                || src.contains("Option<mlua::Table>")
                || src.contains("(mlua::Table, mlua::Table)");
            if !takes_table {
                continue;
            }
            if src.contains("check_keys") || NOT_OPTIONS.iter().any(|(f, _)| *f == file) {
                continue;
            }
            unexcused.push(file);
        }
        assert!(
            unexcused.is_empty(),
            "these files take a Lua options table but neither check its keys nor appear in \
             NOT_OPTIONS with a reason — an unrecognised key there defaults silently, which \
             is 43% of every bug ever filed against this engine:\n  {}",
            unexcused.join("\n  ")
        );
    }
}
