# Audio (`floptle-audio`)

Everything audible: decoded clips, playing voices (spatial or flat), and the
project mixer — tracks with faders, effect chains, and routing, all folding
down into Master.

> Reads on: ADR-0023 Audio system ·
> [Scripting §11](../scripting.md#11-audio-audioplay-nodesound--the-mixer) ·
> [Editor](./editor.md). Crate: `floptle-audio` (glam + serde; `cpal` +
> `symphonia` behind the `backend` feature).

## Why this exists

Sound in most engines is a scavenger hunt: make a prefab, add a source, wire a
clip, spawn it, fetch the component, play, remember to destroy it. Floptle's
bet is **a clip path is enough** — `audio.play("audio/hit.ogg", x, y, z,
{ maxDistance = 35 })` — with a real DAW-shaped mixer behind everything for the
sound-design pass.

## Architecture

```
control side (editor / Lua)                audio thread (cpal callback)
────────────────────────────               ─────────────────────────────
AudioEngine ── commands ──────────────▶    AudioCore
  play/stop/move/params/mixer/listener       voices → track buffers
  ◀── status snapshots (try_lock) ──         effect chains per track
    playing / playhead / finished            routing (topo order) → Master
    per-track meters
```

- **`Clip`** — decoded f32 PCM at native rate, shared by `Arc`; voices
  resample on the fly (which is also how `pitch` works). Decode via symphonia:
  wav / ogg-vorbis / mp3 / flac.
- **`AudioCore`** — the whole audible state (voices, mixer, listener) with a
  pure `render()`. The cpal callback owns one; tests drive one directly, so
  the DSP is verified headless.
- **Voices** — up to 256; per-block spatialization, per-sample gain smoothing
  (~4 ms declick on start/stop/param changes). A stopping voice fades, then
  reports finished.
- **Spatial** (`spatial.rs`) — f64 listener-relative math (large-world safe).
  Modes: `Spatial` (attenuation + equal-power azimuth pan, image collapses to
  center inside `minDistance`), `Distance` (attenuation only), `Flat` (2D).
  Falloffs: `Inverse` (default), `Linear`, `Exponential` — all smoothly fade
  to true zero over the last 20% of range so nothing pops at `maxDistance`.
  The listener is the active camera.
- **Mixer** (`mixer.rs`) — `MixerDesc` (serde, lives in `project.ron`) vs
  `MixerDsp` (audio thread). Tracks route into tracks (cycles fall back to
  Master); solo keeps the soloed track's downstream chain audible; `apply`
  diffs a new desc onto the running graph so live edits never cut effect
  tails. Per-track post-fader peak meters flow back for the UI.
- **Effects** (`effects/`) — descriptor + processor pairs: parametric EQ (RBJ
  biquads; `EqBand::response_db` also draws the editor curve — the graph you
  drag *is* the filter math), delay (ping-pong, damped feedback), Freeverb
  reverb, chorus, flanger, phaser, dual-tap granular pitch shift, compressor,
  limiter, distortion, utility (gain/width). Processors absorb param changes
  via `update()` without clearing state.

## The component

`AudioSource` (defined in `floptle-audio`, serialized as its own doc in
`NodeDoc::audio`): clip path + `PlayParams` (volume, pitch, pan, mode,
falloff, min/max distance, track, end behavior) + `play_on_start`.
End behavior: **Stop** (replayable), **Destroy** (despawn the node when the
sound ends — self-cleaning one-shots), **Loop**.

## Editor integration

- **`AudioSystem`** (`floptle-editor/src/audio.rs`) — the glue field on
  `Editor`: clip cache, play-mode voice per `AudioSource`, script one-shots,
  the runtime mixer overlay (Lua tweaks hold across `scene.load` — kept as
  `session_edits` and laid back over the project mixer at every scene start —
  and revert on Stop). Ticked in the play
  loop after physics/attachments so emitters ride final transforms.
- **Clip cache** (`floptle-editor/src/audio_clips.rs`) — a clip is decoded on
  a worker the first time anything asks for it (inline in a browser, like
  every other background job). A sound asked for before its clip is in waits
  in `AudioSystem` as `Waiting`, reads as playing with `loading` set, and
  keeps whatever a script does to it (seek, pause, params, stop) until
  `AudioSystem::pump` starts it. Idle clips — held by no voice — are kept up
  to `IDLE_BUDGET_BYTES` (64 MiB), least recently played dropped first; a
  preloaded clip is pinned until its first play. The cache only drops a clip
  it holds the last `Arc` to, so a free never happens on the audio thread.
  Eviction runs AFTER the waiters start: a clip that just arrived is idle
  until then, and one bigger than the whole budget would be dropped on
  arrival.
- **🎧 Mixer tab** — strips (Master + tracks): fader, pan, mute/solo, live
  meter, output routing, effect chain; right panel edits the selected effect
  (the EQ gets the draggable response curve). Saves with the project.
- **Inspector** — ♪ Audio Source section with a searchable clip picker and a
  ▶ preview button; component copy/paste like everything else.
- **Assets** — `.wav/.ogg/.mp3/.flac` show a ♪ icon; double-click previews.

## Lua

Command-queue + info-mirror, like anim/vfx — scripts never touch the engine:
`audio.play(...) → SoundHandle`, `node:sound()` for components,
`audio.track(name)` for live mixer control (nil for a track the mixer does
not have; `setLowpass` is a bus low-pass after the effect chain, the
`TrackDesc::lowpass` field), `audio.tracks()` to read the mixer, tunables via
`getcomponent("AudioSource")`. See scripting.md §11.

## Voice groups

`PlayParams::group` + `priority`, capped per group with
`AudioCore::set_group_limit` (`audio.limit` in Lua). The cap is enforced on
the audio thread, in `AudioCore::start`, where the true voice list lives: a
control-side count would lag the mixer by a callback and could not see a
voice that had just ended.

- A group is named on the control side: `AudioEngine` keeps the names and
  sends each one to the audio thread once (`Cmd::Group`), ahead of the first
  sound or limit that uses it, so the string is moved in, never copied there.
  The core's group list is allocated at `MAX_GROUPS` (64) up front; a voice
  carries its group as a `u16`. A new sound finds its group by name among at
  most 64; counting the group and picking the voice to give way is then one
  pass over at most 256 voices comparing integers, with one `spatialize` per
  candidate. No allocation, no lock. A group past the 64th
  plays ungrouped.
- Rank: lowest `priority`, then the quietest at the listener (`volume ×
  distance gain`, the same number the voice is mixed at), then the oldest.
  The newcomer is ranked with the rest: one that ranks below every voice in
  the group is dropped (it reports finished at once) rather than replacing a
  louder sound.
- A replaced voice gets `stopping = true`, the ordinary declick fade. It
  stops counting against its group at once and is reaped a few milliseconds
  later.
- Per-group counts are published with the other status as a plain `u32`
  per group, by index (`try_lock`, into a buffer allocated at `MAX_GROUPS`);
  the engine puts the names back on, and scripts read it as
  `audio.groupCount`.
- Limits live in the core and are cleared by `reset` (Play stopped).

## What scripts read back

`VoiceStatus` (published after every callback with the other status) carries
`audible`: `volume × distance gain` at the listener, the same `heard` number
voice stealing ranks by, 0 while paused or stopping. Scripts read it as
`s:audible()`. Clip lengths are a control-side fact: `ClipCache` records each
clip's length in seconds when its decode lands, in an `Arc<HashMap>` it hands
to the script mirror every frame (cloned only on a frame a clip arrives),
and keeps after the clip itself is evicted. `audio.duration` reads that, and
a miss queues `AudioCmd::Decode`, an unpinned background read.

## The per-voice low-pass

Each voice has one `LowPass` (two one-poles, 12 dB an octave, coefficient
gliding across a block so a change never clicks). Its cutoff is worked out
per block in `render_block`, beside the gain, by `voice_cutoff`: the lowest
non-zero of

- `PlayParams::lowpass` (`audio.play{lowpass=}`, `s:setLowpass`);
- the distance cutoff, `spatial::distance_cutoff`, from `air_absorption`
  (`20 kHz / (1 + k × (d − min_distance))`) and `lowpass_falloff` (`near` at
  `min_distance` to `far` at `max_distance`, geometric in between);
- the world low-pass, `AudioCore::world_lowpass` (`set_world_lowpass`,
  `audio.setWorldLowpass` in Lua): the listener's medium, under water or in a
  suit. A `listener_space` voice is exempt. Cleared by `reset`.

Distance and the medium apply only to a voice in the world: positioned and
not `Flat` (`world_distance`). A voice with none of these set gets the same cutoff
every block, so its filter does exactly what it did before. The lowest
cutoff, not a filter per source, keeps the cost at one filter per voice; the
track's bus low-pass (`TrackDesc::lowpass`) is a separate filter after the
effect chain and stacks in series with it.

## Real-time rules (keep these)

- The callback never locks/allocates on the steady path. Status publishing
  uses `try_lock` and skips a frame under contention.
- Never use a raw parameter jump on the audio thread — everything audible
  goes through the smoothed targets (that's where the no-click guarantee
  lives).
- `Globals`-style struct pairs don't exist here, but `MixerDesc`/`MixerDsp`
  must stay behaviorally in sync: a desc field without an `apply` mapping is
  a knob that silently does nothing.

## Future

Doppler, SDF-based occlusion, streaming decode for long music (a playing
track is still whole PCM, ~50 MB for 2.5 minutes of stereo), convolution
reverb, per-effect automation (the particle timeline's lane pattern fits).
