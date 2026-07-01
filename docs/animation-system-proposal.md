# Floptle Animation System — Design Proposal

**Status:** proposal / decision document
**Author:** lead engine architect (synthesis of four research reports)
**Scope:** skeletal (glTF/Blender) + in-engine timeline/cutscene animation, controllers, layers, Lua control
**Grounded against:** the live Floptle workspace as of this writing (crate list, `floptle-core::Transform`, the `floptle-script` change-map bridge, the `floptle-editor` Play loop, and the empty `floptle-anim` stub)

---

## 1. Executive summary

Floptle should grow **one animation runtime with two front-ends**: a *skeletal* pipeline (Blender clips imported with the glTF, driven by an **AnimationController** state machine with layers) and a *property/cutscene* pipeline (an in-engine **Timeline** that keys node transforms + component fields and fires Lua code events). Both front-ends already have a home: `floptle-anim` exists as a stub whose planned modules (`clip`/`skeleton`/`state`/`blend`/`events`) exactly match this design, and the engine already ships an ECS-↔-Lua **change-map bridge** (`body_changes`, `model_changes`, `visible_changes`, `component_changes` drained after `ScriptHost::run` at `floptle-script/src/lib.rs:687–718`) that is the natural way to apply animated writes to the ECS. Animation integrates as **new component types + new systems + new asset types**, orthogonal to what exists — no refactor of the ECS, scripting, or render core is required.

The four big decisions, made and defended below:

1. **GPU vertex skinning, not CPU.** Floptle's `GpuMesh` is an upload-once, immutable buffer and the raster pipeline is already instanced with a per-instance stream. CPU skinning would re-upload every vertex every frame and break that model; GPU skinning keeps the mesh static and uploads only ~20–120 `Mat4` per character. (CPU skinning is kept only as an editor-preview/thumbnail fallback.)

2. **Joints are a flat matrix array, not ECS entities.** The runtime holds a `Skeleton` (topologically sorted `Vec<Joint>`) shared across instances and computes a per-instance `Vec<Mat4>` palette. We do **not** spawn one ECS entity per bone. Bone-per-entity multiplies `World` churn by 20–120× per character and forces the general `Parent`/`world_transform` walk (which is `f64`/`DVec3`, sized for light-years, `transform.rs:16`) to do work that a tight `f32` parent-first loop does in microseconds. Joints live near the model origin, so `f32` is correct and cheaper.

3. **Per-frame evaluation lives in `floptle-anim` (+ a thin timeline core), run in the editor/runtime Play loop right after scripts and before physics.** Insertion point: immediately after `self.script_host.run(...)` (`floptle-editor/src/main.rs:6810`) and before `sim.advance(...)` (`:6860`). Ordering is **scripts → animation/timeline → physics**: scripts set intent, animation overrides the properties/poses it owns, physics integrates last. Evaluation is allocation-free (preallocated scratch poses, binary-search + lerp), assets are compiled once and shared via `Arc`.

4. **Lua drives everything through the existing change-map bridge — never by touching the ECS directly.** A new `anim_changes` map (parallel to `component_changes`) carries `play`/`crossfade`/`setFloat`/`setLayerWeight`; a new `TimelineDirector` control map carries `play`/`seek`/`speed`. Both are drained after `run` and applied before the animation system's own `update(dt)`, so a parameter set this frame affects this frame's pose. Timeline property writes and skeletal-notify/timeline **events** dispatch back into Lua via a small `ScriptHost::call_on(entity, script, func, args)` — the missing "call a named function on a script from Rust", which the code already does internally for `start`/`update`.

**Recommended crate layout:** put the *pose/skeletal* runtime in the existing **`floptle-anim`**, and the *property/cutscene sampler* in a new **`floptle-timeline`** crate that depends only on `floptle-core`. Rationale: the timeline sampler is pure data + `floptle-core`, so the headless `floptle-runtime` can play cutscenes without pulling in `floptle-assets`/render; it mirrors the existing split (`floptle-scene` = DTO, `floptle-physics` = pure sim). Both feed one shared `Pose`/change-map vocabulary and one event bus.

---

## 2. Unified data model

Everything reduces to a **`Pose`** plus a **change map**. For skeletal animation a pose is a `Vec<TransformTRS>` indexed by joint; for the timeline a "pose" is a set of `(entity, field) → f64` writes. States, transitions, layers, and blending all operate on poses; the final result is applied to the GPU (skinning palette) and/or the ECS (property writes) through the existing bridge.

### 2.1 ECS components (plain data, live in `floptle-core` beside `Scripts`/`Visible`)

Serializable, `Clone`, no GPU/Lua deps — exactly the discipline of `Matter`/`Material`/`Visible`/`Scripts`. Heavy data lives in assets keyed by path (the same indirection `Matter::Mesh { asset_path }` uses).

```rust
/// Drives a skeleton (and/or blend-tree) via a controller asset.
/// The node's Mesh supplies the Skeleton; the controller supplies clips + state machine.
#[derive(Clone, Debug, PartialEq)]
pub struct Animator {
    pub controller: String,                       // "anim/Hero.controller.ron"
    pub param_overrides: Vec<(String, ParamValue)>, // per-instance tuning, like ScriptInst.params
    pub enabled: bool,
    pub apply_root_motion: bool,                  // feed root-bone delta into physics/transform
}

/// Attaches a cutscene/timeline clip to a node and controls playback.
/// Usually placed on an Empty "Director" node; a reusable local-motion clip
/// (bound to Self_) is placed on the node it animates.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineDirector {
    pub asset: String,        // "animations/door_open.timeline.ron"
    pub autoplay: bool,       // start on Play (cutscene) vs. wait for Lua
    pub state: PlayState,     // playing, t, speed, prev_t, wrap  (runtime cursor)
}
```

