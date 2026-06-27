//! # floptle-script
//!
//! Hosts the hot-reloadable Lua scripts you attach to nodes (ADR-0003). Defines
//! the lifecycle (`on_ready`, `on_update`, `on_event`, ...) and the safe API the
//! engine exposes to scripts. Scripts open straight in VSCode (ADR-0011).
//! See `docs/subsystems/scene-and-nodes.md`.
//!
//! Planned modules:
//! - `host`     : the Lua VM, per-script environments, error surfacing.
//! - `bindings` : engine API exposed to Lua (nodes, input, events, pools, vfx).
//! - `lifecycle`: on_ready / on_update / on_fixed_update / on_event hooks.
//! - `reload`   : file-watch hot reload that preserves script state where safe.
