# Animation, particles & sound

The three things that make a scene feel alive, driven from a script.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [9. Animation: `node:animator()`](#9-animation-nodeanimator)
- [10. Particles: `node:particles()`](#10-particles-nodeparticles)
- [11. Audio: `audio.play`, `node:sound()` & the mixer](#11-audio-audioplay-nodesound-the-mixer)

---

## 9. Animation: `node:animator()`

Any node with an **Animation Controller** component (or a rigged model with
embedded clips) exposes an animation handle. See `docs/animation.md` for the
full system (controllers, layers, events, the stepped retro look).

```lua
local anim
function start(node)
  anim = node:animator()
end

function update(node, dt)
  local speed = math.sqrt(node.vx^2 + node.vz^2)
  if not node.grounded then anim:play("Jump")
  elseif speed > 6     then anim:play("Run")
  elseif speed > 0.5   then anim:play("Walk")
  else                      anim:play("Idle") end

  if input.pressed("j") then anim:restart("Slash") end -- one-shot attack layer
end

-- called by a ⚑ event key placed on a clip's timeline:
function onSlashHit(node) log("hit frame!") end
```

| Call | What it does |
|---|---|
| `anim:play(state [, fade [, layer]])` | transition (controller decides the fade; safe every frame) |
| `anim:restart(state [, fade [, layer]])` | force re-entry (re-trigger a one-shot) |
| `anim:crossfade(state, fade [, layer])` | transition with an explicit fade |
| `anim:stop([layer [, fade]])` | stop a layer (all if omitted) |
| `anim:setSpeed(x)` | global speed multiplier |
| `anim:setLayerWeight(layer, w)` | blend a layer over the ones below (0..1) |
| `anim:seek(t [, layer])` | jump the playhead |
| `anim:state([layer])` / `anim:time([layer])` | what's showing / seconds in (`anim:current` is an alias of `anim:state`) |
| `anim:finished([layer])` | a one-shot reached its end |
| `anim:isPlaying([state])` | is a state (or anything) playing |
| `anim:clips()` / `anim:layers()` | available state / layer names |
| `anim:duration(clip)` / `anim:events(clip)` | the clip **as authored** — length in seconds, and its event list |

**`anim:duration` / `anim:events` read the asset, not playback**, so they answer
in `start()`, before anything has played a frame. `events` returns
`{ {t = seconds, func = "onHitboxStart"}, ... }` ascending by `t` — an unknown
clip is `nil`, a clip with no events is an empty list.

They exist so an animator can author timing **by eye** while the game still runs
on integer frames. Drop an event on the frame where the strike connects, then
bake it once at load:

```lua
local dur = anim:duration(clipName)
for _, e in ipairs(anim:events(clipName) or {}) do
  if e.func == "onHitboxStart" then
    move.startup = math.floor(e.t / dur * move.frames + 0.5)
  end
end
```

Don't let events *drive* frame-exact gameplay directly: they fire off float
playback time, stepped playback (`sample_fps`) quantises them to its grid, clip
time and gameplay frame disagree mid-crossfade, and a prediction replay
deliberately doesn't re-fire them. Every machine loads the same `.anim.ron`, so
a baked number is identical everywhere and constant thereafter.


**Conditional expressions.** Lua's `and`/`or` chain is an inline if/else —
handy for mapping states to values without an if-ladder:

```lua
local speed = anim:isPlaying("Running") and 2
           or anim:isPlaying("Walking") and 1
           or 0
```

The one gotcha: put the **condition first**. `a and b` yields `b` only when
`a` is truthy, so `2 and anim:isPlaying("Running")` gives you the *boolean*,
not the 2. (And this only picks non-false values — `cond and false or x`
always lands on `x`.) Method names are camelCase: `anim:IsPlaying` is an
error, and the animator will suggest the spelling it thinks you meant.

**Events → functions.** Put a ⚑ event on a clip in the **✎ Animating** tab and
name a function; when the playhead crosses it during Play, that function is
called (with the node) on every script attached to the controller's node that
defines it.

---

## 10. Particles: `node:particles()`

Any node with a **Particle System** component exposes a particle handle, so
scripts can fire and stop effects on cue — muzzle flashes, footstep dust,
thruster plumes, pickups. See `docs/subsystems/particles-vfx.md` for authoring
effects on the ✱ Particles timeline.

```lua
function update(node, dt)
  local p = node:particles()

  -- one-shot burst on each shot (re-fires even mid-play):
  if input.clicked(0) then p:restart() end

  -- a continuous effect that follows a condition:
  local jet = find("Thruster"):particles()
  if input.key("w") then jet:play() else jet:stop() end

  if p:isPlaying() then log("smoke: " .. p:alive() .. " alive") end
end
```

| Call | What it does |
|---|---|
| `p:play()` | start emitting if idle (spawns a fresh instance); no-op if already playing |
| `p:stop()` | stop + despawn — the live particles vanish |
| `p:restart()` | re-spawn from `t=0` (re-fire a one-shot burst) |
| `p:setIntensity(i)` | live emission scale 0..~2 — throttle a plume without touching the asset |
| `p:setBeamEnd(x, y, z)` | aim every **Beam** track's endpoint at a WORLD point (converted to effect-local, so the beam tracks the target as the node moves) |
| `p:isPlaying()` | is an instance emitting/ageing right now |
| `p:alive()` | live particle count across the effect's tracks |
| `p:asset()` | the effect asset key this node references, or `nil` |

> Handles work cross-node: `find("Campfire"):particles():stop()`. A node's
> **Play on start** flag is also scriptable —
> `node:getcomponent("ParticleSystem").play_on_start = 1`.

### `spawnEffect` — fire a one-shot at a world point

For hits, pickups, footstep poofs — effects that aren't tied to a node — spawn
one anywhere in the world and forget it. It plays once and despawns itself:

```lua
function update(node, dt)
  if input.clicked(0) then
    local h = raycast(node.x, node.y, node.z, fx, fy, fz, 100)
    if h then spawnEffect("vfx/Impact", h.x, h.y, h.z) end
  end
end
```

`spawnEffect(key, x, y, z)` — `key` is the effect asset (project-relative, no
`.vfx.ron`); the position is world space. Author it as a **one-shot** effect on
the ✱ Particles timeline so it ends cleanly. That's the whole loop: design it on
the timeline → `spawnEffect` it from gameplay.

---

## 11. Audio: `audio.play`, `node:sound()` & the mixer

Playing a sound needs nothing but a clip path — no prefab, no source node, no
spawn-then-get-component dance:

```lua
audio.play("audio/ding.ogg")                          -- flat 2D (UI, stingers)
audio.play("audio/hit.ogg", h.x, h.y, h.z)            -- 3D at a world point
audio.play("audio/engine.ogg", carNode, {loop = true}) -- follows the node
```

Sounds default to **spatial**: they attenuate with distance and pan toward
where they are relative to the active camera. Every knob rides in the options
table (all optional):

```lua
local s = audio.play("audio/roar.ogg", bossNode, {
  volume = 0.8,             -- linear, 1 = as authored
  pitch = 1.1,              -- playback rate (also shifts pitch)
  mode = "Spatial",         -- "Distance" = attenuate only · "Flat" = plain 2D
  falloff = "Inverse",      -- "Linear" · "Exponential"
  minDistance = 2,          -- full volume inside this range
  maxDistance = 50,         -- silent past this range
  track = "SFX",            -- mixer track to route through (default Master)
  endBehavior = "Destroy",  -- "Stop" (default) · "Destroy" · "Loop"
})
```

`audio.play` returns a **sound handle**, live until the sound ends:

| Call | What it does |
|---|---|
| `s:stop()` | fade out (a few ms — never clicks) and end |
| `s:pause()` / `s:resume()` | freeze / continue playback |
| `s:setVolume(v)` / `s:setPitch(v)` / `s:setPan(v)` | live tweaks |
| `s:setTrack("Music")` | re-route through another mixer track |
| `s:setPosition(x, y, z)` | move the emitter (stops following a node) |
| `s:seek(secs)` | jump the playhead |
| `s:isPlaying()` / `s:position()` | playback state |

`endBehavior = "Destroy"` on a node-following sound despawns that node when
the sound finishes — spawn a node, hang a sound on it, and it cleans itself up.

### The Audio Source component

For authored emitters (ambient loops, music zones, alarm props), add an
**Audio Source** in the Inspector (➕ Add Component): pick the clip, spatial
mode, falloff, distances, mixer track, end behavior, and **Play on start**.
Scripts drive it through `node:sound()`:

```lua
local alarm = find("Alarm"):sound()
alarm:play()                     -- restart its clip
alarm:setClip("audio/alarm2.ogg")
alarm:pause()  alarm:resume()  alarm:stop()
if alarm:isPlaying() then log(alarm:position()) end
```

Its tunables mirror live through `getcomponent` (numbers only, like every
component):

| field | Meaning (Inspector: ♪ Audio Source) |
|---|---|
| `volume` | linear volume 0..2 |
| `pitch` | playback rate (0.5 = octave down) |
| `pan` | stereo pan −1..1 (Flat mode) |
| `minDistance` / `maxDistance` | the falloff range |
| `playOnStart` | 1/0 — play when Play starts |
| `mode` | 0 = Spatial · 1 = Distance · 2 = Flat |
| `falloff` | 0 = Inverse · 1 = Linear · 2 = Exponential |
| `endBehavior` | 0 = Stop · 1 = Destroy · 2 = Loop |

```lua
node:getcomponent("AudioSource").volume = 0.3   -- live while playing
```

### The mixer

Everything audible routes through the **🎧 Mixer** tab: named tracks with a
fader, pan, mute/solo, an effect chain (parametric EQ with a draggable curve,
delay, reverb, chorus, flanger, phaser, pitch shift, compressor, limiter,
distortion, utility), and routing — tracks can output into other tracks
(e.g. `Footsteps → SFX → Master`). The graph saves with the project
(`project.ron`); anything that doesn't name a track plays on **Master**.

Scripts get live control that reverts when Play stops:

```lua
audio.track("Music"):setVolume(-12)   -- duck music (fader dB)
audio.track("SFX"):setPan(0.2)
audio.track("Master"):setMuted(true)
audio.stopAll()                       -- silence everything
```

Clips are plain files under `assets/audio/` (`.wav`, `.ogg`, `.mp3`,
`.flac`) — double-click one in the Assets browser to preview it. Clip
references are project-relative paths (`"audio/hit.ogg"`; the extension may
be omitted).