Scene serialization mirrors the existing DTO exactly: a `NodeDoc` gains optional `animator: Some((controller: "...", ...))` and `director: Some((asset: "...", ...))`, `#[serde(default, skip_serializing_if = ...)]`, just like `material: Some((...))` and `scripts: [...]` today.

### 2.2 Skeleton + skeletal clip (imported from glTF, cached by asset path)

A `Skeleton` is **per-model** (shared by all instances). `AnimationClip`s reference joints **by index resolved once at import**, so a clip authored against one rig drives any structurally identical rig.

```rust
// floptle-anim/src/skeleton.rs
pub struct Skeleton {
    pub joints: Vec<Joint>,               // topologically sorted: parent index < child index
    pub name_to_joint: HashMap<String, u32>,
}
pub struct Joint {
    pub name: String,
    pub parent: Option<u32>,              // index into joints; None = root
    pub local_bind: TransformTRS,         // rest-pose local TRS (fallback for untouched joints)
    pub inverse_bind: Mat4,               // glTF inverseBindMatrices accessor
}

/// The joint-local currency of the whole subsystem. f32 (not core::Transform's f64):
/// joints sit near the model origin, so f32 is correct and half the size.
#[derive(Clone, Copy)]
pub struct TransformTRS { pub t: Vec3, pub r: Quat, pub s: Vec3 }

// floptle-anim/src/clip.rs
pub struct AnimationClip {
    pub name: String,
    pub duration: f32,
    pub tracks: Vec<Track>,               // sparse: only animated joints
    pub events: Vec<ClipEvent>,           // notifies (§5/§6) — SAME type as timeline events
    pub looped: bool,
}
pub struct Track { pub joint: u32, pub channel: ChannelData }
pub enum ChannelData {                    // struct-of-arrays; binary-search on `times`
    Translation { times: Vec<f32>, values: Vec<Vec3>, interp: Interp },
    Rotation    { times: Vec<f32>, values: Vec<Quat>, interp: Interp },
    Scale       { times: Vec<f32>, values: Vec<Vec3>, interp: Interp },
}
#[derive(Clone, Copy)] pub enum Interp { Linear, Step, CubicSpline }
```

**Why struct-of-arrays per track** (parallel `times`/`values`, not `Vec<Keyframe>`): sampling does one binary search over contiguous `times` then touches two `values` — cache-friendly, and the shape ozz-animation uses. CubicSpline stores 3× values (in-tangent/key/out-tangent) per the glTF spec; it's the cold path (Blender exports Linear for bones) but must be correct.

### 2.3 AnimationController asset — `anim/*.controller.ron`

The controller is the asset the user drops in the Assets window. It references clips by name, defines flat parameters, and holds **layers**, each with **its own state machine**, a blend mode, an optional joint mask, and a weight.

```rust
pub struct AnimController {
    pub parameters: Vec<ParamDef>,        // name + kind + default
    pub layers: Vec<Layer>,               // index 0 = base; higher = on top
}
pub enum ParamKind { Bool, Float, Int, Trigger }

pub struct Layer {
    pub name: String,
    pub blend: LayerBlend,                // Override | Additive
    pub default_weight: f32,              // 0..1 (Lua can drive live)
    pub mask: Option<JointMask>,          // None = whole skeleton
    pub states: Vec<State>,
    pub transitions: Vec<Transition>,     // this layer's own state machine
    pub default_state: u32,
}
pub enum LayerBlend { Override, Additive }
pub struct JointMask { pub include: Vec<String>, pub recursive: bool } // bone names → per-joint f32 vector at compile

pub struct State { pub name: String, pub motion: Motion, pub speed: f32 }
pub enum Motion {
    Clip(ClipRef),
    Blend1D { param: String, children: Vec<(f32, ClipRef)> },          // threshold-sorted
    Blend2D { x: String, y: String, children: Vec<([f32;2], ClipRef)> }, // triangulated
}
pub struct ClipRef { pub path: String }   // "anim/clips/Run" or "models/hero.glb#Run"

pub struct Transition {
    pub from: TransFrom,                  // State(u32) | AnyState
    pub to: u32,
    pub conditions: Vec<Condition>,       // ALL must hold (AND)
    pub duration: f32,                    // crossfade seconds
    pub exit_time: Option<f32>,           // 0..1 normalized; None = interrupt immediately
    pub priority: i32,
}
pub struct Condition { pub param: String, pub test: Test }
pub enum Test { If, IfNot, Greater(f32), Less(f32), Equals(i32), NotEquals(i32) }
// If/IfNot apply to Bool & Trigger; a Trigger auto-consumes when its transition fires.
```

### 2.4 Timeline (cutscene) asset — `animations/*.timeline.ron`

Same serde/RON DTO style as `floptle-scene`. A track binds to a node **by name-path** (entity indices are not load-stable, the same reason `Matter::Terrain` carries a `u32 id`), then drives a typed body: a **property curve**, a **code event**, a **clip segment** (drives an `Animator`), or a **sub-timeline**.

