# Object Pooling (`floptle-core::pool`)

Pooling is **built into the engine**, not bolted onto every project. Declare a pool,
`take()` an instance, `return()` it (or just drop the guard) — the engine handles
allocation, reuse, growth, and reset.

> Decision & rationale: [`../decisions/0008-object-pooling.md`](../decisions/0008-object-pooling.md).
> Scripting: [`../decisions/0003-scripting-lua.md`](../decisions/0003-scripting-lua.md).
> Siblings: [`./scene-and-nodes.md`](./scene-and-nodes.md) ·
> [`./particles-vfx.md`](./particles-vfx.md). Where it sits:
> [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §6.

The developer hates wiring up pooling by hand for arrows, sparks, and enemies every
single time (ADR-0008). So it's a first-class `floptle-core` service: particles,
projectiles, and transient scene spawns use it **by default**, and games get the same
zero-ceremony API for their own types.

## The API

Two layers: a generic `Pool<T>` for any Rust type, and a `PrefabPool` specialization
that the node/prefab model and scripts use.

```rust
pub struct Pool<T: Poolable> {
    free: Vec<T>,            // parked, reset instances
    in_use: usize,           // accounting only (live handles aren't held here)
    cap: usize,              // current capacity (free.len() + in_use)
    grow: GrowPolicy,
}

pub trait Poolable: Default {
    fn reset(&mut self);     // wipe to a clean reusable state (see "Reset semantics")
}

impl<T: Poolable> Pool<T> {
    pub fn with_capacity(n: usize, grow: GrowPolicy) -> Self;  // pre-warms n instances
    pub fn take(&mut self) -> Handle<T>;     // pops free (or grows); never returns null
    pub fn give_back(&mut self, h: Handle<T>);  // explicit return (or let the guard drop)
    pub fn stats(&self) -> PoolStats;        // { cap, in_use, free, peak }
}
```

**RAII guard.** `take()` returns a `Handle<T>` that derefs to the instance and, on
**drop**, returns it to the pool automatically — so "forgot to return it" is hard to
get wrong (ADR-0008). Explicit `give_back` exists for when you want to control timing.

```rust
pub struct Handle<T: Poolable> { /* pool ref + slot */ }
impl<T: Poolable> Drop for Handle<T> {
    fn drop(&mut self) { /* instance.reset(); push to free list; in_use -= 1 */ }
}
// usage:
{
    let mut arrow = arrows.take();   // borrowed from the pool
    arrow.fire(dir);
}                                    // <- dropped here: reset + returned automatically
```

A `PrefabPool` wraps `Pool<NodeInstance>`: `take()` spawns/reuses a prefab subtree
([`./scene-and-nodes.md`](./scene-and-nodes.md)) and hands back a node handle; drop or
`give_back` despawns-to-pool instead of freeing.

## Auto-grow, pre-warm, and reset

**Auto-grow policy** — what happens when `take()` hits an empty free list:

```rust
pub enum GrowPolicy {
    Fixed,            // capped: take() on empty fails loudly (or returns None) — hard budget
    Additive(usize),  // allocate N more instances
    Double,           // capacity *= 2 (amortized growth; default for gameplay pools)
}
```

`Double` is the default for transient gameplay spawns (amortized, rarely hits in
practice once warmed). `Fixed` is for hard budgets (e.g. "max 200 bullets") where you'd
rather drop a spawn than allocate mid-combat.

**Pre-warm.** `with_capacity(n, ..)` builds `n` instances up front so the *first* burst
(first arrow, first explosion) doesn't pay allocation + GPU-buffer + script-env cost
mid-frame. Pre-warm at scene load, off the hot path. This is the cure for first-use
hitches.

**Reset semantics (per type).** The single most important contract: what state is
cleared when an instance goes back. `reset()` must return the instance to "as if
freshly spawned." Defaults the engine ships for built-in pooled types:

| Pooled type        | Reset clears                                                        | Keeps (cheap to retain) |
|--------------------|--------------------------------------------------------------------|-------------------------|
| Projectile / node  | transform, velocity, script locals, timers, parent link, collider state | allocated buffers, prefab id |
| VFX instance       | particle SoA counts → 0, playhead → 0, emitter state, GPU instance slot | GPU buffers, curve tables |
| Generic `Poolable` | whatever `reset()` defines (`*self = T::default()` is the safe baseline) | —                       |

The rule: **clear identity and live simulation state; keep heap/GPU allocations.**
That retention is the whole point — reuse the expensive allocation, wipe the cheap
state.

## Registry (scripts request pools by prefab id)

A scene-scoped `PoolRegistry` maps a **prefab id** to its `PrefabPool`, so scripts
never construct pools — they ask for one by name. Pools are declared in the scene/
prefab data or auto-created on first use with sane defaults.

```rust
pub struct PoolRegistry { pools: HashMap<PrefabId, PrefabPool> }
impl PoolRegistry {
    fn declare(&mut self, id: PrefabId, cap: usize, grow: GrowPolicy, prewarm: bool);
    fn take(&mut self, id: PrefabId) -> NodeHandle;     // backs scene.spawn(...)
    fn give_back(&mut self, h: NodeHandle);             // backs node:destroy()
}
```

```lua
-- Lua (ADR-0003): same names, pool-backed and ceremony-free
local h = pool.take("Arrow")          -- reuse or grow; never a raw alloc
h.transform = self.transform
h:fire(self.aim)
-- ...later, on hit:
pool.give_back(h)                     -- reset + park; or just let it auto-return
```

Declared in RON next to the scene:

```ron
// pools/arena.pools.ron
Pools([
    Pool( prefab: "Arrow",   capacity: 64,  grow: Double,        prewarm: true ),
    Pool( prefab: "HitSpark", capacity: 32, grow: Additive(16),  prewarm: true ),
    Pool( prefab: "Grunt",   capacity: 24,  grow: Fixed,         prewarm: false ),
])
```

## Default integrations

Pooling is *invisible* in the common path — the systems that spawn a lot already use it:

```
 vfx.play("360Slash")  ─┐
 scene.spawn("Arrow")  ─┼─► PoolRegistry.take(prefab) ─► reuse free slot ─► live
 anim Vfx notify event ─┘                                       │
                                                                ▼ on finish / destroy
                                              reset() ─► park on free list (no free())
```

- **Particles / VFX** ([`./particles-vfx.md`](./particles-vfx.md)) — every effect
  *and* its particles come from pools; "spawning never churns the heap" is a stated
  rule of that subsystem.
- **Projectiles** — the canonical case: `pool.take("Arrow")` / `give_back`.
- **Transient scene spawns** ([`./scene-and-nodes.md`](./scene-and-nodes.md)) —
  `scene.spawn` / `node:destroy()` route through the registry when the prefab has a
  pool, so enemies, pickups, and debris reuse instances transparently.

## Editor UX (minimal)

A small **Pools** panel — declare and watch, nothing fancy (this is "not Unreal").

```
┌ Pools (Arena) ─────────────────────────────────┐
│ Prefab     Cap  In-use  Free  Peak  Grow  Warm  │
│ Arrow       64    12      52    41   ×2    [✓]   │
│ HitSpark    32     0      32     8   +16   [✓]   │
│ Grunt       24    24       0    24   fix   [ ]   │ ◀ Fixed pool at cap (highlighted)
│ [ + Declare Pool ▾ ]                            │
└─────────────────────────────────────────────────┘
```

- Declare a pool: pick a prefab, set capacity / grow policy / pre-warm.
- Live `in-use` / `free` / `peak` counts at runtime — peak tells you the *real*
  capacity to pre-warm to. A `Fixed` pool sitting at cap is highlighted (you're
  dropping spawns).

## Why this is fast

- **No per-spawn heap churn:** `take()` is a `Vec::pop` + a `reset()`; `give_back` is a
  `push`. No allocator, no `Drop` of the underlying buffers, no GPU buffer re-create on
  the hot path.
- **Pairs with the data-oriented ECS** (ADR-0005): pooled instances keep their ECS rows
  / GPU slots warm and contiguous, so reuse stays cache-friendly — pooling and the
  archetype layout reinforce each other.
- **Pre-warm kills first-use hitches**, so frame-time stays flat during the first
  volley or explosion.

## Out of scope

- **Cross-process / networked pools** — pools are local to a running instance; the
  network layer ([`../ARCHITECTURE.md`](../ARCHITECTURE.md) §10) replicates *spawn
  events*, and each peer pools locally.
- **A generalized garbage collector** — Rust has no GC and we don't want one (ADR-0008);
  pooling is explicit reuse of a known type, not automatic reclamation of arbitrary
  graphs.
- **Auto-shrink heuristics** beyond a manual `trim_to(peak)` — pools don't silently give
  memory back mid-session; you size them and pre-warm.
