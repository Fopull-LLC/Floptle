# ADR-0007 — Shaders: a custom IR, editable as graph AND text, transpiled to WGSL

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
Shaders are Floptle's single biggest lever for visuals nobody has seen. The
developer prefers a node graph but also wants to write/AI-edit shaders as text,
and wants the two views to be the *same shader*, switchable at any time.

## Decision
Define a custom **shader IR** as the single source of truth. The editor presents
it as a node **graph**; an "Open in VSCode" button serializes it to a readable
**text format (`.flsl`)**; either representation round-trips back into the IR;
the IR **transpiles to WGSL** (validated by naga) for wgpu.

## Why
- **One source of truth** is what keeps graph and text perfectly in sync.
- **Dual authoring:** artists use the graph; text enables AI-assisted authoring
  and power users — exactly the developer's working style.
- Owning the representation lets us add non-standard nodes (raymarch/SDF warps,
  feedback, impossible color transport) that drive the otherworldly look.

## Alternatives considered
- **WGSL-only** — simplest, but no graph and no custom artistic abstraction.
- **Graph-only** — friendly, but blocks text/AI editing and version-control diffs.
- **An existing node-graph library** — wouldn't give us our own text format or
  custom semantic nodes.

## Consequences
- Building an IR + parser/printer + transpiler is real work. We **start small**
  (a usable subset: inputs, math, texture, noise, SDF, output) and grow the
  stdlib over time. This is the language-building effort we deliberately take on.
- `.flsl` files live in the project and are git/AI-friendly.