```rust
pub struct TimelineDoc {
    pub name: String,
    pub duration: f32,
    pub fps: f32,                         // editor snap grid; sampling is continuous seconds
    pub wrap: Wrap,                       // Hold | Loop | PingPong
    pub tracks: Vec<TrackDoc>,
    pub markers: Vec<MarkerDoc>,          // named ruler labels; also seek() destinations
}
pub struct TrackDoc {
    pub target: Binding,                  // Path("Player/Rig/Sword") | Self_ | Lighting
    pub muted: bool,
    pub weight: f32,                      // 0..1 authority when blending onto the ECS
    pub body: TrackBody,
}
pub enum TrackBody {
    Property { channel: Channel, keys: Vec<KeyDoc> },  // one scalar lane per axis
    Event    { keys: Vec<EventKeyDoc> },               // fire Lua callbacks
    Clip     { keys: Vec<ClipKeyDoc> },                // drive an Animator state on the target
    SubTimeline { asset: String, keys: Vec<SubKeyDoc> },
}
/// Each variant is ONE scalar lane; a vec3 like translation is three tracks
/// (PosX/PosY/PosZ) so each axis has independent keys/easing (Godot/Unity curve model).
pub enum Channel {
    PosX, PosY, PosZ, RotYaw, RotPitch, RotRoll, ScaleX, ScaleY, ScaleZ,   // Transform
    Visible,                                                                // Visible
    MatColorR, MatColorG, MatColorB, MatEmissiveR, MatEmissiveG, MatEmissiveB,
    MatEmissiveStrength, MatAlpha, MatRimStrength,                          // Material
    LightIntensity, LightRange, LightColorR, LightColorG, LightColorB,     // PointLight
    KeyIntensity, KeyColorR, KeyColorG, KeyColorB,                         // scene key Light
    CamFovY,                                                                // Camera
}
pub struct KeyDoc { pub t: f32, pub v: f64, pub interp: Interp2, pub tan_in: Option<[f32;2]>, pub tan_out: Option<[f32;2]> }
pub enum Interp2 { Step, Linear, Smooth /*Catmull-Rom default*/, Bezier, EaseIn, EaseOut, EaseInOut }
pub struct EventKeyDoc { pub t: f32, pub action: EventAction, pub fire_on_seek: bool }
pub enum EventAction {
    CallScript { script: String, func: String, args: Vec<Arg> },   // reuses getscript path
    Emit       { event: String, args: Vec<Arg> },                  // global event bus
    SwitchCamera { node: String },
    SetAnimState { state: String },                                // drives the target's Animator
}
```

Note `KeyDoc::v` is **`f64`** so `PosX/Y/Z` keep large-world precision to match `Transform::translation: DVec3` (`transform.rs:16`). Timeline events reuse the **same `ClipEvent`/event-bus machinery** as skeletal notifies, so a script's `on_event("slash_hit", ...)` handles both uniformly.

### 2.5 How they relate

```
                 assets (RON + glTF)                         ECS components
  models/hero.glb ──import──► Skeleton + AnimationClips  ◄── Animator { controller }
  anim/Hero.controller.ron ─► AnimController ───────────────┘   (compiled → CompiledController, Arc-shared)
  animations/intro.timeline.ron ─► TimelineDoc ────────────◄── TimelineDirector { asset }
```

- **Animator** → resolves its skeleton from the node's Mesh, its clips + state machine from the controller; produces a **skinning palette** (GPU) each frame.
- **TimelineDirector** → samples property curves into **change-map writes** (ECS) and fires **events** (Lua); a `Clip`/`SetAnimState` track hands off to that node's **Animator**.
- Both share: the `Pose`/`TransformTRS` vocabulary, the event bus, and the change-map apply path.

---

## 3. Runtime & performance

### 3.1 The per-frame pipeline

Inserted in the Play loop **after scripts, before physics** — concretely between `floptle-editor/src/main.rs:6810` (`script_host.run`) and `:6860` (`sim.advance`). Drains sit beside the existing `take_model_changes` (`:6832`) / `take_body_changes` (`:6854`) drains:

```
if self.playing {                                    // main.rs:6756
    // ... set_bodies / set_input / set_colliders / set_materials (existing) ...
    self.script_host.run(&mut world, &dir, sdt, self.play_t);      // :6810 (existing)

    // 1. apply Lua anim/director commands queued THIS frame (so they affect this frame)
    self.anim.apply_cmds(self.script_host.take_anim_changes());
    self.director.apply_cmds(self.script_host.take_director_changes());

    // 2. advance the timeline/director: sample property curves + collect events
    self.director.advance(&mut world, sdt);           // NEW

    // 3. advance skeletal animators: sample → blend layers/states → pose → palette
    self.anim.advance(&mut world, self.director.take_anim_triggers(), sdt);  // NEW

    // 4. union property writes and apply ONCE (director wins ties over scripts)
    let mut props = self.script_host.take_component_changes(); // refactor: expose (was drained inside run)
    props.extend(self.director.take_prop_changes());
    for ((eid, comp, field), v) in props { apply_component_field(&mut world, eid, comp, field, v); }

    // 5. dispatch fired events (timeline keys + skeletal notifies) back into Lua
    for ev in self.director.take_fired().chain(self.anim.take_notifies()) { self.dispatch_event(ev); }
    for cam in self.director.take_camera_switches() { self.set_active_camera(cam); }

    // ... existing model/material/visible drains, body_changes → sim, sim.advance :6860 ...
}
```

**Precedence, made explicit and documented: scripts → timeline → physics.** Scripts set intent; the timeline overrides keyed properties (a cutscene reliably owns what it animates, even on a node a script also writes); physics integrates last so a keyed kinematic transform still resolves collisions. This is a one-line ordering choice, not new machinery.

> **Refactor note (small but load-bearing):** today `component_changes` is drained and applied *inside* `ScriptHost::run` (`floptle-script/src/lib.rs:710–718`). To union script + timeline writes into one apply pass, expose a `take_component_changes()` and move the apply to the editor loop (step 4). This also gives deterministic script-vs-timeline precedence in one place.

### 3.2 Skeletal evaluation (`anim.advance`)

Per **enabled** animator, per layer (base → top):

```
1. TRANSITION SELECT   (cheap: only the current state's transition bucket + AnyState bucket)
     if no crossfade active:
        for t in transitions_from[current] (+ any_state), priority-ordered:
           if t.exit_time.map_or(true, |x| phase >= x) && conditions_hold(t, params):
              begin_crossfade(t); consume_triggers(t); break        // first match wins
2. ADVANCE PHASE       phase += dt * speed / duration   (wraps if looped)
3. SAMPLE POSE         sample current state → scratch_a
                       if crossfade: sample target → scratch_b; blend(a, b, smoothstep(k)) → a
                       (blend tree samples ≤2 clips for 1D, ≤3 for 2D — never all children)
4. FIRE NOTIFIES       edge-triggered on phase crossing (§6) → notify queue
5. COMPOSITE           fold this layer into the accumulator pose (§5 layer math)
```

