//! # floptle-ui
//!
//! The game-facing UI system (NOT the editor UI — that's egui). The goal is the
//! opposite of a giant property list: drop elements, anchor them, script what
//! they do. See `docs/subsystems/ui.md`.
//!
//! Planned modules:
//! - `element`  : the UI element tree (panels, text, images, buttons).
//! - `anchor`   : anchor + pivot + offset layout (resolution independent).
//! - `style`    : a small style set (not a thousand properties).
//! - `interact` : hover/press/focus -> events scripts subscribe to.
//! - `dialogue` : the built-in, themeable dialogue widget (typewriter + voices).
