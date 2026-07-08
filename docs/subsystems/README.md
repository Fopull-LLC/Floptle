# Subsystem designs

Deep-dive design notes for each part of Floptle. Each is opinionated and
concrete (data structures, data flow, editor UX, what's out of scope). They build
on the [`../decisions/`](../decisions/) ADRs and the top-level
[`../ARCHITECTURE.md`](../ARCHITECTURE.md).

### The rules substrate (the spine)
- [world-rules.md](world-rules.md) — the **Lawset/Realm** meta-spine: a world's laws as inheritable data, resolved by the SDF inside-test.
- [field-interaction.md](field-interaction.md) — how fields **affect each other** (composition by design, not coincidence) — the emergence seam.

### The signature visuals
- [renderer.md](renderer.md) — the otherworldly render graph; SDF raymarching you fly *into*; reality-bending post.
- [shaders.md](shaders.md) — the shader IR: one source of truth, edited as graph **and** `.flsl` text → WGSL.
- [light.md](light.md) — programmable light transport: light as the fourth field; bend rays (bend can *be* gravity).
- [deformable-matter.md](deformable-matter.md) — the unifying idea: everything is malleable matter (morph · blend/soup · soft-body · stick · fracture).

### World & simulation
- [physics.md](physics.md) — SDF-first collision; character & raycast-vehicle controllers on morphing worlds.
- [gravity-and-density.md](gravity-and-density.md) — gravity as a field emitted by matter; density → mass, crushability, and "walk up a fractal wall."
- [large-world-space.md](large-world-space.md) — default-on floating-origin / camera-relative space: simulate a galaxy with no jitter.
- [time.md](time.md) — time as a rate field `r(p)`: per-entity local clocks; slow/freeze/dilation regions.
- [particles-vfx.md](particles-vfx.md) — the timeline particle editor (groups, emit events, per-property curves).
- [audio.md](audio.md) — spatial sound + the project mixer (tracks, effects, routing) and the one-line `audio.play` API.
- [scene-and-nodes.md](scene-and-nodes.md) — Node/Component authoring facade over the archetype ECS.
- [animation.md](animation.md) — glTF skeletal clips + a lightweight state machine + notify events.

### Interaction & game-feel
- [input.md](input.md) — action mapping across keyboard/mouse/gamepad.
- [ui.md](ui.md) — the dead-simple arrange-and-script in-game UI.
- [camera-and-dialogue.md](camera-and-dialogue.md) — virtual-camera rigs + the built-in typewriter dialogue system.
- [object-pooling.md](object-pooling.md) — automatic take/return pooling.

### Content pipeline & tooling
- [materials-and-textures.md](materials-and-textures.md) — easy materials; drag-on, tile-without-a-shader textures.
- [asset-pipeline.md](asset-pipeline.md) — Blender → glTF import + the asset database & hot reload.
- [editor.md](editor.md) — the authoring app: scene view, in-scene shape building, all panels, the theme.

### Future
- [networking-future.md](networking-future.md) — deferred authoritative server + clients on self-hosted infra.