Then once per instance:

```
skinning_matrices(skeleton, final_pose):        // parent-first, single pass (joints topo-sorted)
    for j in 0..n:
        local = Mat4::from_scale_rotation_translation(pose[j].s, pose[j].r, pose[j].t)
        world[j] = match parent(j) { Some(p) => world[p] * local, None => local }
        palette[j] = world[j] * inverse_bind[j]     // ← the skinning matrix uploaded to GPU
```

`palette[j] = worldJoint * inverseBind` is exactly what the vertex shader multiplies each bone-weighted vertex by. The glTF skinning matrix **excludes the mesh node's model transform** — which is perfect, because the renderer already applies a per-instance camera-relative model matrix (`Transform::render_matrix`, `transform.rs:50`), so the existing `f64→f32` large-world trick carries over untouched.

**Sampling** a track: binary-search `times` (glam `slice::partition_point`), then `slerp` for rotation (shortest-arc, glam handles the sign flip), `lerp` for translation/scale, hold for Step. Cost scales with *animated tracks* (~30–150), not vertices.

### 3.3 Timeline evaluation (`director.advance`) — see §6 for the seek/scrub-correct algorithm.

### 3.4 Performance strategy (the "highly optimized" contract)

- **Compile once, run on indices.** `AnimController`/`TimelineDoc` (name-based RON) compile to a `CompiledController`/`Timeline` on load/hot-reload: parameter names → indices, condition params → indices, clip refs → `Arc<AnimationClip>`, `JointMask` bone-names → a per-joint `Vec<f32>` weight vector, Blend2D triangulation precomputed, transitions bucketed per source state. **Zero string work per frame.**
- **Only active work.** Per layer, evaluate ≤2 states (current + crossfade target); blend trees sample ≤2/≤3 clips; check only the current state's transition bucket. A layer at weight 0 is skipped entirely in the fold. Muted/finished directors are skipped in the `query` loop.
- **Zero per-frame allocation.** All pose/scratch/palette buffers are preallocated on the per-instance runtime and reused; sampling writes into fixed-size `Vec` sized to the joint count; events use monotonic cursors.
- **SoA params** (`Vec<f32>`/`Vec<i32>`/`Vec<u8>`) — cache-friendly, matching the ECS column philosophy.
- **Shared immutable assets.** `Arc<CompiledController>` / `Arc<AnimationClip>` / `Arc<Skeleton>` shared across all instances of a character; only the tiny per-instance runtime (params + layer cursors + scratch) is unique. Meshes sharing a rig sample once per frame.
- **GPU skinning** by default: one `queue.write_buffer` of the concatenated palette per frame (grow-on-demand storage buffer, same pattern as the existing `instance_buf`), one bind group.
- **LOD / off-screen skip:** freeze the palette (skip sampling) for distant/off-screen instances; drop update rate for far characters. A per-instance flag on the runtime.
- **Optional later wins:** a per-track sampling cursor (ozz-style) makes forward playback O(1) amortized instead of a binary search each frame; `rayon` across characters if you ever have hundreds; quantized keyframe compression (the SoA split already makes this a drop-in).

---

## 4. Skeletal import (extending `floptle-assets/src/gltf_import.rs`)

The `gltf` crate (v1.4.1, already a dependency) exposes everything; the API below was verified against docs.rs. Today `import()` **bakes each node's world transform into the vertices** and discards the node tree (`gltf_import.rs` header + the bake noted around the primitive loop). The current `ImportedModel` carries only `parts` + `textures` (confirmed in the struct at the top of the file).

**Touch-points (from Report 1, verified):**

- `gltf_import.rs` header (lines ~9–11) explicitly defers *skins* and *animations* — that's the extension point.
- `ImportedModel` (top of file) gains two optional fields behind a `SkinnedData` so unskinned models keep the existing fast bake path byte-for-byte:

```rust
pub struct ImportedModel {
    // ... existing: name, parts, textures, size, bounds ...
    pub skinned: Option<SkinnedData>,     // present only when the file has a skin
}
pub struct SkinnedData {
    pub skeleton: Skeleton,
    pub clips: Vec<AnimationClip>,
    pub skin_streams: Vec<SkinAttrStream>, // per-part: [u16;4] joints + [f32;4] weights
}
```

**Critical fix:** for **skinned** primitives, *skip the world-transform bake* — skinned vertices must stay in skin/mesh space and be posed by joint matrices. Gate the existing bake on "primitive has no `JOINTS_0`".

**The `gltf` calls (all confirmed on docs.rs 1.4.1):**

```rust
// Skeleton
let skin = doc.skins().next()?;                        // one skin per model (typical)
let reader = skin.reader(|b| Some(&buffers[b.index()].0));
let ibms: Vec<Mat4> = reader.read_inverse_bind_matrices()
    .map(|it| it.map(|m| Mat4::from_cols_array_2d(&m)).collect())
    .unwrap_or_else(|| vec![Mat4::IDENTITY; skin.joints().count()]);
let node_to_joint: HashMap<usize,u32> =
    skin.joints().enumerate().map(|(j,n)| (n.index(), j as u32)).collect();
for (j, node) in skin.joints().enumerate() {
    let (t, r, s) = node.transform().decomposed();     // decomposed TRS
    // parent = walk doc for the joint whose child list contains this node
}

// Clips
for anim in doc.animations() {
    for chan in anim.channels() {
        let joint = node_to_joint[&chan.target().node().index()];
        let interp = chan.sampler().interpolation();    // Linear | Step | CubicSpline
        let r = chan.reader(|b| Some(&buffers[b.index()].0));
        let times: Vec<f32> = r.read_inputs()?.collect();          // keyframe times
        match r.read_outputs()? {                                  // ReadOutputs
            ReadOutputs::Translations(it) => /* Vec3 */,
            ReadOutputs::Rotations(rot)   => /* rot.into_f32() → [f32;4] → Quat */,
            ReadOutputs::Scales(it)       => /* Vec3 */,
            ReadOutputs::MorphTargetWeights(_) => /* deferred */,
        }
    }
}

// Per-vertex skin attributes (per skinned primitive)
let joints  = prim_reader.read_joints(0);   // → [u16;4]
let weights = prim_reader.read_weights(0);   // → [f32;4]
```

