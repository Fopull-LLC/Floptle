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

## Layers & tags (2026-07-14)

Two per-node grouping primitives, live end-to-end:

- **`Layer(String)`** — a named collision/query layer. The project defines up
  to 32 (`project.ron` `layers`, "Default" implicit at bit 0) and stores the
  collision matrix as **exceptions** (`no_collide` name pairs; default =
  everything collides). Names resolve to bit indices once per Play through
  `floptle_core::Layers` — scenes/scripts never touch indices, so reordering
  the project list can't re-layer a scene, and an unknown (removed) name falls
  back to Default with a Console warning. Physics filters body-vs-collider
  pairs with `PhysicsWorld::matrix`; `raycast_colliders/_hulls` take the same
  `u32` mask. Lua: `node.layer` (get/set — a typo'd write ERRORS listing the
  project's layers), `raycast(..., { layers = {"Ground"} })`. Inspector: layer
  picker at the top of every node; Project Settings: layer list editor +
  matrix grid (renames follow through to the open scene per keystroke).
- **`Tags(Vec<String>)`** — free-form identity strings on any node. Lua:
  `node:hasTag/addTag/removeTag`, `node.tags`, `findTagged(tag)` (scene-order
  node handles). Inspector: tag chips + adder under the node name. No physics
  meaning — identity only.

Both serialize on `NodeDoc` (`layer` by name, skipped when Default; `tags`
skipped when empty), ride the node clipboard, and replicate with networked
spawns (spawn docs carry them). Dynamic bodies re-resolve their layer every
frame (`sync_dynamic_params`), so `node.layer = "Ghosts"` takes effect live;
static colliders bake their bit at sim build (layer edits rebuild the sim).

## Cross-scene copy/paste, string params, vectors, collision events (2026-07-14)

- **Node clipboard → OS clipboard**: `copy_selected` mirrors the copied
  `Vec<NodeDoc>` onto the system clipboard as tagged RON
  (`//floptle-nodes-v1` + pretty RON); `paste()` prefers a valid tagged OS
  clipboard over the in-app one — so copy → open another scene, another
  editor window, even another project → paste just works (and copied nodes
  are shareable as plain text).
- **String params**: a plain string default (`destination = "hub"`) is a
  per-instance text tunable — `ScriptInst.strs` / `ScriptDoc.strs`, Inspector
  text field, seeded/synced like numerics, two-way (`ParamWrite::Str`).
- **Vectors**: `floptle-script/src/math_api.rs` — `vec3`/`vec2` userdata with
  operators + methods, `distance(a, b)` (vectors/tables/node handles),
  `node.pos` read/write as vec3. Anything vector-shaped (numeric x/y/z)
  is accepted anywhere a vector is.
- **Collision & trigger events**: `Sim::step_tick` accumulates solver contacts
  across substeps + tests body-vs-sensor and body-vs-body overlap
  (matrix-gated), diffs touching pairs into `TouchEvent`s
  (Enter/Stay/Exit, world-space point+normal, sensor flag). The editor
  dispatches each to BOTH nodes' scripts as
  `onCollisionEnter/Stay/Exit(node, other, hit)` /
  `onTriggerEnter/Stay/Exit`. `Trigger` component (+ `NodeDoc.trigger`, the
  Collider section's "trigger" switch) makes a Collidable a sensor: solver
  and raycasts pass through, events still fire. Static colliders now carry
  `eid` + `sensor` (`StaticTag`); `Contact` records its collider index.
  Prediction replays (`step_body_tick`) never produce events.

## Rigidbody modes (2026-07-14)

`RigidBody.mode: BodyMode` — the one dropdown replacing hand-frozen axes +
gravity toggles:

- **Dynamic** — simulated, as before.
- **Kinematic** — `Body.kinematic`: the step skips it entirely (near-zero
  cost); `Sim::sync_dynamic_params` drives its pose FROM the node transform
  each tick (origin-relative f64 → exact far out; on net clients the
  interpolated snapshot transform keeps the hull where players see it) and
  refreshes `PhysicsWorld::kin_hulls`. Dynamic bodies depenetrate from those
  hulls inside the solver's relaxation passes (matrix-gated) — moving
  platforms carry/push players — and `kin_contacts` feeds the touch-event
  diff. Live-switchable Dynamic ↔ Kinematic (`rig.kinematic = true`, or the
  Inspector; waking into Dynamic zeroes velocity).
- **Static** — no body at all: `Sim::build`/`add_body_for` bake an immovable
  collider in the body's shape (sphere/capsule/box, `StaticTag` with the
  node's eid + layer). Zero per-tick cost; touch events still name the node.
  Structural — the Inspector dropdown rebuilds the live sim
  (`cmd.rebuild_physics`); `remove_body` also drops eid-tagged colliders so
  net despawns clean up.

Serialized as `RigidBodyDoc.mode` (RON enum, omitted when Dynamic) — spawns,
clipboard, and replication carry it automatically. Inspector greys out
bounce/friction/gravity/locks for non-Dynamic modes.

## Trigger rigidbodies + Play-mode terrain safety (2026-07-14)

**Trigger + RigidBody**: a `Trigger` component on a rigidbody node sets
`Body.sensor` — the body integrates but skips ALL contact resolution (passes
through everything, nothing pushes back), stays out of `kin_hulls` (a
kinematic trigger never pushes) and out of the lent raycast hulls (rays skip
it, like static trigger colliders). Touch events: body-vs-body overlap
involving a sensor body reports `sensor: true` (trigger hooks), and a
dedicated `detect_touches` pass tests sensor bodies against SOLID static
colliders (the solver skipped them) so a trigger sweeping through a wall
still events. Static-mode rigidbody + Trigger bakes a sensor collider. The
Inspector's trigger checkbox lives on the Rigidbody component for body nodes
(the Collider section's checkbox remains for pure-collidable nodes); toggles
sync live via `sync_dynamic_params`. Test: `trigger_rigidbodies_are_sensors`.

**Terrain could be overwritten across scene switches** (real lost work):
terrain fields live OUTSIDE the scene doc (`terrain/<scene>.<id>.tfield`,
keyed by `scene_name`), and Stop restored the world BEFORE restoring the
pre-Play scene name — so after a mid-play `scene.load(...)`,
`restore() → adopt_terrain()` filled the editor scene's terrain nodes with
the PLAYED scene's fields, and the next save wrote them over the real
terrain. Fixes (all in concert):

- Stop restores `scene_name`/`scene_rel` BEFORE `restore()` runs.
- Play snapshots the live terrain fields (id-keyed) + texture palette
  (`Editor.play_terrains`); Stop restores them — unsaved sculpts now survive
  Play (they used to be silently re-read from disk), and no disk re-read can
  leak another scene's terrain in.
- `save_scene` refuses to run during Play (loud Console warning) — the world
  holds simulation state and possibly another scene entirely.
- Opening a scene from the editor while playing stops Play first, so the
  unsaved-changes prompt and its save operate on real edit state.

## Prefabs, subtree clipboard, spawn/destroy, multi-select (2026-07-14)

**Subtree capture is the shared foundation** (`Editor::subtree_docs` /
`spawn_docs` in scene_ops.rs): roots + all descendants serialize to a flat
`Vec<NodeDoc>` whose `parent` fields are in-list indices (`None` = root).
Children keep local transforms + bone attachments; roots bake their WORLD
transform. Copy / duplicate / paste now carry children (they used to capture
single flat nodes — pasting a parent silently lost its subtree), and
`delete_selected` removes whole subtrees instead of orphaning children to the
scene root. Also fixed: `spawn_node` dropped the `cast_shadow` opt-out.

**Prefabs** (`prefab.rs`, `PREFAB_EXT = ".prefab.ron"`): the same flat format
as a file — a saved clipboard IS a valid prefab (the loader tolerates the
`//floptle-nodes-v1` tag line). Create: drag Hierarchy node(s) into the
Assets panel (folder rows, grid background, tree empty space → prefabs/) or
right-click → "⬡ Save as Prefab". Instantiate: drag the asset into the
viewport (cursor placement), onto a Hierarchy row (spawns as child, authored
local offset), the empty Hierarchy area, or context menu "⬡ Add to scene"
(camera-front). Blue ⬡ icon; instantiate is blocked during Play (Console
points at `spawn()`).

**Lua runtime**: `spawn(prefab [, pos [, fn]])` queues a `SpawnRequest`
(host-side Rc queue, like spawnEffect); the editor drains after script passes
(`apply_script_spawns`), spawns the subtree (mtime-cached parse,
`Editor::prefab_cache`), registers meshes + `sim.add_body_for` per entity,
then invokes the callback with the new root's handle via
`ScriptHost::call_spawn_callback` (re-syncs the mirror first, flushes after —
same pattern as call_touch). `destroy(node)` / `node:destroy()` queue entity
indices; the drain despawns whole subtrees + bodies, routes net-tracked nodes
through `Session::despawn` on the server, and refuses client-side destroys of
replicated nodes. `net.spawn` also accepts prefab names/paths now (first root
only — replication is per-node). Scripts/animators/vfx/audio self-wire via
their per-frame query loops; prediction replays never run scripts, so no
double-spawn. Scene switches clear pending queues (`reset_instances`).

**Multi-select**: Hierarchy now matches the Assets browser — plain click =
single, Ctrl/Cmd = toggle, Shift = range over the VISIBLE row order (DFS,
collapsed subtrees skipped); the clicked row becomes primary. Assets: delete
runs on the whole multi-selection ("Delete N files" + listing confirm modal);
`delete_asset`/`do_delete_asset` cmds are now `Vec<String>`
(`Editor::delete_assets`, one tree rebuild).

**UX audit fixes** (same pass): `record()`/`push_history` no-op during Play
(play edits polluted undo + left phantom scene_dirty after Stop);
`set_active_camera` records undo; Networked/Collider/AnimController headers
use `component_header_no_copy` (their "Copy values" did nothing); folder
context menus unified tree+grid (`folder_menu`: New/Rename/Reveal/Delete —
grid tiles had NO menu, folders were un-renamable); file tile/row open+menu
logic deduped (`asset_open`/`asset_file_menu`); hierarchy menu gained Paste +
shortcut hints; viewport Add menu now uses the shared `node_new_menu` catalog
(was a stale 3-item copy); Hierarchy auto-scrolls to the selection primary
(`hier_scrolled` once-per-change); rename modal titled "Rename", preserves
compound extensions (`.prefab.ron`/`.vfx.ron` used to get mangled), and
follows `asset_selection`; layer delete is a two-step inline confirm; Edit
menu greys out selection-dependent items; Cube/Sphere/Blob tooltips;
"object"→"node" terminology.
