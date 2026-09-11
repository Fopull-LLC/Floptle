//! The workspace can only ask for one script VM, and this is what keeps it able to.
//!
//! `mlua` links exactly one Lua, and Cargo features are additive. So the
//! `vm-luau` feature (ADR-0028) rests on one rule that is invisible in the
//! code and easy to leave out of a new manifest:
//!
//! > **every dependency on a crate that carries a VM feature must pass
//! > `default-features = false`.**
//!
//! Leave it out and that crate's default is back in the graph whatever the
//! build asked for — and, should a second VM ever return, what the developer sees is
//! `mlua-sys`'s own build script saying *"You can enable only one of the
//! features: lua54, lua53, …"*, which names none of the features anybody wrote
//! and reads identically to having selected none at all. That diagnosis cost
//! real time once; it should cost a test failure instead.
//!
//! This is a manifest test on purpose. The `compile_error!`s in `src/vm.rs`
//! state the same invariant but cannot reach the developer: `mlua-sys`'s build
//! script runs before this crate compiles, so it always fails first.
//!
//! The set of VM-carrying crates is **derived**, not listed — a crate carries a
//! VM if its own manifest declares a `vm-luau` feature. A new forwarder is
//! covered the moment it exists, which a hand-written list would not be.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").canonicalize().expect("crates/")
}

/// Every `crates/<name>/Cargo.toml`, as (name, body).
fn manifests() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(crates_dir()).expect("read crates/").flatten() {
        let toml = e.path().join("Cargo.toml");
        if let Ok(body) = std::fs::read_to_string(&toml) {
            out.insert(e.file_name().to_string_lossy().into_owned(), body);
        }
    }
    assert!(out.len() > 20, "only {} crates found — the walk is broken", out.len());
    out
}

/// A crate carries the VM switch if it declares the Luau half of the pair.
fn vm_carriers(all: &BTreeMap<String, String>) -> Vec<String> {
    all.iter()
        .filter(|(_, body)| body.contains("\nvm-luau = ["))
        .map(|(name, _)| name.clone())
        .collect()
}

/// **Rule 1** — nobody may depend on a VM-carrying crate with its defaults on.
#[test]
fn no_dependency_smuggles_a_second_script_vm_into_the_graph() {
    let all = manifests();
    let carriers = vm_carriers(&all);
    assert!(
        carriers.contains(&"floptle-script".to_string()),
        "floptle-script no longer declares `vm-luau`; this test is asserting nothing"
    );

    let mut bad = Vec::new();
    for (name, body) in &all {
        for (n, line) in body.lines().enumerate() {
            let Some(dep) = line.split(" = {").next().filter(|d| carriers.iter().any(|c| c == d))
            else {
                continue;
            };
            if dep == name || !line.contains(" = {") {
                continue;
            }
            if !line.contains("default-features = false") {
                bad.push(format!("crates/{name}/Cargo.toml:{}  {dep}", n + 1));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} dependency line(s) on a VM-carrying crate keep its default features, so the \
         feature cannot be switched off for a build that needs to:\n  {}\n\n\
         Add `default-features = false` to each, and forward `vm-luau` from the depending \
         crate if it actually needs a VM.",
        bad.len(),
        bad.join("\n  ")
    );
}

/// **Rule 2** — every carrier defaults to `vm-luau`, and the LuaJIT half is
/// gone for good.
///
/// The default has to be the SAME on every carrier. They are separate
/// manifests with no shared switch, so a carrier that quietly stops
/// defaulting to a VM leaves anything depending on it with no Lua — reported
/// by `mlua-sys` as a message naming neither feature. And `vm-luajit` was the
/// one-release escape hatch ADR-0028 scheduled (v0.84.x); a manifest that
/// grows it back is a build that hands every script `io`, and this is where
/// that is refused rather than reviewed.
#[test]
fn every_vm_carrier_defaults_to_luau_and_none_carries_luajit() {
    let all = manifests();
    let mut bad = Vec::new();
    for name in vm_carriers(&all) {
        let body = &all[&name];
        if body.contains("\nvm-luajit = [") || body.contains("mlua/luajit") {
            bad.push(format!(
                "crates/{name}/Cargo.toml — declares a LuaJIT feature. The escape hatch was \
                 removed in v0.89.0 (ADR-0028); a LuaJIT build exposes `io` to every script"
            ));
        }
        // **Which VM the default selects**, not the exact text of the list.
        // A carrier may legitimately default to more than a VM —
        // `floptle-editor` also defaults to `editor-ui`, the feature that makes
        // it the authoring application rather than the engine a build ships —
        // and pinning the literal `[\"vm-luau\"]` turned that into a failure
        // about something else entirely. What must never drift is the VM.
        let default_line =
            body.lines().find(|l| l.trim_start().starts_with("default = [")).unwrap_or("");
        if !default_line.contains("\"vm-luau\"") {
            bad.push(format!(
                "crates/{name}/Cargo.toml — `default` must select `vm-luau`: a carrier that \
                 does not leaves whatever depends on it with no Lua at all. Found: \
                 {default_line:?}"
            ));
        }
    }
    assert!(bad.is_empty(), "the VM feature wiring is off:\n  {}", bad.join("\n  "));
}

/// **Rule 3** — the switch has to stay readable in one place.
///
/// A `[dependencies.floptle-script]` table would satisfy rule 1's intent while
/// slipping past its line-based reading, and nothing in the workspace uses that
/// form today. If one ever needs to, this failure is the prompt to teach rule 1
/// about it rather than to delete the assertion.
#[test]
fn no_dependency_on_a_vm_carrier_hides_in_a_table_header() {
    let all = manifests();
    let carriers = vm_carriers(&all);
    let mut bad = Vec::new();
    for (name, body) in &all {
        for c in &carriers {
            let header = format!("dependencies.{c}]");
            if body.contains(&header) {
                bad.push(format!("crates/{name}/Cargo.toml — [{header}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a dependency on a VM-carrying crate is written as a table header, which \
         `no_dependency_smuggles_a_second_script_vm_into_the_graph` reads line by line and \
         cannot see:\n  {}",
        bad.join("\n  ")
    );
}
