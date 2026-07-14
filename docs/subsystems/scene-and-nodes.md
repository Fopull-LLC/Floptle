# Scene & Nodes (`floptle-core`)

The authoring model: a friendly **Node tree** you build and script, sitting on a
data-oriented **archetype ECS** that actually runs. One scene, two views.

> Decision & rationale: [`../decisions/0005-scene-model-ecs-node-hybrid.md`](../decisions/0005-scene-model-ecs-node-hybrid.md).
> Scripting: [`../decisions/0003-scripting-lua.md`](../decisions/0003-scripting-lua.md) ·
> Open-in-VSCode: [`../decisions/0011-vscode-integration.md`](../decisions/0011-vscode-integration.md) ·
> Pooling: [`../decisions/0008-object-pooling.md`](../decisions/0008-object-pooling.md).
> Siblings: [`./particles-vfx.md`](./particles-vfx.md) · [`./physics.md`](./physics.md) ·
> [`./ui.md`](./ui.md) · [`./animation.md`](./animation.md). Where it sits:
> [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §2.

## Two views of one world

There is exactly one source of truth — the ECS. The Node tree is a *facade*: an
ergonomic projection for the human and for Lua. Nothing is duplicated; a Node is
not an object that "owns" components, it's a **handle**.

```
  AUTHORING VIEW (facade)            RUNTIME VIEW (archetype ECS)
  ───────────────────────            ────────────────────────────
  Player  ── Node(id=7)              archetype [Transform|Hierarchy|MeshRenderer|Script]
   ├─ Sword ── Node(id=8)              row 7:  T  H  M  S
   └─ Cam   ── Node(id=9)            archetype [Transform|Hierarchy|Camera]
                                       row 9:  T  H  C
  "add component"  ───────────────►  ECS insert (moves entity to new archetype)
```

- A **Node** = an entity id + a `Transform` + a `Hierarchy` component. Nothing more.
- **"Add component"** in the inspector = an ECS insert (the entity migrates to a
  new archetype). "Remove" = an ECS remove.
- A **Script** is just a component (`Script`) holding a Lua ref — see below.

Systems iterate the packed archetype arrays; they never walk the node tree. The
tree exists for authoring, serialization, and the scripting API, not for the hot
loop.

## Component catalog (built-ins)

Lean and curated — this is "not Unreal." The facade exposes these; games add their
own components from Rust crates or carry game state in scripts.

```rust
struct Transform { local: Affine3, /* pos, rot, scale; world cached/dirty-flagged */ }
struct Hierarchy { parent: Option<NodeId>, children: SmallVec<[NodeId; 4]> }

struct MeshRenderer { mesh: AssetId, material: AssetId, visible: bool }   // ./renderer
struct Camera       { proj: Projection, clear: ClearMode, active: bool }
struct Light        { kind: LightKind, color: Vec3, intensity: f32 }      // Dir|Point|Spot
struct ParticleEmitter { effect: AssetId, playing: bool, auto_play: bool } // ./particles-vfx.md
struct Collider     { source: ColliderSource, kind: Body, collidable: bool } // ./physics.md
struct UIElement    { widget: UiWidgetId, anchor: Anchor, rect: Rect }    // ./ui.md
struct Script       { source: AssetId, env: LuaEnvId, enabled: bool }     // 0003
```

Built-ins stay small on purpose: `Transform`/`Hierarchy` are the spine; the rest
are thin pointers into the subsystem that owns the heavy data (mesh, material,
effect, collider source). The catalog *links out*, it doesn't absorb.

## Scripts & lifecycle

A `Script` component attaches a Lua file to a node. Each instance gets its own
sandboxed environment with `self` bound to the owning node. The engine calls a
small, fixed set of hooks (ADR-0003):

```lua
function on_ready()                 -- once, after the node + components exist
function on_update(dt)              -- every render frame (variable dt)
function on_fixed_update(fixed_dt)  -- every physics step (see ./physics.md)
function on_event(name, payload)    -- engine/gameplay/anim/collision events
```

Scripts talk to the engine only through a **curated API surface** (ARCHITECTURE
§7) — no raw ECS pointers, no unsafe handles. The shape:

```lua
self.transform          -- get/set local transform of the owning node
self:get("Light")       -- borrow a component on self (typed accessor)
self:add("Collider", {kind="Character"})   -- ECS insert via the facade
scene.find("Player")    -- query the node tree by name/tag
scene.spawn("Arrow", t) -- instantiate a prefab (pool-backed; see below)
node:destroy()          -- despawn (pool-return if pooled)
input.down("Jump")      -- ./input
vfx.play("360Slash", t) -- ./particles-vfx.md
anim.crossfade(self, "Attack", 0.08)  -- ./animation.md
events.emit("HitLanded", { dmg = 12 })
```

**Hot-reload (ADR-0003).** Saving a `.lua` file re-runs it in a *fresh* environment;
`on_ready` fires again, persistent node/component state is preserved, transient
script-locals reset. A failed reload keeps the old environment live and surfaces the
error in the editor console — you never lose the running scene to a typo.

**Open in VSCode (ADR-0011).** The inspector's Script field has an "edit" affordance;
clicking it (or double-clicking the script in the hierarchy) shells out:

```
code <projectRoot> --goto <scriptFile>:<line>
```

so VSCode opens *rooted at the project* with that file focused — same flow used for
`.flsl` shaders.

## Scene & prefab serialization (RON)

Scenes serialize to **RON** (like the rest of the engine), diffable and
hand/AI-editable. A scene is a flat list of nodes; hierarchy is by id.

```ron
// scenes/arena.ron
Scene(
    name: "Arena",
    nodes: [
        Node( id: 1, name: "Player", parent: None,
            transform: (pos: (0, 0, 0), rot: (0, 0, 0, 1), scale: (1, 1, 1)),
            components: [
                MeshRenderer( mesh: "models/hero.glb#Mesh", material: "mat/hero.ron" ),
                Collider( source: Capsule(r: 0.4, h: 1.8), kind: Character, collidable: true ),
                Script( source: "scripts/player.lua" ),
            ],
        ),
        Node( id: 2, name: "MainCam", parent: Some(1),
            transform: (pos: (0, 1.6, -4), rot: (0, 0, 0, 1), scale: (1, 1, 1)),
            components: [ Camera( proj: Perspective(fov: 60.0), active: true ) ],
        ),
        Prefab( id: 3, name: "TorchA", parent: None, source: "prefabs/torch.ron",
            transform: (pos: (5, 0, 2), rot: (0, 0, 0, 1), scale: (1, 1, 1)),
            overrides: { "Light.intensity": 2.5 },   // sparse field overrides
        ),
    ],
)
```

**Prefabs** are reusable node subtrees stored in `prefabs/*.ron` — a mini-scene with
one root. Placing a prefab in a scene writes a `Prefab` reference, not a copy: the
subtree is expanded at load, then **sparse overrides** (a flat `"Component.field"`
map) are applied on top. Edit the prefab once, every instance updates; per-instance
tweaks live as overrides. Pooled spawns (below) are prefab instances too.

## Editor UX

```
┌ Hierarchy ───────┐  ┌ Inspector ─────────────────────┐
│ ▾ Arena          │  │ Player                          │
│  ▾ Player    ◀───┼──┤ Transform   pos[0 0 0] rot[…]   │
│     Sword        │  │ MeshRenderer  mesh ▸ material ▸  │
│   MainCam        │  │ Collider    [Capsule ▾] r 0.4    │
│   TorchA (pf)    │  │ Script  player.lua   [edit ↗]    │
└──────────────────┘  │ [ + Add Component ▾ ]            │
                      └─────────────────────────────────┘
```

- **Hierarchy panel:** the node tree. Right-click to add/duplicate/delete; prefab
  instances render with a badge.
- **Inspector:** per-component field editors; **Add Component** is a searchable
  dropdown of the catalog (an ECS insert). The Script row carries the VSCode jump.
- **Drag-to-reparent:** dragging a node onto another rewrites `Hierarchy` for both
  and keeps world transform stable (re-derives local from the new parent).

## Data flow: keeping the facade in sync

The facade is a thin mapping layer (ADR-0005). Operations are *commands* against the
ECS; the tree is rebuilt/patched from `Hierarchy`, never held as a parallel truth.

```
 editor/script action ─► facade command ─► ECS mutation ─► (events) ─► systems
   add component            insert<C>         archetype move
   reparent                 patch Hierarchy   (no archetype move)
   destroy                  despawn / pool-return
```

- **Create:** `scene.spawn(prefab, t)` → ECS spawn + component inserts → `on_ready`.
- **Destroy:** `node:destroy()` → run despawn → ECS remove (children cascade).
- **Pooling (ADR-0008).** Transient spawns — arrows, hit-sparks, enemies — route
  through the pool registry instead of fresh alloc + free. `scene.spawn("Arrow")`
  with a declared pool does `pool.take("Arrow")`; `node:destroy()` does
  `pool.give_back(handle)`, which **resets** the instance (transform, script state,
  velocity) and parks it on the free list. No per-spawn heap churn; see
  [`./object-pooling.md`](./object-pooling.md) for reset semantics. Whether a spawn
  is pooled is a property of the prefab, invisible to the calling script.

## Out of scope (at launch)

- **Deep prefab variant trees** (nested overrides-of-overrides, prefab inheritance
  chains). One level of subtree + sparse overrides only — keep it legible.
- **Multi-scene streaming / open-world chunk loading.** One active scene at a time.
  Scene *transitions* (load/unload, fade) are an editor/runtime concern, not a
  scene-graph feature — see [`../ARCHITECTURE.md`](../ARCHITECTURE.md) and the
  runtime crate.
- **A full undo-graph / ECS command journal as a public API** — the editor has undo;
  scripts mutate directly.

## Scene management (2026-07-14)

A project has an **entry scene** (Edit ⏵ Project Settings ⏵ Game → `project.ron`'s
`entry_scene`): the scene a build boots into, and the scene the editor opens on
project load — what you see is what ships. The same panel holds the game
**title** (names exported builds).

Runtime transitions are script-driven — `scene.load(name)` (plus
`scene.current()` / `scene.list()`), performed at a frame boundary: the world
swaps to the new scene, physics/animators/particles/audio rebuild, every
script's `start` re-fires. In the editor, Stop still restores the scene you
were editing (name and all). In multiplayer only the server switches; clients
follow via the wire protocol's scene epoch (docs/netcode-design.md §5.2b), and
late joiners land in the session's current scene from the Welcome handshake.
Full guide: docs/scripting.md §17.
