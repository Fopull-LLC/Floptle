//! **The vendored Luau still carries the armed-table patch everywhere it
//! raises on a readonly table.** A source test, because what it guards is a
//! re-vendor: a new Luau brings its own write paths, and one that raises
//! "attempt to modify a readonly table" directly instead of going through
//! `luaG_readonlywrite` would make a write to an armed `node` table an error
//! in a script that works today (see the third patch in
//! `third_party/luau0-src/FLOPTLE-PATCHES.md`). The behaviour itself is swept
//! by `every_write_disarms_an_armed_table_and_no_read_does`; this catches the
//! site that sweep has no case for yet.

use std::path::{Path, PathBuf};

fn luau_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/luau0-src/luau")
}

/// Every C++ source and header under `dir`, as (path, body).
fn sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())).flatten() {
        let p = e.path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "cpp" || x == "h")
            && let Ok(body) = std::fs::read_to_string(&p)
        {
            out.push((p, body));
        }
    }
}

#[test]
fn no_readonly_error_is_raised_outside_the_armed_table_helper() {
    let mut all = Vec::new();
    for sub in ["VM", "CodeGen"] {
        sources(&luau_dir().join(sub), &mut all);
    }
    assert!(all.len() > 50, "only {} Luau sources found; the walk is broken", all.len());
    let mut bad = Vec::new();
    let mut helper_calls = 0;
    for (path, body) in &all {
        let name = path.file_name().unwrap().to_string_lossy();
        for (n, line) in body.lines().enumerate() {
            if !line.contains("luaG_readonlyerror(") {
                continue;
            }
            let t = line.trim_start();
            // The declaration and the definition.
            if (name == "ldebug.h" && t.starts_with("LUAI_FUNC l_noret luaG_readonlyerror("))
                || (name == "ldebug.cpp" && t.starts_with("l_noret luaG_readonlyerror("))
            {
                continue;
            }
            // The one call, inside `luaG_readonlywrite`.
            if name == "ldebug.cpp" && t == "luaG_readonlyerror(L);" {
                helper_calls += 1;
                continue;
            }
            bad.push(format!("{}:{}  {}", path.display(), n + 1, t));
        }
    }
    assert!(
        bad.is_empty(),
        "{} place(s) raise on a readonly table without going through luaG_readonlywrite, so a write \
         there to an armed table raises instead of disarming it. Replace `if (x->readonly) \
         luaG_readonlyerror(L);` with `if (x->readonly) luaG_readonlywrite(L, x);`:\n{}",
        bad.len(),
        bad.join("\n")
    );
    assert_eq!(helper_calls, 1, "luaG_readonlywrite no longer raises for a frozen table in ldebug.cpp");
    let ldebug = &all.iter().find(|(p, _)| p.ends_with("VM/src/ldebug.cpp")).expect("ldebug.cpp").1;
    assert!(
        ldebug.contains("void luaG_readonlywrite(lua_State* L, LuaTable* t)") && ldebug.contains("t->readonly == 2"),
        "luaG_readonlywrite is gone from ldebug.cpp: the armed-table patch was lost"
    );
    let lapi = &all.iter().find(|(p, _)| p.ends_with("VM/src/lapi.cpp")).expect("lapi.cpp").1;
    assert!(
        lapi.contains("int res = t->readonly == 1;"),
        "lua_getreadonly reports an armed table as frozen again: the armed-table patch was lost"
    );
}