Sort the skeleton topologically at import (`parent < child`) so the runtime hierarchy walk is a single forward pass. Clips cache next to the model and are referenced as `"models/hero.glb#ClipName"`. Recommended build/test: unit-test `import_skeleton`/`import_clips` against a Blender-exported `.glb`, then prove the CPU sample → palette path by CPU-transforming and drawing with the **existing** pipeline before adding any shader — correctness in isolation.

### 4.1 Render changes (`floptle-render`)

`Vertex` stays byte-identical (`mesh.rs:13–34`: `pos@0, normal@1, uv@2`, stride 32) — zero regression for static meshes. Skinned meshes add a **second vertex stream** and one bind group:

```rust
#[repr(C)] #[derive(Pod, Zeroable)]
pub struct SkinAttr { joints: [u16;4], weights: [f32;4] }  // stream 2
// @location(15) joints  : Uint16x4   (next free after the instance stream)
// @location(16) weights : Float32x4
// group(2) binding(0):  var<storage, read> joint_mats: array<mat4x4<f32>>;  // all palettes concatenated
// instance carries:     joint_base : u32   (offset into joint_mats)
```

The skinning pipeline is a **second `RenderPipeline`** sharing the globals + texture bind-group layouts, with a `vs_skinned` entry that computes `skin = Σ weights[i]*joint_mats[joint_base+joints[i]]`, `local_pos = (skin*pos).xyz`, then the *existing* model-matrix → view-proj path (fragment stage unchanged → full material/lighting reuse). N characters = 1 storage buffer + `joint_base` offset + N draws; the offset design leaves the door open to single-draw instanced skinning later.

---

## 5. Animation controllers + layers

### 5.1 The state machine

One state machine **per layer**. A `State` plays a `Clip` or a **blend tree** (1D by one param, 2D by two). Transitions are `from → to` with AND-ed conditions on parameters, a crossfade `duration`, an optional normalized `exit_time` gate, and a `priority`. `AnyState` transitions apply from any state (e.g. "attack from anywhere"). Triggers auto-consume when their transition fires. Parameters are a flat namespace of `Bool`/`Float`/`Int`/`Trigger` — the Mecanim model, deliberately minimal.

`.controller.ron` example (base locomotion + upper-body combat layer):

```ron
(
  parameters: [
    (name: "speed",    kind: Float,   default: Float(0.0)),
    (name: "grounded", kind: Bool,    default: Bool(true)),
    (name: "attack",   kind: Trigger, default: Trigger),
  ],
  layers: [
    ( name: "Locomotion", blend: Override, default_weight: 1.0, mask: None, default_state: 0,
      states: [
        (name: "Move", speed: 1.0, motion: Blend1D(param: "speed",
           children: [ (0.0, (path:"clips/Idle")), (2.0, (path:"clips/Walk")), (6.0, (path:"clips/Run")) ])),
        (name: "Jump", speed: 1.0, motion: Clip((path:"clips/Jump"))),
      ],
      transitions: [
        (from: State(0), to: 1, duration: 0.10, exit_time: None, conditions: [(param:"grounded", test: IfNot)]),
        (from: State(1), to: 0, duration: 0.15, exit_time: None, conditions: [(param:"grounded", test: If)]),
      ]),
    ( name: "Combat", blend: Override, default_weight: 0.0,
      mask: Some((include: ["Spine","Clavicle.L","Clavicle.R"], recursive: true)), default_state: 0,
      states: [ (name:"Idle", speed:1.0, motion: Clip((path:"clips/CombatIdle"))),
                (name:"Slash", speed:1.3, motion: Clip((path:"clips/Slash"))) ],
      transitions: [
        (from: AnyState, to: 1, duration: 0.05, exit_time: None, conditions: [(param:"attack", test: If)]),
        (from: State(1), to: 0, duration: 0.20, exit_time: Some(0.9), conditions: []),
      ]),
  ],
)
```

### 5.2 Layer / mask / weight — the pose-composition math

Layering happens in **local joint space, before the hierarchy walk** (matrix-lerping world transforms distorts limbs). Per layer `L`: sampled local pose `P_L[j]`, live weight `w_L`, compiled per-joint mask `m_L[j]∈[0,1]` (1.0 if no mask). **Effective weight** `a_L[j] = w_L · m_L[j]`.

Seed the accumulator from the base layer, then fold each higher layer in order:

**Override layer** (combat *takes over* masked joints):
```
Acc[j].t = lerp (Acc[j].t, P_L[j].t, a_L[j])
Acc[j].r = slerp(Acc[j].r, P_L[j].r, a_L[j])      // shortest-arc; nlerp for speed
Acc[j].s = lerp (Acc[j].s, P_L[j].s, a_L[j])
```
At `a=1` the layer owns joint `j`; at `0` the layer below shows through. Mask → only upper-body joints are affected; the legs keep locomotion.

**Additive layer** (combat *influences* locomotion — lean, recoil, breathing), relative to a reference pose `Ref_L`:
```
Acc[j].t += a_L[j] * (P_L[j].t - Ref_L[j].t)
Acc[j].r  = Acc[j].r * slerp(IDENTITY, Ref_L[j].r⁻¹ * P_L[j].r, a_L[j])
Acc[j].s *= lerp(1.0, P_L[j].s / Ref_L[j].s, a_L[j])
```
So a half-weight "lean" biases the run without replacing it. Quats re-normalize after every blend (slerp returns unit; explicit `.normalize()` after nlerp). For 3+ layers, fold pairwise by weight — but combat/locomotion realistically needs 1–2 active layers.

