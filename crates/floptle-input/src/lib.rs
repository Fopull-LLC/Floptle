//! # floptle-input
//!
//! Bind any number of physical inputs (keys, mouse buttons, gamepad buttons and
//! sticks) to a named **action**; scripts ask "is `Jump` pressed?" and never
//! touch raw devices. See `docs/subsystems/input.md`.
//!
//! ```text
//! winit events ┐
//! gilrs events ├─▶ RawInput ─▶ ActionRuntime::resolve ─▶ ActionState ─▶ scripts
//! mouse motion ┘                    (per domain, per player)
//! ```
//!
//! ## Modules
//!
//! - [`source`]  — everything bindable: keys, mouse buttons/axes, pad controls.
//! - [`map`]     — the project's `input.ron`: actions, 1D/2D axes, motions.
//! - [`raw`]     — one sampling window's device truth, producer-agnostic.
//! - [`runtime`] — resolution: `RawInput` + `InputMap` → `ActionState`.
//! - [`context`] — the prioritised context stack (gameplay / menu / dialogue).
//! - [`history`] — the fighter layer: input buffering and motion recognition.
//! - [`rebind`]  — press-to-bind capture, shared by the editor and game menus.
//! - [`system`]  — [`InputSystem`], the one object a host owns.
//! - [`pads`]    — the `gilrs` backend (feature `pads`).
//!
//! ## Deliberately dependency-light
//!
//! No `winit` and no `gilrs` appear in the public model — producers translate
//! their device types at the boundary. That keeps every rule in here (deadzones,
//! SOCD, chords, edges) testable with no window, no GPU, and no controller
//! plugged in.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod context;
pub mod history;
pub mod map;
#[cfg(feature = "pads")]
pub mod pads;
pub mod raw;
pub mod rebind;
pub mod runtime;
pub mod source;
pub mod system;

#[cfg(feature = "pads")]
pub use pads::{Pads, MAX_SLOTS};

pub use context::{AllowMask, ConsumeMode, Context, ContextStack};
pub use history::{dir_of, History, DIRECTION_THRESHOLD, HISTORY_TICKS};
pub use map::{
    Action, Axis1, Axis1Binding, Axis2, Axis2Binding, Binding, Curve, InputMap, Motion, Socd,
    DEFAULT_THRESHOLD, MAX_ACTIONS,
};
pub use raw::{PadState, RawInput};
pub use rebind::{BindFilter, Capture};
pub use runtime::{ActionRuntime, ActionState};
pub use system::{Domain, InputSystem, PendingRebind, TickSnapshot};
pub use source::{
    Device, Key, KeyGroup, MouseAxis, MouseButton, PadAxis, PadButton, PadControl, PadId,
    Source,
};

use std::path::Path;

/// The map's filename inside a project (`<project>/input.ron`).
pub const MAP_FILE: &str = "input.ron";

/// Why loading a map failed.
#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Parse(ron::de::SpannedError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "{e}"),
            LoadError::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Load `<project_root>/input.ron`.
///
/// A **missing** file is not an error — it yields `Ok(None)`, so a project that
/// predates the input system (or has simply never opened the Input settings)
/// keeps working on raw keys. A file that exists but won't parse IS an error:
/// silently substituting an empty map there would unbind the whole game and
/// look to the developer like a hardware fault.
pub fn load_map(project_root: &Path) -> Result<Option<InputMap>, LoadError> {
    let path = project_root.join(MAP_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => InputMap::parse(&text).map(Some).map_err(LoadError::Parse),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LoadError::Io(e)),
    }
}

/// Write `<project_root>/input.ron`.
pub fn save_map(map: &InputMap, project_root: &Path) -> Result<(), std::io::Error> {
    std::fs::write(project_root.join(MAP_FILE), map.to_ron())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("floptle_input_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_map_is_not_an_error() {
        let dir = tmp("missing");
        assert!(load_map(&dir).unwrap().is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmp("roundtrip");
        let map = InputMap::starter();
        save_map(&map, &dir).unwrap();
        assert_eq!(load_map(&dir).unwrap(), Some(map));
    }

    #[test]
    fn a_corrupt_map_is_an_error_not_an_empty_map() {
        // Substituting a default here would unbind every control in the game
        // and read to the developer as broken hardware.
        let dir = tmp("corrupt");
        std::fs::write(dir.join(MAP_FILE), "InputMap( actions: [ this isn't RON").unwrap();
        assert!(load_map(&dir).is_err());
    }

    #[test]
    fn the_starter_map_is_within_the_wire_cap() {
        assert!(InputMap::starter().actions.len() <= MAX_ACTIONS);
    }
}
