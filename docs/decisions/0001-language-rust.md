# ADR-0001 — Core language: Rust

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
Floptle must be lightweight, hyperoptimized, cross-platform (Linux/Windows/macOS),
and maintainable by a solo developer over a long time. The developer is
comfortable in Rust, C++, C#, and JS/TS, so familiarity isn't the constraint —
fitness for the job is.

## Decision
Build the entire engine and editor in **Rust** (edition 2024).

## Why
- **No garbage collector.** Predictable frame times — central to "hyperoptimized."
- **Memory safety without a GC.** For a solo dev maintaining a large engine, the
  borrow checker eliminates a whole class of crashes that would otherwise eat days.
- **Best-in-class ecosystem for this exact domain:** `wgpu`, `winit`, `egui`,
  `glam`, `naga`, `mlua`, `gltf`, `parry`/`rapier` are all first-class in Rust.
- **Cross-compilation** to all three OS targets is straightforward.
- **Fearless concurrency** for later job-system parallelism (vfx sim, culling).

## Alternatives considered
- **C++** — mature and the developer knows it, but more footguns (UB, manual
  lifetimes) for a solo long-haul project; build/tooling is heavier.
- **C#** — great tooling, but GC pauses fight the "hyperoptimized" goal; engine
  ecosystem (Stride/MonoGame) is thinner for our custom-renderer ambitions.
- **Zig** — appealing low-level story, but the graphics/tooling ecosystem is too
  immature to lean on for a one-person team.

## Consequences
- Borrow-checker learning curve in a few subsystems (e.g. graph-shaped editor
  state) — managed with arena/handle patterns rather than fighting it.
- Longer cold compiles; mitigated by the crate split, `opt-level` tuning, and an
  optional fast linker (`.cargo/config.toml`).
