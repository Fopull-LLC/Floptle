//! # Floptle Editor
//!
//! The authoring application — egui + egui_dock, dark/high-contrast/retro theme
//! (ADR-0004). Hosts the scene view, the particle timeline, the shader graph,
//! and the UI/material editors, with a live wgpu viewport. Opens scripts and
//! `.flsl` shaders in VSCode at the project root (ADR-0011).
//!
//! See `docs/subsystems/editor.md`. This is a planning stub.

fn main() {
    println!(
        "{} editor v{} — planning scaffold. See docs/ for the design.",
        floptle_core::ENGINE_NAME,
        floptle_core::ENGINE_VERSION
    );
}
