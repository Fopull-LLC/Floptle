# ADR-0002 — GPU portability: wgpu (not raw Vulkan)

- **Status:** Accepted · confirmed 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The developer wants a **custom renderer** ("libs for OS plumbing only") that runs
on Linux, Windows, and macOS. macOS has no native Vulkan — it requires Metal (or
MoltenVK translation). The renderer's *architecture and look* must stay ours.

## Decision
Use **wgpu** as the thin GPU portability layer (with **winit** for windowing).
The render graph, passes, materials, raymarching, and post stack — everything
that defines how Floptle looks — are built by us on top of wgpu.

## Why
- One API targets **Vulkan (Linux/Win), Metal (macOS), DX12 (Win), GL** — macOS
  "just works" instead of a month of MoltenVK plumbing.
- Ships with **naga**, which we already need for shader IR → WGSL/SPIR-V (ADR-0007).
- Mature, actively developed, validation layers for fast debugging.
- wgpu is "OS/GPU plumbing" in the same spirit as winit — it does **not** dictate
  our renderer design.

## Alternatives considered
- **Raw Vulkan via `ash`** — maximum control, but ~3× the work, and macOS still
  needs MoltenVK. Justified only if we hit a wall wgpu can't express.
- **OpenGL** — simplest, but deprecated on macOS and too limited for our effects.
- **bgfx/sokol (C)** — would add an FFI boundary and a non-Rust dependency.

## Consequences
- A small abstraction ceiling: a few bleeding-edge GPU features may be gated
  behind wgpu's portability model. Acceptable for our art-driven goals.
- **Reversible:** the backend is isolated inside `floptle-render::device`/`graph`.
  If we ever need to drop to `ash`, gameplay/editor code is unaffected.
- **Confirmed 2026-06-27** to enable shipping games on Steam with first-class
  Linux + Windows + macOS support; wgpu's portable backends are the deciding
  factor. Steamworks integration (overlay, achievements, cloud, input) is a
  later export-time concern layered on the runtime (see ROADMAP Phase 9–10),
  not a renderer concern.