**Timeline layers** composite differently (they target ECS nodes, not joints): each resolved property write is lerped by the track `weight` against the node's current value and pushed into the change maps — so cutscene tracks and code writes coexist through the same drain. `weight=1.0` (default) is a plain overwrite, zero overhead.

This is the Unity/Mecanim layer model, kept to the minimal-but-complete subset; the `Layer + weight` structure upgrades cleanly to a full blend graph later (the lesson from bevy_animation) without reshaping the runtime.

### 5.3 Root motion

If `Animator.apply_root_motion`, extract the root joint's per-frame local delta (do **not** apply it to the pose), convert to a world delta via the node's transform, and feed it to physics as a velocity/translation through the existing `body_changes` bridge — so an imported run clip actually moves the character controller.

---

## 6. Cutscene timeline + code events

### 6.1 The timeline model

A `TimelineDirector` (§2.1) references a `.timeline.ron` (§2.4). The compiled `Timeline` (sorted key arrays + resolved bindings) lives in a `Director` registry keyed by asset path — the ECS holds only the tiny director (asset path + play cursor), the same discipline as `Matter::Mesh { asset_path }`. The `Director` mirrors `ScriptHost` structurally: it owns compiled timelines and drains change maps the editor applies; it writes **nothing** to the ECS directly.

### 6.2 Property writes reuse the existing bridge — with one small extension

A property track sampling `Transform`/`Material`/`PointLight`/`Visible` produces the **same `(entity, comp, field) → f64` writes** that `node:getcomponent(x).field = v` produces today, applied by the **same `apply_component_field`** path. **However:** `apply_component_field` (`floptle-script/src/lib.rs:1451`) currently handles only `PointLight` and `RigidBody` — **Transform, Visible, Material, and Camera are not reachable there yet** (verified: the `match comp` has exactly two arms + a `_ => {}`). So the genuine, small extension is adding those arms — used by both the timeline *and* by Lua (it makes `node:getcomponent("Transform")` writable too). `RotYaw/Pitch/Roll` lanes are collected per entity and recombined into one `Quat` write (`Quat::from_euler(YXZ, …)`), matching how the Lua `node.yaw/pitch/roll` setter already round-trips Euler.

### 6.3 Seek/scrub-correct event firing (the correctness core)

Property state is **idempotent** — re-sampling at any `t` is free and always right, so scrubbing just re-derives the visual. Events are **effectful** — they must fire *exactly once* when the playhead passes their time in the direction of travel, and scrubbing must neither spam nor miss them. Implement as a half-open interval test on `(prev_t, now)`:

```
advance(dt):
    prev = state.t
    raw  = prev + state.speed*dt                       // speed may be negative
    (now, crossings) = wrap(raw, duration, state.wrap)  // Hold clamps; Loop/PingPong split into legs

    # property/clip tracks: sample at `now`
    for track in tracks (not muted):
        Property → emit_prop(ent, channel, sample_curve(keys, now), track.weight)
        Clip     → anim_triggers.push((ent, active_segment(keys, now).state))

    # event tracks: fire keys CROSSED between prev..now, per leg, in travel order
    for track in event tracks (not muted):
        for (lo, hi, fwd) in legs(prev, now, crossings):
            for k in keys with (lo <= k.t < hi) if fwd else (lo < k.t <= hi):
                if seeking && !k.fire_on_seek: continue   # scrub only re-fires state-establishing events
                fired.push(FiredEvent { action: k.action, ent, forward: fwd })
    state.prev_t = prev; state.t = now
```

| Operation | Property tracks | Event tracks |
|---|---|---|
| Play forward | sample at `now` | fire keys in `[prev, now)` |
| Reverse (`speed<0`) | sample at `now` | fire keys in `(now, prev]`, reversed |
| Scrub / `seek()` | jump + sample once | fire **only** `fire_on_seek=true` keys in the jumped range (gameplay/SFX suppressed) |
| Loop / PingPong | modulo/triangle wrap | events fire once **per leg** (looping ambient cutscene re-fires SFX each loop) |

`fire_on_seek` is the per-key Unity-Timeline distinction between re-evaluated "clip" state (e.g. `SwitchCamera`, "enable this light") and play-only "signal" emitters (SFX, gameplay) — made explicit per key, which is easier to author than per track.

### 6.4 Event dispatch back to Lua

Firing does **not** call Lua from inside the timeline/anim crates (they carry no Lua dep — preserving the crate boundary). It appends to a `fired` queue the editor drains after `run` and dispatches via a new **`ScriptHost::call_on(entity, script, func, args)`** — a ~15-line addition that grabs the instance's stored `env` and calls the function exactly as the host already calls `start`/`update` (see the per-instance env + call path in `floptle-script`). `Emit{event}` pushes to a global event bus (the engine already has `floptle_core::Events<T>` double-buffered) that scripts subscribe to via a new `on_event(name, fn)` global. Skeletal notifies flow through the **same** `fired` queue, so gameplay code written for animation notifies works unchanged when a cutscene triggers the same beat.

---

## 7. Lua scripting API

Follows the established conventions: component handles via `node:getcomponent(...)` (like the numeric `comp_mt`), globals registered like `find`/`raycast`, and — critically — **all writes go through change maps drained after `run`, all reads mirror from the ECS each frame** (identical to how `node.vx`/`node.grounded` read the `bodies` mirror and `node.x = …` queues a write). New drain maps: `anim_changes` (parallel to `component_changes`) and `director_cmds`.

