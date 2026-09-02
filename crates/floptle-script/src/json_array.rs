//! The one tag that lets Lua say **"this is a list"**.
//!
//! Lua has a single table type and JSON has two, so every encoder has to guess.
//! The guess — keys `1..n` and nothing else is an array — is right for every
//! non-empty case and cannot be right for the empty one: `{}` is both an empty
//! list and an empty object, and an encoder must pick. It picks `{}`, because
//! an empty body posted to an API that reads objects has to stay an object.
//!
//! That left no value a script could build that wrote `[]`, which is the
//! error path of every list a script assembles: the selection with nothing
//! selected, the ids of the rows nobody ticked. It is invisible while testing
//! with data and appears against the empty case.
//!
//! So a table can carry a metatable saying what it is, and the guess is only
//! consulted when nothing said. `json.decode` tags every array it builds, which
//! is what makes read → edit → send back preserve the types it was handed
//! without the script having to remember which fields were lists.
//!
//! The tag is a plain `__jsonarray = true` on the metatable rather than a
//! hidden identity, so a script that builds its own tables can set it too —
//! there is nothing here worth making unforgeable.

use mlua::{Lua, Table, Value};

/// Registry key for the shared metatable, so every tagged table in one Lua
/// state shares one.
const ARRAY_MT: &str = "floptle.json.array";

/// The metatable that marks a table as a JSON array, created once per state.
pub fn array_metatable(lua: &Lua) -> mlua::Result<Table> {
    if let Ok(Value::Table(t)) = lua.named_registry_value::<Value>(ARRAY_MT) {
        return Ok(t);
    }
    let mt = lua.create_table()?;
    mt.raw_set("__jsonarray", true)?;
    // **Not handed out.** `getmetatable` returns this string instead of the
    // table, so a script cannot reach in and clear the mark. One Lua state runs
    // every package, so a single `getmetatable(json.array{}).__jsonarray = nil`
    // would turn every other package's lists back into objects for the rest of
    // the session. Rust's `set_metatable` bypasses `__metatable`, which is what
    // tagging needs and nothing else has.
    mt.raw_set("__metatable", "json.array")?;
    // Shows up in `tostring` and in mlua's own errors as something with a name
    // rather than as an anonymous table.
    mt.raw_set("__name", "json.array")?;
    lua.set_named_registry_value(ARRAY_MT, mt.clone())?;
    Ok(mt)
}

/// Has somebody said this table is a list?
pub fn is_tagged(t: &Table) -> bool {
    t.metatable().and_then(|mt| mt.raw_get::<bool>("__jsonarray").ok()).unwrap_or(false)
}

/// Would an encoder write this table as a JSON array?
///
/// The tag first, then the shape. Keeping the two answers in one function is
/// what stops `json.isArray` and `json.encode` from disagreeing.
pub fn encodes_as_array(t: &Table) -> bool {
    if is_tagged(t) {
        return true;
    }
    let len = t.raw_len();
    len > 0 && t.clone().pairs::<Value, Value>().count() == len
}

/// What is wrong with this tagged table, if anything.
///
/// Two mistakes, both of which an encoder would otherwise paper over:
///
/// * **A key an array cannot hold.** A table that says it is a list and also
///   carries `name = "x"` is a mistake somewhere, and dropping the key silently
///   is how it would stay one.
/// * **A hole.** `t[2] = nil` on a three-item list leaves `#t` at 3 in LuaJIT,
///   so the range check alone passes — and then one encoder stops at the hole
///   (losing item 3) while the other writes `null` in the middle of a list of
///   numbers. Two encoders quietly disagreeing about a value is worse than
///   either answer, so it is refused and named.
pub fn problem(t: &Table) -> Option<String> {
    // Built from the KEYS, not from `#t`. Lua's length operator is free to stop
    // at a hole — `t[2] = nil` on a three-item list makes `#t` report 1 — so a
    // range check against it calls the surviving item 3 a stray key and says
    // something true about the wrong thing. True of both VMs (ADR-0028).
    //
    // `mlua::Integer`, not `i64`: these count Lua table indices, and Lua's
    // integer is 32-bit under Luau and 64-bit under LuaJIT.
    let mut highest: mlua::Integer = 0;
    let mut count: mlua::Integer = 0;
    for pair in t.clone().pairs::<Value, Value>() {
        let Ok((k, _)) = pair else { continue };
        let index = match &k {
            Value::Integer(i) => Some(*i),
            Value::Number(n) if n.fract() == 0.0 => Some(*n as mlua::Integer),
            _ => None,
        };
        match index.filter(|i| *i >= 1) {
            Some(i) => {
                highest = highest.max(i);
                count += 1;
            }
            None => {
                return Some(format!(
                    "it also carries the key {} — a JSON array has no keys",
                    match k {
                        Value::String(s) => format!("\"{}\"", s.to_string_lossy()),
                        other => format!("{other:?}"),
                    }
                ));
            }
        }
    }
    if count != highest {
        let missing = (1..=highest)
            .find(|i| matches!(t.raw_get::<Value>(*i), Ok(Value::Nil) | Err(_)))
            .unwrap_or(highest);
        return Some(format!(
            "item {missing} of {highest} is missing — a JSON array cannot have a hole in it. \
             Use table.remove to take an item out, which closes the gap."
        ));
    }
    None
}

