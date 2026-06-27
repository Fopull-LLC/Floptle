# ADR-0008 — Engine-native automatic object pooling

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The developer almost always needs object pooling (projectiles, VFX, enemies) and
hates wiring it up by hand every time. It should be a first-class engine tool.

## Decision
Provide a built-in pooling system in `floptle-core::pool`: declare a pool for a
prefab/type, then **take** an instance and **return** it — the engine handles
allocation, reuse, growth, and reset. Particles and transient spawns use it by
default.

## Why
- Removes repetitive, error-prone setup the developer explicitly dislikes.
- Reusing instances avoids per-spawn heap churn — directly serves "fast."
- Pairs naturally with the node/prefab model and the VFX system.

## Design sketch
- `Pool<T>` / `PrefabPool` with capacity + auto-grow policy.
- `take()` returns a **handle/guard**; on drop (or explicit `return`) the instance
  resets and goes back to the free list.
- A registry so scripts can request a pool by prefab id: `pool.take("Arrow")`.
- Optional pre-warm to avoid first-use hitches.

## Alternatives considered
- **Leave it to the user** — the status quo the developer wants to escape.
- **Rely on the allocator/GC** — no GC in Rust, and raw allocation churn is what
  we're avoiding.

## Consequences
- Requires clear reset semantics per pooled type (what state is cleared on return).
- RAII guard handles make "forgot to return it" hard to get wrong.
