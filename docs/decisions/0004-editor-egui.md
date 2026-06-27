# ADR-0004 — Editor UI: egui + egui_dock

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The editor needs a scene view, an inspector, a video-editor-style particle
timeline, a shader node graph, and UI/material editors — dark-themed, high-
contrast, retro-inspired, but organized and readable. It should share the
engine's language and integrate with the live wgpu viewport.

## Decision
Build the editor with **egui** (immediate-mode) plus **egui_dock** for dockable
panels. Custom theme + custom widgets for the timeline and node graph.

## Why
- **Immediate-mode** is fast to build and iterate — ideal for a solo dev's tools.
- **Pure Rust**, same language as the engine — no second toolchain/boundary.
- Renders through wgpu, so the editor and the game viewport coexist cleanly.
- Trivial to apply a fully custom **dark/high-contrast/retro** theme.

## Alternatives considered
- **Dear ImGui** — excellent, but C++ bindings add an FFI boundary.
- **Retained toolkits (Slint/iced/Qt)** — nicer for some complex widgets, but
  heavier integration with a real-time viewport and a different mental model.
- **Web/webview (Tauri)** — great for elaborate panels, but adds a JS/HTML
  language boundary and IPC we don't want for a tightly-coupled editor.

## Consequences
- Complex widgets (timeline, node graph) are **custom-built** on egui's
  primitives. This is deliberate — it's where our editor's feel comes from.
- Immediate-mode state management requires discipline (IDs, retained-state
  side tables); standard egui patterns cover it.