```lua
-- ============ ANIMATOR (skeletal controller) ============
function start(node)
    anim = node:getcomponent("Animator")
end

function update(node, dt)
    -- parameters (queued into anim_changes, flushed before anim.advance → affect this frame)
    anim:setFloat("speed", math.sqrt(node.vx^2 + node.vz^2))
    anim:setBool("grounded", node.grounded)
    if input.pressed("j") then anim:setTrigger("attack") end   -- auto-consumed by its transition

    -- direct play (bypass transitions — great for scripted one-offs / cutscenes)
    if input.pressed("k") then anim:play("Jump") end
    anim:crossfade("Slash", 0.05, "Combat")        -- state, seconds, optional layer

    -- layers: the base/combat blend the user asked for
    anim:setLayerWeight("Combat", inCombat and 1.0 or 0.0)
    anim:layer("Combat"):fadeWeight(1.0, 0.2)       -- blend combat in over 0.2s

    -- read state (from the mirror, like transform read-back)
    if anim:state("Locomotion") == "Jump" then ... end
    local t = anim:normalizedTime("Locomotion")     -- 0..1 phase
    if anim:justFinished("Combat") then ... end      -- non-looped clip ended this frame
end

-- ============ TIMELINE / CUTSCENE (director) ============
local cut = find("Intro"):director()               -- director on the "Intro" node
cut:play(); cut:pause(); cut:stop()                 -- stop resets to t=0 (holds first frame)
cut:seek(0.5)                                       -- seconds (fires only fire_on_seek events between)
cut:seek("impact")                                  -- jump to a named marker
cut.speed = 2.0                                     -- negative = reverse
cut.t; cut.playing                                  -- reads
cut:onFinish(function() door:play("shut") end)      -- callback when a Hold cutscene ends

-- ============ GLOBAL EVENT BUS ============
on_event("door_opened", function(args) door:play("shut") end)
emit("cheer", { volume = 0.8 })                     -- any script/track/notify can raise events

-- ============ TIMELINE EVENT TARGET (called by the dispatcher) ============
function onIntroImpact(who, power) spawnFx(who, power) end
```

**Setters** (`setFloat`, `setTrigger`, `play`, `crossfade`, `setLayerWeight`, `cut:play`, `cut:seek`) push into the new drain maps and are applied **before** the animation/director `advance`, so intent set this frame lands this frame. **Getters** (`state`, `normalizedTime`, `getLayerWeight`, `justFinished`, `cut.t`, `cut.playing`) read from the per-frame anim mirror — a 1:1 reuse of the transform/body mirroring already in place. Timeline `AnimProp::Component` tracks reuse the **same field-name vocabulary** as `getcomponent`, so cutscene property tracks and script writes share one apply path.

---

## 8. Editor UX

All new asset types are `.ron` you double-click — keeping the mental model uniform with materials/scenes/scripts today. Note `EditorTab` currently has variants Hierarchy/Inspector/Terrain/Assets/Console/Scene/Game/Scripting (`floptle-editor/src/main.rs:1579–1600`); we add one (`Timeline`) and two floating editor windows.

**Assets browser.** `.controller.ron` and `.timeline.ron` get icons in the tree/grid. Animated `.glb` files gain **expandable clip children** (`models/hero.glb#Run`), parsed via a lightweight header read. **Double-click** opens the corresponding editor (matching the existing double-click-to-open behavior for scripts).

**Inspector — Animator + Director components** (Unity-style modular, via **Add Component**, matching the current modular Inspector with per-component copy/paste and `⋮` overflow):
- *Animator*: controller path (with the existing copy-path affordance), an **Open** button (→ controller window), a live parameter monitor during Play, and per-instance `param_overrides`. Runtime rebuilds on controller change (like the physics sim rebuild on Play).
- *TimelineDirector*: asset path, autoplay toggle, transport (play/pause/seek) that drives the same runtime the Timeline tab uses.

**Controller window** (floating `egui::Window`, like the material editor):
- **Layers panel** (left): list with weight slider, blend-mode dropdown (Override/Additive), and a **joint-mask editor** — a checkbox tree of the skeleton's bones with a `recursive` toggle. This is where the dev controls layer influence.
- **State graph** (center): states as draggable rounded rects, arrows as transitions (hand-painted on an `egui::Painter` — the editor already hand-paints physics gizmos, so the primitive is proven). Drag between states to add a transition; select an arrow to edit conditions/duration/exit-time; entry + AnyState pseudo-nodes.
- **Clip slots:** **drag a clip from the Assets window onto a state** (or a blend-tree threshold) to include it — the exact "manage which clips are included by dragging them in" flow requested. 1D blend = a slider strip, 2D blend = a gradient pad.
- **Parameters panel** (bottom): add/rename params with type + default + a "poke" button to test-fire a trigger in the live preview; these populate the condition dropdowns.

**Timeline tab** (`EditorTab::Timeline`, docked in the bottom split beside Assets/Console — add the enum variant + `title()` + `ui()` dispatch + `default_dock()` placement, mirroring the existing tabs at `:1633`/`:1640`). Opening a `.timeline.ron` focuses this tab bound to that asset. Layout:
- **Transport bar:** `⏮ ⏵/⏸ ⏹ ⏭`, a time field, a `speed` drag-value (negative = reverse), a `● Record` toggle, `Loop`, `fps`, and `+ Track` / `+ Event Track` menus.
- **Left = track headers:** label, a **bind button** (shows the target node path; drag a Hierarchy node here to rebind), channel dropdown, mute/solo, weight slider.
- **Right = time area** (`egui::ScrollArea::horizontal`, ctrl-scroll to zoom): a ruler with fps ticks + markers; a draggable **playhead** (drag = scrub → `seek`, `seeking=true`); per-track lanes — property keys as draggable diamonds (drag = retime; expand a lane to **curve mode** with draggable Bezier tangents), event keys as flags (click → pick `EventAction`; for `CallScript`, a **dropdown of the target's actual scripts + their parsed top-level functions** — no typo'd names, since the editor already parses scripts for the IDE), clip segments as colored bars.
- **Record mode** (the ergonomic centerpiece): with `● Record` on and a node selected, **any gizmo/Inspector transform edit writes a key at the playhead** on the matching channel(s), auto-creating the track+binding on first touch (snapped to the fps grid). Blocking a cutscene = scrub → pose → scrub → pose. This is Godot's "key on move".
- **Live scrub coupling:** because sampling is idempotent, moving the playhead even when *not* playing calls `director.advance` with `dt=0`/forced `t` and applies the property changes exactly as in Play — true WYSIWYG in the Scene viewport with no separate code path.

