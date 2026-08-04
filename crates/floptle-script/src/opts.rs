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

/// Refuse an options table containing anything the engine does not read.
///
/// Names the key, and suggests the nearest real one — a rejected typo that
/// doesn't say what you meant is only half an error message.
pub fn check_keys(opts: &Table, known: &[&str], call: &str) -> mlua::Result<()> {
    for pair in opts.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        let Value::String(k) = k else { continue };
        let key = k.to_str()?.to_string();
        if known.contains(&key.as_str()) {
            continue;
        }
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
}
