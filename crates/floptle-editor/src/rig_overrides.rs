//! Per-model authoring overrides stored in a `<model>.rig.ron` sidecar beside the
//! `.glb` — currently object **re-parenting** (make a forearm follow a shoulder
//! without editing the source model). The sidecar is loaded at import and applied
//! to the rig's skeleton (see [`crate::anim::rig_from_model`]); it's written by the
//! Inspector's Objects & Rig "under ▸" dropdown. Non-destructive: deleting the
//! sidecar restores the model's authored hierarchy, and the Mirror-apply bake folds
//! the overrides into the exported `.glb` so the result is a plain, normal model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The `.rig.ron` payload. Extend with future per-model authoring aids (pivot
/// nudges, renames, …); everything is `#[serde(default)]` so old sidecars load.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct RigOverrides {
    /// child object/bone name → new parent name (`""` or absent parent = model root).
    #[serde(default)]
    pub reparent: BTreeMap<String, String>,
}

impl RigOverrides {
    /// The sidecar path for a model file (`…/Sae.glb` → `…/Sae.rig.ron`).
    pub fn sidecar_path(model_abs: &Path) -> PathBuf {
        model_abs.with_extension("rig.ron")
    }

    /// Load the sidecar beside `model_abs` (absolute path). Missing / unparseable
    /// → empty (the model imports with its authored hierarchy).
    pub fn load(model_abs: &Path) -> Self {
        let p = Self::sidecar_path(model_abs);
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| ron::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist beside `model_abs`. An empty override removes a stale sidecar so a
    /// fully-cleared reparent doesn't linger on disk.
    pub fn save(&self, model_abs: &Path) -> std::io::Result<()> {
        let p = Self::sidecar_path(model_abs);
        if self.reparent.is_empty() {
            let _ = std::fs::remove_file(&p);
            return Ok(());
        }
        let s = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .map_err(std::io::Error::other)?;
        std::fs::write(&p, s)
    }
}