---

## 9. Phased roadmap

Each milestone is independently useful and shippable. The **minimum viable first slice is M1+M2**: import a Blender clip and see it play. Layers, controllers, and the timeline build on that spine.

**M0 — Foundations (fast, unblocks everything).**
Fill `floptle-anim` with `skeleton`/`clip` types + sampling (§2.2, §3.2). Add `TransformTRS`, `Pose`, `sample_clip`, `skinning_matrices`. Pure Rust, unit-testable. *Risk: none. Deliverable: a tested pose evaluator.*

**M1 — glTF skin + clip import (§4).**
Extend `gltf_import.rs`: `ImportedModel.skinned`, walk `Skin`/`Animation`, skip world-bake for skinned prims, read `JOINTS_0`/`WEIGHTS_0`. Unit-test against a real Blender `.glb`. *Risk: the world-bake gating and inverse-bind correctness — both isolated and testable. Deliverable: a loaded skeleton + clips in memory.*

**M2 — Play one clip on screen (the MVP).**
Add the `Animator` component (single clip, looping, no state machine), run `anim.advance` in the Play loop (§3.1 steps 3), and **prove correctness with CPU skinning through the existing pipeline** (no shader change yet). Then add the GPU skinning pipeline (§4.1: second stream + storage buffer + `vs_skinned`). *Risk: GPU skinning is the first real shader work; the CPU-first proof de-risks it. Deliverable: a Blender character plays its idle in-engine.*

**M3 — Crossfade + Lua play/crossfade (§3.2, §7).**
Two active layers for a blend; add `anim_changes` + `install_anim_api`; `anim:play`/`crossfade`/`setFloat`. *Risk: the `take_component_changes` refactor (move apply to the editor loop) — small, do it here. Deliverable: script-driven clip switching with smooth blends.*

**M4 — State machine + parameters (§5.1).**
Compile `.controller.ron` → `CompiledController`; transitions, conditions, triggers, exit-time; Blend1D. Inspector Animator component + the **controller graph window** with drag-clips-in (§8). *Risk: state-graph UI is the biggest single UI lift; the runtime logic is straightforward. Deliverable: authored locomotion (idle/walk/run/jump) with no code.*

**M5 — Layers + masks (§5.2).**
Layer stack, Override/Additive, joint masks (compiled to per-joint weights), live weights from Lua. Mask editor in the controller window. *Risk: additive-layer math + reference poses; validate visually. Deliverable: the base-locomotion + upper-body-combat setup the user described.*

**M6 — Timeline core + property tracks (§6, §2.4).**
New `floptle-timeline` crate; `TimelineDoc`, sampler, seek/scrub-correct property writes; extend `apply_component_field` with Transform/Visible/Material/Camera arms; the **Timeline tab** with keyframes + record mode + live scrub. *Risk: the `apply_component_field` extension touches a shared path — small and additive. Deliverable: keyframed cutscenes for transforms + properties, scrubbable.*

**M7 — Code events + event bus (§6.3–6.4).**
Edge-triggered event firing with `fire_on_seek`; `ScriptHost::call_on`; `on_event`/`emit`; skeletal notifies through the same queue; the event-key editor with the script/function picker. *Risk: exactly-once firing across dt jitter/loop/reverse — the interval algorithm handles it; test with fast/slow frames. Deliverable: cutscenes that call Lua at timeline points.*

**M8 — Polish & optimization.**
Blend2D, sub-timelines, clip tracks driving Animators (`SetAnimState`), root motion, sampling cursors, off-screen LOD skip, morph targets (deferred until now). *Risk: none blocking; incremental. Deliverable: full-featured, optimized system.*

---

## 10. Open questions / decisions for the user

1. **Two crates or one?** Recommended: `floptle-anim` (skeletal/pose) + a new **`floptle-timeline`** (property/cutscene, `floptle-core`-only) so headless `floptle-runtime` can play cutscenes without render/assets. Alternative: fold the timeline into `floptle-anim` (fewer crates, but drags `floptle-assets` into cutscene-only builds). **Decision needed before M6.**

2. **Where does the property-write apply live?** Recommended refactor: expose `ScriptHost::take_component_changes()` and move the apply out of `run` into the editor loop so script + timeline writes union in one pass with deterministic precedence. Alternative: keep them separate and apply twice (simpler, but timeline-vs-script ties become order-dependent and harder to reason about). **Affects M3/M6.**

3. **One skin per model, or many?** The design assumes `doc.skins().next()` (one skin per character — the common Blender export). Multi-skin models (rare) would need a skin→part mapping. **Confirm this is acceptable, or flag models that need multi-skin.**

4. **Clip identity across rigs.** Clips target joints by **index resolved at import against this model's skeleton**. Cross-rig retargeting (same clip on a differently-built skeleton) would need name-based rebinding (bevy's approach). Do you want retargeting now, or is per-model clip binding enough for v1? **Recommendation: per-model for v1; the name map is already stored, so retargeting is a later add.**

5. **Root motion → physics coupling.** Feeding root-motion deltas through `body_changes` assumes character controllers want velocity, not teleport. Confirm the intended coupling (velocity vs. direct transform) for your first-person/character setup. **Affects M8.**

6. **CubicSpline / morph targets scope.** CubicSpline import is on the cold path (Blender exports Linear for bones); morph targets (facial/blendshapes) are deferred to M8. Confirm nothing in your first content needs them earlier.

7. **In-engine `Camera` FoV / active-camera switching from timelines** touches the editor's camera-selection logic (`main.rs:6878+`). Confirm cutscenes should be able to drive the active game camera (the design assumes yes via `EventAction::SwitchCamera`).
