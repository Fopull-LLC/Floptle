//! # floptle-shader
//!
//! Floptle's signature feature: a single shader **IR** that is the source of
//! truth, presented to the artist as either a node graph (in-editor) or as
//! readable text (`.flsl`, opened in VSCode for AI-assisted editing). The IR
//! transpiles to WGSL for wgpu. See `docs/subsystems/shaders.md` + ADR-0007.
//!
//! Planned modules:
//! - `ir`        : the in-memory shader IR (nodes, edges, types).
//! - `graph`     : graph view <-> IR mapping (what the editor manipulates).
//! - `text`      : `.flsl` text format <-> IR parser/printer (round-trippable).
//! - `transpile` : IR -> WGSL (via naga validation).
//! - `stdlib`    : built-in nodes (noise, sdf primitives, color ops, warps).

/// File extension for the textual shader format ("FLoptle Shading Language").
pub const SHADER_TEXT_EXT: &str = "flsl";
