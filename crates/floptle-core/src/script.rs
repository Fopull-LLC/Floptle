//! Node scripting — v1 built-in behaviors (ADR-0003 lands a real Lua VM later).
//!
//! A node can carry a [`Scripts`] component: a list of attached [`ScriptInst`]s,
//! each a named built-in behavior (`pulsate`, `rotate`, …) with float parameters
//! and an enabled flag. [`run_scripts`] applies the enabled behaviors to the
//! world each frame. The data is plain (no GPU/serde here) so it serializes through
//! the scene DTOs and a future Lua host can replace the behavior table without
//! changing the component shape.

use crate::ecs::{Entity, World};
use crate::math::{Quat, Vec3};
use crate::transform::Transform;

/// One attached script: a behavior kind, its parameters, and whether it runs.
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

    /// A fresh instance of `kind` seeded with its default parameters.
    pub fn new(kind: &str) -> Self {
        Self { kind: kind.to_string(), enabled: true, params: default_params(kind) }
    }
}

/// The scripts attached to a node.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Scripts(pub Vec<ScriptInst>);

/// The built-in behavior kinds the editor offers.
pub const SCRIPT_KINDS: &[&str] = &["pulsate", "rotate"];

/// Default parameters for a behavior kind (used when attaching a new script).
pub fn default_params(kind: &str) -> Vec<(String, f32)> {
    match kind {
        "pulsate" => {
            vec![("amplitude".into(), 0.3), ("speed".into(), 2.0), ("base".into(), 1.0)]
        }
        "rotate" => vec![("speed".into(), 45.0)],
        _ => Vec::new(),
    }
}

/// Run every enabled script in `world`, given seconds since play started.
pub fn run_scripts(world: &mut World, elapsed: f32) {
    // Snapshot (entity, scripts) first so we can mutate Transforms while iterating.
    let work: Vec<(Entity, Scripts)> =
        world.query::<Scripts>().map(|(e, s)| (e, s.clone())).collect();
    for (e, scripts) in work {
        for inst in &scripts.0 {
            if inst.enabled {
                apply(world, e, inst, elapsed);
            }
        }
    }
}

fn apply(world: &mut World, e: Entity, inst: &ScriptInst, t: f32) {
    let Some(tr) = world.get_mut::<Transform>(e) else { return };
    match inst.kind.as_str() {
        "pulsate" => {
            let amp = inst.param("amplitude", 0.3);
            let speed = inst.param("speed", 2.0);
            let base = inst.param("base", 1.0);
            let f = (base * (1.0 + amp * (speed * t).sin())).max(0.01);
            tr.scale = Vec3::splat(f);
        }
        "rotate" => {
            let speed = inst.param("speed", 45.0); // degrees / second
            tr.rotation = Quat::from_rotation_y((speed * t).to_radians());
        }
        _ => {}
    }
}
