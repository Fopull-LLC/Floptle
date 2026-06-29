//! Node scripting — the *data* a node carries (ADR-0003).
//!
//! A node can hold a [`Scripts`] component: a list of [`ScriptInst`]s, each naming
//! a `.lua` script (the file stem under the project's `scripts/` folder) plus the
//! per-instance float `params` it's tuned with and an enabled flag. The data is
//! plain (no GPU/serde/Lua here) so it serializes through the scene DTOs; the Lua
//! VM that *executes* these lives in the `floptle-script` crate (`ScriptHost`).

/// One attached script: which `.lua` script runs, its parameters, and the toggle.
///
/// `kind` is the script's name — the file stem of `scripts/<kind>.lua`. (The field
/// kept its name across the move from built-in behaviors to Lua so existing scenes
/// keep loading; "rotate" now resolves to `scripts/rotate.lua`.)
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptInst {
    pub kind: String,
    pub enabled: bool,
    pub params: Vec<(String, f32)>,
}

impl ScriptInst {
    /// Look up a parameter, falling back to `default`.
    pub fn param(&self, name: &str, default: f32) -> f32 {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| *v).unwrap_or(default)
    }

    /// A fresh instance of the named script with no params yet (the editor seeds
    /// them from the script's `defaults` table when attaching).
    pub fn new(kind: &str) -> Self {
        Self { kind: kind.to_string(), enabled: true, params: Vec::new() }
    }
}

/// The scripts attached to a node.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Scripts(pub Vec<ScriptInst>);
