# ADR-0005 — Data model: ECS core with a Node/Component facade

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The authoring experience the developer wants is the familiar one: a tree of
**nodes**, each with **components** and **scripts** you add. But "hyperoptimized"
demands a data-oriented runtime, which a naive OOP scene graph fights.

## Decision
Store everything in a **data-oriented archetype ECS** at runtime, and expose a
**Node/Component facade** on top for authoring and scripting.

## Why
- **Ergonomic authoring** (nodes/components/scripts) **and** cache-friendly,
  tightly-iterating systems (ECS) — no compromise between feel and speed.
- A Node is just an entity id + transform/hierarchy components; "add component" =
  insert into the ECS; a "script" = a component holding a Lua ref.
- Systems iterate packed component arrays — the path to "fast by default."

## Alternatives considered
- **Pure OOP scene graph** — most familiar, but pointer-chasing and virtual calls
  hurt cache behavior at scale.
- **Pure ECS exposed directly** — maximally fast, but a less friendly authoring
  model than nodes-with-components for this developer's workflow.

## Consequences
- The facade and the ECS must stay consistent — a thin, well-tested mapping layer.
- We may build on an existing archetype ECS (e.g. `hecs`) initially and replace
  it with a bespoke one only if profiling demands. The facade hides which.