/// The body of `json.array(t)`: tag a table, or make a tagged empty one.
pub fn tag(lua: &Lua, v: Value) -> mlua::Result<Table> {
    let t = match v {
        Value::Nil => lua.create_table()?,
        Value::Table(t) => t,
        other => {
            return Err(mlua::Error::runtime(format!(
                "json.array takes a table — or nothing, for an empty list. This one is of type {}",
                other.type_name()
            )));
        }
    };
    t.set_metatable(Some(array_metatable(lua)?));
    Ok(t)
}

/// Install `json.array` and `json.isArray` into a `json` table.
///
/// Shared so the editor's package API and a game script's API cannot answer
/// this differently — the two have separate encoders, and a list that is a list
/// in one and an object in the other would be worse than neither having it.
pub fn install(lua: &Lua, json: &Table) -> mlua::Result<()> {
    json.set("array", lua.create_function(|lua, v: Value| tag(lua, v))?)?;
    json.set(
        "isArray",
        lua.create_function(|_, v: Value| {
            Ok(matches!(&v, Value::Table(t) if encodes_as_array(t)))
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_json() -> Lua {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        install(&lua, &t).unwrap();
        lua.globals().set("json", t).unwrap();
        lua
    }

    #[test]
    fn an_empty_table_is_an_object_and_a_tagged_one_is_a_list() {
        let lua = lua_with_json();
        let plain: Table = lua.load("return {}").eval().unwrap();
        assert!(!encodes_as_array(&plain), "an empty table has to stay an object");
        let tagged: Table = lua.load("return json.array{}").eval().unwrap();
        assert!(encodes_as_array(&tagged));
        assert!(problem(&tagged).is_none());
    }

    #[test]
    fn a_filled_table_needs_no_tag() {
        let lua = lua_with_json();
        let t: Table = lua.load("return {1, 2, 3}").eval().unwrap();
        assert!(encodes_as_array(&t));
        assert!(!is_tagged(&t), "the shape answered; nothing had to be marked");
    }

    #[test]
    fn a_map_is_not_a_list() {
        let lua = lua_with_json();
        let t: Table = lua.load("return { a = 1 }").eval().unwrap();
        assert!(!encodes_as_array(&t));
    }

    #[test]
    fn is_array_agrees_with_the_encoder_in_every_case() {
        let lua = lua_with_json();
        for (src, want) in [
            ("{}", false),
            ("json.array{}", true),
            ("{1,2}", true),
            ("json.array{1,2}", true),
            ("{a=1}", false),
            ("json.array()", true),
            ("'text'", false),
            ("nil", false),
        ] {
            let got: bool = lua.load(format!("return json.isArray({src})")).eval().unwrap();
            assert_eq!(got, want, "json.isArray({src})");
        }
    }

    #[test]
    fn a_list_with_a_name_on_it_names_the_key() {
        let lua = lua_with_json();
        let t: Table = lua.load("local t = json.array{1} t.name = 'x' return t").eval().unwrap();
        assert!(problem(&t).unwrap().contains("\"name\""));
    }

    /// A hole is refused rather than encoded, because the two encoders in the
    /// engine disagree about what to do with one: a sequence walk stops at it
    /// and drops everything after, and an index walk writes `null` into the
    /// middle of a list of numbers. Neither is what anybody meant.
    #[test]
    fn a_list_with_a_hole_in_it_says_which_item_is_missing() {
        let lua = lua_with_json();
        let t: Table = lua.load("local t = json.array{1, 2, 3} t[2] = nil return t").eval().unwrap();
        let why = problem(&t).expect("a hole must be refused");
        assert!(why.contains("item 2"), "{why}");
        assert!(why.contains("table.remove"), "the message should say the fix: {why}");
        // …and taking one out properly is fine.
        let ok: Table =
            lua.load("local t = json.array{1, 2, 3} table.remove(t, 2) return t").eval().unwrap();
        assert!(problem(&ok).is_none());
    }

    /// A package cannot clear the mark for every other package in the session.
    #[test]
    fn the_shared_metatable_cannot_be_reached_from_lua() {
        let lua = lua_with_json();
        let got: String =
            lua.load("return tostring(getmetatable(json.array{}))").eval().unwrap();
        assert_eq!(got, "json.array", "the real metatable was handed out");
        let broke: bool = lua
            .load(
                "local t = json.array{} \
                 local ok = pcall(function() getmetatable(t).__jsonarray = nil end) \
                 return ok and not json.isArray(json.array{})",
            )
            .eval()
            .unwrap();
        assert!(!broke, "a script cleared the shared mark");
    }

    #[test]
    fn tagging_something_that_is_not_a_table_says_what_it_got() {
        let lua = lua_with_json();
        let e = lua.load("return json.array(7)").exec().unwrap_err().to_string();
        assert!(e.contains("json.array takes a table") && e.contains("type integer"), "{e}");
    }

    #[test]
    fn one_metatable_is_shared_by_every_tagged_table() {
        let lua = lua_with_json();
        let same: bool =
            lua.load("return getmetatable(json.array{}) == getmetatable(json.array{1})")
                .eval()
                .unwrap();
        assert!(same, "each call built its own metatable");
    }
}
