# ADR-0023 — Audio system: custom mixer-graph engine on cpal

- **Status:** Accepted · 2026-07-08
- **Decider:** Ty Johnston (Fopull LLC)
- **Detail:** [Audio subsystem](../subsystems/audio.md) ·
  [scripting.md §11](../scripting.md#11-audio-audioplay-nodesound--the-mixer)
- **Relates to:** [0003 — Scripting](0003-scripting-lua.md) (the `audio` Lua API follows the
  anim/vfx command-queue pattern), [0015 — Large world space](0015-large-world-space.md)
  (emitter/listener positions are f64).

## Context

The engine had no sound at all. Requirements: 3D spatial audio with selectable distance
falloff, a project-wide mixer (unlimited tracks, per-track volume/pan/effects, tracks routing
into tracks, everything ending at Master), an `AudioSource` node component, and one-line
playback from Lua (`audio.play(clip, at, {opts})`) — deliberately lighter than Unity's
prefab-with-a-source workflow.

## Decision

Build a **custom engine in a new `floptle-audio` crate** rather than adopting audio middleware
(kira, oddio, FMOD):

- **`cpal`** for device output, **`symphonia`** for decoding (wav/ogg/mp3/flac) — both behind a
  `backend` feature so data-model crates (`floptle-scene`) can use the *types* (component,
  mixer graph, effect descriptors) without linking the OS audio stack. Matches the engine's
  hand-rolled ethos (physics, renderer, particles) and keeps the mixer/effects design fully
  ours.
- **Hand-written DSP**: RBJ biquad parametric EQ, feedback delay, Freeverb reverb,
  chorus/flanger/phaser, dual-tap granular pitch shift, compressor/limiter, distortion,
  utility. Each effect is a serde descriptor (scene/Lua/UI face) + a real-time processor that
  absorbs live edits without resetting tails.
- **Real-time discipline**: the audio callback never locks or allocates on the steady path;
  control side talks through a command channel; status returns via `try_lock` snapshots that
  skip a frame under contention rather than stall the mix.
- **The mixer graph persists in `project.ron`** (`ProjectConfigDoc::mixer`) — project-wide, not
  per-scene. Lua track tweaks apply to a runtime overlay that reverts on Stop.
- Lua integration follows the established **command-queue + info-mirror** pattern (like
  anim/vfx): scripts never touch the engine directly, keeping the determinism invariant.

## Consequences

- One implementation serves editor Play and exported builds (the player *is* the editor binary).
- Headless machines degrade to silence (engine open fails cleanly; everything is best-effort).
- The DSP core (`AudioCore`) is device-free and unit-tested; the cpal layer is a thin shell.
- Future work: doppler, per-source occlusion (the SDF field is sitting right there), audio
  streaming for long music (clips currently decode fully into memory), convolution reverb.
