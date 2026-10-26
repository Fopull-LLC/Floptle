# Lua API reference

Every name a script can reach, grouped the way the editor's **Docs** tab groups
them. The same table drives this page, that tab, the hover docs and autocomplete —
so there is one description of each call, in one place, and it is the one you get
everywhere.

*Generated — do not edit by hand.* Change the entry in `crates/floptle-editor/src/ide.rs`
and run `UPDATE_DOCS=1 cargo test -p floptle-editor lua_api_reference_file`.

New here? [`scripting.md`](scripting.md) is the guided tour — it teaches in order,
with worked examples. This page is the reference: complete, alphabetical within
each group, and meant to be searched.

## Contents

- [script basics — lifecycle, params, log](#script-basics--lifecycle-params-log) — 208
- [node — transform & body fields](#node--transform--body-fields) — 44
- [node — methods & handles](#node--methods--handles) — 37
- [vectors, directions & easing](#vectors-directions--easing) — 51
- [scene lookups & raycast](#scene-lookups--raycast) — 18
- [references — wire nodes in the Inspector](#references--wire-nodes-in-the-inspector) — 3
- [input — keyboard & mouse](#input--keyboard--mouse) — 49
- [drawing — draw.*](#drawing--draw) — 19
- [the web — http.*, json.*](#the-web--http-json) — 15
- [the player's account — account.*](#the-players-account--account) — 13
- [game UI — text, buttons & hooks](#game-ui--text-buttons--hooks) — 74
- [networking — net.*, synced](#networking--net-synced) — 41
- [scenes — load, unload & persist](#scenes--load-unload--persist) — 6
- [terrain — runtime sculpt & queries](#terrain--runtime-sculpt--queries) — 27
- [pathfinding — nav.*](#pathfinding--nav) — 26
- [water — depth, buoyancy & ice](#water--depth-buoyancy--ice) — 11
- [weather — wind](#weather--wind) — 6
- [scatter — instanced props](#scatter--instanced-props) — 12
- [2D — sprites, sorting & the flat camera](#2d--sprites-sorting--the-flat-camera) — 38
- [vessels — assembly.*](#vessels--assembly) — 16
- [the camera & the screen](#the-camera--the-screen) — 9
- [physics controls — pause & step](#physics-controls--pause--step) — 5
- [frame cost — perf.*](#frame-cost--perf) — 23
- [accessibility — access.*](#accessibility--access) — 11
- [persistence — save.*](#persistence--save) — 7
- [timers — after, every, tween](#timers--after-every-tween) — 4
- [space — orbits & time-warp](#space--orbits--time-warp) — 21
- [components — getcomponent](#components--getcomponent) — 113
- [animation — node:animator](#animation--nodeanimator) — 29
- [particles — effects from script](#particles--effects-from-script) — 13
- [audio — sounds & the mixer](#audio--sounds--the-mixer) — 36
- [assets](#assets) — 19
- [debug gizmos](#debug-gizmos) — 5
- [lua stdlib](#lua-stdlib) — 43

## script basics — lifecycle, params, log

### `access`

Accessibility a game offers its players: UI text scale, a colour-vision filter, reduced motion and captions. The engine honours what it owns — text sizes go through the LAYOUT so scaling reflows, the filter is a post-chain stage, and UI transitions snap when motion is reduced. What it cannot honour for you (your camera shake) reads access.reducedMotion(). These are the PLAYER's settings, so persist them with save.*; the editor's ⚙ Settings → Accessibility drives the same values so you can try them. See docs/accessibility.md.

### `agent`

A nav agent handle, from nav.agent(node). It walks its node along the navmesh: agent:moveTo(point) and read agent.state as it goes. Everything about it is a field or a method on this handle — there is no per-frame step to call.

### `agent.alive`

False once the agent has been destroyed (or its node has). A handle kept in a variable answers about itself rather than pointing at whoever took its place.

### `agent.arrived`

True once it got there. The flag to hang "and then attack / gather / open the door" off.

### `agent.blocked`

True when it cannot get there right now: unreachable, or no progress for giveUpAfter seconds. A crowd pin clears itself; a cut-off goal does not.

### `agent.complete`

Whether the route it is walking actually reaches the order. False means it is heading for the closest it can get, which is the right behaviour and worth being able to say out loud — "can't get there, going as near as I can".

### `agent.link`

The name of the Nav Link being crossed right now, or nil the rest of the time. This is the hook for "play the climb animation": if agent.link == 'ladder' then ... end.

### `agent.linkKind`

What kind of crossing it is on: 'placed' for a Nav Link node somebody put there, or 'drop' / 'jump' for one the bake worked out from the shape of the floor. nil when it is not on one. This is the animation rule — a climb and a fall are not the same move, and a level with six hundred generated drops in it needs one rule rather than six hundred names.

### `agent.linkProgress`

How far across a link it is, 0 to 1 — nil when it is not on one. What a vault or climb animation is driven by, so the animation and the movement cannot disagree.

### `agent.moving`

True while it still has somewhere to be — walking or crossing a link.

### `agent.offMesh`

True when the order named a place the navmesh does not cover — as opposed to a place it cannot reach. Nearly always a Nav Mesh volume smaller than the level; ordering it somewhere else nearby will not help.

### `agent.pos`

Where it is, in world space.

### `agent.remaining`

How far there is left to walk, in metres, ALONG THE ROUTE rather than through the walls. The number an ETA or a progress bar wants.

### `agent.speed`

How fast it is going along the ground, in units per second. What an idle/walk/run animation blend reads.

### `agent.state`

'idle' | 'moving' | 'arrived' | 'blocked' | 'crossing' — and 'gone' for a handle whose agent has been destroyed. 'blocked' is the one worth acting on: it means the goal cannot be reached from here, or the unit has made no progress for giveUpAfter seconds. A unit pinned by its own crowd rests and tries again on its own; one whose goal is genuinely cut off stays blocked until something changes. A unit standing still with no explanation is the commonest "the pathfinding is broken" report there is, and this is the explanation.

### `agent.target`

Where it was told to go, or nil if it has no order.

### `agent.velocity`

How fast it is going, as a vec3. With drive = 'none' this is the whole point of the agent: it steers, and your script decides what that means for a vehicle, a boat or an animation.

### `agent:corners`

agent:corners() — the corners still to walk, as a list of vec3 in world space. For drawing the route while working out why a unit went the way it did.

### `agent:destroy`

agent:destroy() — take it out of the crowd. Not required (an agent whose node is destroyed goes with it on the next frame) but the right thing to call from a script's own teardown.

### `agent:moveTo`

agent:moveTo(point) — send it to a world point (a vec3, anything with x/y/z, or a node). Idempotent: ordering it to where it is already heading costs nothing, so calling this every frame to follow a moving target is fine. The point does not have to be exactly on the navmesh — it is snapped — and a point that is on the mesh but cut off makes the agent walk as near as it can and then report blocked.

### `agent:set`

agent:set{ ... } — change how it walks, mid-game. Takes the same options as nav.agent; anything left out is left alone. Slowing a unit down, making it stop giving way, or swapping its filter when it picks up a boat.

### `agent:stop`

agent:stop() — cancel the order and stand still. Anything mid-crossing finishes the crossing first: halfway up a ladder is not a place to be left.

### `agent:teleport`

agent:teleport(point) — put it AND its node somewhere without walking there, and forget what it was doing. For spawns, respawns and cutscenes. (With drive = 'none' the engine leaves the node alone, so move it yourself.)

### `app`

The settings a game offers the person playing it, and the one thing every main menu needs: quit. app.quit(), app.title(), app.version(), and the video settings — app.vsync/setVsync, app.retro/setRetro, app.retroHeight/setRetroHeight, app.retroIntegerScale/setRetroIntegerScale. A setting you change here is for THIS SESSION: it lives in project.ron, the file that ships to every player, so Stop puts it back — persist the player's choice yourself with save.*, the same rule access.* follows. Window resolution and fullscreen are NOT here yet; that is one question (windowing) waiting to be answered properly. See docs/scripting.md §30.

### `app.dataPath`

app.dataPath() — the folder a player's own files are in, as a full path: where `user://` points, for a screen that says "your levels are in …". In an exported build that is the per-user data folder (%APPDATA%\<studio>\<data_id>\user on Windows, ~/Library/Application Support/<data_id>/user on macOS, ~/.local/share/<data_id>/user on Linux); in the editor, floptle run and shot it is the project's save/user. nil in a browser, where the files live in the page's storage and there is no folder to show.

### `app.displayModes`

app.displayModes([monitor]) → { {width=, height=, refresh=, monitor=}, … } — the full-screen modes the monitors offer (one monitor's when you pass its index), largest first, each once. What a Resolution dropdown lists. Under Wayland each monitor offers its current mode only: the compositor owns the screen's mode, and a game's full screen there is the monitor as it is set. Empty where there is no window; a browser lists none it can switch to.

### `app.dynamicResolution`

app.dynamicResolution() → whether the engine is choosing the render scale itself (app.setDynamicResolution).

### `app.frameCap`

app.frameCap() → the frame cap in frames a second, 0 when there is none.

### `app.fullscreen`

app.fullscreen() → whether the game's window covers the screen. The real state, not the last thing asked for — so a Video tab shows what the player sees, including after F11 or Alt+Enter, which a build answers on its own.

### `app.isTouch`

app.isTouch() — true when the player's main pointer is a finger: a phone or tablet in a browser, or any screen that has been touched. Start in your touch layout on it.

### `app.isWeb`

app.isWeb() — true in a browser build. The same answer as app.platform() == "web", for the common question.

### `app.monitors`

app.monitors() → { {index=, name=, width=, height=, refresh=, x=, y=, current=}, … } — the displays the game's window could be on, in the system's order; current is the one it is on now. Sizes are physical pixels. Under Wayland a window learns which screen it is on only once the compositor has shown it there, so for the first frames (or always, on a compositor that never says) no monitor is current. An empty list where there is no window (floptle run, a dedicated server). Kept up to date every couple of seconds, so a monitor plugged in while a settings page is open shows up.

### `app.openFile`

app.openFile([{ title=, filter={"png", "jpg"}, maxBytes= }], function(name, bytes, err) end) — ask the player for a file with the system's own picker: an emblem, an avatar, a save a friend sent, a mod. The game keeps running while the picker is open, and the callback runs on a later frame with the file's name and its bytes (a string, ready for assets.textureFromBytes or assets.writeBytes to user://), never its path. maxBytes defaults to 8 MB and is at most 64 MB. err is "cancelled" when the player closes the picker, "too-large", "busy" when a picker is already open (one at a time), or "unsupported" where there is no picker: a dedicated server, and the browser for now.

### `app.platform`

app.platform() — where the game is running: "windows", "macos", "linux" or "web" (else the operating system's own name). For a settings screen that differs per platform, and for a browser build to hide what a page cannot do (it can join an online lobby but not host one) instead of showing a button that does nothing.

### `app.quit`

app.quit() — end the game. What that means depends on where it is running, and they are genuinely different things: an EXPORTED BUILD closes (your save.* data is flushed first, since somebody quitting from a settings menu expects the setting they just changed to have been kept); the EDITOR stops Play and says so in the Console, deliberately NOT a process exit — an editor that closed because a game under test called quit would take your unsaved work with it; `floptle run` ends the run where it stands and reports that it stopped early.

### `app.refreshRate`

app.refreshRate() → the refresh rate in Hz (143.9, 60, …) of the display the window is on, or nil where there is no display. What a frame cap is chosen from. It follows the window to another display, so read it when the cap is applied rather than once at start: until a window is first shown some systems cannot say which display it is on, and until they can it answers the fastest one.

### `app.renderScale`

app.renderScale() → the fraction of the window the 3D scene renders at, 0.25–1 (1 = native). The project's render_scale until a script sets it.

### `app.renderSharpness`

app.renderSharpness() → how hard the upscale from a render scale below 1 sharpens, 0–1 (default 0.4).

### `app.retro`

app.retro() → whether the retro presentation is on: the game composites at a small internal resolution and upscales, which is what gives it chunky pixels.

### `app.retroHeight`

app.retroHeight() → the internal height the game composites at, in pixels. In a pixel-art game this IS the resolution setting a player means — the window size is a separate question the engine does not answer yet.

### `app.retroIntegerScale`

app.retroIntegerScale() → whether the retro composite is upscaled by a WHOLE number and letterboxed rather than stretched to fill.

### `app.setDisplayMode`

app.setDisplayMode{ width =, height = [, refresh =, monitor =, fullscreen = ] } — set the resolution, refresh rate and monitor. fullscreen = true switches that monitor to that mode (exclusive full screen), so it must be one app.displayModes lists — otherwise it RAISES naming the sizes the monitor has; refresh left out takes the highest at that size. fullscreen = false makes a window that many pixels, centred on the monitor; any size. Left out, the game stays as full-screen or windowed as it is. monitor is an index from app.monitors() (default: the one the window is on). A browser covers the screen instead of switching modes. Only a game's own window: in the editor it says so once in the Console and leaves the editor alone. For this session; persist the player's choice with save.set.

```lua
-- a Resolution dropdown: list what the screen offers, apply a pick
local modes = app.displayModes()
local m = modes[1]
app.setDisplayMode{ width = m.width, height = m.height, refresh = m.refresh, fullscreen = true }
```

### `app.setDynamicResolution`

app.setDynamicResolution{ targetMs =, min =, max = } — let the engine choose the render scale each frame from how long the GPU took, so a view that costs more renders fewer pixels instead of missing the frame. targetMs is the GPU time a frame should fit in (default: 90% of the frame's period, from the frame cap or else the display); min and max bound the scale (default 0.5 and 1). It steps down 5% at a time as soon as a frame overruns, sized so one step usually covers it, and back up only after a second and a half of frames well inside the budget, so it settles rather than hunting. app.renderScale() answers the scale it chose. true turns it on with the defaults, false turns it off and leaves the scale where it is, and app.setRenderScale turns it off too — a scale chosen outright is the game's decision. Refuses: an unknown key, min above max. For this session only; persist it with save.*.

### `app.setFrameCap`

app.setFrameCap(72) — draw no more than this many frames a second, on a steady clock; 0 removes the cap. A rate the game can always make reads smoother than a higher one it keeps missing: on a 144 Hz display a game that flips between 144 and 72 frame to frame judders by metres at speed, and one capped to 72 does not. Pick it from app.refreshRate() — half of it, or a third — so every frame lands on a refresh. 10–1000, or 0; outside that RAISES. Headless runs are never capped. For this session only; persist it with save.*.

### `app.setFullscreen`

app.setFullscreen(true) — cover the screen (borderless, on the monitor the window is on; no mode switch), or false to go back to a window. In an EXPORTED BUILD it applies this frame. In the EDITOR the window is the editor's, so it is left alone and the Console says so once — the same honesty app.quit() has about where it is running. For this session only; persist it with save.*.

### `app.setRenderScale`

app.setRenderScale(0.67) — render the 3D scene at that fraction of the window and upscale it smoothly; the game's UI stays at full resolution, so text stays sharp. The one setting that rescues a weak GPU: on one shipped level every lighting feature together was worth 9% of the GPU time and 0.67 was worth 44%. 0.25–1; outside that RAISES. Independent of retro mode (which, when on, decides the resolution itself). For this session only; persist it with save.*.

### `app.setRenderSharpness`

app.setRenderSharpness(0.4) — how hard the upscale from a render scale below 1 sharpens, 0..1. Contrast-adaptive: edges get crisper, flat sky and gradients do not turn to grain. 0 is a plain bilinear stretch. A retro upscale (hard pixels) is never sharpened. Refuses: outside 0..1.

### `app.setRetro`

app.setRetro(false) — turn the retro presentation off (or on), so the game composites at the window's own resolution instead. For this session only; persist it with save.*.

### `app.setRetroHeight`

app.setRetroHeight(360) — set the internal height the game composites at, 32–4320 pixels. Outside that range RAISES rather than clamping, the way access.setTextScale does: a settings slider hands over a number it already bounded. For this session only; persist it with save.*.

### `app.setRetroIntegerScale`

app.setRetroIntegerScale(true) — upscale the retro composite by a whole number and centre it, letterboxing the remainder. A fractional upscale puts some source rows on two screen pixels and some on three, which on pixel art with a small font is the difference between crisp and mush. For this session only; persist it with save.*.

### `app.setVsync`

app.setVsync("Adaptive") — set the frame pacing. "On" is classic vsync; "Adaptive" renders freely and lets the display take the newest frame; "Off" presents the instant a frame is ready, tearing and all. A name it does not recognise RAISES and lists the three — a control that silently kept the old value is a control that appears to work. For this session only: Stop puts the project back, so persist the choice with save.*.

### `app.title`

app.title() — the game's title, for a menu to put at the top of itself. The export manifest's title in a build, the project's otherwise, and the project folder's name when neither is set — the same chain the window title uses, so a menu and the title bar never disagree.

### `app.version`

app.version() — the engine version this build was made with, e.g. "0.81.0". For an about box, and for a bug report to quote.

### `app.vsync`

app.vsync() → the current frame pacing as a name: "On", "Adaptive" or "Off" — the same names project.ron uses, so a Video tab can show what is set and save it straight back.

### `app.wallClock`

app.wallClock() → seconds on a monotonic wall clock (from an arbitrary start; only differences mean anything). For timing and time-slicing your own work, e.g. stop generating once 2 ms of this frame are spent. os.clock is CPU time summed over every thread of the engine, which runs faster than the wall, and `time` stands still within a frame.

### `boxcast`

boxcast(origin, dir, halfX, halfY, max [, opts]) → the first hit, or nil — sweep a box held in the XY plane, the shape a 2D body is, along a direction: "will my player fit through there", "is there a ledge a step ahead". The box is swept by its edges, so a low step a ray from its middle would pass over is still met, and a box resting on the floor can sweep along it without hitting it. The hit has the same fields as spherecast's: point, normal, distance, node. opts: ignore = {nodes}, layers = {names}, ignoreStart = true.

### `cloud`

Floptle Cloud read as the GAME, with the key in project.ron: cloud.avatar for a player's picture, cloud.config for your remote config, cloud.get for any public read under /games/ or /players/. Reads only; what a player does goes through account.*.

### `cloud.avatar`

cloud.avatar(playerId [, size], function(tex, err) end) — a player's Foverse picture as a texture, playerId being net.identity(peer).id or account.player().id. size is 64, 128 (default) or 256; the picture is square. err is "no_picture" when the player has none: draw your own stand-in. tex works anywhere a texture path does; a UI image with radius = half its size draws round. Loaded once per session per player and size; assets.release(tex) lets it go. Refuses: a size other than 64/128/256; a project not connected to Cloud.

### `cloud.blobs`

cloud.blobs(name [, { private = true }]) — a collection of files (bytes): :list(opts, cb), :get(key, function(bytes, err) end), :put(key, bytes [, { ifVersion }], cb), :delete(key, cb). Refuses: a :put that is not a string.

### `cloud.config`

cloud.config(function(config, err) end) — the remote config you publish from the game's page on fopull.com: a table of your keys, {} when none are set, or nil and why. Changed on the page, live in every copy of the game without a new build. Refuses: a project not connected to Cloud.

### `cloud.counter`

cloud.counter(name) — atomic counters: :add(key [, n], cb) as the player (a unique counter counts each player once; -1 takes it back), :get(key, cb) → {key, value, mine}, :top({ prefix, sort, limit }, cb). A counter collection must be declared in cloud_collections.ron first.

### `cloud.docs`

cloud.docs(name [, { private = true }]) — a collection of JSON documents: :list({ prefix, owner, sort, after, limit }, cb), :get(key, cb), :put(key, data [, { ifVersion }], cb), :delete(key, cb). Reads with the game key (as the player for a private collection); writes as the player. ifVersion = 0 claims a new public key without racing another player; err.code = "key_taken" or "version_conflict" when that loses.

### `cloud.game`

cloud.game() -> string|nil — this project's game slug (what /games/<slug>/… addresses), or nil when the project is not connected to Floptle Cloud.

### `cloud.get`

cloud.get(path, function(res) end) — read from Floptle Cloud as the game: the engine attaches the project's game key, so it works with nobody signed in. Paths under /games/… and /players/… only, GET only (a game key never writes). res as http.get: res.json when the reply is JSON; a blob comes back as its bytes in res.body, with no res.json. Play only, same rate limits as http.*. Refuses: any other path; a project not connected to Cloud.

### `cloud.rank`

cloud.rank(board) — a leaderboard by name ("laps" or "laps:canyon"; its collection is the part before the first ':'). :page({ limit, offset, around, player, sort }, function(page, err) end) reads with the game key → {entries, total, keep, next}; :submit(value [, { meta, blob }], function(r, err) end) as the player → {board, value, best, kept, rank}; :remove([playerId], cb) takes an entry down (default: the signed-in player's). err is { code, message, status } for every cloud call. Refuses: a non-finite value, an unknown option.

### `createNode`

createNode(name [, parent] [, fn]) — create a PLAIN node (Empty matter). It does NOT return the node: the create is queued and the node is made after this pass, so the handle arrives ONLY through the callback — `local n = createNode("Stain")` gives you something that says so the moment you touch it, rather than a nil that fails a line later. fn(n) gets its handle: combine with n:setTerrain(id) / n:setCelestial{...} / n:setPrimitive(shape, color) / n:setMaterial{...} + transform writes to build content from script (procgen, editor actions). Nested creates inside callbacks are fine.

### `data`

data.deflate / data.inflate and data.base64Encode / data.base64Decode — bytes made small, and bytes carried as text. A game that saves a picture or a level in JSON no longer writes its own compressor in Lua.

### `decals`

Pictures laid on the world's static surfaces: blood, scorch marks, bullet holes, paint. A decal folds over edges and into corners, is lit like the surface under it, and is not a node; every decal sharing a picture is one draw. decals.add lays one, decals.set fades or tints it, and the engine keeps the budget (decals.setMax).

### `decals.add`

decals.add{ texture, pos, normal, size [, height, depth, up, rotation, color = {r,g,b}, alpha, sheetCols, sheetRows, cell, layers, maxAngle, frame] } → id — lay a picture on the static surfaces under a box standing at pos, facing normal (a raycast hit's point and normal). It folds over edges and into corners, follows curves, and is lit, shadowed and fogged like the surface. No node: the decals sharing a picture are drawn in pages of 16, one mesh and one draw a page, and a change rebuilds only its own page. A mark is laid a piece (8 m) at a time while the frame's budget of about 2 ms lasts, and the rest is carried over to later frames, so a big mark gets its id at once and lands a few frames later (decals.landed(id) says when). nil when nothing under it can take a mark (the sky, or a sphere or capsule collider, which has no faces). maxAngle (default 100°) is how far a face may turn from normal and still take it. A mark rides what it was laid on as that moves: frame = a node (a ship), or by default the planet on rails whose sphere of influence it is in. When the ground under a mark is dug or re-meshed it is laid again on the surface there (a few marks a frame, counted in perf's terrain bucket), and goes if nothing is left to lie on. A mark never lies on the collider of a node that draws nothing (an Empty with a box collider), only on surfaces that show. Cleared by a new Play or a scene switch.

### `decals.clear`

decals.clear() — take every decal off the world.

### `decals.count`

decals.count() — how many decals are in the world.

### `decals.landed`

decals.landed(id) — true once the mark is laid, false while it is still being laid, nil when there is no such mark (removed, trimmed, or it found nothing to lie on). A mark is laid a piece of its box at a time while the frame's budget of about 2 ms lasts, the rest carried over to later frames, so a 29 m crater over finely meshed terrain (tens of milliseconds of work) lands over a few frames instead of stalling one.

### `decals.max`

decals.max() — the budget decals.setMax set (1024 by default).

### `decals.remove`

decals.remove(id) — take one decal off; false if it had already gone.

### `decals.set`

decals.set(id, { alpha = 0.5, color = {r, g, b} }) — change a decal's alpha or tint. false if it has already gone (removed, or pushed out by the budget).

### `decals.setMax`

decals.setMax(n) — the budget: past n decals the oldest goes as each new one arrives. 1024 until set, at most 65536.

### `defaults`

defaults = { name = value } — tunables shown in the Inspector.

```lua
defaults = {
  --@header Movement
  -- How fast you walk on flat ground.
  --@range 0 20 --@units m/s
  walk = 4.5,
  --@options Off|On|Auto
  assist = 1,
  invert = false,
}
```

### `destroy`

destroy(node) — remove a node AND its whole subtree (physics body included). Queued: applied after the pass, so the handle stays readable through the current call. Method form: node:destroy(). On a client, replicated nodes refuse (server authority — net.despawn). Any node can be destroyed, including an empty root such as one assembly.split makes; a handle to a node already gone logs a warning and removes nothing.

### `dt`

Seconds since the last frame (number).

### `filesDropped`

function filesDropped(node, files) — files the player dropped on the game's window during play, with files[i] = { name=, bytes= }: the file's name and its bytes (a string), never its path, each at most 64 MB. Every script that defines it hears it. A file that cannot be read, or is larger, is left out and named in the Console. Windows, macOS and X11 deliver drops; a window on native Wayland gets none, and the browser does not yet.

### `fixedUpdate`

function fixedUpdate(node, dt) — runs every GAMEPLAY TICK (60 Hz, constant dt). Movement/gameplay/physics writes belong here; cameras & followers in lateUpdate; other cosmetics in update. Same cadence physics steps at — frame-rate independent.

```lua
-- gameplay writes belong on the tick, not the frame
function fixedUpdate(node, dt)
  node.vel = node.vel + vec3(0, -9.8, 0) * dt
end
```

### `function`

Define a function.

### `http`

Talk to a web server: http.get / post / put / delete, plus json.*. Every call is asynchronous — the reply arrives in a callback, never as a return value.

### `json`

json.encode(t) and json.decode(s) — the wire format for http.*. decode returns nil, message on bad input rather than raising, because a reply from someone else's server is data, not a bug in your script.

### `lateUpdate`

function lateUpdate(node, dt) — runs once per frame AFTER physics and the interpolated transform writeback: the CAMERA pass. Anything that follows something else (orbit cameras, name tags, listeners) belongs here so it samples this frame's FINAL poses. Following from update reads LAST frame's pose — a velocity × dt lag that turns frame-time noise into visible jitter. Animator pose ops queued here (reach, lookAt, setBoneRot, …) land on this frame's pose too. Every node's update has run before the first lateUpdate. Between nodes lateUpdate runs in the same order update does, which a removed node can change; when one follower must come after another (an atmosphere after the camera that sets it up), call it from that script rather than counting on scene order.

```lua
-- follow AFTER physics, so the camera samples this frame's final pose
function lateUpdate(node, dt)
  local t = find("Player")
  node.pos = t.pos + t.forward * -6 + vec3(0, 2, 0)
end
```

### `local`

Declare a local variable.

### `log`

log("message") — print to the engine console.

### `node`

The node's transform: x/y/z, scale, scale_x/y/z, yaw/pitch/roll.

### `obstacle`

A navmesh obstacle handle, from nav.obstacle(centre, size). Fields: id, active, position, size (the hole ACTUALLY cut, grown out to whole cells). One method: ob:remove(), which gives the ground back and returns false if it had already gone.

### `obstacle:remove`

obstacle:remove() — give the ground back where this obstacle was, and return whether it was still there. Everything that gave up on a route through it is un-blocked, so a unit that stopped beside the crate starts walking again. Calling it twice is false, not an error.

### `onCollisionEnter`

function onCollisionEnter(node, other, hit) — fires the tick this node's body STARTS touching something solid (a collider or another body). `other` = the other node's handle (check other:hasTag("...") / other.name); hit = { x, y, z, nx, ny, nz } (world contact point + normal). Also onCollisionStay (every tick while touching) and onCollisionExit (on separation).

```lua
function onCollisionEnter(node, other)
  if other:hasTag("hazard") then hp = hp - 10 end
end
```

### `onCollisionExit`

function onCollisionExit(node, other, hit) — fires the tick the touch ends (hit = the last known contact).

### `onDestroy`

function onDestroy(node) — runs once when this script's node goes: destroy() on it or an ancestor, a scene.unload of its layer, a scene switch (unless it is persistent). The node is still there, so read what you need and destroy() anything you made outside your own subtree. Not when the script is merely switched off, nor when you press Stop. A node removed without warning (deleted in the editor during Play) is told afterwards: its fields then read nil, but your script's own variables are all still there.

### `onTriggerEnter`

function onTriggerEnter(node, other, hit) — fires the tick a body enters a TRIGGER (the "trigger" switch on a Collider or Rigidbody: it stops blocking, events still fire — a Kinematic trigger rigidbody = a moving pickup). The portal/pickup/checkpoint hook — pair with a string param: scene.load(params.destination). Also onTriggerStay / onTriggerExit.

### `onTriggerExit`

function onTriggerExit(node, other, hit) — fires the tick a body leaves the trigger.

### `overlapBox`

overlapBox(center, halfX, halfY [, opts]) — everything a box in the XY plane overlaps, deepest first: the level's colliders and other bodies, each once. A hitbox, a "can I stand up here", a trigger zone made on the spot. opts: ignore = {nodes}, layers = {names}.

### `params`

This instance's tunables, a table seeded from `defaults` (params.speed, …). NUMBERS and STRINGS both work — a string default (destination = "arena") becomes an Inspector text field, so two portals can share one script with different destinations. TWO-WAY: writing a declared key persists across frames, shows live in the Inspector during Play, and is readable by other scripts through a handle (Stop reverts it). Undeclared keys stay frame-local; reference params (noderef & friends) never round-trip.

```lua
function update(node, dt)
  node.x = node.x + params.walk * dt   -- Inspector-tuned
end
```

### `perf`

Where YOUR frame time goes — per subsystem and per script, readable from Lua so a game can assert its own budget in a smoke test rather than filing an engine ticket. Off by default and free while off: call perf.enable(true) first. Every getter RAISES while collection is off rather than answering 0, because a budget assertion that passes on no data is worse than no assertion.

### `raycastMany`

raycastMany(origins, dirs, maxes [, opts]) — many rays in one call: origins and dirs are arrays of vec3s (or flat {x1,y1,z1, ...} arrays), one of each per ray; maxes is one distance or a list of one per ray; opts are raycast's ({ ignore=, layers= }). Returns one entry per ray, in order: raycast's hit table, or false for a miss, so #hits is the ray count. For probes known up front: a decal's corners, a ragdoll's contacts. Refuses: origins and dirs of different lengths, a flat list that is not x,y,z triples, an unknown option or layer.

### `script`

Turning a script down when nobody needs it: script.sleep stops the engine calling its hooks, script.setRate runs its fixedUpdate less often. A level with dozens of NPCs pays for every one of them every tick unless the far ones are asleep.

### `script.asleep`

script.asleep(node, kind) → true while that script is asleep (script.sleep).

### `script.setRate`

script.setRate(hz) — run this script's fixedUpdate hz times a second instead of every tick, handing it the time since its last call as dt; nil or 0 goes back to every tick. script.setRate(hz, node, kind) sets another's. Nodes are staggered, so a crowd at 10 Hz doesn't all think on the same tick. For NPCs far enough away that nobody sees them react late.

### `script.sleep`

script.sleep() — stop calling this script's update, fixedUpdate and lateUpdate on this node until it is woken. The engine skips it entirely, so a sleeping NPC costs nothing a frame. Wakes on script.wake(node, kind), on a collision or trigger event on its node (the event is delivered, and wakes it), or on its own terms: script.sleep{ seconds = 2 } wakes after that much game time, script.sleep{ wakeWithin = 40 } once the active camera is within 40 m. Calling one of its functions from another script does NOT wake it — call script.wake there. Called from start(), the same frame's update is skipped too.

### `script.wake`

script.wake(node, kind) — wake that script on that node; script.wake(node) wakes every script on it. Harmless on one that is awake.

### `spawn`

spawn(prefab [, pos [, fn]]) — spawn a PREFAB instance (make one by dragging a node into the Assets panel). "bullet" finds prefabs/bullet.prefab.ron. pos = a vec3/node for the root; fn(root) runs with the new node's handle the same frame. It does NOT return the node — the spawn is queued and the node is made after this pass, so the handle only ever arrives through the callback; `local b = spawn("bullet")` then tells you so if you touch it — spawn("bullet", node.pos + dir, function(b) b.vx = dir.x * 40 end). A 4th argument is a parent node, or an options table { parent = node, body = false }: body = false spawns the prefab's nodes, meshes and scripts without its bodies and colliders, a display copy that costs physics nothing. Local-only in multiplayer: the server uses net.spawn for replicated objects.

```lua
local b = spawn("Bullet", node.pos + node.forward * 1.5, function(n)
  n.vel = node.forward * 40
end)
```

### `spawnBudget`

spawnBudget([ms]) → the budget it replaced (nil for none) — the milliseconds of each frame the engine spends making spawned prefabs; past it, the rest wait in order for the frames after, and each one's callback runs as it is made. At least one is made a frame. nil (the default) makes every spawn the frame it was asked for. A thousand-part craft or a city block spread over frames instead of stopping one. Clamped to 0.5..1000; spawnBudget(nil) clears it, back to every spawn the frame it was asked for, and with no argument it only reads it. Reset when Play stops or the scene changes.

### `spawnDone`

spawnDone(fn) — call fn once every spawn asked for so far has been made and its callback run: the end of a build spread over frames by spawnBudget (or the same frame, without one). Each fn is called once; 256 can wait at a time.

### `spawnPending`

spawnPending() → how many spawns asked for have not been made yet: queued this pass, or held back by spawnBudget. 0 means every spawn so far exists.

### `start`

function start(node) — runs once when play begins.

### `steam`

Steam integration: identity, app/build info, and steam.available() for branching. Always present so steam.available() is always safe to call — nil (not an error) is what every other steam.* getter answers when it's false. See the Steam integration plan.

### `steam.achievementDescription`

steam.achievementDescription(id) — id's display description, in Steam's own current language.

### `steam.achievementGlobalPercent`

steam.achievementGlobalPercent(id) — the percentage of players globally who've unlocked id, once Steam has it cached; nil before then.

### `steam.achievementName`

steam.achievementName(id) — id's display name, in Steam's own current language.

### `steam.achievementUnlocked`

steam.achievementUnlocked(id) — true/false, or nil if stats aren't ready or id isn't a real achievement (check it against the Steamworks App Admin — a mistyped id is the single most common cause).

### `steam.available`

steam.available() — true only for a floptle run/exported/served session with a real Steam client initialized. false in the editor's own docked Play-mode viewport, in every other session, and whenever no Steam client is running — branch on this before any other steam.* call, none of which raise when it's false (they answer nil).

### `steam.betaName`

steam.betaName() — the beta branch this build was installed from. nil on the default branch, and nil when steam.available() is false.

### `steam.buildId`

steam.buildId() — this build's Steam build id. nil when steam.available() is false.

### `steam.clearAchievement`

steam.clearAchievement(id) -> ok, err — resets id to locked, locally. Same batching as steam.unlockAchievement. Refuses: the achievement id is not one this app declares on Steamworks; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.clearRichPresence`

steam.clearRichPresence() — clears every rich-presence key set via steam.setRichPresence.

### `steam.cloudDelete`

steam.cloudDelete(name) -> ok, err — deletes name locally AND remotely. Refuses: no file of that name in this app's Cloud; Cloud is off for the app or for the player; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.cloudEnabled`

steam.cloudEnabled() — whether Cloud is enabled for THIS app specifically (independent of the account-wide setting). nil when steam.available() is false.

### `steam.cloudEnabledForAccount`

steam.cloudEnabledForAccount() — whether Cloud is enabled account-wide. Read-only: a player controls this from the Steam client itself, not from inside a game.

### `steam.cloudFileExists`

steam.cloudFileExists(name) — whether name exists in Cloud storage. It needn't exist to be named in any other steam.cloud* call — steam.cloudWrite creates it.

### `steam.cloudFileTimestamp`

steam.cloudFileTimestamp(name) — name's last-write time (Unix seconds), or nil if it doesn't exist. Compare against your own local save's modification time to build your own conflict policy — Steam Cloud has no built-in one to expose.

### `steam.cloudFiles`

steam.cloudFiles() — every file in Cloud storage for this app, as a list of { name, size } tables. nil when steam.available() is false.

### `steam.cloudForget`

steam.cloudForget(name) -> ok, err — deletes name from the Cloud while keeping the local copy, for a player who wants this one save to stop syncing without losing it. Refuses: no file of that name in this app's Cloud; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.cloudRead`

steam.cloudRead(name) -> data, err — reads name's full contents (a binary-safe Lua string), or nil, message on failure (not in Cloud storage, most commonly). Refuses: no file of that name in this app's Cloud; Cloud is off for the app or for the player; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.cloudWrite`

steam.cloudWrite(name, data) -> ok, err — writes data (a binary-safe Lua string) as name's full contents, replacing whatever was there and creating the file if it didn't exist. Refuses: the write would put this app over the player's Cloud quota; Cloud is off for the app or for the player; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.createLobby`

steam.createLobby(opts, cb) — creates a lobby and puts you in it. A lobby is DISCOVERY, not transport: it's how players find each other and agree what they're about to play, and it decides nothing about how the game's packets travel. opts (optional): kind = "public" (default; anyone can find it), "friendsOnly", "private" (invite only) or "invisible" (joinable by id but never returned by a search); maxMembers = 8 by default, 1 to 250. cb(lobby, err) on a later frame, exactly once — lobby is { id, memberCount, memberLimit, owner, data }.

### `steam.dlc`

steam.dlc() — every DLC this game declares on Steam, owned or not: { {id, name, available, owned, installed}, ... }. available = on sale (false for DLC Steam lists but keeps off the store). What a DLC screen is built from. nil without Steam.

### `steam.dlcInstalled`

steam.dlcInstalled(appId) — true if the player owns that DLC AND its content is installed. For DLC with no download of its own, owning it is installing it. nil without Steam.

### `steam.downloadScores`

steam.downloadScores(boardId, opts, cb) — downloads leaderboard rows. opts (optional): scope = "global" (default, ranks from the top), "friends" (only your friends) or "aroundUser" (ranks RELATIVE to your own — start = -4 with count = 9 gives you plus the four either side); start (default 1) and count (default 10). cb(rows, err) with rows a list of { userId, rank, score, details }; userId is a STRING and details is an empty list when none was uploaded.

### `steam.findLeaderboard`

steam.findLeaderboard(name, cb) — looks a leaderboard up by name. cb(board, err) runs on a LATER frame, exactly once, in every session — including when there is no Steam at all, where err says so. board is { id, name, entryCount, sort, display }, or nil with NO err when there simply is no board by that name (a normal answer to branch on, not a failure). board.id is a STRING and lasts only this session — find the board again next run rather than saving it.

### `steam.findLobbies`

steam.findLobbies(opts, cb) — searches for lobbies. opts (optional, all additive): match = a table of lobby data that must be equal (a string compares as a string, a whole number as a number); compare = numeric comparisons, each an operator and a value as in { skill = { ">=", 500 } } — takes ==, ~=, >, >=, <, <=; openSlots = only lobbies with at least this many free seats; distance = "close", "default", "far" or "worldwide"; maxResults. cb(lobbies, err) with a list of lobby tables — an empty list means nothing matched, which is not an error.

### `steam.findOrCreateLeaderboard`

steam.findOrCreateLeaderboard(name, opts, cb) — like steam.findLeaderboard, but creates the board if it doesn't exist. opts (optional): sort = "descending" (default; higher is better) or "ascending" (lower is better, for times); display = "numeric" (default), "seconds" or "milliseconds". Creating boards this way is a development convenience — a shipping game's boards are normally declared on the Steamworks admin site, where they can also be reset and moderated.

### `steam.flushStats`

steam.flushStats() — sends every pending achievement/stat write to Steam now, instead of waiting for the automatic batch (every 5s while something's pending). Safe to call with nothing pending.

### `steam.friendRichPresence`

steam.friendRichPresence(id, key) — reads one of a FRIEND's own rich-presence values (id from steam.friends(), a string) — different from your own, which you set with steam.setRichPresence. nil if they haven't set it or aren't reachable.

### `steam.friends`

steam.friends() — the local user's friend list, as a list of { id, name, state, playingThisGame } tables. id is a STRING (a SteamID64 exceeds what an f64 represents exactly). state is one of "online"/"away"/"busy"/"snooze"/"looking to trade"/"looking to play"/"invisible"/"offline". nil when steam.available() is false. Group/clan membership isn't exposed — Steam doesn't wrap that.

### `steam.installDir`

steam.installDir() — this app's install directory, as Steam reports it. nil when steam.available() is false.

### `steam.installDlc`

steam.installDlc(appId) -> ok, err — asks Steam to download an owned DLC's content; steam.onDlcInstalled fires when it lands. Refuses: the player doesn't own it (open its store page with steam.openOverlayStore(appId)); Steam isn't available in this session.

### `steam.inviteFriend`

steam.inviteFriend(friendId, connect) -> ok, err — invites a friend (id from steam.friends(), a string) with a connect string of your own: their game gets it as steam.onJoinRequested { connect }, or is launched with it. Start it with + ("+join ABC123"): that is how a launch tells it apart from the game's own arguments. Refuses: the id isn't a friend id; the string is over 255 bytes or holds a NUL; Steam isn't available in this session.

### `steam.isBigPictureMode`

steam.isBigPictureMode() — true if Steam's own full-screen "10-foot" mode is active. nil when steam.available() is false.

### `steam.isCybercafe`

steam.isCybercafe() — true if Steam has flagged this as a cybercafe/shared-computer license. nil when steam.available() is false.

### `steam.isFamilyShared`

steam.isFamilyShared() — true if this app is being played on a license borrowed from another account (Steam Family Sharing), not one the signed-in user owns. nil when steam.available() is false.

### `steam.isSteamDeck`

steam.isSteamDeck() — true if this session is running on Steam's own handheld hardware. Assume no physical keyboard/mouse when true. nil when steam.available() is false.

### `steam.joinLobby`

steam.joinLobby(id, cb) — joins a lobby by id (a string). cb(lobby, err) exactly once, on a later frame. When it fails, err is a real reason where Steam gives one — "that lobby is full", "you're banned from that lobby", "that lobby no longer exists" — worth showing the player as-is.

### `steam.leaderboardsInFlight`

steam.leaderboardsInFlight() — how many leaderboard requests have been made and not yet called back. What a "loading scores…" spinner hangs off. Pressing Stop drops every pending callback, so this returns to 0.

### `steam.leaveLobby`

steam.leaveLobby(id) — leaves a lobby. Safe to call when you aren't in it, and safe when there's no Steam; never raises.

### `steam.lobbiesInFlight`

steam.lobbiesInFlight() — how many lobby requests have been made and not yet called back. Pressing Stop drops every pending callback, so this returns to 0.

### `steam.lobbyData`

steam.lobbyData(id, key) — one of the lobby's own data values, or nil. Called with NO key, steam.lobbyData(id) answers the whole table at once, which is what a lobby browser wants. nil when there's no Steam.

### `steam.lobbyMemberData`

steam.lobbyMemberData(id, memberId, key) — one of THAT member's own values in this lobby (their character, their ready flag), or nil. Steam cannot tell a key set to "" from one never set, so both answer nil.

### `steam.lobbyMemberLimit`

steam.lobbyMemberLimit(id) — the most members the lobby will hold, or nil.

### `steam.lobbyMembers`

steam.lobbyMembers(id) — everyone currently in the lobby, as a list of id STRINGS (a Steam id exceeds what an f64 represents exactly). nil when there's no Steam.

### `steam.lobbyOwner`

steam.lobbyOwner(id) — the lobby's owner (the host) as an id string, or nil. Only the owner may change lobby data or open and close the lobby.

### `steam.localUserId`

steam.localUserId() — the signed-in local user's SteamID64, as a STRING (it exceeds what an f64 represents exactly). nil when steam.available() is false.

### `steam.onDlcInstalled`

steam.onDlcInstalled(fn) — fn(appId) runs when a DLC finishes installing, a purchase made during play included, so a game can unlock it without restarting. Not held when no handler is registered: steam.dlcInstalled already answers what is installed now. One handler; registering again replaces it.

### `steam.onJoinRequested`

steam.onJoinRequested(fn) — fn(req) runs when the player accepts an invite or clicks Join Game on a friend in Steam, and once for the invite the game was launched from. req is { lobby = id } (join it with steam.joinLobby) or { connect = "..." } (the string the other game advertised or sent), with friend = their id when Steam says. Ids are strings. A request that arrives before any handler is held until one registers, so register it in your first scene. One handler; registering again replaces it.

### `steam.onLobbyEvent`

steam.onLobbyEvent(fn) — fires for each thing that happens in a lobby you're in. e.kind is "member" — with e.user and e.change of "entered", "left", "disconnected", "kicked" or "banned" — or "data", with e.whose telling you whether the LOBBY's data or a MEMBER's changed, so re-read the right one. Who did the kicking is deliberately not reported: Steam's binding fills that field from the wrong id. One handler; registering again replaces it, and Stop clears it.

### `steam.onOverlayChanged`

steam.onOverlayChanged(fn) — fn(active) runs once per open (true) and close (false) of the overlay, in order, on the frame after it happened. Pause a single-player game on true and resume on false; a networked session keeps running regardless. One handler; registering again replaces it, and Stop clears it.

### `steam.onPersonaChanged`

steam.onPersonaChanged(fn) — fires once when the local user's persona (name or avatar) changes. Re-read steam.personaName() from inside it; avatars aren't exposed to Lua yet (Steam hands out raw pixels, not an image file, so they need a binding of their own — see the Steam integration plan).

### `steam.openInviteDialog`

steam.openInviteDialog(lobbyId) -> ok, err — opens Steam's invite-friends dialog for a lobby you're in (the id from steam.createLobby / joinLobby). A friend who accepts gets steam.onJoinRequested in their own game with this lobby's id, and joins it there. (false, why) when the overlay can't open: show the lobby code instead. Refuses: the id isn't a lobby id — pass the `id` from a lobby table, as a string; the overlay cannot open; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.openOverlay`

steam.openOverlay(page) -> ok, err — opens one of the overlay's own pages: "friends", "community", "players", "settings", "officialgamegroup", "stats" or "achievements". A misspelt page is refused with the list, in EVERY session — you don't need Steam running to find the typo. (false, why) when the overlay can't open, where Steam's own call would silently do nothing. Refuses: the page name is not one of the seven listed — refused with the list in EVERY session, so a typo is found without Steam running; the overlay cannot open (it is switched off in Steam's settings, or this renderer is not hooked into it); Steam isn't available in this session (no client running, or a session that never had one).

### `steam.openOverlayStore`

steam.openOverlayStore([appId]) -> ok, err — opens a store page in the overlay: your own game's with no argument, another app's (a DLC's) with its id. When it can't (false, why): this is the purchase flow to degrade rather than break — tell the player where to look. Refuses: the app id is not a positive number; the overlay cannot open; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.openOverlayUrl`

steam.openOverlayUrl(url) -> ok, err — opens the overlay's web browser at a full http:// or https:// URL. When it can't (false, why): show the URL on screen instead, so the player can still get there. Refuses: the URL is not http(s); the overlay cannot open; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.openOverlayUser`

steam.openOverlayUser(dialog, userId) -> ok, err — opens a page about one user (their id as a string, e.g. from steam.friends()): "steamid" is their profile; also "chat", "jointrade", "stats", "achievements", "friendadd", "friendremove", "friendrequestaccept", "friendrequestignore". Same refusals as steam.openOverlay. Refuses: the dialog name is not one Steam knows; the user id is not a number in a string; the overlay cannot open; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.overlayActive`

steam.overlayActive() — true while the overlay is being shown over the game (Shift+Tab, or one of your own opens). nil when steam.available() is false — which is falsy, so `if steam.overlayActive() then` is safe in every session. The engine already feeds scripts neutral input while it's up; use this (or steam.onOverlayChanged) to pause a single-player game.

### `steam.overlayEnabled`

steam.overlayEnabled() — true once the Steam overlay has hooked this game and can open; false while it's still attaching at startup, when the player has it disabled in Steam's settings, or on a setup where it can't inject (some Linux/Proton configurations). nil when steam.available() is false. Every steam.openOverlay* call answers (false, why) in the same situations, so you rarely need to poll this yourself.

### `steam.ownsDlc`

steam.ownsDlc(appId) — true if the player owns that DLC (its Steam app id, a number). Asks the Steam client on this machine, not the network, so call it whenever you need the answer; nothing to cache. nil when steam.available() is false — falsy, so `if steam.ownsDlc(id) then` is safe everywhere.

### `steam.personaName`

steam.personaName() — the local user's current display name. nil when steam.available() is false.

### `steam.resetAllStats`

steam.resetAllStats(achievementsToo) -> ok, err — wipes every stat, and every achievement if achievementsToo. Development/QA only — never call this from a shipping build's own normal logic. Refuses: Steam isn't available in this session (no client running, or a session that never had one).

### `steam.screenshot`

steam.screenshot() -> ok, err — takes a screenshot into the player's Steam library, as their screenshot key would. A photo mode's shutter button. Refuses: Steam isn't available in this session.

### `steam.setCloudEnabled`

steam.setCloudEnabled(enabled) -> ok, err — toggles steam.cloudEnabled(). Refuses: the player has turned Cloud off for this app in their own Steam settings, which no game may override; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.setLobbyData`

steam.setLobbyData(id, key, value) -> ok, err — sets one of the lobby's own data values; this is what a lobby search matches against. Passing NO value deletes the key. ONLY THE OWNER may change lobby data, and err says so rather than failing quietly. Refuses: the id isn't a lobby id — pass the `id` from a lobby table, as a string; you are not the lobby's OWNER (only the owner may write lobby data); Steam isn't available in this session (no client running, or a session that never had one).

### `steam.setLobbyJoinable`

steam.setLobbyJoinable(id, joinable) -> ok, err — opens or closes the lobby to new members; close it when the match starts. Owner only. Refuses: the id isn't a lobby id — pass the `id` from a lobby table, as a string; you are not the lobby's OWNER; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.setLobbyMemberData`

steam.setLobbyMemberData(id, key, value) -> ok, err — sets one of YOUR OWN values in this lobby. Any member may set their own, unlike steam.setLobbyData which is owner-only. Refuses: the id isn't a lobby id — pass the `id` from a lobby table, as a string; you are not IN that lobby; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.setRichPresence`

steam.setRichPresence(key, value) -> ok, err — sets a rich-presence key for the local user, visible to friends in their friend list. Steam caps the number of keys and their length; err names the reason, not a bare failure. Refuses: more than 20 keys set, or a key or value past Steam's length limit (the whole set must fit in 8 KB); Steam isn't available in this session (no client running, or a session that never had one).

### `steam.setStatFloat`

steam.setStatFloat(name, value) -> ok, err — writes a float stat LOCALLY. Same batching as steam.unlockAchievement. Refuses: the stat id is not one this app declares on Steamworks, or it is declared as an integer; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.setStatInt`

steam.setStatInt(name, value) -> ok, err — writes an integer stat LOCALLY. Same batching as steam.unlockAchievement. Refuses: the stat id is not one this app declares on Steamworks, or it is declared as a float; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.statFloat`

steam.statFloat(name) — a float stat's current value, or nil.

### `steam.statInt`

steam.statInt(name) — an integer stat's current value, or nil before stats are ready / if name isn't real.

### `steam.statsReady`

steam.statsReady() — true once achievements/stats have finished loading from Steam. Every achievement/stat call below answers nil (reads) or false with a message (writes) before this, rather than guessing.

### `steam.timelineEvent`

steam.timelineEvent{ title, description, icon, priority, ago, duration, clip } -> ok, err — marks a moment on Steam's game-recording timeline, so a player can find it and clip it. icon = a Steam built-in ("steam_marker" by default, "steam_death", "steam_combat", …) or one uploaded on the Steamworks site. priority 0–1000 picks between overlapping events. ago = how many seconds back it started; duration = how long it lasted (0 = an instant). clip = "none" (default), "standard" or "featured" (a highlight, offered first). Refuses: a Steam client too old for game recording; Steam isn't available in this session. A missing title, an unknown key or clip name, or a number out of range is an error.

### `steam.timelineMode`

steam.timelineMode(mode) -> ok, err — what the player is doing, which colours Steam's game-recording timeline: "playing", "staging" (a multiplayer lobby), "menus" or "loading". Set it as the game moves between them. Refuses: a Steam client too old for game recording; Steam isn't available in this session. An unknown mode is an error.

### `steam.timelineState`

steam.timelineState(text) -> ok, err — describes where the player is now ("Level 3 — the tower") on Steam's game-recording timeline, replacing the last description; steam.timelineState(nil) clears it. Refuses: a Steam client too old for game recording; Steam isn't available in this session.

### `steam.uiLanguage`

steam.uiLanguage() — Steam's own UI language right now (e.g. "english", "french") — a reasonable default for your own localization. nil when steam.available() is false.

### `steam.uninstallDlc`

steam.uninstallDlc(appId) -> ok, err — asks Steam to remove an installed DLC's content. Refuses: the player doesn't own it; Steam isn't available in this session.

### `steam.unlockAchievement`

steam.unlockAchievement(id) -> ok, err — unlocks LOCALLY (cheap, in-memory); reaches Steam's server and triggers its own unlock notification on the next automatic batch or steam.flushStats(). err is nil on success, an actionable message (e.g. an unknown id) otherwise. Refuses: the achievement id is not one this app declares on Steamworks; Steam isn't available in this session (no client running, or a session that never had one).

### `steam.uploadScore`

steam.uploadScore(boardId, score, opts, cb) — uploads a score for the local user. opts (optional): method = "keepBest" (default — keeps whichever score ranks better) or "forceUpdate" (overwrite even with a worse one, for a "most recent run" board); details = a list of whole numbers stored alongside the score (ghost data, a replay seed), up to 64. cb(res, err) with res = { score, changed, rank, previousRank } — score is what is STORED, which under keepBest is not necessarily what you uploaded, and changed is how a "new personal best!" banner knows.

### `time`

Seconds since play started (number). The pass's time in every script, set before any script runs, so a function of another script called through findScript reads the same value. update/lateUpdate see the frame clock and fixedUpdate the tick clock; after a hitch or a long load the two still agree to within one tick.

### `ui`

Screen UI from scripts: ui.on / ui.events for input, ui.bind for data, ui.make for whole trees. See the game-UI section for the full set.

### `update`

function update(node, dt) — runs every frame while playing. Each frame every node's update runs before any node's lateUpdate. Between nodes, the order is the order they were loaded or created, but removing a node can move another into its place, so never rely on one node's update running before another's: put the one that must run after in lateUpdate, or have one script call the other's function.

```lua
function update(node, dt)
  node.yaw = node.yaw + math.rad(90) * dt
end
```

### `voice`

Proximity voice chat: the microphone, and every remote player's voice as an ordinary spatial sound. A remote speaker plays through a mixer track like anything else, so a track carrying PitchShift + Distortion + Reverb turns the killer into a monster with no new audio API. Frames are Opus (48 kHz, 20 ms, ~24 kbps mono) on unreliable datagrams: loss shows as a gap the decoder conceals, never as a stall. WHO HEARS WHOM IS THE SERVER'S DECISION (voice.setForward) — attenuating a stream a client already received is a volume slider a modified client turns back up.

### `voice.attach`

voice.attach(peer, node, { mode = "Spatial", falloff = "Inverse", minDistance = 2, maxDistance = 22, track = "Voice", sends = { Room = 0.4 }, lowpass = 850 }) — a remote player's voice comes out of that node and follows it. The same knob set audio.play takes, because a remote speaker IS an ordinary spatial sound: sends put it partly in a reverb bus (one stream, heard dry and wet at once), lowpass muffles it through a wall. A mode or falloff the engine does not know raises, naming what it takes. Survives scene.load: the stream lives with the SESSION, so a server switching maps does not cut anyone off mid-sentence — re-attach in the new scene and the stream never restarted.

### `voice.detach`

voice.detach(peer) — the voice stops following a node and goes back to being unpositioned. The stream keeps running.

### `voice.device`

voice.device() — the input device currently open, or nil.

### `voice.devices`

voice.devices() — input device names, for a settings screen. EMPTY on a machine with no microphone, which is most machines and is not an error: every other voice call goes on working as a no-op.

### `voice.level`

voice.level() — the local microphone's level, 0..1, for a settings-screen meter. LIVE WHETHER OR NOT TRANSMIT IS ON, which is the point: a player can prove their mic works without joining a lobby to find out. 0 with no device.

### `voice.mute`

voice.mute(peer, on) — a LOCAL mute: one player's choice not to listen, which never leaves this machine. It stops the decode as well as the sound, so a muted speaker costs nothing.

### `voice.muted`

voice.muted(peer) — has this machine muted that peer locally?

### `voice.setAutoGain`

voice.setAutoGain(on) — level this machine's microphone toward a steady speaking level before it is sent: a quiet mic is raised (up to +24 dB), a loud one lowered, and silence between sentences is never pumped up. On by default; off sends the microphone at voice.setInputGain alone.

### `voice.setDevice`

voice.setDevice(name) — open that input device; voice.setDevice(nil) opens the system default. A device that isn't there is one Console line, not a crash.

### `voice.setForward`

SERVER ONLY: voice.setForward(peer, { peers }) — who may hear that speaker; nil for everyone. THIS IS WHERE PROXIMITY VOICE IS ENFORCED. A game using it sets this from distance every tick or so, and a peer not on the list is never sent the audio at all — because attenuating a stream a client already received is a volume slider a modified client turns back up, and in a hidden-role game hearing someone is knowing where they are.

### `voice.setInputGain`

voice.setInputGain(db) — a fixed gain on this machine's microphone, before the leveller (or alone, with voice.setAutoGain(false)). 0 by default; a settings slider.

### `voice.setTransmit`

voice.setTransmit(on) — open or close the microphone. Push-to-talk is your decision (voice.setTransmit(input.action("Talk"))); this is where it takes effect. The gate is on the CAPTURE side, so nothing further down the path can leak a live mic into a lobby, and releasing it discards the half-built frame rather than splicing two moments together.

### `voice.sidetone`

voice.sidetone(on) — hear your own microphone. Off by default because it is disconcerting; it plays the raw capture rather than the encoded copy, so it confirms the mic is live without the codec's delay.

### `voice.source`

voice.source(peer) — a handle shaped like the one audio.play returns: :setTrack, :setVolume, :setPosition(node), :setMode, :setFalloff, :setMinDistance, :setMaxDistance, :setSend(track, level) (0 removes it), :setLowpass(hz) (0 = off). `.live` says whether that stream exists at all, so a game can tell "quiet" from "not in this session". Moving a peer between mixer tracks is how a voice becomes a monster: voice.source(peer):setTrack("Voice Monster").

### `voice.speaking`

voice.speaking(peer) — true while that peer's frames are arriving, for a HUD indicator. Holds for a moment past the last frame so it does not flicker between syllables.

### `voice.transmitting`

voice.transmitting() — is the microphone open right now?

## node — transform & body fields

### `node.asleep`

True while the physics solver is skipping this rigidbody. Set it true to put the body to sleep now: it stops where it is and costs nothing until something wakes it (a node.vel write, a moving platform, or its ground going away — a body slept in mid-air wakes and falls at the next check). Set false to wake it. Better than switching it kinematic, which changes how it behaves rather than whether it is simulated. For far-away NPCs, pair it with script.sleep. On an assembly root it is the whole craft's: a parked vessel falls asleep on its own once it has been still on the ground for half a second, and a held force, an impulse, a teleport, a push from something moving or its ground changing wakes it. Pieces lying against each other sleep too, and things held together by joints (a rover's chassis and its wheels) fall asleep together, once all of them have been still for half a second. On a part of an assembly it reads the craft's: true while the whole craft is asleep (put it to sleep or wake it from the root).

### `node.carry`

How far the world under this node was carried since the last pass, as a vec3 in world units (read-only): over this tick in fixedUpdate, over the ticks since the last frame pass in update and lateUpdate. On a planet moving on its rails it is how far that planet's frame moved (the frame the body's sphere of influence puts it in); zero when nothing moves. Dynamic bodies and assemblies are carried for you; a body a script positions itself (a kinematic tyre set on its mount) is not, so add node.carry to the position you give it, the way you use dt. Recentring the floating origin is not a carry. Rails bodies do not turn yet; a turning frame would add a rotation.

### `node.castShadow`

Whether the node casts sun shadows (read/write, default true) — the Inspector's casts-shadows checkbox. A body or collider's stand-in in the shadow march, or a mesh's baked shadow volume. Turn it off for bodies that are dormant or far away: the march takes the 32 casters nearest the camera, so a crowd of idle ones spends nothing once they are out of it.

### `node.dropThrough`

Let this rigidbody fall through one-way platforms for that many seconds; true means a quarter second. Write it when the player holds down and presses jump on a one-way ledge, and they drop through to the level below. A quarter second is long enough to clear a ledge one tile thick at walking speed. Write-only. Solid tiles still hold the body.

### `node.enabled`

Whether the node is switched on (read/write). node.enabled = false takes it and everything under it out of the game: not drawn, not simulated (its body or assembly stops where it is and leaves contacts, touches and rays; its static colliders leave physics and rays too; all of it comes back when the node does, and a body carries on from where it stopped), and its scripts stop running. A node that starts the scene switched off gets its body the first time it is switched on. A switched-off script keeps its state, so a call made into it through a handle is still there when it is switched back on. find() skips switched-off nodes unless asked with { scope = "all" }.

### `node.forward`

The node's facing as a vec3, from its OWN rotation (-Z forward, matching the camera), in its PARENT's space — the same space as node.pos, so node.pos = node.pos + node.forward * d moves it forward whatever its parent is doing. For the direction in the world (aiming, raycasts, a camera's right for panning), use node:worldForward(). On a node with no parent, or an unrotated one, the two are the same. Works on anything with a transform, body or not.

```lua
-- facing, from the node's rotation: -Z forward, +X right
local aim = node.forward
if raycast(node.pos, aim, 50) then log("something ahead") end
```

### `node.groundNormal`

The floor the body is standing on, as a vec3 normal — nil when airborne, so it is exactly node.grounded with the surface attached. Read-only. `node.groundNormal:dot(node.up)` is the cosine of the slope: 1 is flat, 0.5 is 60°. Align a character to the ground, judge a landing, or refuse to walk up something too steep.

### `node.grounded`

True while the rigidbody rests on a surface it can stand on (read-only). Gate jumps on it. A one-way platform counts once the body is standing on its top: not while it rises up through it, and not while node.dropThrough is letting it fall through. The same whether the engine applies the body's gravity or a script does.

### `node.height`

Capsule standing height — write a smaller value to crouch (the engine resizes it, feet planted).

### `node.id`

A stable numeric id for this node.

### `node.layer`

The node's collision/query layer, by project-defined NAME ("Default" when unset). Assign to move it (node.layer = "Ghosts") — a name the project doesn't define is an ERROR, so typos surface immediately. The Project Settings matrix decides which layers collide; a dynamic body re-layers live.

### `node.material`

Apply a material — assign a preset name ("Gold") or an assets.getFile("materials/X.ron").

### `node.model`

A Mesh node's model path — read it, or ASSIGN it to swap the model live (e.g. node.model = assets.getFile("models/x.glb")). Assigned to an Empty — what createNode makes — it turns the node into that model, which is how a script builds an object out of parts at runtime: createNode("Barrel", gun, function(n) n.model = "models/barrel.glb" end). On any other kind of node (a light, a camera, a primitive) it does nothing and the Console says so once. A model not loaded yet loads in the background, and the node draws nothing until it is in: the frame never stops for it. When it has to be there on the frame you assign it, load it earlier with assets.preload.

### `node.name`

The node's name (string). Writable: node.name = "Kestrel" renames it — the handle reads the new name back at once, the scene has it after the pass, and find("Kestrel") answers from the next pass. A non-string raises.

### `node.parent`

The parent node handle, or nil. A handle has the same fields (x/y/z, …) so you can read/write another node.

### `node.pitch`

Pitch about X, in radians. Reads back canonical (within ±pi/2); see node.yaw for why one-field writes can change a rotation, and node:setAngles / node:setUp for the whole-rotation setters.

### `node.pos`

The node's position as a vec3 (read/write): node.pos = node.pos + dir * dt. Accepts anything with x/y/z.

```lua
node.pos = node.pos + node.forward * (params.walk * dt)
```

### `node.right`

The node's +X axis as a vec3, from its OWN rotation, in its PARENT's space (like node.forward and node.pos). Pairs with node.forward for movement. For the world direction — a camera's right under a turned parent — use node:worldRight().

### `node.roll`

Roll about Z, in radians. See node.yaw: node:setAngles sets all three at once, node:setUp stands the node on a normal.

### `node.scale`

Uniform scale (shortcut). Setting it scales all axes.

### `node.scale_x`

Scale along X.

### `node.scale_y`

Scale along Y.

### `node.scale_z`

Scale along Z.

### `node.scripts`

Every script on this node as an array of handles, in the order they were attached — possibly empty, never nil. The plural of node:getScript, for when you do not know the name yet: `for _, s in ipairs(n.scripts) do print(s.kind) end` says what a node actually carries, which is exactly the question a getScript that answered nil leaves you holding.

### `node.size`

The node's whole scale as a vec3 (read/write). `node.scale` stays the uniform-scale shortcut, and also accepts a vec3 when you want all three axes at once.

### `node.spin`

How fast a turning body spins about Z, in degrees a second, anticlockwise positive. Write it to set the spin: a wheel kicked into motion, a thrown axe. A body that does not turn (its Rigidbody's rotates is off) ignores it.

### `node.tags`

The node's tags as an array of strings (a fresh table each read). Assign a whole array to replace the list; use node:addTag / node:removeTag for single edits and node:hasTag to test.

### `node.tickPos`

The body's TICK pose as a vec3 (read/write) — where the simulation says it is, as opposed to node.pos, which is the interpolated pose the camera renders. Inside fixedUpdate use this one: move with node.tickPos = node.tickPos + vec3(d, 0, 0) and build hurtboxes from it. `node.x = node.x + d` in fixedUpdate teleports the body onto its VISUAL position, so the model slides and the hitbox doesn't follow. In a rollback match this is the difference between a hit registering and not. Writing either one teleports a body: tickPos is the exact one, node.pos goes through the transform. From the write on, every read answers the new position (this node's next hook, and another node's handle), even while physics.pause is on.

### `node.tickYaw`

The body's tick-domain yaw (read/write) — node.yaw's simulation-truth counterpart, for facing a fighter inside fixedUpdate.

### `node.up`

The body's up as a vec3 — minus gravity, so Y on flat ground and RADIAL on a planet. The direction to jump in, wherever the player is standing.

```lua
-- the body's up (-gravity): Y on flat ground, radial on a planet
local lean = node.up:dot(vec3(0, 1, 0))
```

### `node.up_x`

Body up (−gravity) X — radial on a planet, so move along it for planet gravity. Read-only.

### `node.up_y`

Body up (−gravity) Y (read-only).

### `node.up_z`

Body up (−gravity) Z (read-only).

### `node.vel`

The body's velocity as a vec3 (read/write). `node.vel = node.vel + node.up * jump` replaces three vx/vy/vz lines, and it accepts anything with x/y/z. A body's fields are there from the first tick its scripts run — a rig that net.spawn just handed you included; nil only on a node with no RigidBody.

```lua
-- one write instead of vx/vy/vz, and it reads as physics
if node.grounded and input.pressed("space") then
  node.vel = node.vel + node.up * params.jump
end
```

### `node.visible`

Whether the node's geometry is drawn — set node.visible = false to hide it and everything under it (true to show). A child of a hidden node is not drawn whatever its own flag says, and its own flag reads back unchanged, so showing the parent again brings back exactly what was shown before. Hiding keeps the node's transform and physics.

### `node.vx`

Rigidbody velocity X (m/s). Read + write to drive the body; the engine integrates it.

### `node.vy`

Rigidbody velocity Y (m/s). Keep this for gravity/jump while replacing the horizontal part.

### `node.vz`

Rigidbody velocity Z (m/s).

### `node.wake`

Why this rigidbody is awake, as a name (read-only): what last woke it or started its half second toward sleep again, kept while it counts that half second. "settling" while it is still and counting toward sleep after stopping on its own (and while asleep, since nothing has woken it); "moving" when it moved or turned more in a step than a sleeper may; "unsupported" when nothing holds it up; "contact" when something moving touched it; "ground" when what it rests on changed or went away; "joint" when a joint woke it from sleep or a motor drives it; "force" when the force or torque held on it changed while it slept (one held steadily lets it sleep); "impulse" for an impulse; "script" for a velocity, position or asleep written to it; "reshaped" when its parts moved or changed mass. On a part of an assembly it reads the craft's. Read it to find what keeps a parked vehicle awake: a wake of joint on a parked rover is its motors, a wake of moving that it rocks or creeps, a wake of script a script writing its velocity every tick.

### `node.wallNormal`

The steepest surface the body is pressed against, as a vec3 normal — the cliff you ran at, the crate you're shoving — or nil when there's nothing but floor. Read-only. This is what stops a controller launching itself: driving into a steep face means the solver pushes the capsule out along a normal that points partly UP, every frame, which reads as being fired into the sky. Take that component out of your movement (see first_person.lua's `slide`) and you slide along the face instead. Also: wall jumps, wall slides, 'you can't go that way'.

### `node.x`

X position relative to the parent node (local; the world X for a node with no parent). node.worldPos for the world position.

### `node.y`

Y position relative to the parent node (local; the world Y for a node with no parent). node.worldPos for the world position.

### `node.yaw`

Heading about Y, in radians. A node stores a rotation, not three numbers: writing one angle of another node re-reads the other two in canonical form (pitch within ±pi/2), so a triple with pitch past 90° written field by field lands as a DIFFERENT rotation. To set all three use node:setAngles; to stand a node on a surface normal use node:setUp; lookRotation gives canonical angles.

### `node.z`

Z position relative to the parent node (local; the world Z for a node with no parent). node.worldPos for the world position.

## node — methods & handles

### `node:addTag`

node:addTag("burning") — add a tag at runtime (duplicates are ignored). findTagged sees it next frame.

### `node:animator`

node:animator() — the animation handle for this node's Animation Controller (or a rigged model's embedded clips). Setters: :play/:restart/:crossfade/:stop/:setSpeed/:setLayerWeight/:seek/:setParam/:setEnabled/:setCulling. Bones, per frame: :setBoneRot/:addBoneRot/:setBonePos/:addBonePos/:reach/:lookAt, many at once with :boneIndex/:setBones, kept with :holdPose. Getters: :state/:time/:finished/:isPlaying/:clips/:layers/:boneWorld.

```lua
local anim = node:animator()
anim:crossfade(node.vel:length() > 4 and "run" or "walk", 0.15)
```

### `node:bonePos`

node:bonePos("mixamorig:Head") → the bone's world position this frame, as a vec3, on an animated model — the same point a node attached to that bone would be at, without adding one. nil while the model has no posed skeleton (not loaded yet, or not animated). A bone name the model doesn't have raises, naming the closest. node:bones() lists them.

### `node:boneRot`

node:boneRot("mixamorig:Head") → yaw, pitch, roll of the bone in world space this frame, in the same convention as node.yaw/pitch/roll, so a node spawned at the root can be set to match. nil, nil, nil while the model has no posed skeleton.

### `node:bones`

node:bones() → the names of the animated model's skeleton nodes, in skeleton order; nil while it has no posed skeleton. What node:bonePos and node:boneRot accept.

### `node:children`

An array of this node's child handles, built fresh on every call. A script that keeps the list asks node:childrenGeneration() instead of rebuilding it.

### `node:childrenGeneration`

node:childrenGeneration() → a number that changes whenever this node's child list does — a child added, removed, reparented or reordered — and only then. Keep the list node:children() built, and rebuild it when this number is not the one you built it at.

### `node:destroy`

node:destroy() — remove this node and its children (same as destroy(node)). The classic pickup: onTriggerEnter → award score → node:destroy().

### `node:find`

node:find("Muzzle") — the first descendant (any depth) with that name, or nil.

### `node:getChild`

node:getChild("Gun") — the first child with that name (a node handle), or nil. node:getChild(id) (also node:child(id)) — the direct child with that node id, one lookup however many children there are; a number that is no child's id is still tried as a name.

### `node:getParent`

The parent node handle, or nil (same as node.parent).

### `node:getScript`

node:getScript("health") — a script handle for that script on this node, or nil. Read/write its state, call its methods, reach .node / .params. A script's state exists from the frame after its node is spawned or first switched on; a handle to a script that has never run reads nil for everything and warns once. Once built, it is kept while the node or the script is switched off, so a call made then is not lost.

### `node:hasTag`

node:hasTag("enemy") — whether the node carries that exact tag. The classic hit-filter: local hit = raycast(...) if hit and hit.node and hit.node:hasTag("enemy") then ... end

### `node:material`

node:material() / node:material("Clothing") — a material you can read AND assign, in code. With no name it is this node's OWN Material, which on a model covers every part of it. With a name — an object like "Torso#2" or a material like "Clothing", both from node:materials() — it is that part of the model alone, and the override is created the first time you write to it. Fields: texture (and normalMap/roughnessMap/metallicMap/occlusionMap/emissiveMap) by path, color/emissive/specular/rim as colours, plus alpha, roughness, metallic, emissiveStrength, unlit, fog, cell. This is how a clothing system works: node:material("Clothing").texture = "art/shirt.png". A part's override starts as the engine's default material, not as the part's imported look — state what you want it to be.

`m.source` is read-only: the project material file this one follows ("materials/Checkerboard.ron"), or nil for a material of its own. Key gameplay rules on it rather than on the texture, which survives the file being re-textured. From a raycast: `hit.node:material(hit.material).source` (or `hit.node:material().source` when hit.material is nil).

### `node:materials`

node:materials() — what this model's parts are CALLED, which is what you need before you can address one: a list of { object =, material =, textured =, overridden = }. `object` names one sub-object exactly (import renames repeats, so a model with two Torso nodes has a "Torso#2" — which is why guessing does not work); `material` is the glTF material name and reaches every part wearing it, usually the grouping you mean. Empty on a node that is not an imported model.

### `node:removeJoint`

node:removeJoint() — take this node's joint off. It lets go at once.

### `node:removeTag`

node:removeTag("burning") — remove a tag (no-op when absent).

### `node:setBodyOutline`

node:setBodyOutline{ vec2(-0.4, 0), vec2(0.4, 0), vec2(0, 1) } — make this node's Rigidbody a 2D polygon of these corners, x and y from the node before its scale.

For a body that changes shape with what the character is doing: shorter when crouched, round when rolled into a ball. The corners are kept convex and cut down to the 8 that enclose the most area, so any list of points makes a shape the solver can trust. Three corners at least, and not all in a line, or the call raises.

The outline mirrors when the sprite is flipped, and scales with the node.

### `node:setCamera`

node:setCamera{fovY=1.0, active=true, target="minimap", width=256, height=256, hz=10, cullMask=…} — aim a camera, hand it play-mode authority, and point it at a RENDER TARGET. With a `target` the camera draws the world into a live texture any material or UI image wears as "rt:<name>" — minimaps, mirrors, security monitors, scopes, split-screen. `width`/`height` are the texture's pixels (8–4096) and `hz` how often it redraws (0 = every frame), so a 10 Hz minimap costs a sixth of a 60 Hz one. `active=true` clears every other camera's authority, because two active cameras is not a choice anyone made; `getComponent("Camera").active = true` does the same. `projection="orthographic"` (or `"perspective"`) switches the camera at runtime, with `orthoHeight` the world height it shows, and the Camera component reads and writes `projection` by the same names. fovY is RADIANS. Every value is checked at the call: an unknown key, a `width=0` or an `hz="10"` raises naming the property, the value and the range.

`stack={"Arms", "Hud"}` names cameras drawn ON TOP of this one's picture, bottom first, and replaces the whole list (`stack={}` clears it). Each stacked camera draws only its own layers (its `cullMask`) over an empty depth buffer, so first-person arms on their own layer never push into a wall: put the arms on a layer, give a second camera a cullMask of just that layer, take the layer out of the main camera's, and stack the second camera on the main one. A stacked camera draws no sky, terrain or particles, needs no `active`, and keeps its own fovY — a narrower viewmodel FOV is the usual choice. Its own stack is not drawn.

### `node:setCelestial`

node:setCelestial{mu=…, bodyRadius=…, soi=0, parent="Sun", a=…, e=…, i=…, m0=…, atmoColor={r,g,b}, atmoHeight=…, atmoDensity=…, clouds=…, cloudColor={r,g,b}, cloudDarkness=…, cloudHeight=…, aureole=…, aureoleSize=…, luminosity=…, starColor={r,g,b}, occluderRadius=…, reach=4} — set (creating if absent) the node's CelestialBody. The cloud deck's look: cloudColor its daylight colour (white = as before), cloudDarkness 0..1 (1 = a storm with almost no light through it), cloudHeight its height as a share of atmoHeight (0.35 = as before). The deck shades the ground under it: the sun is dimmed where its beam crosses a cloud, so broken cloud casts drifting shadows and a full deck dims the day, more the darker it is. aureole / aureoleSize = the glow around the star in this sky: strength and spread, 1 = natural. Every field reads back through node:getcomponent("CelestialBody"), colours as R/G/B lanes (cloudColorR, atmoColorG, starColorB…), and writes there too. camelCase fields; colors take {r,g,b}. occluderRadius = occlusion culling: the solid-core radius geometry never pierces — terrain chunks fully behind it skip their draw calls (keep it below the deepest cave/dig; 0 = off). reach = with soi left at 0, the least the sphere of influence reaches, in the body's own radii (4 by default): what stands that close rides the body as it orbits, which is what lets a craft land on a small rock whose own pull would not hold it. The atmosphere (atmoColor, atmoHeight, atmoDensity) is the whole sky from inside it: the air's colour overhead, brighter toward the horizon, a glow around the star, thinning with height until it is space at atmoHeight with the stars showing through — no sky shader needed. From outside it is the halo around the planet's edge.

### `node:setGlyphOffsets`

node:setGlyphOffsets{ vec2(0,0), vec2(0,-3), … } — displace individual characters at draw time, in design units, one entry per character of the authored string (shorter is fine; the rest stay still, and {} clears).

The half spans cannot do. Glyph positions are computed inside the renderer and never surface, so a game could not move one letter at any price — no wobble, no shake, no per-glyph reveal. Applied AFTER layout: a displaced glyph never re-wraps its line and never moves its neighbours, so an effect cannot reflow the sentence it is decorating.

Deliberately offsets rather than named effects: which characters move, by how much and on what phase is the game's to decide, and the engine only agrees to move a glyph it has already positioned. A shadow and an outline follow the glyph they belong to.

### `node:setJoint`

node:setJoint{ kind = "rope", to = hook, length = 4 } — hold this node's body to another node, or (with no `to`) to where it is now. Replaces any joint the node had.

Kinds: "hinge" (a pin it turns about), "fixed" (a weld), "distance" (a rod), "rope" (no further apart than its length), "spring", "slider" (along the other's axis), "wheel" (a wheel on the other, on a suspension), "target" (pulled toward a point; see node:setJointTarget), "ball" (turns every way about the anchor: a shoulder, a hitch; max is the cone its axis may lean, in degrees).

Options: to (a node or a node's name), anchor and otherAnchor (vec2 or vec3, in each node's own space), axis, length, min and max (a hinge's degrees or a slider's units; setting either turns the limit on), motorSpeed (degrees or units a second; setting it turns the motor on), goal (a servo's or a piston's target, degrees or units; setting it turns the motor on and makes it a servo), steer (a wheel's steering in degrees, about its suspension axis), maxForce, stiffness (cycles a second), damping, breakForce, collide.

The joint goes on the part that moves and names what it hangs from: a wheel names the car, a link names the link above. Either end may be an assembly (its root or any of its parts stands for the whole craft), so a vessel built from sub-assemblies articulates: a hinge, a piston, a rotor (a hinge with a motor), a robotic arm. A wheel hung from an assembly chassis grips the ground by its suspension load, and a braked wheel (motor on, motorSpeed 0) holds the spot it stopped on. For a grappling hook, set a rope to the hook's node when it lands and removeJoint when the player lets go.

### `node:setJointTarget`

node:setJointTarget(point) — where this node's target joint pulls it, in world space (a vec2 or vec3); nil lets go. Picking something up and carrying it: set a target joint once, then point it at the mouse each frame. The joint's stiffness and maxForce say how firmly.

### `node:setLighting2D`

node:setLighting2D{mode="2d", layers={"Terrain","Characters"}, blocks="on", inner=4, falloff=2, shadows=true, shadowStrength=1, height=0, sun=false} — 2D lighting, from a script. `mode` is auto/2d/3d and says whether this node is on the 2D lighting path at all; auto decides from the scene and is never re-decided once you say otherwise. On a LIGHT, `layers` is the sorting layers it reaches — empty or absent means all of them, which is how you keep a torch off the background. `inner` is full brightness out to that radius before the ramp starts (0 = the ramp starts at the light) and `falloff` is its exponent (2 = the curve every light has always had): together they let a posterized game land a whole light inside one band instead of drawing concentric rings. `shadows=false` makes this one light pass through everything, whatever the scene blocks; `shadowStrength` (0 to 1, default 1) is how much of it a caster stops, so 0.5 lets half through. `height` is how far above the plane the light hangs for normal-mapped art (0 = a third of its range). `sun=true` makes it a sun: every surface it reaches, at full strength, from one direction (the node's down arrow, −Y; rotate it about Z to turn it), with blockers throwing long shadows away from it. A lamp with a spot angle aims its cone the same way in 2D. On a RECEIVER, `blocks` is auto/on/off for whether it occludes light — under auto a tilemap casts from the collision it already declares, so a level's collision IS its light occlusion. A bad spelling names the accepted set rather than silently meaning auto.

### `node:setMaterial`

node:setMaterial{color={r,g,b}, emissive={r,g,b}, emissiveStrength=…, unlit=true, texture="…", alpha=…, …} — set the node's Material, or on a model that has none, change only the keys you give over each part's own material. texture also takes a live render target: "rt:<name>". slice=8 gives a Sprite a 9-slice border of that many texture pixels on every side: it stretches with its node's scale while the corners keep their size (0 = off).

On a MODEL with no Material of its own, setMaterial changes only the keys you give and every part keeps the rest of its own look: its textures, cut-out leaves, maps and custom shader. setMaterial{ color = {1, 0.4, 0.4} } on an imported plant gives a red plant, still leafy. A Material authored on the node in the scene is different: it SUPERSEDES the ones the model was imported with, every part drawing with it, textures included. That is what a node Material is for: "this whole thing is made of THIS". To change one part instead, use node:material("<name>") — node:materials() lists what the parts are called.

blend = "opaque", "cutout" or "blend" is how alpha is drawn: opaque ignores it, cutout cuts holes where the texture's alpha is under alphaCutoff, blend blends; it alone decides whether a surface is drawn see-through, and setting alpha below 1 without naming blend turns blend on. alphaCutoff = 0.5 is where a cutout surface's texture alpha cuts holes; doubleSided = false hides a face seen from behind. On a model with no Material both change only the parts' own values, so a leaf model keeps its texture.

Surface maps: normalMap / roughnessMap / metallicMap / occlusionMap / emissiveMap (paths, "" clears; emissiveMap multiplies emissive) with normalStrength / roughness / metallic / occlusionStrength. shading="physical" switches from the hand-set Blinn-Phong highlight to metal-rough; roughness and metallic only mean anything there, while a normal or occlusion map works under either.

Under physical shading a surface also REFLECTS the sky. A mirror is metallic=1 with roughness=0; raise the roughness and the same reflection blurs. reflectivity scales it (1 = the real amount and the default, 0 = none, above 1 = a deliberate cheat).

fog=false exempts the surface from the scene's fog — both the distance ramp and the volumetric layer — so it draws at its own colour however far away it is. For the things that are not really in the world at that distance: a first-person weapon, a backdrop card, a marker that has to stay readable through the weather. A planet's atmosphere is a separate effect and still applies.

sway=0.3 bends the surface in the game's wind (weather.addWind): the metres a point one metre above the node's origin moves in a 10 m/s wind, growing with the square of the height, so a stem stays rooted and its tip swings. So size it to the prop: a 0.5 m grass blade wants about 1 (its tip moves 0.25 m), a 12 m tree about 0.01 to 0.02 (its crown moves 1.4 to 2.9 m); at 3 a 12 m tree lies flat. Grass, leaves, flags, a scatter field of plants. backlight=0.6 lets the key light on the back of a thin surface through to the front, shadowed as the back face is: a leaf against the sun. nearFadeStart=1, nearFadeEnd=5 dissolves the surface toward the camera: none of it is drawn nearer than 1 m, all of it beyond 5 m, and between them a share of its pixels that follows the distance, so a wall between the camera and the player thins out of the way. It stays solid (no sorting, depth written for what is left) and keeps casting its sun shadow. 0 and 0 is off; in a scene file, near_fade: (1.0, 5.0). ditherAlpha surfaces keep their sun shadow too.

Retro artefacts: jitter (screen-grid vertex snapping, 0 = follow the project), affineUv, vertexLit, ditherAlpha. The project can ask for all four at once (Project Settings ⏵ Rendering); retroExempt=true takes none of them, which is how you hold a viewmodel steady in a world that wobbles.

```lua
-- setup-time; use setShaderParam for per-frame values
node:setMaterial{ unlit = true, emissive = {1, 0.45, 0.15}, emissiveStrength = 2.5 }
```

### `node:setParent`

node:setParent(parent [, { keepWorld = false }]) — move this node under another node, or to the top level with nil. By default it stays exactly where it is in the world and its local position/rotation are recomputed for the new parent; keepWorld = false keeps its local numbers instead, so it jumps to the same spot relative to the new parent — what you want when a part is placed by its offset from a hand or a mount. A node pinned to a bone is unpinned. Applied after this pass; a move under its own child is ignored.

### `node:setPointLight`

node:setPointLight{color={1,0.8,0.5}, intensity=2, range=8} — make this node a light, or retune one. Every field is optional and keeps what the node had, INCLUDING its emitter shape — retuning a window’s colour never turns it back into a bare point. The shape itself is set through node:getcomponent("PointLight").shape. Sixteen lights reach the shader at once; past that the ones contributing most at the camera win, and a light at intensity=0 gives its slot back — which is how you pool them. perf.counts().lights and .lightsDropped say where you stand.

### `node:setPrimitive`

node:setPrimitive("Sphere" [, {r,g,b}]) — make the node a primitive (Cube/Sphere/Capsule/Plane).

### `node:setScreenShader`

node:setScreenShader("inkOutline", false) — switch one of the Post Processing node's screen shaders on or off. The name is the file without its extension, the one the Inspector lists. The pass and its knobs stay in the scene, so this is a switch and not a deletion: turn the outline on for a boss fight and off again after. Pass "" for every pass on the node.

```lua
-- switch one of the scene's screen shaders on or off (it keeps its knobs)
local post = find("Post Processing")
post:setScreenShader("inkOutline", bossFight)
post:setShaderParam("inkOutline.thickness", 1 + rage * 2)
```

### `node:setShaderParam`

node:setShaderParam("glow", 2.5) / node:setShaderParam("nose", x, y, z) — drive a .flsl uniform on this node every tick (a GPU uniform write, never a recompile). Targets the node's Material shader, its UI element's `stage ui` shader (the navball pattern: a script feeds an instrument's uniforms each tick), the Skybox's sky shader, or — on the Post Processing node — its SCREEN shaders: name one with `"inkOutline.thickness"`, or leave the prefix off to set that knob on every pass. Unset lanes are 0. On a MODEL with per-part material overrides and no node Material it fans out to every part that wears a shader; to reach ONE part, go through its handle: node:material("Head#2"):setShaderParam(...). A write with nothing to land on is said once in the Console rather than lost silently.

```lua
-- a live uniform write: safe every tick, never recompiles
node:setShaderParam("cell", math.floor(time * 8) % 16)
```

### `node:setShaderTexture`

node:setShaderTexture(slot, ref) — point one of this node's .flsl shader TEXTURE SLOTS somewhere else, at runtime. `slot` is the name the shader declares (`texture ramp` -> "ramp"); `ref` is a project-relative image path, an `rt:<name>` render target (what another camera sees, live), or "" to clear it. A shader may declare up to 8 slots, so a material can mix a base, a mask, a ramp and a screen — and a script can swap any of them per frame.

```lua
-- swap a shader's texture slot at runtime (a path, or a live render target)
node:setShaderTexture("decal", damaged and "textures/scorch.png" or "")
node:setShaderTexture("screen", "rt:securityCam")
```

### `node:setTerrain`

node:setTerrain(id) — make the node a Terrain volume with that id; fill it with terrain.generatePlanet(id, opts).

### `node:setTerrainGen`

node:setTerrainGen(opts) — attach an ON-DEMAND generation spec (the same opts table terrain.generatePlanet takes): the body's field generates in the background when something first approaches, so no field file is needed at all — a rolled galaxy is playable instantly and unvisited worlds cost one scene node. Player edits saved under terrain.saveDir take priority over regeneration. nil clears.

### `node:setTextSpans`

node:setTextSpans{ {len=10}, {len=3, color={1,0.3,0.3}}, {len=8} } — colour stretches of this UI element's text instead of the whole string. A TextSpec carries one colour, which is a fine default and the wrong floor: the moment a game writes prose at the player it wants a proper noun in the speaker's colour, a keyword tinted to match the key it names, or an item name in its rarity. The only alternative was splitting the line into sibling elements laid out by hand, which re-wraps wrong at every resolution and cannot be revealed a glyph at a time.

Spans style; they never lay out. Wrapping, alignment, max_lines and ellipsis are computed across the whole string exactly as before, so a two-colour run wraps identically to the same string in one colour — a span boundary is not a line break a plain string would not have had. That is also why a span cannot change size or font: those WOULD move the text, and a field that quietly did nothing would be worse than its absence.

len is CHARACTERS of the authored string, not bytes, and spans run end to end from the start — a span with no color is how you spell a gap. Characters no span reaches keep the element's own colour. Pass {} to clear.

### `node:setTint`

node:setTint(color [, alpha]) — a colour MULTIPLIED over everything this node draws, keeping its own textures and each part's own colour. The easy "same model, but red": a hit flash, a team colour, a highlighted selection, a building ghosted while it is placed, a body fading out (that is what the alpha is for). node:setTint() with no argument clears it.

node:setTint{ color =, alpha =, rim =, rimStrength =, ambient =, emissive =, emissiveStrength = } sets the rest. emissive is a glow over the whole model in its own colour (strength 1 when not named), replacing each part's own glow while it is on; a part with an emissive map still glows only where its map is bright. emissiveStrength = 0 gives the parts their own glow back. A multiply can only take light AWAY, which is why a team colour on a mid-toned model arrives as a slightly warm grey — so a tint also carries the two knobs that ADD light: `rim` is an additive fresnel edge in its own colour (what actually tells two team-coloured fighters apart in motion), and `ambient` multiplies this node's share of the scene's ambient, lifting a character out of the room's shadow. A table is read as a colour unless it carries one of those names, so setTint{1, 0.5, 0.2} is still a colour.

Every field is optional and the ones you leave out KEEP their value, so a hit flash that rewrites the colour each frame does not cost the model its rim or its ambient lift. node:setTint() with nothing takes the whole tint away.

Not a Material. A Material says what a thing is MADE OF and supersedes the materials a model was imported with; a tint leaves all of that alone. Reachable as a component too — node:getcomponent("Tint").color = color(1, 0.3, 0.3) — so an animation clip can key a flash.

### `node:sound`

node:sound() — the handle for this node's Audio Source component. :play() (restarts), :stop(), :pause(), :resume(), :setClip("audio/x.ogg"), :seek(secs), :isPlaying(), :isLoading(), :position(). Tunables (volume/pitch/distances/…) live on node:getcomponent("AudioSource").

### `node:uiRect`

node:uiRect() -> x, y, w, h — where this UI element was actually laid out on screen this frame, in pixels, or nil if it is not a UI element or has not been drawn yet. The layout is the engine's, so this is the only way to find out where a Stack or a Pin put something — for a tooltip that follows a button, an arrow pointing at it, or a hit test of your own. Under `floptle run` there is no surface, so nothing is ever laid out and this is always nil — `floptle shot` is the headless way to see a screen; guard the call (`if x then`) in anything that also runs headless.

## vectors, directions & easing

### `dirFromYaw`

dirFromYaw(yaw [, pitch]) — the unit direction those angles face: the inverse of yawOf/pitchOf. Without a pitch you get the ground direction, which is what movement wants; with one you get a camera's view direction.

```lua
-- the yaw/pitch -> direction pair, with the right signs
local look = dirFromYaw(node.yaw, node.pitch)
node.pos = head - look * distance   -- an orbit camera, in one line
```

### `dirTo`

dirTo(from, to) — the UNIT direction from one thing to another. Both may be a vec3, a {x=,y=,z=} table or a NODE handle, so dirTo(node, target) is the whole sentence. Same point twice → vec3(0,0,0), never a NaN.

```lua
local aim = dirTo(node, find("Enemy"))
spawn("Bullet", node.pos + aim * 1.5, function(b) b.vel = aim * 60 end)
```

### `distance`

distance(a, b) — distance between two points: vec3/vec2 values, {x=,y=,z=} tables, or NODE handles (distance(node, target) just works). Also distance(x1,y1,z1, x2,y2,z2) for raw numbers.

### `ease`

ease(a, b, rate, dt) — frame-rate-independent exponential ease: `a` covers a rate-dependent FRACTION of the remaining distance each second, so 30 fps and 240 fps feel identical. Numbers or vectors. rate <= 0 snaps. This is what a camera's "smoothing" knob is; three shipped camera scripts each defined it privately before it lived here.

```lua
-- the same feel at 30 fps and at 240 (this is what "smoothing" is)
function lateUpdate(node, dt)
  node.pos = ease(node.pos, target.pos + offset, params.smoothing, dt)
end
```

### `lookRotation`

lookRotation(dir [, up]) -> yaw, pitch, roll — the angles that face `dir`, WITHOUT applying them (node:lookAt applies them). Three returns, so `node.yaw, node.pitch, node.roll = lookRotation(f, up)` is one line. No up = roll 0.

```lua
-- the angles, without applying them
node.yaw, node.pitch, node.roll = lookRotation(forward, node.up)
```

### `moveTowards`

moveTowards(node, target, maxDelta) — walk a node toward a WORLD point at a speed, never overshooting it. Pass `speed * dt`. Returns true once it has arrived, so `if moveTowards(node, goal, s * dt) then` is the whole patrol step. Also spelled node:moveTowards(target, maxDelta).

```lua
-- a patrol, in two lines: it returns true on arrival
if node:moveTowards(waypoints[i], params.speed * dt) then
  i = i % #waypoints + 1
end
```

### `node.worldPos`

The node's position in WORLD space as a vec3, composed up the parent chain (read-only; node.worldX/worldY/worldZ are the components). node.x/y/z are LOCAL — comparing those against a world target is how a unit under a container walks past its destination and keeps going.

```lua
-- x/y/z are LOCAL; this is where it really is
if node.worldPos:distance(order) < params.arrive then arrived() end
```

### `node:distanceFlat`

node:distanceFlat(other [, up]) — distance ignoring the up axis (default +Y): the "have I arrived?" test for anything that walks on ground it doesn't control the height of. Pass an up for a planet.

### `node:distanceTo`

node:distanceTo(other) — distance to a node or a point, measured in WORLD space, which is the answer people mean. `distance(a, b)` compares LOCAL positions — correct right up until one of the two is parented, and then quietly about the wrong frame.

```lua
-- WORLD space, so a unit under a container measures the real gap
if node:distanceTo(player) < params.aggro then chase(player) end
```

### `node:lookAt`

node:lookAt(target [, up]) — point this node at another node or a world point. Sets yaw + pitch and leaves roll alone; pass an `up` and it sets the roll too, to whatever puts that up over the node's head (a level horizon on a planet — the twenty-line undo-yaw-then-pitch dance, in one call). Measured in WORLD space on both ends, and the node faces the target whatever its parent's rotation: the angles are worked out in the parent's frame, since yaw, pitch and roll are local.

```lua
-- point at a node or a world point; the up makes the horizon level
node:lookAt(find("Enemy"))
node:lookAt(aimPoint, node.up)   -- roll set too, for a planet camera
```

### `node:moveTowards`

node:moveTowards(target, maxDelta) — the method spelling of moveTowards(node, …). World-space and placed through the parent inverse, so a node under a container arrives where you actually pointed.

### `node:setAngles`

node:setAngles(yaw, pitch, roll) — set all three angles in one write, composed exactly as given (yaw about Y, then pitch about X, then roll about Z), canonical or not. Writing node.yaw / node.pitch / node.roll one at a time re-reads the other two in canonical form between writes, so pitch 120° and roll 10° written that way is a different rotation; this is not.

```lua
-- all three angles in one write, composed exactly as given
node:setAngles(yaw, math.rad(120), roll)
```

### `node:setUp`

node:setUp(up [, forward]) — turn the node so its +Y lies along a WORLD direction (a surface normal, the way out from a planet's centre), whatever the hemisphere and whatever its parent's rotation. With a forward, its -Z faces that way as nearly as the up allows; without one it tips the shortest way from where it points now, so its heading is kept. No Euler angles involved, so a normal with y < 0 stands it as surely as one with y > 0.

```lua
-- stand on a surface: +Y along the normal, anywhere on a planet
node:setUp(hit.normal)
node:setUp(hit.normal, toCamera)   -- and face that way as nearly as it can
```

### `node:setWorldPos`

node:setWorldPos(v) — put this node at a WORLD point, whatever it is parented to, without deriving the parent inverse by hand. Goes through the componentwise TRS inverse, so it stays exact under a MIRRORED (negative-scale) parent, where a matrix decomposition puts the flip on the wrong axis.

```lua
-- land on a world point whatever this node is parented to
node:setWorldPos(hit.node:toWorld(vec3(0, 1, 0)))
```

### `node:toLocal`

node:toLocal(v) — the inverse of node:toWorld: a world point expressed in this node's frame.

### `node:toWorld`

node:toWorld(v) — a point in this node's own frame, converted to world space: its position, rotation AND scale, composed up the whole parent chain. "Where is the muzzle?" is gun:toWorld(vec3(0, 0, -1.2)).

```lua
-- composes position, rotation AND scale up the whole parent chain
local muzzle = gun:toWorld(vec3(0, 0, -1.2))
spawn("Bullet", muzzle, function(b) b.vel = gun:worldForward() * 60 end)
```

### `node:turnTowards`

node:turnTowards(target, maxRadians) — turn toward something by at most that much, the SHORT way round (the ±pi seam is handled). Pass `rate * dt` for a frame-rate-independent turn. A node handle or a world point is somewhere to face; any other vector is taken as a DIRECTION, so node:turnTowards(node.vel, 6 * dt) steers a unit to face where it is going. A zero-length direction leaves the facing alone.

```lua
-- swing round at a rate instead of snapping. Short way, always.
node:turnTowards(find("Enemy"), params.turn_rate * dt)
node:turnTowards(node.vel, 6 * dt)   -- or: face where you're going
```

### `node:worldForward`

node:worldForward() — the node's forward AFTER the parent chain. node.forward is the LOCAL one: a gun barrel parented to a swinging arm points where the ARM says, so shooting along node.forward misses. Also node:worldRight() and node:worldUp().

### `node:worldRight`

node:worldRight() — the node's +X axis after the parent chain.

### `node:worldUp`

node:worldUp() — the node's +Y axis after the parent chain (not the same as node.up, which is the body's −gravity up).

### `pitchOf`

pitchOf(dir) — the pitch that faces along a direction, positive looking up. asin, clamped, so a denormalised vector can't produce a NaN.

### `smoothDamp`

smoothDamp(current, target, vel, smoothTime, dt) -> value, vel — a critically-damped spring: unlike ease it has MOMENTUM, so a follow keeps moving for a moment after the target stops. Lua has no reference parameters, so the velocity comes back as the second return: camX, camVX = smoothDamp(camX, wantX, camVX, 0.25, dt). Numbers or vectors.

```lua
-- a follow with momentum: it keeps moving after the target stops
camX, camVX = smoothDamp(camX, target.worldX, camVX, 0.25, dt)
```

### `vec2`

vec2(x, y) — a 2-vector value (UI/screen math), same operators and methods as vec3 (minus cross).

### `vec2.x`

The vector's X.

### `vec2.y`

The vector's Y.

### `vec2:distance`

vec2:distance(other) — the distance between two 2-D points.

### `vec2:dot`

vec2:dot(other) — the dot product; the cosine of the angle when both are unit length.

### `vec2:length`

vec2:length() — how long the 2-D vector is.

### `vec2:lengthSquared`

vec2:lengthSquared() — length without the square root, for comparisons.

### `vec2:lerp`

vec2:lerp(other, t) — a straight-line blend from this (t = 0) to other (t = 1).

### `vec2:magnitude`

vec2:magnitude() — how long the 2-D vector is. The same call as vec2:length(), under the name most engines use for it.

### `vec2:normalized`

vec2:normalized() — a unit-length copy, pointing the same way. Zero stays zero rather than becoming a NaN.

### `vec3`

vec3(x, y, z) — a 3-vector VALUE with real operators: a + b, a - b, v * 2, -v, a == b. Methods: :length() (:magnitude()), :lengthSquared(), :normalized(), :dot(o), :cross(o), :lerp(o, t), :distance(o), :flatten(up), :withX/:withY/:withZ(n), :rotatedY(rad), :rotatedAround(axis, rad), :towards(o, maxDelta), :angleTo(o). vec3() = zero, vec3(s) = splat, vec3(other) = copy. Anything that takes a vector also takes a {x=,y=,z=} table or a node handle. A project chooses which vec3 this is — `exact` (64-bit, mutable; every project made before the setting) or `fast` (32-bit, immutable, no allocation; new projects) — in Project Settings → Scripting → Script vec3. The surface above is identical in both; the two differences are that `fast` cannot be assigned into (use :withX/:withY/:withZ) and `type(v)` answers "vector" there rather than "table". An `exact` vec3 is a table holding x, y and z as its own fields, so reading one is as cheap as reading any table's field (before 0.117 it was a userdata, and `type(v)` answered "userdata"). `floptle lint --vec3` lists what a project would change to switch.

```lua
local v = vec3(1, 0, 0) * 5 + vec3(0, 2, 0)   -- real operators
log(v:length(), v:normalized(), v:dot(node.forward))
```

### `vec3.x`

The vector's X. Vectors are values, not handles: nothing you do to one reaches whatever it came from. Whether you can WRITE it depends on the project's vec3 (Project Settings → Scripting → Script vec3): under `exact` (every project made before the setting existed) `v.x = 5` changes that vector; under `fast` (new projects) a vector cannot be assigned into and `v.x = 5` raises, naming the fix — `v = v:withX(5)`, which works on both. Nodes are unaffected either way: `node.x = 5` moves the node.

### `vec3.y`

The vector's Y.

### `vec3.z`

The vector's Z.

### `vec3:angleTo`

v:angleTo(other) — the unsigned angle between two directions, in radians. Clamped before the acos, so parallel vectors give 0 and a zero vector gives 0 — never a NaN.

### `vec3:cross`

vec3:cross(other) — a vector perpendicular to both, right-handed. The way to build a basis, or to ask which side of a plane something is on.

### `vec3:distance`

vec3:distance(other) — the distance between two points. Reads better than (a - b):length() and does the same thing.

### `vec3:dot`

vec3:dot(other) — the dot product. With unit vectors it is the cosine of the angle between them: node.forward:dot(toEnemy) > 0.7 is a 45° cone in front.

### `vec3:flatten`

v:flatten(up) — the part of v that lies in the plane PERPENDICULAR to up, renormalised. THE planet-safe move: "forward along the ground" is dirFromYaw(node.yaw):flatten(node.up) whatever the local vertical is, and on a flat world :flatten() (default +Y) is the familiar "drop the Y". Straight up or down leaves nothing in the plane → vec3(0,0,0), never a NaN.

```lua
-- "forward along the ground" — on a flat world AND on a planet
local up = node.up or vec3(0, 1, 0)
local fwd = dirFromYaw(node.yaw):flatten(up)
local right = fwd:cross(up)
```

### `vec3:length`

vec3:length() — how long the vector is. The distance form of a difference: (b - a):length().

### `vec3:lengthSquared`

vec3:lengthSquared() — length without the square root. Compare distances with it (d2 < r*r) and skip the expensive part.

### `vec3:lerp`

vec3:lerp(other, t) — a straight-line blend, t from 0 (this) to 1 (other). The one-liner behind smooth camera and marker movement.

### `vec3:magnitude`

vec3:magnitude() — how long the vector is. The same call as vec3:length(), under the name most engines use for it — both are here so neither spelling is a dead end.

### `vec3:normalized`

Unit-length copy (zero stays zero).

### `vec3:rotatedAround`

v:rotatedAround(axis, rad) — Rodrigues rotation about ANY axis, which is what a planet camera's yaw actually is (about the LOCAL up, not about +Y).

### `vec3:rotatedY`

v:rotatedY(rad) — spun about world +Y (the yaw of a flat world). For any other axis use v:rotatedAround(axis, rad).

### `vec3:towards`

v:towards(other, maxDelta) — step toward another point without ever overshooting it: math.approach, for positions. Pass `speed * dt`.

### `vec3:withY`

v:withX(n) / v:withY(n) / v:withZ(n) — the same vector with one component replaced. node.vel:withY(0) keeps your fall speed out of a horizontal speed clamp.

### `yawOf`

yawOf(dir) — the yaw that faces along a direction. This is atan2(-x, -z) (engine forward is −Z), once and with the right signs. Zero direction → 0.

```lua
-- which way is that? (atan2(-x, -z), once and correctly)
local heading = math.deg(yawOf(node.vel))
```

## scene lookups & raycast

### `capsulecast`

capsulecast(origin, dir, radius, halfHeight, max [, opts]) — the player-shaped sweep: "can I actually move there", asked with the shape that will be moving. Upright along the capsule's own axis, matching how the solver keeps a capsule body aligned, so the cast and the move agree. Same hit fields as raycast, hit.material included. opts: ignore, layers, and ignoreStart = true, so a capsule standing on a floor can cast along it: surfaces it starts against do not count, only one it moves into.

### `find`

find("Player") — the first node in the scene with that name (a node handle), or nil.

```lua
-- cache in start; find() every frame is wasteful
function start(node) player = find("Player") end
```

### `findAll`

findAll("Coin") — an array of every node with that name.

### `findScript`

findScript("GameManager") — a script handle for the first node anywhere running that script (the manager pattern), or nil. Call its methods / read its state. RESERVED KEYS: a handle answers `node` (its own node), `kind` (which script it is) and `valid` (still loaded?) ITSELF, so a script exporting one of those three can reach it and nobody else can — the editor lints the export and the Console says so at load. `name` is NOT reserved: a script's own `name` wins, and `kind` is the same string.

### `findScriptInScene`

Alias of findScript(kind).

### `findScripts`

findScripts(kind) — EVERY node carrying that script, as script handles in scene order. Pair with net.isMine to pick the local player out of many avatars: for _, s in ipairs(findScripts("third_person")) do if net.isMine(s.node) then ... end end

### `findTagged`

findTagged("enemy") — EVERY node carrying that tag (Inspector tag chips / node:addTag), as node handles in scene order. Empty table when none; findTagged("enemy")[1] grabs the first.

```lua
for _, e in ipairs(findTagged("enemy")) do
  if distance(node, e) < 10 then e:destroy() end
end
```

### `hit.distance`

How far along the ray the hit is, in world units.

### `hit.node`

The node whose collider was hit (a node handle), or nil for terrain or a collider with no node.

### `hit.nx`

Contact normal X (unit, out of the hit surface). From `raycast` it is the normal of the face the ray struck, facing back toward where the ray came from, so a map mesh or model hit from either side reads the side you hit.

### `hit.ny`

Contact normal Y.

### `hit.nz`

Contact normal Z.

### `hit.x`

Contact point X (world).

### `hit.y`

Contact point Y (world).

### `hit.z`

Contact point Z (world).

### `overlapSphere`

overlapSphere(center, radius [, opts]) — everything inside a sphere, DEEPEST overlap first, as hit tables ({x,y,z, nx,ny,nz, distance, node, material}) — the same fields raycast returns. Reports static geometry AND body hulls. opts takes { ignore = node or {nodes}, layers = {"Enemies"} }. The blast-radius / "what is in this area" query.

### `raycast`

raycast(origin, dir, max [, ignore]) — or raycast(ox,oy,oz, dx,dy,dz, max [, ignore]). Cast a ray against the terrain + mesh colliders AND every physics body (players, crates). Returns a hit {x,y,z, nx,ny,nz, distance, node, material} or nil. node is the node it hit, whether that is a body or a piece of level. material is WHICH MATERIAL SLOT of it — the name a level author typed on that face — so a footstep can tell the boards from the grass inside one big map mesh; nil for anything with a single surface (terrain, a Collidable cube, an imported model, a body), and it costs nothing until you read it. Your own node's body is excluded; pass a node as `ignore` to skip its body too. The last arg can instead be an options table: raycast(..., { ignore = target, layers = {"Ground"} }) — ignore takes a node or a list of nodes (every query's does), and a list holding anything else is an error — layers (name or array, Project Settings → Layers) filters what the ray can hit; a misspelled layer is an error. Use for ground checks, line-of-sight, shooting.

```lua
local hit = raycast(node.pos, vec3(0, -1, 0), params.ground_ray)
if hit then log("ground at " .. hit.y) end
```

### `spherecast`

spherecast(origin, dir, radius, max [, opts]) — the first thing a moving BALL of that radius would hit, or nil. A raycast that can't slip through a gap narrower than the thing you are actually moving. The hit carries the same fields raycast's does, hit.material included — the usual ground check for a footstep. opts: ignore, layers, and ignoreStart = true (surfaces the ball starts against do not count, only one it moves into).

## references — wire nodes in the Inspector

### `componentref`

defaults = { body = componentref("RigidBody") } — the param binds to that COMPONENT on the wired node: params.body is a component handle directly (params.body.friction = 0.05). Components: RigidBody, PointLight, Camera, ParticleSystem, UiElement, UiSlider, UiLayer. nil while unwired/invalid.

### `noderef`

defaults = { target = noderef() } — a NODE REFERENCE param: the Inspector shows a node picker (or drag a node from the Hierarchy onto it) and the script reads params.target as a node handle (nil while unwired). The preferred way to point a script at a specific node — no find() calls.

### `scriptref`

defaults = { hp = scriptref("health") } — the param binds to that SCRIPT on the wired node: params.hp is a script handle directly (call its functions, read its state). The Inspector only lists nodes carrying the script. nil while unwired/invalid.

## input — keyboard & mouse

### `input`

Player input (play mode). input.key/pressed/axis/mouse/button — make interactive games.

### `input.action`

input.action("Jump") — true while a NAMED action is held, from any of its bindings (key, mouse button, pad button, trigger). Define actions in Project Settings → Input; the list there is scanned from your scripts, so a name you type here shows up ready to bind. Prefer actions over input.key: they work on a gamepad, the player can rebind them, and they're what multiplayer replicates.

```lua
-- actions, not raw keys: rebindable, gamepad-ready, replay-safe
if input.action("jump") and node.grounded then
  node.vel = node.vel + node.up * params.jump
end
```

### `input.actions`

input.actions() — every action name in the map, for drawing an in-game controls screen.

### `input.aimPitch`

The active camera's world pitch (radians), captured with the input snapshot.

### `input.aimYaw`

The ACTIVE camera's world yaw (radians), captured with the input snapshot — use it for camera-relative movement (in multiplayer it rides the input command, so server + prediction replay see exactly your view angle). nil without an active camera.

### `input.axis`

input.axis("a", "d") — returns -1/0/1 from a negative/positive key pair (e.g. strafing).

### `input.axis1`

input.axis1("Zoom") — a named 1D axis in -1..1 (triggers, wheel, or a key pair).

### `input.axis2`

local x, y = input.axis2("Move") — a named 2D axis clamped to the unit disk. Reads identically on WASD and on a stick; deadzone and SOCD are handled for you.

```lua
local mx, my = input.axis2("move")
node.pos = node.pos + (node.right * mx + node.forward * my) * params.walk * dt
```

### `input.bindingsOf`

input.bindingsOf("Jump") — an action's bindings as printable chips ("⌨ Space", "🎮 South").

### `input.buffered`

input.buffered("Punch", 4) — was it pressed within the last 4 TICKS and not yet consumed? The input buffer: a player who hits Punch a couple of frames before recovery ends still gets the punch. Pair with input.consume so it fires once. fixedUpdate only.

### `input.button`

input.button(0) — true while a mouse button is held (0 left, 1 right, 2 middle).

### `input.cancelRebind`

input.cancelRebind() — abandon a rebind in progress, leaving the old binding alone.

### `input.clicked`

input.clicked(0) — true only on the frame a mouse button goes down.

### `input.commitRebind`

input.commitRebind(["replace" | "add"]) — accept the captured binding. It replaces the action's bindings on the same kind of device for this player (keyboard and mouse together, or a pad), so a pad binding survives a keyboard rebind; "add" keeps the old ones beside it. Returns false if nothing was captured yet or nothing changed.

### `input.consume`

input.consume("Punch", 4) — spend a buffered press. Without it a 4-tick buffer fires your attack on all four ticks.

### `input.dir`

input.dir() — the current numpad direction from "Move", from the character's point of view: 7 8 9 / 4 5 6 / 1 2 3, where 5 is neutral and 6 is forward.

### `input.dirHeldTicks`

input.dirHeldTicks(4) — consecutive ticks a numpad direction has been held. Build your own charge or leniency rules on it.

### `input.facing`

input.facing() — which way this player's character is facing, as -1 or 1. The fighter layer mirrors directional input by it, so "forward" means toward the opponent on both sides of the screen.

### `input.heldSecs`

input.heldSecs("Charge") — seconds the action has been continuously held (0 when up). Hold-to-charge without your own timer.

### `input.justPressed`

input.justPressed("Punch") — true only on the frame (or tick, inside fixedUpdate) the action goes down.

### `input.justReleased`

input.justReleased("Block") — true only on the frame/tick the action goes up.

### `input.key`

input.key("w") — true while the key is held. Names: a-z, 0-9, space, enter, shift, ctrl, alt, left/right/up/down, escape, tab.

### `input.loadBindings`

input.loadBindings(text) → ok, err — put back bindings input.saveBindings() wrote. Actions the project no longer declares are skipped, and actions the text leaves out keep theirs, so an older save still loads.

### `input.lockMouse`

input.lockMouse() — pin the cursor to the window center and hide it (FPS / free-look mouselook without holding a button). Read motion with input.mouse_delta(). Released on Stop.

### `input.motion`

input.motion("qcf") — has a fighting-game motion just been completed? Seeded set: qcf, qcb, dp, rdp, hcf, hcb, dd, ff, bb, chargeF, chargeU (edit them in input.ron). Combine with input.buffered for a special: `if input.motion("qcf") and input.buffered("Punch", 4) then`. fixedUpdate only.

### `input.mouse`

local x, y = input.mouse() — cursor position in pixels.

### `input.mouse_delta`

local dx, dy = input.mouse_delta() — mouse movement since last frame.

### `input.padAxis`

input.padAxis(1, "leftx") — read a pad axis raw, -1..1, past the action map. Same diagnostic purpose as input.padButton; bind through actions for real gameplay.

### `input.padButton`

input.padButton(1, "a") — read a pad button RAW, straight past the action map. Deliberately unmediated: this is what distinguishes "your pad works, your bindings are wrong" from "your pad is not here".

### `input.padCount`

input.padCount() — how many gamepads are connected. The quick check behind a "press a button to join" prompt.

### `input.pads`

input.pads() — every gamepad the engine has enumerated: { index, name, connected }. Show it in your options screen; "the pad isn't listed" and "the pad is listed but nothing is bound" are different problems and only this can tell them apart.

### `input.pendingRebind`

input.pendingRebind() — the captured chip text once something has been pressed, an EMPTY string while still waiting, or nil when no rebind is running. Enough for a menu to show "press any button…" and then the result.

### `input.player`

input.player(2) — the same input API bound to another LOCAL player (1-based). Two characters can run the same script: pass the slot as a param and use `local me = input.player(params.player)`. Set the count in Project Settings → Input. Sharing ONE keyboard: scope a binding to a player (right-click its chip) so a single action name can be J for P1 and 1 for P2 — pads sort themselves out already.

### `input.popContext`

input.popContext("menu") — remove an input layer. Returns whether one was removed.

### `input.pressed`

input.pressed("space") — true only on the frame the key goes down (an edge).

### `input.pushContext`

input.pushContext("menu", { priority = 100, consume = true, enabled = { "Pause" } }) — a consuming layer swallows every action it doesn't list, so a menu or dialogue eats movement without the player controller knowing. Pop it with input.popContext("menu").

### `input.released`

input.released("space") — true only on the frame the key goes up (an edge).

### `input.resetBindings`

input.resetBindings([action]) → bool — back to input.ron's bindings, for one action or all of them. False for an action the project does not declare.

### `input.saveBindings`

input.saveBindings() → text — every action's current bindings, for save.set. The engine does not keep a player's rebinds across runs; this is how the game does.

### `input.scroll`

input.scroll() — mouse wheel delta this frame.

### `input.setFacing`

input.setFacing(-1) — mirror this player's directions after a cross-up, so motion("qcf") keeps meaning "toward the opponent". The engine has no opinion about who faces where; the game sets it.

### `input.setMouseLocked`

input.setMouseLocked(true/false) — lock or unlock the mouse from a boolean (e.g. a menu toggle).

### `input.simulate`

input.simulate{ key = "w", down = true }, { button = 0, down = true } (0 left, 1 right, 2 middle, as input.button numbers them), { mouse = {x, y} }, { scroll = 1 } or { text = "hello" } — a testing aid: input as if the player gave it, from the next frame, for every script (input.key, pressed, released, mouse, scroll, typed and the actions alike) and for the game's UI, so a simulated click clicks a button and text types into a focused field. Several in one call apply pointer, wheel, text, key, button in that order. For probes and scripted test runs.

### `input.startRebind`

input.startRebind("Jump", "pad") — arm press-to-bind from a settings menu. Poll input.pendingRebind() for the captured chip, then input.commitRebind(). Filters: "keyboard", "pad", "axis", or nil for any button. Escape always cancels.

### `input.touchBegan`

input.touchBegan(id) — true on the frame that finger went down.

### `input.touchEnded`

input.touchEnded(id) — true on the frame that finger lifted (or was cancelled).

### `input.touches`

input.touches() — every finger on the screen this frame: a list of { id, x, y, dx, dy, phase, pressure, began, ui }. x/y are input.mouse() pixels, dx/dy this frame's movement, phase "began" | "moved" | "held" | "ended" | "cancelled" (a lifted finger is listed once more, then gone), pressure 0..1 (1 where the screen does not say), began = it went down this frame, ui = it landed on an interactive UI element and is working it like the mouse (skip it in your own controls). A finger that lands off the UI is only the game's and clicks nothing.

### `input.typed`

input.typed() — the CHARACTERS entered this frame, as a string, resolved by the OS keyboard layout (a paste folded in). Not the same question as input.pressed: that one is physical, so "q" is the key where Q sits on QWERTY and types `a` on AZERTY. Never contains control characters — Enter and Backspace stay actions. Empty while a UI text field has focus, because the field ate them.

### `input.unlockMouse`

input.unlockMouse() — release the cursor back to the desktop and show it again.

## drawing — draw.*

### `draw`

The GAME's telegraph layer — 3D lines/shapes and screen-space rects, circles and text that SHIP with your game. gizmo.* is the debug-only twin that never appears for a player.

### `draw.box`

draw.box(cx,cy,cz, hx,hy,hz, yaw, r,g,b [,a]) — a yaw-rotated wireframe box from half-extents. Trigger volumes, build footprints, an attach point.

### `draw.circle`

draw.circle(x, y, radius, r,g,b [, a]) — a filled circle in screen pixels, x/y its CENTRE. draw.circleOutline(..., [px]) is the hollow twin. Same immediate-mode rules as draw.rect: over the scene, over the HUD, one frame each.

```lua
-- x, y is the CENTRE
draw.circle(mx, my, 6, 0.3, 1.0, 0.5, 0.9)
draw.circleOutline(mx, my, 18, 0.3, 1.0, 0.5, 0.5, 2)
```

### `draw.circleOutline`

draw.circleOutline(x, y, radius, r,g,b [, a] [, px]) — a hollow circle, `px` thick (default 2).

### `draw.cone`

draw.cone(bx,by,bz, dx,dy,dz, radius, height, r,g,b [,a]) — a SOLID cone: base disc at b, apex `height` along the unit direction d. Gizmo arrowheads, thruster plumes, direction markers.

### `draw.conic`

draw.conic(center, e1, e2, p, ecc, segs, r,g,b [,a [, maxR]]) — a Kepler orbit around the focus `center` (a vec3), in the plane of e1 (toward periapsis) and e2: r(θ) = p / (1 + ecc·cos θ), p the semi-latus rectum. ecc < 1 is a closed ellipse of `segs` segments; ecc ≥ 1 is an open arc that stops where r reaches maxR (default 10·p). Refuses: p ≤ 0.

### `draw.depthTest`

draw.depthTest(true) — the lines, rings, spheres, boxes, polylines and conics queued after it hide behind whatever is in front of them: a ground reticle behind the character, an orbit behind its planet. Off by default, so lines draw through the scene. Holds until changed, so switch it around the calls that want it: draw.depthTest(true) … draw.depthTest(false). Works with draw.nativeLines too. draw.depthTest() answers the setting.

### `draw.disc`

draw.disc(cx,cy,cz, nx,ny,nz, r0, r1, r,g,b [,a]) — a filled annulus around normal n (r0 = inner, r1 = outer; r0 = 0 gives a full disc). Rotation gizmo bands, ground markers.

### `draw.line`

draw.line(x1,y1,z1, x2,y2,z2, r,g,b [, a]) — queue one world-space 3D line for THIS frame (immediate mode: re-draw every lateUpdate — the camera pass — while wanted). Drawn OVER the scene, never occluded, so an orbit reads through its planet — unless draw.depthTest(true) came first, which hides lines behind what is in front of them.

### `draw.nativeLines`

draw.nativeLines(true) — draw.line / ring / sphere / box / polyline / conic lines over the finished picture, one pixel wide at the window's own resolution and in exactly the colour given, instead of into the 3D scene. At a render scale below 1 scene lines are drawn at the lowered resolution and go soft; these do not. They skip the post effects (bloom, depth of field, grain). draw.depthTest(true) still hides them behind the scene, tested against the depth the scene was drawn with. Persistent until switched off; draw.nativeLines() answers the setting.

### `draw.polyline`

draw.polyline(points, r,g,b [,a [, closed]]) — a connected line through every point, in ONE call: points is a flat {x1,y1,z1, x2,y2,z2, ...} array or an array of vec3s. closed = true joins the last point back to the first. The same segments as one draw.line per pair, without crossing into the engine once per segment — the way to draw an orbit, a trajectory or a path with hundreds of points.

### `draw.quad`

draw.quad(texture, x0,y0,z0, x1,y1,z1, x2,y2,z2, x3,y3,z3, r,g,b [,a] [,u0,v0,u1,v1]) — one textured quad IN the world: depth-tested against the scene (a trail behind a pillar stays behind it), blended with the image's alpha, visible from both sides. The corners run around the quad; the colour tints the image. The UV rectangle defaults to the whole image — corner 0 sits at (u0,v0), corner 1 at (u1,v0), corner 2 at (u1,v1), corner 3 at (u0,v1). A ribbon is one quad per segment with u walking along it: paint the streak with u running tail→head and v across the width, then give segment i the slice u = i/n .. (i+1)/n. A sword trail, a tyre mark, a decal, a ground ring. Immediate mode: draw it every lateUpdate you want it.

```lua
-- a ribbon between two marker nodes on a moving object (a blade's mid and tip):
-- sample them each frame, keep half a second, one textured quad per segment
local samples = {}
function lateUpdate(node, dt)
  local tip, mid = node:find("BladeTip"), node:find("BladeMid")
  samples[#samples + 1] = { tip = tip.worldPos, mid = mid.worldPos, t = time }
  while time - samples[1].t > 0.5 do table.remove(samples, 1) end
  local n = #samples
  for i = 2, n do
    local a, b = samples[i - 1], samples[i]
    local alpha = 1 - (time - b.t) / 0.5
    -- u walks tail→head along the ribbon, v runs mid→tip across it
    local u0, u1 = (i - 2) / (n - 1), (i - 1) / (n - 1)
    draw.quad("textures/streak.png",
      a.tip.x, a.tip.y, a.tip.z,  b.tip.x, b.tip.y, b.tip.z,
      b.mid.x, b.mid.y, b.mid.z,  a.mid.x, a.mid.y, a.mid.z,
      0.6, 0.9, 1.0, alpha,  u0, 0, u1, 1)
  end
end
```

### `draw.rect`

draw.rect(x, y, w, h, r,g,b [,a] [,radius]) — a filled rectangle in SCREEN PIXELS, in input.mouse()'s space. An RTS marquee is just the two corners you dragged between — the 3D version has to be projected onto a ground plane, which fights the camera angle and misses whatever the plane doesn't cross.

### `draw.rectOutline`

draw.rectOutline(x, y, w, h, r,g,b [,a] [,thickness]) — the hollow twin of draw.rect. The last number is the border thickness rather than a corner radius.

### `draw.ring`

draw.ring(cx,cy,cz, nx,ny,nz, radius, r,g,b [,a]) — a circle around normal n at c. Range rings, selection circles, an area-of-effect telegraph.

### `draw.setFrame`

draw.setFrame(node) — the 3D shapes this script queues after it (lines, rings, spheres, boxes, polylines, conics, triangles, cones, discs, quads) are in that node's own space: its position, rotation and scale. The engine places them after every tick and lateUpdate have moved things, so they are drawn exactly where the node is drawn: an overlay on a planet on rails stays on it even when drawn from update. draw.setFrame(nil) is world space again. Holds for this script until changed, and never applies to another script's shapes. Returns the frame it replaced (a node, or nil).

### `draw.sphere`

draw.sphere(cx,cy,cz, radius, r,g,b [,a]) — three rings, i.e. a wireframe ball. Cheap enough to draw per-frame for every marker on screen.

### `draw.text`

draw.text(x, y, s, size, r,g,b [, a] [, align] [, font]) — a string on the SCREEN, in the pixels input.mouse() reports, without building a UI tree: a damage number, a frame-time readout, the count under a selection box. The engine measures and lays out the glyphs with the same font stack ui.make uses — and measures with the SAME font it draws, so a centred run lands where you asked. align is "left" (default) | "center" | "right", and x is that edge. font is a project-relative .ttf/.otf/.ttc/.woff/.woff2, or a BMFont .fnt (a bitmap font, drawn exactly at whole multiples of its size); leave it out and you get the project's UI font (Project Settings ▸ UI font), which is where to set it once rather than at forty call sites. Immediate mode: re-draw it every frame you want it.

```lua
-- a HUD with no UI tree; align says which edge x is
draw.text(24, 24, "HP " .. hp, 22, 1, 0.4, 0.4)
draw.text(w - 24, 24, string.format("%.0f fps", 1 / dt), 18, 1, 1, 1, 0.7, "right")
```

### `draw.tri`

draw.tri(x1,y1,z1, x2,y2,z2, x3,y3,z3, r,g,b [,a]) — one filled triangle. The raw primitive under the solid shapes, for when you want your own.

## the web — http.*, json.*

### `data.base64Decode`

data.base64Decode(text) -> bytes, err — base64 back to bytes. Reads either alphabet, padded or not, and ignores line breaks. Refuses: text that is not base64 in either alphabet (as nil, err, never a raise).

### `data.base64Encode`

data.base64Encode(bytes [, { url = true }]) -> text — bytes as base64 text, to carry a picture or a blob inside JSON or a URL. url = true uses the URL-safe alphabet with no padding.

### `data.deflate`

data.deflate(bytes [, { level = 6, format = "zlib"|"raw"|"gzip" }]) -> bytes — compress a Lua string. A level, a ghost or a picture saved as text shrinks many times over; pair it with data.base64Encode to keep it in JSON. zlib is the default and what most servers mean by "deflate"; raw has no header, gzip is what a .gz file is. level 0 stores, 9 is smallest. Refuses: a level outside 0–9; an unknown format; an unknown option.

### `data.inflate`

data.inflate(bytes [, { format = "zlib", maxSize = 64 MB }]) -> bytes, err — undo data.deflate. It reads what somebody else sent, so bad data answers nil and why rather than raising, and it stops at maxSize bytes of output so a small hostile blob cannot expand to fill memory. Answers nil, err: the bytes are not that format; the output would pass maxSize. Refuses: an unknown format or option.

### `http.cancelAll`

http.cancelAll() — forget every pending callback. Stop and scene.load do this for you: a callback closes over nodes from the scene that asked, and delivering it into a fresh session is how one run inherits the previous one's network.

### `http.delete`

http.delete(url [, opts], function(res) end) — as http.get, with DELETE.

### `http.get`

http.get(url [, opts], function(res) end) — fetch a URL. NON-BLOCKING: the callback runs on a later tick on the MAIN thread, so it is safe to touch nodes from it and a slow server can never stall a frame. opts = { headers = {...}, timeout = 10, json = true }. res = { ok, status, body, json, error, location } — `body` is the reply's bytes exactly (a picture arrives whole; a string posted goes out as the bytes it holds); `ok` is a 2xx with no error; a 404 still hands you `body`, because that is where an API explains itself; a 3xx is NOT followed — `location` says where it pointed. Public addresses only: loopback, private-network and link-local addresses are refused (the editor's Play allows loopback and private ones, and says so once). Play only.

```lua
-- non-blocking: the callback runs on a later tick, on the main thread
http.get(params.api .. "/me/cards", {
  headers = { Authorization = "Bearer " .. token },
}, function(res)
  if not res.ok then return log("failed: " .. tostring(res.error)) end
  for _, card in ipairs(res.json.cards or {}) do addCard(card) end
end)
```

### `http.inFlight`

http.inFlight() — how many requests are still waiting on a reply. Up to 8 may be in flight and 20 may start per second; past that, calls fail fast with res.error and the cap announces itself once in the Console. A cap you are hitting is nearly always a request inside update().

### `http.post`

http.post(url, body [, opts], function(res) end) — same rules as http.get, plus a body: a STRING is sent as-is, a TABLE is encoded as JSON for you. http.put and http.delete round out the set.

```lua
-- a TABLE body is sent as JSON; no json.encode dance needed
http.post(params.api .. "/me/loadout", { deck = deckId }, function(res)
  if not res.ok then log("the server said no: " .. res.body) end
end)
```

### `http.put`

http.put(url, body [, opts], function(res) end) — as http.post, with PUT.

### `json.array`

json.array(t) -> t — mark a table as a JSON LIST, and return it. The encoder guesses from the shape (keys 1..n and nothing else is an array), which is right for every list with something in it and cannot be right for the empty one: {} is both an empty list and an empty object, and it stays an object. So json.encode{ ids = json.array{} } sends "ids":[] where a plain {} would send "ids":{} and the server would read the wrong type. json.array() with no argument builds a new empty list, and json.array(t) returns the SAME table it was given, so `local ids = json.array{}` then `ids[#ids+1] = x` reads normally. json.decode marks every array it builds, so read -> edit -> send back keeps its lists as lists; note that body.ids = {} throws the mark away with the table, and body.ids = json.array{} is the replacement that keeps it. A marked table that also carries a name, or that has a hole in it, is REFUSED by json.encode with a message saying which.

### `json.decode`

json.decode(s) -> value, err — parse JSON. Bad input returns nil AND a message rather than raising: a reply from someone else's server is data, not a bug in your script. JSON null becomes nil, so a null field reads exactly like a missing one. Refuses: the string is not valid JSON. It returns the message rather than raising, on purpose — a reply from someone else's server is data, not a bug in your script.

```lua
-- bad input is a VALUE, not an error
local save, why = json.decode(text)
if not save then return log("corrupt save: " .. why) end
```

### `json.encode`

json.encode(value) — a Lua value as a JSON string. A table with a [1] is an ARRAY, anything else is an object (that is the only rule Lua's single table type can support), and json.array(t) says list for the empty case the shape cannot answer. http.post takes a table body directly, so you rarely need this by hand.

### `json.isArray`

json.isArray(v) -> bool — would json.encode write this as a JSON array? True for a table marked by json.array, and for any table whose keys are exactly 1..n with n at least 1. FALSE for an empty unmarked table, which is the whole point: this is how json.decode('[]') and json.decode('{}') are told apart, and before it they were the same empty table.

### `openUrl`

openUrl(url) — open an http:// or https:// address in the player's own browser. The address is parsed first: one with a username in it, whitespace, or a character a shell would read is refused at the call, with the reason. The device-code sign-in flow needs it: the player approves the pairing on your real site, so the game never sees a password and needs no secret baked into it. Play only; if the platform refuses, the URL is logged instead so the player can still get there.

```lua
-- the player approves the pairing on your real site
openUrl(res.json.verify_url)
```

## the player's account — account.*

### `account`

The signed-in player: account.signIn(), account.player(), and http verbs that carry the session. A script asks for a PLAYER, never for a token, and the server decides what that player owns.

### `account.cancel`

account.cancel() — abandon a sign-in in progress (the player pressed Escape). Harmless at any other time.

### `account.code`

account.code() — while state() is "waiting": { code = "WXYZ-9999", url = "...", expiresIn = 900 }. Show the code and send them to the url (openUrl does it) — that pairing is what the player approves. nil at any other time, and ALWAYS nil in a browser build, which redirects rather than pairing a code — so a sign-in screen that only draws the code shows an empty box there. Draw a button that calls signIn() as well.

### `account.delete`

account.delete("/games/mygame/saves/slot1", function(res) end) — remove something from Floptle Cloud.

### `account.error`

account.error() — why the last sign-in failed, as a sentence you can put on screen. nil unless state() is "failed".

### `account.get`

account.get("/wallet", function(res) end) — a Floptle Cloud call with the player's bearer token attached for you. Takes a PATH, not a URL: there is exactly one host it can reach, which is what makes attaching a token to it safe. Bare paths get the /api/floptle/v1 prefix. A script acts as the PLAYER: /wallet, /missions, /games/... and /me/... are callable; the developer surface under /cloud/... and the identity endpoints are refused at the call. res is the same table http.* gives you. A blob (/games/<game>/blobs/...) is bytes both ways: a string body goes out exactly as it is (a table is refused), and res.body is the blob's bytes, ready for assets.textureFromBytes; every other path is JSON.

### `account.inFlight`

account.inFlight() — how many account calls are still waiting on a reply (cap 6). A spinner, or a guard against firing the same request every frame.

### `account.player`

account.player() — { id, name, email, tier } once signed in, else nil. `tier` is "unknown" when fopull.com could not be reached to ask what the plan is — the engine fails soft to the free tier's limits but will not report a guess as the answer, so gate a paid feature on a tier you were TOLD rather than on "not free". There is deliberately no way to read the access token: a shipped game's Lua is readable, so anything a script can hold a player can read out of the file.

### `account.post`

account.post("/games/mygame/events", { event = "boss_killed", event_id = id }, function(res) end) — report what HAPPENED and let the server decide what it is worth. A table body is sent as JSON. There is no endpoint that credits currency directly, by design: anything a client can announce, a modified client can announce.

### `account.put`

account.put("/games/mygame/saves/slot1", { data = t, expected_version = v }, function(res) end) — a cloud save. expected_version is optimistic concurrency: send the version you last read and a stale write gets 409 instead of silently clobbering the player's other machine.

### `account.signIn`

account.signIn() — begin signing the player in to their Foverse account (fopull.com). Returns IMMEDIATELY; watch account.state() and draw account.code(). The engine drives the OAuth device flow in Rust — the player approves in their browser, so the game never sees a password and never holds a token. Play only. IN A BROWSER BUILD this is a redirect instead: the page leaves for fopull.com and the game RESTARTS when it comes back already signed in, so call it from a menu and never mid-play, and expect account.code() to stay nil (there is no code to show). The game's address has to be registered as a redirect URI first, or the sign-in is refused.

### `account.signOut`

account.signOut() — forget the session NOW, then clear the keyring and revoke the refresh token in the background. In that order on purpose: a player who presses Sign Out is signed out whether or not the network agrees.

### `account.state`

account.state() — "signedOut" | "starting" | "waiting" | "signedIn" | "failed". Polled rather than called back, because signing in takes as long as a person takes to pick up their phone and a sign-in screen is redrawing anyway. The first ask in a run reads the stored session (the Hub's, or this game's last sign-in) with no Cloud call, and answers "starting" until it has, so a Sign in button shown only on "signedOut" never flashes at a signed-in player.

## game UI — text, buttons & hooks

### `cancelled`

function cancelled(node) — UI hook: the UiCancel action (Escape / B) while this element has focus. Back out of a screen from the element the player is on.

### `changed`

function changed(node) — UI hook: a text field's value changed (typing, paste, backspace), or a draggable slider's did (a drag or a click on the track). Once per frame however many keystrokes landed; never for a frame the slider was held still. Read node.text, or getComponent("UiSlider").value.

### `clicked`

function clicked(node) — UI button hook: fires when this node's element (with 'button' on) is pressed AND released on it. Style states in Lua; no imposed look.

### `color`

color(r, g, b [, a]) — a colour, 0..1 per channel, alpha 1 by default. Also color(gray [, a]) and color(other [, a]) to copy with a new alpha. It's a plain table {r,g,b,a} (also [1]..[4]) so it prints, saves and compares. Assign it whole: el.fill = color(1, 0.85, 0.35), el.textColor, el.borderColor, el.tint, el.groupTint, el.caretColor.

### `color.hex`

color.hex("#ff8800") / color.hex("ff8800aa") — 6 or 8 hex digits. A 3-digit shorthand is refused rather than guessed at.

### `color.lerp`

color.lerp(a, b, t) — blend two colours per channel, t clamped to 0..1.

### `dragCancel`

function dragCancel(node) — UI hook: a drag was released over nothing. Put the item back; a half-finished gesture must not leave it stuck to the cursor.

### `dragEnter`

function dragEnter(node) — UI hook: a drag moved over this `drop target`. Pair with `dragLeave`; highlight the slot here.

### `dragLeave`

function dragLeave(node) — UI hook: the drag moved off this drop target.

### `dragMove`

function dragMove(node) — UI hook: fires every frame of a drag on the SOURCE. Use input.mouse() / node:uiRect() to position whatever you're showing.

### `dragOver`

function dragOver(node) — UI hook: fires every frame a drag rests over this drop target.

### `dragStart`

function dragStart(node) — UI hook: a `draggable` element has been picked up (the pointer travelled far enough that it isn't a click). The engine does NOT move the element — draw the drag however your game wants.

### `dropped`

function dropped(node) — UI hook: fires on BOTH ends of a completed drag — the target (which now has it) and the source (which gave it away). `ui.dragging()` and `ui.dropTarget()` name the pair.

### `el.border`

Shape border thickness (design units).

### `el.cell`

Spritesheet cell index the image shows (set per frame for sprite animation).

### `el.fillA`

Shape fill alpha 0..1.

### `el.fillB`

Shape fill blue 0..1.

### `el.fillG`

Shape fill green 0..1.

### `el.fillR`

Shape fill red 0..1.

### `el.height`

Height (same rules as width).

### `el.opacity`

Multiplies every color the element draws, 0..1.

### `el.posX`

Free position X / Pin offset X (design units).

### `el.posY`

Free position Y / Pin offset Y (design units).

### `el.radius`

Shape corner radius (design units).

### `el.scrollY`

Scroll-view position, design units (0 = top; the wheel drives it too, clamped to the content). Present only on elements with the scroll-view option.

### `el.textA`

Text color alpha 0..1.

### `el.textB`

Text color blue 0..1.

### `el.textG`

Text color green 0..1.

### `el.textR`

Text color red 0..1.

### `el.textSize`

Text glyph size (design units; ignored while fit is on).

### `el.tintA`

Image tint alpha 0..1.

### `el.tintB`

Image tint blue 0..1.

### `el.tintG`

Image tint green 0..1.

### `el.tintR`

Image tint red 0..1.

### `el.visible`

Shown (1/0; assign true/false).

### `el.width`

Width in the axis's sizing mode (px value, % fraction, or grow weight). Absent (nil) on a fit axis; writing one makes it fixed px.

### `focusEnter`

function focusEnter(node) — UI hook: keyboard/gamepad focus arrived here. What focus LOOKS like is your style's `focus` block; this is for the rest (a sound, a preview, a description panel).

### `focusExit`

function focusExit(node) — UI hook: focus left this element.

### `hoverEnd`

function hoverEnd(node) — UI hook: the pointer left this node's clickable element.

### `hoverStart`

function hoverStart(node) — UI hook: the pointer entered this node's clickable element. Pair with hoverEnd.

### `layer.designHeight`

Design units that span the window height.

### `layer.enabled`

Master switch (1/0; assign true/false) — an off layer draws nothing.

### `layer.textSnap`

Round every rasterized text size to a whole multiple of this many SCREEN PIXELS; 0 = off. For a pixel font, whose art is a grid: a cell only looks like a pixel when it lands on a whole one, and `text size x layer scale` almost never does — so every stem is softened by a different fraction and the text reads as badly spaced even though nothing is mispositioned. Set it to the number of cells in an em.

### `layer.worldSpace`

1 = a panel inside the 3D world at this node's transform; 0 = a screen overlay.

### `layer.z`

Draw order: lowest z first.

### `node.index`

Which row of a UI repeater this node is, 0-based — nil on anything a repeater didn't spawn, so `if node.index then` is a fine "am I a row". Read the count with getcomponent("UiElement").count on the container.

### `node.text`

A UI element's label text — read/write; numbers coerce (hpLabel.text = 42). nil on nodes without UI text; writing to a UI element without a text spec creates one.

### `pressed`

function pressed(node) — UI hook: LMB went down on this node's clickable element.

### `released`

function released(node) — UI hook: LMB came back up (on or off the element).

### `slider.max`

Range end.

### `slider.min`

Range start.

### `slider.value`

Current value (clamped to min..max at draw time).

### `submitted`

function submitted(node) — UI hook: Enter (UiSubmit) in a focused TEXT FIELD. Read the value with node.text. A field fires this instead of `clicked`, so a field inside a button doesn't run the button.

### `ui.bind`

ui.bind(node, "property", function() ... end) — say the relationship once instead of writing an update() that keeps it true. The engine calls the function once a frame, after every update, and writes what it returns: a string or number to "text", a color(...) to a colour field, a number/boolean to any component field (the component is picked by which one actually has that field, so "value" finds UiSlider). Re-binding the same property replaces. A binding whose node is gone is dropped silently; one that throws is dropped after reporting once.

### `ui.changed`

ui.changed(element) — a text field's or a draggable slider's value changed this frame. Read the value with element.text, or getComponent("UiSlider").value.

### `ui.clicked`

ui.clicked(element) — did it fire `clicked` THIS frame? The polling half of ui.on, for a manager that already has an update(). Reads the same event list the hooks fire from (published before scripts run), so a poll and a hook can never disagree.

### `ui.dragging`

ui.dragging() — the element being dragged, as a node, or nil. Live for the whole drag AND for the frame the `dropped` hooks run on. There is no separate payload channel because a node already carries params, a name and tags — ask it what it is.

### `ui.dropTarget`

ui.dropTarget() — the drop target the drag is currently over, as a node, or nil.

### `ui.engineSettings`

ui.engineSettings(container, "controls" | "accessibility" [, { styles = { page=, row=, label=, value=, button= }, onChanged = function(key, value) end, filter = "keyboard" }]) → panel — build the engine's own settings page into a container of yours, with ui.make. controls: one row per input.ron action with its bindings; clicking a row arms press-to-bind (filter narrows it to "keyboard", "pad" or "axis"). accessibility: text size, colour filter, reduce motion and captions, each a button that steps through its values. Nothing is painted: every element takes the style you name for its slot, so the page wears your theme; a slot can instead be a table of ui.make properties (a style among them, and sizes such as w and h, which a style cannot set). Call panel:update() from your update while the page is open — a pressed key arrives between frames, and update commits it; panel:refresh() redraws after you change a setting yourself. Pass nil as the container to build nothing and place panel:tree() in a screen you describe with ui.make every frame. A rebind replaces the action's binding on that kind of device; keep it with input.saveBindings(). onChanged(key, value) fires after each change ("textScale", "colorFilter", "reducedMotion", "captions", or "binding" with the action) so you can save.set it: the settings are the player's, and the engine does not save them for you. Rebinding reaches only actions: a script reading raw input.key gets nothing from it.

```lua
-- the engine's Controls page, in your panel, in your styles
function start(node)
  panel = ui.engineSettings(find("Settings Body"), "controls", {
    styles = { row = "settingsRow", button = "chip" },
    onChanged = function(key, value) save.set("settings." .. key, value) end,
  })
end
function update(node, dt) panel:update() end
```

### `ui.event`

ui.event(element, "dropped") — did that element fire that hook this frame? Any hook by name; ui.clicked/pressed/released/changed/submitted are the shorthands.

### `ui.events`

ui.events() — everything that happened on the UI this frame, as { node = element, event = "clicked" } rows. ui.events("clicked") filters. Lets one manager handle a whole screen without naming a single element: for _, ev in ipairs(ui.events("clicked")) do ... end.

```lua
-- the whole screen, without naming a single element
function update(node, dt)
  for _, ev in ipairs(ui.events("clicked")) do
    log("clicked " .. ev.node.name)
  end
end
```

### `ui.focus`

ui.focus(node) — move the keyboard/gamepad focus. ui.focus(nil) drops it (a screen that wants nothing focused until the player touches something). Focusing a text field starts editing it.

### `ui.focused`

ui.focused() — the focused element as a node, or nil. ui.focused(el) answers yes/no for one element. Also readable per-node as node.focused.

### `ui.held`

ui.held() — the element the pointer is holding down, as a node, or nil. ui.held(el) answers yes/no. Hold-to-charge, press-and-hold repeat, a dip while pressed.

### `ui.hovered`

ui.hovered() — the element under the pointer, as a node, or nil. ui.hovered(el) answers yes/no for one element. A STATE, not an event: true for as long as it's true (hoverStart/hoverEnd are the edges).

```lua
-- a state, not an event: true for as long as it's true
local over = ui.hovered()
find("Caption").text = over and over.name or ""
```

### `ui.make`

ui.make(container, tree) — build a UI subtree from data and RECONCILE it with the one already there: call it again and only the difference is spawned and destroyed, so surviving rows keep their entity, their hover, their scroll and their in-flight transitions. An element is { "kind", prop = value, ..., children }, where kind is box/row/col/text/image/button/field/slider/scroll. A container (row, col, or any element given dir/gap/pad/justify/align) starts at pad = 8 and gap = 8; write pad = 0, gap = 0 for a flush stack. `wrap = true` on a container flows its children onto new lines when a line is full — an inventory grid of fixed-size tiles whose column count follows the container's width (on a text element, `wrap` wraps the text). `items = {...}` plus a function child makes one child per item (the function gets (item, i); return nil to skip it). `key = "id"` is how a row is matched through a re-sort. `onClicked = function(node) ... end` (any UI hook, `on` + its name) carries behaviour inline — no prefab, no script file. Properties the table stops mentioning go back to default; what the PLAYER did (scroll, typing, a toggle, a dragged slider) is kept. Play only, and a mistyped property raises rather than being ignored. Elements you placed by hand under the same container are never touched.

```lua
ui.make(find("Crew Panel"), {
  "col", gap = 8, pad = 12, style = "panel", items = crew,
  function(m) return { "text", key = m.id, text = m.name } end,
})
```

### `ui.off`

ui.off(element) stops every hook YOUR script is listening to on that element; ui.off(element, "clicked") stops one. Only your own — two managers on one button must not be able to unregister each other.

### `ui.on`

ui.on(element, "clicked", function(el, hook) ... end) — listen to an element from a script that does NOT live on it, so ONE manager holds a whole menu instead of a three-line script file per button. Any UI hook: clicked, pressed, released, hoverStart, hoverEnd, changed, submitted, cancelled, focusEnter, focusExit, dragStart/Move/Enter/Over/Leave/Cancel, dropped. The handler gets the element that fired and the hook name, so one function can serve a row of buttons. Registering again for the same element and hook REPLACES (so calling it from update() is harmless, not a leak). A listener dies with its element or with the script that registered it; a hot reload re-registers. Listening for an interaction the element doesn't take warns in the Console — it would otherwise be silent.

```lua
-- one menu script instead of a script file per button
function start(node)
  ui.on(find("Play"), "clicked", function() scene.load("level1") end)
  for _, b in ipairs(find("Toolbar"):children()) do
    ui.on(b, "clicked", function(el) selectTool(el.name) end)
  end
end
```

### `ui.pressed`

ui.pressed(element) — LMB went down on it this frame. Pair with ui.held(element) for hold-to-charge.

### `ui.released`

ui.released(element) — LMB came back up this frame (on or off the element).

### `ui.scale`

ui.scale() → the interface scale ui.setScale set (1 by default).

### `ui.setScale`

ui.setScale(s) — one size for the whole interface, 0.25 to 4 (1 = as designed): every UI layer's layout and text grow together, multiplied over each layer's own designHeight scaling. access.setTextScale still sizes text alone, on top of this. What an "Interface size" setting sets. Back to 1 when Play starts.

### `ui.submitted`

ui.submitted(element) — Enter in this focused text field this frame.

### `ui.unbind`

ui.unbind(node) drops every binding on that node; ui.unbind(node, "text") drops one.

## networking — net.*, synced

### `net`

Multiplayer: host and join, synced state, RPCs, ownership (net.isMine), and the rollback readouts. Open netcode — you can self-host the relay.

### `net.despawn`

SERVER ONLY: net.despawn(node) — remove a replicated runtime object, and the whole subtree it spawned with, everywhere.

### `net.host`

net.host{ maxPlayers = 16, port = 7777, relay = "addr", interest = 150, interestBudget = 16384 } — become the authoritative host. relay = a rendezvous relay address (you get a LOBBY CODE, nobody port-forwards), or "cloud" for Floptle Cloud's managed relay — which needs the project connected to a game at fopull.com/cloud, and refuses a keyless host by telling you where to get one. "cloud:us-east" pins a region instead of picking the nearest. port = direct UDP (QUIC) for LAN; neither = the in-editor loopback harness. Refused while this peer is in a session (joined, joining, or already hosting) — net.leave() first; the session you have is kept. interest = metres: each client hears about its own neighbourhood instead of the whole world (leave it off below a few dozen players — broadcasting is cheaper); interestBudget = bytes/sec of entity updates per client; inputDelay = rollback input delay in TICKS (clamped to 6) — omit it and the host derives one from the worst peer's measured RTT (2 on a LAN, 5 across a country). Who may join: requireIdentity = true refuses anyone not signed in; requireVerified = true also refuses a signed-in player whose account fopull.com did not prove with a join token; allow = { ids } makes the server invite-only and deny = { ids } bans, both checked before the join is accepted. address = "host:port" tells a direct (port) host the address players join it at, which is what their join tokens name — without it a direct host cannot verify anybody. A relay host needs no address: its lobby code is one.

### `net.identity`

net.identity(peer) — who a connected peer is: { id, name, tier, verified }. `id` is the account's stable subject, the same across sessions and machines; an anonymous peer (a LAN game with nobody signed in) has no `id`, which is a normal state and not an error. READ `verified` BEFORE ACTING ON `id`. It is true when the player's join token from fopull.com proved the account to this server, and then id, name and tier are fopull.com's. It is false for an anonymous peer, and for a claim that came without a valid token — then every field is what the client typed, and a ban list or statistic keyed on it is keyed on a string the client chose. net.host{ requireVerified = true } admits only verified players. See docs/multiplayer.md, "Identity".

`tier` is the plan the account is on, or "unknown" when that peer's client could not reach fopull.com to ask. A named tier is one the server confirmed; "unknown" is the engine refusing to guess, which it used to do by reporting "free" — so a peer that reads as free really is free.

### `net.inputDelay`

net.inputDelay() — the session's FIXED input delay in ticks. Never changes mid-match, because how the game feels must not.

### `net.isClient`

net.isClient() — true on a connected client.

### `net.isDedicated`

net.isDedicated() — true on a server with NOBODY SITTING AT IT: `floptle serve` / `floptle-server`, as opposed to a player hosting the game they are in. Both answer net.isServer() the same way, and for the simulation that is right — it does not care who started it.

This is the one thing that differs, and a game cannot work it out for itself. A dedicated server must not take a seat in its own lobby, deal itself a character, count toward "enough players to start", or be waited on to press Ready — and a lobby that treats it as a player can never begin a match, because the empty chair never readies.

### `net.isMine`

net.isMine(node) — is this node under MY control on this machine? Offline/non-networked → true; server → true unless a remote peer owns it; client → only your own predicted node(s). Cameras/HUDs use it to pick the local player out of many avatars (pair with findScripts).

### `net.isServer`

net.isServer() — true on the authoritative host.

### `net.join`

net.join(addr) — join a session: "cloud://UABCDE" = a Floptle Cloud lobby code (six characters; the first names the region, so it resolves with no call to fopull.com and works during an outage), "relay://relayaddr/CODE" = a lobby code through any relay (no port-forwarding), "wss://relayaddr/CODE" = the same lobby through the relay's browser leg (how a web build joins: a page cannot speak QUIC, so in a browser cloud:// goes this way by itself and relay:// / quic:// are refused with the address to use instead), "quic://host:port" = a server directly, "local://" = the in-editor test harness. A browser build joins; it cannot host. Optional second argument is an options table: net.join(addr, {timeout = 30}) sets how long to wait on a server that is WAKING UP (default 90 s) before giving up with "refused". It bounds only a wake — an ordinary join is answered in one relay round trip.

### `net.joinState`

net.joinState() -> state, reason — how a join is going: "offline" | "connecting" | "joined" | "refused" | "starting", plus "reconnecting" on a RELAY HOST whose relay is unreachable — during which net.lobbyCode() is nil, because the code the relay gave you does not exist while the relay does not have it. "starting" means the lobby is REAL and its dedicated server is waking up — the engine keeps retrying by itself and gives up after net.join's timeout option (default 90 s), then reports "refused" with "took too long to start" — it is not "refused", which means the attempt will never succeed, and it can take tens of seconds where "connecting" is one relay round trip. On "refused", "reconnecting" and "starting" the second return says why or how long, in the relay's own words ("no lobby QK7RM", "about 20 seconds") — print it. WAIT ON THIS, not on net.role(): joining does not block, so role reads "client" from the frame you called net.join, whether or not that code matched any lobby.

### `net.kick`

SERVER ONLY: net.kick(peer, reason) — remove a player, with words that reach them. The reason goes out before the link closes, so their UI can say what happened instead of showing the generic "connection lost" every unexplained drop produces; `playerLeft` fires with it attached, and on their machine net.on("kicked", fn) does. Kicking is not banning: without a verified id it lasts until they reconnect — pair it with net.host{ deny = { ids } }.

### `net.leave`

net.leave() — end the session.

### `net.lobbyCode`

net.lobbyCode() — the code friends type in to join, on a host that used net.host{ relay = "…" }: six characters on Floptle Cloud (they join with net.join("cloud://" .. code)), five on your own floptle-relay. Put it on your own lobby screen. nil until the relay answers (POLL it, don't read it once), and nil for good on a client or a direct/LAN host — there is no code there, joiners use the address.

### `net.mispredictRate`

net.mispredictRate() — 0..1, the fraction of simulated ticks that had to guess a peer's input. Rises with latency; what the input delay is chosen against.

### `net.notice`

net.notice() — what the relay last told this HOST about the session, or nil. Today there is one: your account is at its player ceiling, so new joins are being turned away — and nobody already playing was disconnected. Said once per episode, not once per refused join.

Worth putting on your own lobby screen. The people it concerns are the friends who cannot get in, and they are looking at your game rather than at your console; "we are full right now, someone will be along" is a far better experience than a join that silently fails. nil on a client and on a direct/LAN host, where there is no relay to say anything.

### `net.on`

net.on(event, fn) — session events: playerJoined/playerLeft (peer id), connected, disconnected (reason).

### `net.peers`

net.peers() — connected client peer ids (server).

### `net.ping`

net.ping(peer?) — round-trip time in ms.

### `net.random`

net.random(a?, b?) — deterministic RNG for a rollback match, drawn from (match seed, tick, draw index): every peer rolls the same number AND a re-simulated tick rolls it again. Use this instead of rng() in anything a rollback node reads — an unseeded roll comes from the clock, and two peers drawing differently is a match that quietly forks in two. No args → [0,1); one → integer 1..a; two → a..b.

### `net.receiving`

net.receiving() — bulk messages still on their way in: { {from, name, got, total}, ... }, bytes so far and in all. What a joiner's loading bar reads while the level arrives. Empty when nothing is arriving.

### `net.replaying`

net.replaying() — true while the engine is RE-SIMULATING ticks it already ran after a correction. For cosmetics the engine can't gate for you (a screen shake, a UI poke). NEVER branch simulation on it: a replayed tick that computes something different from the live one is the definition of a desync.

### `net.rewind`

SERVER ONLY, inside onRpc for an rpc sent {withInput=true}: run the closure against the world as that peer PERCEIVED it — raycasts and other scripts' synced vars read the rewound tick (clamped ~250 ms). A parry that was up on the attacker's screen counts.

### `net.role`

net.role() — "offline" | "server" | "client".

### `net.rollbackAverage`

net.rollbackAverage() — mean ticks re-simulated per correction. The texture of the connection, where rollbackMax is only its worst moment. A healthy match sits low.

### `net.rollbackDepth`

net.rollbackDepth() — ticks re-simulated by the most recent correction.

### `net.rollbackMax`

net.rollbackMax() — the deepest rollback this session has had to perform: its worst moment.

### `net.rpc`

net.rpc(name, args, {to=peer, withInput=true}) — remote call: server→clients or client→server. withInput stamps a client intent with the tick it was seeing (for net.rewind). Handle with function onRpc.name(args, sender). Args: scalars + tables (≤4 deep, ≤1KB).

### `net.send`

net.send(name, data, {to=peer}) — a bulk message: any bytes (a Lua string) up to 16 MB, such as a whole player-built level for someone who just joined. The engine cuts it into slices and puts it back together; it arrives whole, reliably, and in order with RPCs, so an rpc sent after it is handled after it. Server→one peer (to) or everyone; client→server. Handle with function onData.name(data, sender). Over 16 MB is refused with a Console warning.

### `net.setInputDelay`

net.setInputDelay(ticks) — the rollback input delay for the NEXT match, in ticks, clamped to 6. Too low and the opponent's input lands after the tick that needed it on every tick, so the driver guesses and re-simulates: correct, and five times the work. Fixed for a session on purpose — adaptive delay hides a bad connection by changing how the game FEELS while you are playing it. Call it between matches; the roster re-announce restarts the driver.

### `net.setOwner`

SERVER ONLY: net.setOwner(node, peer) — hand a replicated node to a player after it already exists; net.setOwner(node, nil) releases it back to the server. Ownership used to be settable only at spawn, which is why a player who dropped could never be given their own slot back. On a dedicated server, authored Predicted slots are handed out from #1 as players join and freed when they leave — a slot you assign yourself is never reassigned behind your back.

### `net.setRelevant`

SERVER ONLY: net.setRelevant(node, peer, true|false) — decide per client whether that client may be told about that node at all, on top of the interest radius; pass nil to hand the decision back to the radius and the line-of-sight test. This is the hidden-role hook. It has to be server-side: a client that has already been sent a position knows the position, whatever it chooses to draw, so hiding or attenuating it client-side is a setting a modified client turns back off. Holds whether or not interest management is on: with it off, every client is still sent everything EXCEPT what it has been pinned away from. The 🌐 panel shows how many nodes each client is being withheld, and why.

### `net.spawn`

SERVER ONLY: net.spawn(what, { x, y, z, owner }) — spawn a replicated runtime object on every client (available next tick). `what` is a prefab by name ("Knight" finds prefabs/Knight.prefab.ron), a prefab path, or a scene path ("scenes/thing.ron", whose first root node and its children are spawned). The table is optional: x, y, z are three numbers (not a vec3); owner = a peer id makes a Predicted rig that player's avatar. Offline, spawn(what) does the same thing without a session.

### `net.stalled`

net.stalled() — true while the sim is waiting for a peer's input rather than guessing past the depth cap. The game runs slightly slow instead of teleporting the opponent. Drive your own "connection trouble" banner off this — a stall is otherwise indistinguishable from a bad frame rate.

### `net.traffic`

net.traffic() -> {{kind=, count=, bytes=}, ...} — what this peer has SENT since the last call, broken down by message kind, biggest first. RESETS on read, so successive calls measure the interval between them. Use it to find out where your bandwidth goes: a rollback game should be almost entirely "Input"/"Inputs" and a few hundred bytes a second — a large "Snapshot" row in a rollback match means state is going out where inputs should.

### `onData`

onData.<name>(data, sender) — handles net.send("name", data): data is the whole message as a Lua string, sender the verified peer id (0 = server).

### `onRpc`

onRpc.<name>(args, sender) — handles net.rpc("name", args). sender is the verified peer id (0 = server).

### `replicated`

replicated = { hp = 100 } — declare synced script vars (top level). Read/write them as synced.hp; the server's writes replicate to every client.

### `restore`

function restore(s) — the other half of snapshot(): put the table back. Called before the engine re-simulates a tick it already ran. Restore every key snapshot() returned, and nothing else.

### `snapshot`

function snapshot() — REQUIRED on a rollback node's scripts. Return a flat table of every gameplay value this script owns (state, frame counters, health, stun). The engine calls it each tick and restores it when a correction arrives. ANYTHING you leave out is a value that survives a rewind unchanged — which is exactly what a desync is made of. Transforms and physics bodies are saved for you; do NOT put them in here.

### `synced`

The synced-vars table (declared via replicated = {...}). Server writes replicate; client writes warn and get overwritten.

## scenes — load, unload & persist

### `scene`

Which world is loaded: scene.load / scene.unload, additive layers, and scene.onLoaded. Pair with node.persistent to carry a node across a swap.

### `scene.current`

scene.current() — the running scene's name (its file stem, e.g. "first").

### `scene.list`

scene.list() — every scene in the project as names scene.load accepts (sorted; subfolders kept).

### `scene.load`

scene.load("arena") — switch to another scene at the next frame boundary: the world swaps, physics/animators/particles/audio rebuild, every start re-fires (like the scene booting fresh). scene.load("arena", { fade = 0.6, color = {0, 0, 0} }) fades the screen to that colour over half the time, swaps while it is covered, and fades the new scene in over the other half. Accepts a name, a scenes-relative path ("arenas/desert"), "scenes/arena.ron", or a scene in the player's own files ("user://stages/arena"), whose map is user://maps/arena.map.ron. Multiplayer: only the SERVER may call it — every client follows automatically; a client's call is refused (send the server an RPC instead). scene.load("cave", { additive = true }) layers a scene over this one instead of replacing it; add offset = vec3(...) to place it, or parent = node to put its roots under that node so it rides it (offset is then in the node's space; a parented layer stays on this machine), and on a hosting server every client (and every late joiner) loads the same layer, its Networked nodes replicating — replicate = false keeps it on the server. A client's own additive loads stay its own.

### `scene.onLoaded`

scene.onLoaded(function(name, additive) ... end) — run something once a scene has finished loading. Fires AFTER the world is whole, because a loading screen's whole job is to go away once the thing it was covering exists.

### `scene.unload`

scene.unload("Shop") — remove a scene that was loaded additively, and everything under it. The other half of scene.load{ additive = true }. Only that layer's sounds, effects, animators and colliders go; everything else keeps playing. On a hosting server every client unloads it too.

## terrain — runtime sculpt & queries

### `terrain`

Runtime sculpting and queries against the SDF terrain: dig, sculpt, paint, ask what is under a point, and persist edits per save slot.

### `terrain.busy`

terrain.busy() — is the background terrain worker already occupied? True while any field is generating or streaming in. Whole-body fills and residency streaming share one background budget, so a game that BUILDS ITS WORLD AS THE PLAYER TRAVELS should ask before queueing the next one — otherwise the new world goes in behind the ground somebody is standing on. The pattern: build one thing, wait for this to go quiet, build the next.

### `terrain.capacity`

terrain.capacity() → how many more terrain edits the queue takes right now before it refuses them (it holds 4,096 waiting). A game sending a big batch sends at most this many a frame and the rest on the frames after, and none of them is ever refused. A refused edit answers the receipt 0 and is counted in perf.counts().terrainDropped.

### `terrain.deleteSaveDir`

terrain.deleteSaveDir("saves/slot2/terrain") — delete a save slot's persisted terrain from disk (pair with save.deleteSlot in a "delete this save" UI). Narrow by design: relative path, no "..", and only .cfield/.tfield/.meta files in that one directory are removed (emptied dirs are tidied). The ACTIVE saveDir is refused while terrain in the scene still uses it (release it with terrain.saveDir("")); from a scene with no terrain, such as a menu after scene.load, it is deleted and released, so terrain.saveDir() reads nil. Returns the number of files removed.

### `terrain.dig`

terrain.dig(x,y,z, radius [, strength]) — carve a hole: sugar for terrain.sculpt(..., "lower"). Pair with raycast(...) to dig where the player aims.

```lua
if input.clicked("left") then
  local hit = raycast(node.pos, node.forward, 6)
  if hit then terrain.dig(hit.x, hit.y, hit.z, 1.5) end
end
```

### `terrain.editBudget`

terrain.editBudget([ms]) → the milliseconds of each frame the engine spends landing the game's terrain edits (4 by default); the rest wait, in order, for the frames after. Raise it while a loading screen covers the view, so a big batch (a base site graded in a thousand flattens) lands sooner, and set it back, or pass nil for the default, when the view is shown. Clamped to 0.5..1000. Returns the budget it replaced; with no argument it only reads it. A scene swap resets it. Refuses: anything that is not a number or nil.

### `terrain.flush`

terrain.flush() — checkpoint every EDITED resident terrain field to the save slot (terrain.saveDir must be set). Runs IN THE BACKGROUND (amortized encode + threaded write, deferred while a field is actively being dug) so autosaves never stutter; exit paths (Stop / scene.load) finish the writes synchronously so a checkpoint is never lost.

### `terrain.generatePlanet`

terrain.generatePlanet(id [, opts]) — REPLACE terrain id's whole field with a generated planet (sphere ± noise relief, caves + chambers, molten core, craters, layered materials). Background-generated (seconds; Console shows progress). opts (all optional): radius, voxel, relief, bumpFreq, caveDepth, coreR, corePaint, craters, craterMin/Max, craterDust, surfaceA/B {slot,color}, patchBias/Thr, subsoil(+Depth), strata(+Depth), deep, pockets {slot,color,threshold,minDepth}, seam {slot,color,minDepth,center,width}, iceCaps {lat,slot,color}, seed. Relief layers (each a table whose keys all default; see docs/planet-generator.md): mountains {height,scale,sharpness,coverage,octaves}, warp {strength,scale}, plateau {height,smooth}, terrace {step,sharpness} (each of these four also takes latitude={min,max} in degrees from the equator, north or south unless either is negative, and latitudeEdge, the degrees it fades over past them), overhangs {depth,scale,strength}, ravines {count,depth,width,length,meander}, rivers {count,depth,width,seaLevel,meander}, biomes = { {slot,color,latitude={min,max},altitude={min,max},slope={min,max},moisture={min,max},nearRiver}, ... } (first match paints the surface), moistureScale. deferCaves = true leaves the buried cave zone ungenerated until a dig reaches it: a quarter less to fill and keep, the same caves when they are made (to a hair at chunk faces). generator = N makes the world by version N of the generator's rules (0 up to this engine's); left out, by the project's, which an upgraded project keeps at the version it was made by, so its saved digs stay where they were. smallBody = { shape = "potato" | "binary" | "rubble", elongation = 1.5, lumpiness = 0.35, neck = 0.35, lumps = 8 } makes an irregular small body of size radius instead of a ball: one stretched lump, two lobes joined at a neck along X, or a pile of lumps; it has no cave zone or craters, and relief roughens it.

### `terrain.height`

terrain.height(x, z) — world Y of the highest terrain surface under (x,z), or nil when nothing is hit. Spawning, footstep audio by ground, drop-to-floor.

### `terrain.impostorAt`

terrain.impostorAt(id [, { radii = n } | { pixels = n }]) — when the planet with terrain id `id` stops drawing its ground and draws as one shaded ball, one draw in the colour its ground draws: past `radii` of its own radii from the camera, or once its disc is smaller than `pixels` across on screen. Its ground loads in as much farther out as the rule keeps it drawn. With no rule it goes back to the default, sixty radii. Rules last until Play stops.

### `terrain.lodAnchor`

terrain.lodAnchor(node | vec3 | nil) — what terrain detail, the far-planet impostor switch and far-craft freezing follow during Play, in place of the render camera. For a map view: anchor on the ship while the map moves the camera thousands of units away, and the ground under the ship stays meshed exactly as it was; nil hands it back to the camera. Returns the anchor it replaced. A scene swap clears it. Refuses: anything that is not a node, a vec3 or nil.

### `terrain.paint`

terrain.paint(x,y,z, radius, r,g,b [, strength]) → receipt — recolor the terrain surface inside the brush ball (0..1 colors). Answers the edit's id, as sculpt does, or 0 when the queue was full and the edit was refused (see terrain.capacity).

### `terrain.paintTexture`

terrain.paintTexture(x,y,z, radius, slot) → receipt — paint a terrain-palette texture slot (1-based, the Terrain tab's palette; 0 clears to flat color). Answers the edit's id, or 0 when the queue was full and the edit was refused (see terrain.capacity).

### `terrain.pending`

terrain.pending() → how many of the game's terrain edits (dig, sculpt, flatten, paint, paintTexture) have not landed yet. Edits are applied a few milliseconds' worth a frame, in the order they were made, so a batch of a thousand flattens spreads over frames instead of stopping one. An edit too big for a frame (four voxels across its radius or more), or one over ground the world has not made yet, is made off the frame and lands whole a frame or more later, the edits after it waiting their turn; a run of them is made one after another while the frames go on, and each frame lands what is ready. terrain.editBudget sets how much of a frame landing takes. Every edit is applied whole before the next, so queries read the ground after some number of whole edits; 0 means every edit made so far is in the ground.

### `terrain.planetLakes`

terrain.planetLakes(opts [, { cell = 3, minDepth = 0.5, seaLevel = ... }] [, callback]) → the lakes the planet's basins hold, without generating anything: { {level=, center=, reach=, area=, depth=}, ... }, largest first. Every basin is filled to the height it spills at. `level` is metres above the radius, `center` a planet-local point on the lake's surface, `reach` how far its farthest shore is from the centre and `depth` its deepest point, all in metres. Ground below `seaLevel` (default: the rivers' seaLevel, or none) is sea and not a lake; with no sea, the planet's lowest point is where it drains, and holds none. Found on a grid of cells `cell` metres across; only water deeper than `minDepth` counts. For placing water at the level it would settle at. A large world's lakes take seconds to find: given a callback, the call returns nothing, the work runs off the script thread, and callback(lakes) runs in a later frame.

### `terrain.planetRivers`

terrain.planetRivers(opts [, callback]) → the rivers and ravines terrain.generatePlanet carves with the same opts, without generating anything: { {river=true|false, depth=, points={x1,y1,z1, x2,y2,z2, ...}, widths={...}}, ... }, river ones source first. Points are planet-local, on the sphere of the planet's radius (the carved bed is below the ground there); widths are per point in world units. A river runs from its spring the way water drains, across any basin on its way, to the sea or into another river. For laying water, bridges or missions along them. Given a callback, the call returns nothing, the work runs off the script thread, and callback(channels) runs in a later frame.

### `terrain.prepare`

terrain.prepare(x, y, z, r) — make the stand-in ground within r of the point (capped at 256) exactly, on background workers: a streamed world's far ground and deep rock not generated yet. For a game about to plan from queries somewhere nobody is standing; terrain.ready turns true there once it has landed, and ground made this way is kept as long as its world is loaded. An ask over a world still generating or streaming in waits for it and lands when it does; one over a world not loaded at all (terrain.warm brings one in) waits as long as that world is there to come in. On a moving world the ask is the ground where the script read it, wherever the world has gone since.

### `terrain.query`

terrain.query(x,y,z) — signed distance to the nearest terrain surface (negative = inside rock), or nil with no terrain. Where the ground is not made yet, or is a stand-in, the number is not final: terrain.ready says which. Exact within four voxels of the surface (the band the field stores) and clamped to ±4 voxels beyond it — ±6 m at a 1.5 m voxel — so read it as "near the surface or not" further out, and raycast for a far distance. Cheap: read it every frame (burrow checks, depth meters).

### `terrain.raycast`

terrain.raycast(origin, dir [, maxDist]) → { pos =, normal =, distance =, exact = } for the first terrain surface along the ray, read from the ground's own field, not from collision meshes that may not be built yet; maxDist defaults to 1000. A miss answers nil and why: nil, "air" when the ray reached maxDist through open space, or nil, "unknown", distance when it reached ground the world has not made yet first (a body generating, streaming in or put away, or stand-in ground it passed over) — the distance along the ray where it did. exact is false where the ray met stand-in ground anywhere on its way to the hit (see terrain.ready): the hit may move, or come sooner, when that ground is made. A hit with exact true and an "air" miss are final until the ground is edited.

### `terrain.ready`

terrain.ready(x, y, z [, r]) → whether the ground within r of the point (default 0) is made, exactly and for good. False where a terrain body that reaches it is still generating, streaming in or put away — terrain.query, height and raycast read it as open air there — and false where the ground is a stand-in: a streamed world's far ground, held as a coarse copy, and deep rock not generated yet, both made exactly when something comes near or terrain.prepare asks. What query and raycast answer where this is true does not change until the ground is edited. True over open space no terrain reaches. A body that starts streaming this frame is reported from the next.

### `terrain.saveDir`

terrain.saveDir(path) / terrain.saveDir() — set (or read) the game's SAVE-SLOT directory for player-edited terrain, relative to the project root (e.g. "saves/slot1/terrain"). While set, streaming loads fields from here first (before the project file or the genspec) and writes edited fields back on stream-out — per-slot terrain persistence. "" clears; auto-cleared when Play stops. Kept across scene.load, so a menu can set it before loading the world; terrain.deleteSaveDir from a scene with no terrain deletes and releases it.

### `terrain.sculpt`

terrain.sculpt(x,y,z, radius [, strength [, mode [, opts]]]) — sculpt the nearest terrain at a world point, landing the same tick while the frame's few milliseconds of edits allow (collision updates with the surface). mode: "raise" (default), "lower"/"dig", "smooth", "flatten"; strength 0..1. "flatten" levels to the terrain's up, or on a planet to the radial up at the point, over a disc in that plane; opts = { normal = vec3 } names another plane (world space). Edits land a few milliseconds' worth a frame, in order, each whole before the next, and each where it was aimed when it was made, on a planet that moves on its rails too; terrain.pending() counts the ones still to land. No-op when no terrain surface is near the point. Refuses: an unknown opts key, a zero normal. Multiplayer: run on the server + mirror by RPC (deterministic ops).

### `terrain.setBudget`

terrain.setBudget(bytes | nil) → the most memory every terrain together may hold (terrain.stats().total). While the terrain holds more, it keeps less of its detail, the farthest first: a tenth more a second, to at most three quarters, and it takes the detail back once it holds under 85 % of the budget. What the player can touch, the ground within 64 m drawn in full and 120 m at no coarser than half, is kept whatever the budget; a budget too small for that is said once in the Console. nil (the default) sets no budget.

### `terrain.slotAt`

terrain.slotAt(x, y, z) — the texture-palette slot at a world point, or nil where the field is untextured. The material half of the question terrain.query answers the distance half of: survey before you cut, and let a footstep know what it is standing on.

### `terrain.stats`

terrain.stats([id]) → what the terrains hold in memory, in bytes: { total=, field= (voxels at full detail), standIns= (chunks put away as coarse stand-ins), tables=, meshesGpu= (drawn chunk meshes, at their capacity), meshesCpu=, collision=, shadow= (the volume shadows and AO are marched through), undo=, dataChunks=, standInChunks=, terrains=, age=, budget=, detail= }, every terrain summed, or terrain id's alone (nil when there is no such terrain). Counted once a second while a game runs; age is how many seconds old the count is (-1 before the first). Chunks shared between copies of a field (the physics' copy, Play's snapshot) are counted once. budget is what terrain.setBudget set (nil for none), and detail how much of its detail the terrain keeps to stay in it, 0 to 1.

### `terrain.warm`

terrain.warm(bodyName) — keep that body's terrain RESIDENT this frame regardless of where the ship/player physically is: it streams in if cold and never streams out. Immediate mode — call every frame while you care (the map warms its focused planet). Streaming is otherwise anchored to dynamic bodies' physical positions and to the camera (or terrain.lodAnchor, while one is set): a body loads as any of them nears it, and is put away only once all have left it far behind.

### `terrain.yields`

terrain.yields() — drains what recent digs actually removed: { id, removed, added, untextured, slots }, with slots mapping palette slot to volume. This is how mining pays out by MATERIAL — you get ore because you cut rock that was painted as ore.

## pathfinding — nav.*

### `nav`

Pathfinding over the scene's navmesh — where characters can walk, and how they get anywhere. Bake one first: add a Nav Mesh node and press Bake. Everything here is in world coordinates.

### `nav.AREA_STRIDE`

How many numbers nav.areas() uses per area (11). Read it rather than writing the number: fields are appended, never inserted, so code written against the constant keeps working.

### `nav.LINK_STRIDE`

How many numbers nav.links() uses per link (8).

### `nav.agent`

nav.agent(node[, opts]) — make this node something that walks the navmesh, and get a handle to order about. THE call for "move a unit from A to B": agent:moveTo(point), and it finds its own way, goes round its neighbours, slows down at the end and stops. Options, all optional: speed, accel, radius, arrive (how close counts as there), slow (where it starts easing off), avoid (take other agents into account), priority (who gives way), separation, repath (seconds between route checks), giveUpAfter (seconds of no progress before it reports blocked), drive ('auto' | 'transform' | 'velocity' | 'none'), and filter = { avoid = {'water'}, cost = { mud = 0.5 }, canDrop = false, canJump = false }. canDrop and canJump refuse the drops and jumps the bake found for itself — a turret, a cart or anything that does not leave the floor says no here rather than needing its own bake. drive defaults to 'auto': a node with a physics body is steered through the body, one without has its transform moved. The whole crowd is stepped once a frame by the engine, after your update — you never call a step function. ON A PROCEDURAL OR STREAMED LEVEL THE NAVMESH ARRIVES AFTER start(): there is no geometry at all when start() runs, so nav.ready() is false, and asking for the agent once behind that check means it is never made and every routing call silently takes your fallback for the rest of the session. Ask every frame until you have one — a script that handles 'no navmesh yet' gracefully handles 'no navmesh ever' identically, which is why this fails quietly.

### `nav.agents`

nav.agents() — how many nav agents exist right now. For a HUD, a test, or checking that the ones you destroyed really went.

### `nav.areas`

nav.areas() — every walkable area, as ONE FLAT ARRAY of numbers plus a count. Eleven numbers each, in nav.AREA_STRIDE steps: minX, minZ, maxX, maxZ, yMin, yMax, region, centreX, centreY, centreZ, ground — all world space, and ground is a one-based index into nav.ground(). Flat rather than a table per area on purpose: a real bake is thousands of areas, and a held Lua table costs one of a few thousand mlua slots, so a table each exhausts them and panics the editor rather than raising something a script could catch. One array costs one slot however big the level is. Read it as: local a, n = nav.areas(); for i = 0, n - 1 do local o = i * nav.AREA_STRIDE ... end

### `nav.budget`

nav.budget([n]) — how many path searches the whole crowd may run per frame (default 8); returns the current value, and sets it when given a number. A hundred units given one order do not all think on the same frame: they queue, oldest first, and keep walking their old route while they wait. Raise it for a game where a burst of orders should be acted on at once, lower it if the searches show up in a frame graph.

### `nav.clearObstacles`

nav.clearObstacles() — take every runtime obstacle away at once and give the whole level back, returning how many there were. For a wave ending or a level resetting, so nothing has to have kept a list of every crate.

### `nav.distance`

nav.distance(from, to) — how far it is to WALK, in metres, or nil if there is no complete route. This is the number a decision should be made on: the straight-line distance to something on the far side of a wall is a lie, and "chase the nearest one" built on it picks the wrong one every time.

### `nav.ground`

nav.ground() — the kinds of ground this bake knows about, as { {name, cost}, ... } in the order an area's ground numbers them. These are the names a filter says: avoid = {'water'} means something only because the level called an area that, and this is how a script finds out which names the level offers instead of guessing at one and having a typo read as nothing to avoid. Tables rather than a flat array, unlike its neighbours — a level has a handful of these where it has thousands of areas, and a name cannot be a number. nil with no bake.

### `nav.link`

nav.link(name | id[, open]) — open or shut a Nav Link, or ask whether it is open. nil when there is no link by that name. This is the door: nav.link('front gate', false) makes every route that used it repath, nothing is rebaked, and a unit already halfway across finishes crossing rather than stopping in mid-air.

### `nav.links`

nav.links() — every portal between two areas, as one flat array plus a count. Eight numbers each, in nav.LINK_STRIDE steps: from, to, leftX, leftY, leftZ, rightX, rightY, rightZ. from and to are ONE-BASED indices into nav.areas(); left and right are the portal's endpoints as somebody walking from `from` into `to` sees them, so a smoother never has to work out which side of itself it is on. Each portal appears once per direction.

### `nav.nearest`

nav.nearest(point[, maxDistance]) — the closest walkable spot to a world point, or nil if there is none within range (default: the character's own height, so standing on top of the floor or half a step off a ledge is the ordinary case rather than a miss). Use it to drop a click, a spawn or a knocked-back character back onto the navmesh.

### `nav.obstacle`

nav.obstacle(centre, size) — cut a box out of the baked navmesh, right now, and get a handle back. The crate dropped in a corridor: routes through that space stop existing, everything walking one repaths, and the level is not measured again. Hundreds of times cheaper than a rebake for a small thing on a big level — a 256 m level rebakes in ~460 ms and carves in ~0.6 ms. It is an OPTION and not a replacement: where the level genuinely changed shape (a building came down) the background rebake is the honest answer and this is not. The hole is grown outward to whole navmesh cells, so read ob.size rather than assuming you got the box you asked for. nil when the scene has no bake. There is deliberately no moving obstacle: carving every frame is rebuilding every frame, which is the cost this exists to avoid.

### `nav.obstacles`

nav.obstacles() — how many holes nav.obstacle has cut in the navmesh right now. Zero with no bake.

### `nav.offLinks`

nav.offLinks() — every off-mesh link in the level as data: { id, name, from, to, bidirectional, cost, duration, enabled, ground, kind, generated }, world space. kind is 'placed' for a Nav Link node somebody put there, or 'drop' / 'jump' for one the bake worked out from the shape of the floor; generated is the same fact as a boolean. Distinct from nav.links(), which is the thousands of portals the bake derived between neighbouring areas — two things called links is inherited and worth knowing before reading either. Use nav.link(name, open) to change one; this only reads. nil with no bake.

### `nav.onMesh`

nav.onMesh(point[, tolerance]) — is this point on the walkable surface? The allocation-free version of nav.nearest, for the per-frame "am I still on the floor" check that does not want the point back. False when there is no navmesh at all, so it never raises.

### `nav.path`

nav.path(from, to) — the corners to walk between two world points, as a list of vec3, plus a second return saying whether it REACHES the goal. Returns nil when an end is not on the navmesh at all (off the level, or inside a wall) — which is a different thing from a goal that is on the mesh but cut off, and that one comes back as a real route to the nearest reachable point with false alongside it. Walk it and stop is the right behaviour there; standing still because the answer was empty is not.

### `nav.random`

nav.random(u, v[, near, radius]) — a point somewhere on the walkable surface, weighted by area so a big room is likelier than a corridor. The two numbers 0..1 are YOURS — call it as nav.random(math.random(), math.random()). That is deliberate: the engine rolls back and re-simulates, so a wander destination has to come from the same seeded stream as everything else the tick decided, and a navmesh that reached for its own randomness would desync every rollback that touched it. near and radius restrict it to a square neighbourhood (a square, not a circle — sampling a circle needs a re-draw, and there is no stream here to re-draw from).

### `nav.raycast`

nav.raycast(from, to) — walk a straight line across the surface and get back where it stops, or nil if the whole line is walkable. The walker's answer rather than the collider's: a ledge this character would fall off is empty air to a physics ray and a wall to this. Use it to decide "can I just walk at it" before asking for a full path.

### `nav.reachable`

nav.reachable(from, to) — can something actually walk from here to there? Different from nav.path(...) ~= nil: a path that exists but only gets partway comes back with false alongside it, and this is that flag. Cheaper than a path when the yes-or-no is all you wanted.

### `nav.ready`

nav.ready() — whether this scene has a baked navmesh to ask. False is the ordinary state of a project that has not made one, not an error.

### `nav.rebake`

nav.rebake(centre, size) — re-measure this box of the level and splice the answer into the navmesh, in the same frame. THE call for a level that builds itself: a streamer that has just finished a chunk, a generated room, a wall that came down. A full rebake measures the WHOLE level to account for one box, so the cost of building a chunk grows with how much level is already loaded — which is backwards, because the amount of new level per chunk is constant. This costs the box. Different from nav.obstacle: a crate standing on the floor is an obstacle and can be taken away again; a corridor that has just been built is a rebake and becomes the level. Carved obstacles survive it. It QUEUES like spawn (re-measuring needs the world's triangles, which the scripting side does not have) and lands in the same pass, after that pass's spawns and destroys — so build the chunk and ask in the same breath. World coordinates; the box is snapped outward to whole navmesh cells. Needs a navmesh already baked to splice into.

### `nav.regionOf`

nav.regionOf(point[, tolerance]) — which walkable island a point is on, or nil if it is not on the navmesh. Two points in different regions can never be walked between, so comparing two ids rules out a search that was never going to succeed. The number itself means nothing beyond "the same one is the same island".

### `nav.sampler`

nav.sampler([near, radius]) — the neighbourhood gathered ONCE, for many draws: local s = nav.sampler(node.position, 20); local p = s:point(math.random(), math.random()). nav.random re-gathers, re-sorts and re-measures every polygon its window covers on every call, so its cost grows with the radius — a dozen agents redrawing a destination in one frame was measured at 4 ms. A squad wanders around the same place, so hold the gather and each draw becomes a binary search. s.count is how many polygon parts the window covers; a count of 0 answers nil forever, which otherwise reads as bad luck. s:point(u, v) takes the same caller-supplied 0..1 pair nav.random does and for the same reason. A sampler is a SNAPSHOT: nav.splice re-baking the level does not reach one already built, so rebuild it when the ground changes.

### `nav.settings`

nav.settings() — the character the mesh was baked for: radius, height, maxSlope, stepHeight, cellSize, maxDrop (the tallest ledge it steps off), maxJump (the widest gap it clears), minRegionArea (the smallest patch of ground kept), plus areaCount and area (square metres of walkable ground). A script moving a body along a path needs the radius the mesh was eroded by; guessing it is how a character ends up scraping the wall the erosion existed to avoid.

## water — depth, buoyancy & ice

### `water`

Water volumes: how deep a point is (water.depthAt), what is in the water (water.at), freezing and thawing (water.setFrozen). The engine already does buoyancy and drag — these are the questions a GAME still has to answer: swimming, drowning, flooding, a gauge going red.

### `water.addVolume`

water.addVolume{ shape = "lake" | "heightfield", parent = "Planet", ... } → an id. Water no node carries, in the frame of the node it lies on, so it turns with its planet. A lake takes `center` (a point on its surface) and `reach` (how far its farthest shore is), exactly as terrain.planetLakes gives them; a heightfield takes `origin`, `cell`, `columns`, `rows` and `levels` (false = dry), with an optional `flow`. It floats things, carries them on its `current`, and water.at answers for it. `depth` is how far down its bed lies (planetLakes gives it), so a cave under a lake is dry; from inside it looks like a Water Volume node, with `tint = {r,g,b}` and `visibility` (metres) to change that and `underwater = false` for no look. shape = "sim" is water the engine moves: `origin`, `cell`, `columns`, `rows` as a heightfield and `level` to fill it to; the engine finds the ground under it, then the water flows, drains into holes dug beside it and settles, and water.at, flowAt and floating follow it. A sim is a river with `springs = { { pos = vec3, rate = m³/s }, ... }` (each at the point nearest its pos), `roughness` (the bed's Manning roughness: 0 = still water's frictionless model, the default; 0.035 = a natural river bed, which runs the water at a real river's depth and speed) and `holdEdges = true` (edge points below `level` held at it: the sea beyond the patch, which fills a trench dug from its shore). Water off a drop passes what a weir does, so a river over a cliff is stable; water.falls says where it pours.

### `water.at`

water.at(point) — nil in air, else { depth, density, frozen, node, up }. `up` is the way OUT of the water (radial on a sea, the pool's own +Y) — what a swim controller pushes along, and NOT the same as -gravity in a tilted tank. Innermost volume wins, so a tank inside an ocean answers as the tank.

### `water.depthAt`

water.depthAt(x, y, z) — or a vec3, or a node. Metres BELOW the surface at that point; 0 in air. The one number everything else is derived from, and it is the same rule the solver uses, so a swim state can never disagree with the physics that floats you. A frozen volume reads 0 everywhere.

### `water.falls`

water.falls([id]) — every place simulated water (a `shape = "sim"` volume) pours off a drop of more than 1.5 m, in the world: { pos, dir, width, discharge, drop, volume }. `pos` is the top of the fall at the water's surface, `dir` the way it pours across the ground, `discharge` m³/s over the edge and `drop` metres from the surface above to the surface below. Refreshed as the water flows, so a fall that runs dry leaves the list: where a game puts a fall sheet and its mist, and takes them away. With an id, only that volume's.

### `water.flowAt`

water.flowAt(point) → vec3, the water's velocity there in m/s: a river's current, zero in still water and in air. The same current the physics carries a floating crate or a swimmer in.

### `water.isUnderwater`

water.isUnderwater(point) — the yes/no, for when you don't need the depth. Takes x,y,z or a vec3 or a node: if water.isUnderwater(node) then stamina = stamina - dt end

### `water.removeVolume`

water.removeVolume(id) → true if there was such water. Takes away what water.addVolume declared.

### `water.setFrozen`

water.setFrozen(node, true) — freeze a water volume. Freezing is a STATE, not a second system: the same node with a flag flipped, and both the physics (no buoyancy, no drag) and the look follow from it. A world that thaws is one call back.

### `water.surface`

water.surface(id [, { cells = 48 }]) → the arrays mesh.build takes for that declared water's surface, in its parent's frame: mesh.build(water.surface(id)) on a node at the parent's origin draws exactly the surface water.at answers for. A lake is the part of its sphere in its basin: what its centre reaches without crossing the ground, out to a ring under the shore (with no physics running, the whole disc out past its farthest shore); uvs are metres. nil for no such water.

### `water.volumes`

water.volumes() — every body of water in the scene, as node handles. What a climate or weather system iterates when it wants to know where the seas are.

## weather — wind

### `weather`

Wind: one field that particles and props are moved by, declared by your game and read back with weather.windAt. A wind blows in the frame of the body it rides, so a planet's prevailing wind turns with the planet; one with a radius is a local cell (a storm, a valley's down-draught) that adds to the winds under it and fades to nothing at its edge. It is a pure function of place and time, so every machine agrees on it and it costs nothing until something asks.

### `weather.addWind`

weather.addWind{ velocity = vec3(4, 0, 0), gust = 0.4, gustPeriod = 6, turbulence = 0.2, scale = 40 } — declare a wind, get its id. `velocity` is metres a second. `gust` swings the speed by that fraction (0.4 blows between 0.6 and 1.4 times it), roughly every `gustPeriod` seconds, and a gust travels downwind at the wind's speed, so it is seen crossing a field rather than everything moving at once. `turbulence` adds eddies `scale` metres across, as a fraction of the speed, drifting with the wind. `parent = "Kail"` blows in that node's frame (then `velocity` and `center` are in it too). `center` + `radius` makes a local cell. An option this doesn't list is an error.

### `weather.bolt`

weather.bolt(from, to [, { seed = 1, branches = 3, jag = 0.15 }]) → the strands of a lightning bolt to draw: { {points={x1,y1,z1, x2,y2,z2, ...}, width=, parent=}, ... }. The main channel runs from `from` to `to` and comes first; each fork leaves from a point on it and names it as `parent`, with a `width` under 1 to draw it thinner. `jag` is how far the channel wanders sideways for its length (0 is straight). The same seed is the same bolt on every machine, so a strike a host announces looks the same to every player. Draw it with draw.line or as beams, for the frames the flash lasts.

### `weather.removeWind`

weather.removeWind(id) — the wind stops. Returns true if there was one. Every wind is dropped when the scene changes.

### `weather.setWind`

weather.setWind(id, { ... }) — change the options it names and keep the rest: a storm building is `weather.setWind(storm, { gust = 0.8 })` each few seconds. `radius = 0` makes a cell blow everywhere again. Returns false when there is no such wind.

### `weather.windAt`

weather.windAt(point) → vec3, the wind at that point in world space: every wind there added up, at this frame's time. The same number the engine moves particles and props by, so a flag your script waves agrees with the smoke beside it.

## scatter — instanced props

### `scatter`

Thousands of props from a seed — GPU-instanced, with no scene node anywhere in it. Your generator still decides WHAT grows where; the engine decides where each instance stands and draws them. scatter.create declares a source, scatter.remove harvests one.

### `scatter.clearTints`

scatter.clearTints(sourceId) — every prop of the source back to the source's own look; returns how many had one of their own.

### `scatter.cost`

scatter.cost(id) — what this source asks for every frame: { chunks, props, far, chunkSize, perChunk }. Read it BEFORE you ship the field. The knobs look like a look, but the outermost `lod` distance is really the budget: it sets how many chunks stay resident, as a sweep whose side grows with it, walked every frame. Cost is about (far/chunk)^2 per source — halving the distance, or doubling the chunk, quarters it. A field big enough to matter also says so in the Console when you declare it. On a body smaller than your view distance the count saturates at the body, so a planet never costs more than a planet.

### `scatter.create`

scatter.create{ asset = "tree.glb", seed = 7, perChunk = 24, chunk = 16 } — declare a source, get its id. `tint = {r,g,b}` multiplies every prop's colour, and `emissive = {r,g,b}` with `emissiveStrength` makes them glow, so one model serves a field of any shade. `bend` (0 to 1, default 0) is how far its props give way to scatter.setInteractors points: 1 for grass, 0 for rocks and trunks. `emissiveStrength` with no `emissive` glows in each model's own glow colour (or through its emissive map), and a model with none glows in the source's `tint`. Region: center + radius for a sphere (a planet), or center + halfX/halfZ for ground. `parent = "Umunquo"` anchors the region to a NODE (and then `center` is in that node's frame: the planet's own centre is (0,0,0), not where it is in the world), so a planet that orbits carries its props instead of sliding out from under them — every prop keeps its id, its place on the surface and the ground height it settled at, because none of those were ever expressed in world space. Without a parent the region is pinned to the world, which is right for a landscape that never moves and wrong for every celestial body. Also scaleMin/scaleMax, align = "surface" (default) or "world", fade, and lod = { {asset=, distance=}, ... } nearest-first. Placement is a pure function of the seed, so every machine and every session grows the SAME forest without storing one. `density` is how a world gets biomes: pass a function(x, y, z) -> 0..1 and it is sampled ONCE, at declare time, into a densityRows grid (rows x 2*rows for a sphere's longitude) — 0 means no instance is generated at all, not a hidden one. An option this doesn't list is an error, not a shrug. `asset` may be a mesh file OR a .prefab.ron — a prefab is baked once into one instanced draw per Mesh node it holds, each at its authored place in the prop and wearing that node's Material (its colour, sway and backlight, so a field of grass moves in the wind), which is how a prop your own script assembled gets scattered.

### `scatter.destroy`

scatter.destroy(id) — remove a whole source and everything it was drawing. Returns true if there was one. A source also goes when the node whose script created it is destroyed, or when that script is hot-reloaded (the reload makes its own).

### `scatter.instance`

scatter.instance(sourceId, instanceId) — one prop by the id scatter.near answered with: { id, pos, up, yaw, scale, param, settled }, the same row near returns, without asking around a point. `pos` is where the field dropped it onto the ground, wherever that is, so a game needs no record of its own of where each rock stands. nil once it is removed. A prop no camera has been near yet answers with where it was rolled and `settled = false`, as near does; an id this source has never answered near with and that has not settled is nil.

### `scatter.near`

scatter.near(sourceId, point, radius) — the instances around a point, nearest first: { id, pos, distance, scale, param, up, yaw, settled } — `up` is the way it stands (the ground's normal when it aligns to the surface) and `yaw` its spin about that up, in radians. `pos` and `up` are where the prop stands on the ground once the field has dropped it there (`settled = true`); a prop that has not been near a camera yet is not settled, and answers with the region's own surface. `id` is exact: hand it to scatter.remove. What a harvest verb aims with, and what a "is there room to build here" check reads.

### `scatter.remove`

scatter.remove(sourceId, instanceId) — take one prop out, permanently. By id rather than by position, which is what makes it survive streaming out and back in: an id comes from (seed, chunk, index), a position is a float off the end of a chain of arithmetic.

### `scatter.removed`

scatter.removed(sourceId) — the sorted ids this source has lost. A game that wants permanence saves THIS — a handful of numbers — not every plant it ever saw, which is what made permanence unstorable before (save values are capped at about a kilobyte).

### `scatter.restore`

scatter.restore(sourceId [, instanceId]) — put one prop back, or all of them when the instance is omitted (returns how many). This is what "the forest regrows after fifteen minutes" is, without your game having to remember what it cut.

### `scatter.setInteractors`

scatter.setInteractors{ { pos = vec3, radius = 1, strength = 1 }, ... } — this script's points that bend props away: every prop within `radius` of a point, measured across the ground, leans away from it with its foot planted. Only props of a source created with `bend` (0 to 1: 1 for grass, 0 the default for rocks and trunks) give way, and a pushed prop straightens over about a second once the point moves on. `strength` is how far a prop right beside the point leans (the tangent of the lean: 1 = 45 degrees), easing to nothing at the radius. Each call replaces this script's own list, so call it every frame with where the walkers are; `{}` clears it. Other scripts keep their own points. Up to 32 per script; a point more than its radius (and at least 2 m) above or below a prop does not touch it.

### `scatter.setTint`

scatter.setTint(sourceId, instanceId, { tint = {r,g,b}, emissive = {r,g,b}, emissiveStrength = 1, rim = {r,g,b}, rimStrength = 1 }) — one prop drawn in a look of its own, over its source's: the plant a harvest is aimed at, the rock a build would replace. `tint` multiplies the colour it would otherwise be drawn in; `emissive` makes it glow and `rim` lights its silhouette, each at strength 1 when only the colour is named. nil for the look clears it. Read when the prop is drawn, so changing it every frame on a handful of props re-rolls nothing; removing the prop drops its look. Returns false for a source that does not exist.

## 2D — sprites, sorting & the flat camera

### `EMPTY_TILE`

EMPTY_TILE — the tilemap cell value that leaves a square empty (u32::MAX, 4294967295). Prefer -1: any negative cell means empty, which is the convention in Tiled, Godot and LDtk. This constant exists because the API documented the name long before Lua could resolve it.

### `batch:draw`

b:draw(x, y [, z] [, scale] [, rot] [, cell] [, r, g, b, a]) — draw one sprite THIS FRAME, positioned in the batch node's local space. Immediate mode, exactly like draw.* : what you draw this frame is what shows, and next frame starts empty — there is no pool to grow and no clear() to forget. `scale` is one number, or a vec2 for squash-and-stretch: b:draw(x, y, 0, vec2(1.4, 0.6)). The tint is the thing a shared Material could never give one sprite: flash one enemy red without blinking it off.

### `node:setCamera2D`

node:setCamera2D{follow="Player", smoothing=0.12, deadZoneX=1.5, deadZoneY=0.75, limits=true, minX=0, minY=0, maxX=200, maxY=40, pixelSnap=32, lookAheadX=3, roomTag="room"} — how this ORTHOGRAPHIC camera follows. Every key is optional and keeps what the node had, per axis; off=true removes the behaviour. The order is dead zone, then smoothing, then limits: the camera does not move until the target leaves the box, closes the rest exponentially (smoothing is SECONDS to cover about two thirds of the gap, the same at 30fps and 144), then clamps inside the rectangle so it never shows outside the level. Setting `follow` to a DIFFERENT node restarts the follow where the camera is, so handing the camera to a second character does not send it travelling between them; follow="" stops following and keeps the limits, and with no target the camera's position is left to whatever else is moving it. `pixelSnap` is pixels per world unit and lands the DRAWN camera on a whole pixel of that grid — the same number a Sprite's `ppu` uses, and what camera.pixelsPerUnit() answers; 0 turns it off. Without it a camera that stops between two pixels resamples every sprite by a fraction of one and pixel art shimmers along its edges while nothing is moving; the follow keeps its sub-pixel place, so the camera can still creep slower than a pixel a frame. `lookAheadX`/`lookAheadY` lead the target by that many world units the way it last moved, and keep the lead while it stands still. `roomTag` names a tag: every node carrying it is a room, the area of the tile layers it is or holds, and the camera keeps its view inside the room the target is in, gliding to the next as the target crosses over (imported Tiled and LDtk levels are tagged "level"). `artPixels` is your art's pixels per world unit: the camera's height is held at the nearest value where each art pixel covers a whole number of screen pixels, and the pixel snap is set to match; 0 is off. It does nothing on anything that is not an orthographic camera.

### `node:setParallax`

node:setParallax{x=0.3, y=1} — how much of the camera's movement this layer KEEPS, per axis. 1 moves with the world (no parallax, and the default), 0 pins it to the camera as if infinitely far away, 0.3 is a distant range of hills. Both keys optional and both keep what the node had. This exists because the other way of getting parallax — putting a layer further back in Z — only works under a PERSPECTIVE camera, and a flat game wants an orthographic one so its pixels-per-unit holds still; a scroll factor works under either. Like a sorting layer it offsets the DRAWN transform only, so the collider stays where you put it and node.x reads back what you set.

### `node:setSorting`

node:setSorting{layer="Terrain", order=3} — where this 2D node draws in the stack. `layer` is one of the project's sorting layers by name; `order` places it within that layer, higher being nearer the camera. Both optional and both keep what the node had. This is how a character steps behind a counter, or a picked-up card lifts above the hand.

### `node:setSprite`

node:setSprite{ppu=32, size=1, cell=0, flipX=false, flipY=false, pivotX=0.5, pivotY=0} — make this node one sprite, or retune one. Every key is optional and keeps what the node had, including one pivot axis without the other. `ppu` is pixels per unit measured against ONE CELL of the Material's sheet, so re-slicing a sheet finer does not resize every sprite on it; ppu=0 falls back to `size`, a world edge length. flipX/flipY mirror the picture and not the node, so children and normals are left alone. pivotY=0 puts the origin at the sprite's feet, which is what a Y-sorted character wants — sorting reads the node's Y, and a centred origin sorts by a point floating at the character's waist.

### `node:setSpriteBatch`

node:setSpriteBatch{size=1.0} — make this node a SPRITE BATCH, so node:sprites() can draw into it. The counterpart of node:setTilemap: a game's sprite styles are data (one batch per material), so the nodes that draw them are made from the same script that declares them rather than authored one at a time into the scene. `size` is the quad's edge length; every sprite scales it. The sheet is the node's own Material.

### `node:setTilemap`

node:setTilemap{cols=13, rows=7, tile=1.5 [, data={…}] [, tileset="tilesets/bricks.tileset.ron"]} — make this node a TILEMAP: a grid of spritesheet cells drawn as one mesh, one draw call. The sheet is the node's own Material (texture + sheetCols/sheetRows). Neighbouring tiles share an exact edge, so the hairline gaps a grid of separate quads opens up as the camera moves cannot happen. `data` is row-major from the top-left; leave it out for an empty grid you fill with tm:set.

### `node:shake`

node:shake(amount, seconds) — shake a 2D camera. `amount` is a distance in world units, `seconds` defaults to 0.3, and it fades out over that time. Added to what is DRAWN and never fed back into the follow, so it composes with a chase and with the world limits instead of fighting them — a shake at the edge of a level still shakes, and a camera being driven by a script keeps being driven by it. Calling it again takes the LOUDER amplitude and the LONGER time, each independently, so shaking every frame while something explodes cannot build an unbounded shake and a bang cannot cut a long rumble short. It is a function of the play clock, not of a random number, so two machines simulating the same frame see the same camera. On anything that is not an orthographic camera it does nothing.

### `node:sorting`

node:sorting() -> { layer =, order =, mode = } — where this node sits in the 2D stack. A node that has never said anything about sorting answers with the DEFAULT ("Default", 0, "order") rather than nil, because that IS the true answer for it and nil would make every caller write the same three lines of fallback before it could add one to a number.

### `node:sprite`

node:sprite() — this node's Sprite component as a handle you can read AND assign: local sp = node:sprite(); sp.flipX = mx > 0. Fields: flipX, flipY, cell, ppu, size, pivotX, pivotY. Writes land on the component the renderer reads (and the Inspector shows) after the frame, and read back straight away, so the flag a script sets is the flag it can ask about on the next line. Singular: this is the ONE sprite this node draws. node:sprites() (plural) is the batch handle, for a node that draws many. On a node that is not a Sprite this is an error naming what to do about it, not a handle whose writes go nowhere.

### `node:sprites`

node:sprites() — a handle to this node's SpriteBatch (make it one with node:setSpriteBatch{} first; on any other node this is an error rather than a handle that silently draws nothing): b:draw(...) queues one sprite for this frame. N sprites from one node, each with its own position, rotation, scale, cell AND tint — no scene node per sprite and no pool to grow.

### `node:tilemap`

node:tilemap() — a handle to this node's tilemap grid. Squares: tm:set / tm:get / tm:at / tm:fill / tm:fillRect / tm:size / tm:resize. World space: tm:cellAt (which tile is the player standing on) / tm:worldAt / tm:tileSize. What a tile IS, from the node's tileset: tm:solid / tm:tags / tm:hasTag / tm:autotile.

### `sp.cell`

sp.cell — which cell of the Material's spritesheet draws, 0-based. An animation clip's Sprite ▸ frame lane writes this too, so a script that also sets it every frame is the one that wins.

### `sp.flipX`

sp.flipX — mirrored left-to-right. The one line behind a character facing the way it walks: sp.flipX = mx > 0. Reads back as a BOOLEAN.

### `sp.flipY`

sp.flipY — mirrored top-to-bottom.

### `sp.pivotX`

sp.pivotX — where the node's origin sits across the sprite, 0..1 (0.5 = centred). Outside 0..1 is allowed: an origin off the sprite is a legitimate thing to want.

### `sp.pivotY`

sp.pivotY — the origin up the sprite. 0 puts it at the sprite's feet, which is what a Y-sorted character wants; setting one axis leaves the other alone.

### `sp.ppu`

sp.ppu — pixels per world unit, measured against ONE CELL of the sheet: it is what makes a 16-pixel tile exactly one unit wide. 0 means "size me by `size` instead".

### `sp.size`

sp.size — the sprite's world edge length, used when ppu is 0. For art that is not pixel art.

### `tm.EMPTY`

tm.EMPTY — the cell value that means "no tile here", on the handle rather than only as a global. Same number as EMPTY_TILE; -1 and nil mean it too.

### `tm:at`

tm:at(x, y) → cell, rot, flipX — the WHOLE answer for a square, where tm:get gives only the cell. `rot` is degrees clockwise (0/90/180/270). For art that faces a direction: a conveyor, a pipe, a one-way platform.

### `tm:autotile`

tm:autotile(x0, y0, x1, y1) — recompute the region's autotiled squares, plus the one-square ring around it (which is where the stale edge tiles are). Call it after a run of tm:set, not per square: retiling per write would be O(area) each time and would fight a stroke still being laid down. Does nothing when the map has no tileset.

### `tm:cellAt`

tm:cellAt(worldPos) → x, y — which square a WORLD position falls in, or nil off the map. Takes a vec3, an {x=,y=,z=} table, or a node. Goes through the tilemap node's own transform, so a map that has been moved, turned or scaled still answers correctly — which is the part a game cannot reasonably compute itself.

### `tm:fill`

tm:fill(cell) — set every square, including the empty ones. The fast way to reset a room before re-dressing it. tm:fill() with no argument, tm:fill(-1) and tm:fill(EMPTY_TILE) all clear the grid.

### `tm:fillRect`

tm:fillRect(x0, y0, x1, y1, cell [, xform]) — fill a rectangle. Corners in either order, clipped to the grid, so dragging past the edge fills to the edge.

### `tm:get`

tm:get(x, y) → cell, or nil outside the grid and on an empty square.

### `tm:hasTag`

tm:hasTag(x, y, "ice") → the common case of tm:tags without allocating a table per square. What a per-frame ground check should call.

### `tm:oneWay`

tm:oneWay(x, y) → whether that square is a one-way platform: solid from above only, so a character jumps up through it and lands on top. False on an empty square, a walk-through tile, or with no tileset. Read from the tileset, like tm:solid, so a controller can tell a plank from a floor without a table of its own.

### `tm:path`

tm:path(from, to[, opts]) → a list of vec3 square centres from one world point to another, and whether it reaches. A* over the map's own grid, for a top-down game: solid tiles are walls, empty squares and walk-through tiles are floor. When the goal cannot be reached the path ends on the square nearest it, and the second value is false. opts: diagonal (default true; a diagonal step never cuts past a wall's corner), avoid = {"water"} (tags walked round as if solid), cost = { mud = 3 } (tags that cost that many times a plain step). from and to can be nodes. Returns nil when from is off the map or inside a wall.

### `tm:resize`

tm:resize{ cols =, rows =, offsetX =, offsetY = } — resize the grid, keeping whatever overlaps. offsetX/offsetY is where the OLD top-left lands in the new grid, so offsetY = 1 grows a row on top rather than at the bottom. Give at least one of cols / rows.

### `tm:set`

tm:set(x, y, cell) — set one square, 0-based from the TOP-LEFT. Outside the grid is a no-op rather than a wrap. To clear a square pass -1 (any negative works, as in Tiled/Godot/LDtk), nil, or the EMPTY_TILE constant — all three are the same value. A cell that is not a whole number in range is an error naming what it got and what it accepts, never a neighbouring tile. It changes the picture, every tm: query, and the collision: while the game is running, a tile set solid is solid from the next frame, and one cleared stops colliding, so a bridge built from a script can be walked on and a wall broken by one can be walked through.

### `tm:size`

tm:size() → cols, rows.

### `tm:solid`

tm:solid(x, y) → whether the tileset says that square collides. False on an empty square and false with no tileset. Reads the TILESET, so marking one brick solid answers for every brick in every scene — a game keeping its own table of solid cell indices goes stale the day the artist reorders the sheet.

### `tm:tags`

tm:tags(x, y) → the tileset's tags for that square, as a list. This is how a tilemap carries gameplay ("ice", "water", "damage") without the game keeping a second table keyed by cell index.

### `tm:tileSize`

tm:tileSize() → the world edge length of one square. What tm:cellAt divides by, and what a game placing something on a tile needs.

### `tm:tileset`

tm:tileset() → the project-relative .tileset.ron this map is cut from, or nil. The tileset is what says whether a tile collides, what it is tagged, and how it autotiles — see docs/tilemaps.md.

### `tm:worldAt`

tm:worldAt(x, y) → the world position of that square's CENTRE (a vec3), or nil off the grid. The centre and not a corner, because what you do with it is put something on the tile.

## vessels — assembly.*

### `assembly`

Multi-part vessels: hold forces and torques, split parts off, latch parts on, and read the compound's mass and centre of mass. A vessel is one physics body built from many nodes.

### `assembly.force`

assembly.force(node, force) — a HELD force through the centre of mass, re-applied every tick until you change it (engines, thrusters). Through the CoM means no torque: the vessel accelerates without turning.

### `assembly.forceAt`

assembly.forceAt(node, force, at) — a held world-space force at a world point. Off the centre of mass it produces torque as well as acceleration, which is how an off-axis thruster makes a craft tumble — and how RCS steers it.

### `assembly.grip`

assembly.grip(node, at [, target] [, breakForce]) — weld the vessel at world point at to target (a node with a body, any node of an assembly, or a terrain node) or, with no target, to the ground nearest the point: a landing claw, a harpoon, a grab. The pair keeps the pose it has now, the grip rides its ground when that moves (a planet on its rails), and it lets go past breakForce (0: never). assembly.info(node).gripped says whether it still holds. The point must be where the craft is: one more than a unit clear of all its parts is refused with a warning. Applied after the script pass.

### `assembly.impacts`

assembly.impacts(node) — the LAST tick's per-part contact loads: { part, impulse, speed, speedAbs, x, y, z }. What a damage model reads: how hard each part was hit and where.

### `assembly.impulseAt`

assembly.impulseAt(node, impulse, at) — a one-shot kick at a world point, applied once rather than held. Explosions, collisions you resolve yourself, a docking clamp letting go.

### `assembly.info`

assembly.info(node) — { mass, com, origin, vel, angVel, inertia, grounded, anchored, gripped, parts, right, up, forward, carry, asleep, wake }. carry is node.carry for the craft: how far the frame it rides in moved this tick. asleep and wake are node.asleep and node.wake for the craft: whether it is asleep, and why it is awake. right, up and forward are the craft's axes as they are this tick, the way node:worldRight/worldUp/worldForward give a node's drawn ones: read in fixedUpdate they are that tick's, where the node's are a frame's. com is the world-space centre of mass as a vec3 — the number a flight controller, a CoM gizmo and a landing check all need. inertia is the tensor the solver turns torques with, about the centre of mass in world axes, as three rows: inertia[1].x .. inertia[3].z. A torque of I × (wanted change in angVel) / dt stops a spin in one tick without overshooting.

### `assembly.keepLive`

assembly.keepLive(node, true) — exempt this compound from distant-craft LOD, so it keeps simulating in full even when nothing is near it. For the craft the player will come back to and expects to find where physics would have put it.

### `assembly.merge`

assembly.merge(node, other) — latch another assembly onto this one: docking, grabbing, a part snapping into place. The two become one physics body with one mass and one centre of mass.

### `assembly.rebuild`

assembly.rebuild(node [, fn]) — re-gather the compound from the root's CURRENT children. Call it after you have added or removed part nodes yourself, so the physics body matches the scene again. A moving vessel keeps its velocity and spin through it, and an anchored one stays anchored. The rebuild lands after the script pass, so assembly.info read straight after it still describes the old compound; fn(node) is called once the new one exists and assembly.info describes it — read its mass there to size a push: assembly.rebuild(ship, function(s) local i = assembly.info(s); assembly.impulseAt(s, vel * i.mass, i.com) end).

### `assembly.release`

assembly.release(node) — let go of every grip the vessel holds (assembly.grip).

### `assembly.setAnchored`

assembly.setAnchored(node, true) — pin the vessel exactly where it stands (a launch clamp, a craft on a pad, anything that must not drift while you build it). Release it and normal physics resumes.

### `assembly.split`

assembly.split(node, parts [, fn] [, prefab]) — detach part nodes into their own assembly (stage separation, a wing coming off). The new assembly keeps the velocity it had, so debris carries on rather than appearing at rest.

### `assembly.syncColliders`

assembly.syncColliders(node) — re-pose the compound's collision shapes to its parts' current transforms. Needed after you move parts around without a rebuild, or the vessel collides with where it used to be.

### `assembly.teleport`

assembly.teleport(node, pos) — move the assembly origin to a world position, carrying every part with it, and everything jointed to it (a rover's wheels, an arm's segments, and what is jointed to those) at the same offsets and speeds. A teleport rather than a force: no acceleration, no tumble. Its node, and theirs, read the new place from the next script pass.

### `assembly.torque`

assembly.torque(node, t) — a held PURE torque, no linear push: reaction wheels, SAS, anything that turns a vessel without moving it.

## the camera & the screen

### `camera`

The game camera's projection: viewport size and rect, world↔screen conversion, and picking rays. camera.screenRect shares its space with input.mouse(), which is why hit-testing works.

### `camera.capture`

camera.capture([cam,] { w = 640, h = 360 [, format = "png"|"jpeg", quality = 90, ui = false, draws = false] }, function(bytes, err) end) — photograph the world through a camera: a level cover, a photo mode, a save-slot thumbnail. `cam` is a camera node, a camera's name, or left out for the active one; a camera that is not the active one is drawn off-screen, so the player's view never changes. The picture is the game's own (post-processing, fog, retro and render scale included). The screen UI is left out unless ui = true, and draw.line / draw.tri shapes unless draws = true, so a HUD or a debug line never lands in a cover; draw.quad trails are part of the world and always in. draws = true adds the world shapes (draw.line, tri, box, sphere); draw.text and draw.rect are screen UI and come with ui = true. `bytes` is an encoded PNG (or JPEG, quality 1–100), ready for assets.writeBytes, a Cloud blob or assets.textureFromBytes. It renders on a later frame and answers on the one after without waiting on the GPU: one extra scene render, never a stall. At most 4 captures wait at once; a fifth answers nil and why. Refuses: w or h missing or outside 1–4096; an unknown option; a format other than png/jpeg; quality on a PNG. Answers nil, err: "no renderer" on a host that draws nothing (floptle run, a dedicated server); a camera name that matches none; a node that is not a camera; a size past the GPU's limit.

### `camera.captureTexture`

camera.captureTexture([cam,] { w, h [, ui, draws] }, function(tex, err) end) — camera.capture without the encode, for a picture you only want to show: `tex` is a name like "img:7" that goes anywhere a texture path does (draw.quad, a UI image, a material), exactly as assets.textureFromBytes answers. Kept until assets.release(tex) or Stop. Same cameras, options and refusals as camera.capture, minus format and quality.

### `camera.exists`

camera.exists() — true once a live game camera is being fed. Guard the other camera.* calls with it during the first frames, or while a scene without a camera is up. True throughout a `floptle shot`, which feeds its picture's size; never under `floptle run`, which draws nothing.

### `camera.pixelsPerUnit`

camera.pixelsPerUnit([distance]) → px — how many screen pixels one world unit covers. The number every 2D game used to derive by hand from the FOV and the camera's Z, and then snap the camera to a multiple of for crisp pixels.

Under an ORTHOGRAPHIC camera the answer is the same everywhere and `distance` is ignored — that is what an orthographic projection means, and it is the case a flat game is in. Under a perspective one it is measured at `distance`, defaulting to the camera's distance from the origin.

A 2D camera can do the snapping for you: node:setCamera2D{ pixelSnap = 32 }.

### `camera.screenRect`

camera.screenRect() -> x, y, w, h — the game viewport in the SAME space as input.mouse() and camera.worldToScreen, offset included. That shared space is the only reason hit-testing the mouse against a projected point works; screenSize alone would be wrong wherever the viewport isn't at the window origin. Under `floptle shot --size WxH` it is 0, 0, W, H from the first update — the picture is the viewport — so a layout can be checked at 1440p, an ultrawide or a portrait window without a window; `floptle run` renders nothing and reports 0, 0, 0, 0.

### `camera.screenSize`

camera.screenSize() → w, h — the game viewport size in pixels. camera.exists() is true once a live game camera is being fed. Under `floptle shot` this is the picture's --size from the first update, so a HUD can be checked at any screen size headless; `floptle run` renders nothing and reports no camera.

### `camera.screenToRay`

camera.screenToRay(sx,sy) → ox,oy,oz, dx,dy,dz — a world ray from a screen pixel (inverse of worldToScreen; the same camera, read at the moment of the call).

### `camera.worldToScreen`

camera.worldToScreen(x,y,z) → sx, sy, depth, onscreen — project a world point into the game view (pixels in input.mouse()'s space). onscreen=false behind the camera / off-frustum. Sample a drawn line into points, project each, keep the nearest to the cursor = click-on-line picking (the map's maneuver nodes). Projects with the camera node where it stands at the moment of the call — this frame's rails ticks, physics and any lateUpdate that moved it so far included — so a label placed in lateUpdate sits on its object as this frame draws it. Two exceptions: a move the calling script makes to its OWN node in the same hook counts once the hook returns, and a Camera2D follow lands after lateUpdate.

## physics controls — pause & step

### `physics`

Sim controls: physics.pause(true) freezes the whole gameplay tick while scripts keep running (pause menus, cutscenes, loading screens), and physics.step() advances it one tick at a time.

### `physics.ignorePair`

physics.ignorePair(a, b [, seconds]) — let two nodes pass through each other: each a node with a dynamic body, or any node of an assembly (it stands for the whole assembly). Without seconds it holds until lifted; physics.ignorePair(a, b, 0) lifts it. Both still meet the ground and everything else. A seated passenger inside its own ship, or debris kept off the hull it broke from, is this call: assembly.split(ship, parts, function(piece) physics.ignorePair(piece, ship, 0.5) end). A node with neither a body nor an assembly is reported in the Console and changes nothing. Applied at the next tick.

### `physics.isPaused`

physics.isPaused() — whether the sim is currently frozen, including when the editor froze it rather than your script.

### `physics.pause`

physics.pause(true) — freeze the whole gameplay tick while scripts keep running. Pause menus, cutscenes and loading screens are this call: the world stops, your UI doesn't.

### `physics.step`

physics.step([n]) — advance the frozen tick n times (default 1, max 600) — the same thing the editor's frame-step button does, so a game can build its own training mode. Call it from update: a fixedUpdate caller would never get a second turn, because the tick it is waiting for is the one it just stopped.

## frame cost — perf.*

### `perf.accountedMs`

perf.accountedMs() — the buckets added up. Called 'accounted' and not 'total' on purpose: vsync, the OS and the GPU finishing are outside every bucket, so this is what the engine can see, not the frame time.

### `perf.bodies`

perf.bodies([n]) → the n costliest physics bodies and assemblies last frame, by node name, costliest first: { {name="Plane", ms=11.4}, ... } — each one's own move against the world, sweeps included (the pairs and joints passes are perf.parts' physicsPairs and physicsJoints). Needs perf.enable(true).

### `perf.buckets`

perf.buckets() → the bucket names, in frame order: scripts, mirror, physics, terrain, scatter, particles, audio, animation, models, ui, render, assets, gpuWait, present, untimed. Iterate this rather than keeping your own list, which could go stale. `scripts` is the WHOLE of every script pass — setup, reference params, write flush and hooks; `mirror` is the ECS-to-Lua sync each pass runs first, nested inside a pass and subtracted out of `scripts` so nothing is counted twice. `assets` is textures, shaders and materials used for the first time. `gpuWait` is waiting for a swapchain image (the GPU still drawing an earlier frame, or with vsync on the display not yet wanting one) and for readbacks; `present` is handing the image to the compositor. `untimed` is the rest of the frame, the time between two frames that no other bucket measured: work nothing times yet, or the game not being run at all. It was called `other`, and perf.ms("other") still answers it.

### `perf.counts`

perf.counts() → { nodes=, culled=, instances=, draws=, unbatched=, unbatchedBy=, chunks=, fieldChunks=, fieldEdited=, fieldStandIns=, props=, particles=, effects=, effectsDropped=, lights=, lightsDropped=, voices=, mirrorRebuilds=, mirrorRefreshed=, animators=, animatorsCulled=, rays=, rayMs=, sweeps=, sweepMs=, decals=, decalTris=, decalsChecked=, decalsRelaid=, shadowProxies=, shadowProxiesDropped=, shadowVolumes=, terrainDropped= }. Readable even while collection is off, because counts are free to keep — and three of the four 'the engine is slow' reports this API exists for were answerable from one count alone (a scatter field asking for 117,000 props was one of them). The *Dropped pair is what a ceiling refused this frame: nonzero means the engine is cutting your look, which you should hear from a number rather than from a screenshot. mirrorRebuilds is how many times last frame the scripts' copy of the scene was rebuilt from scratch (each costs time in proportion to the whole scene, in the mirror bucket); mirrorRefreshed is how many single nodes were re-read because a component of theirs changed, which costs only those nodes. animators and animatorsCulled are last frame's animated models and how many culling skipped (see anim:setCulling). rays and rayMs are the raycasts scripts made last frame (raycast and raycastMany) and the milliseconds they took, a part of the scripts bucket (counted with collection on). sweeps and sweepMs are the same for the sweeps scripts made (spherecast, capsulecast and boxcast), each call counted once. chunks is the terrain chunk meshes drawn from (render slots, every terrain summed). fieldChunks, fieldEdited and fieldStandIns are the terrains' fields, summed: chunks holding their voxels at full detail; those of them written since their world was generated (dug, raised or painted), which are kept, saved and never put away (a terrain that was not generated counts none); and chunks put away as coarse stand-ins until something comes near (counted with collection on). decals and decalTris are the decals in the world (decals.add) and the triangles they were laid as. decalsChecked and decalsRelaid are last frame's marks asked whether the ground under them moved (a dig near them, or a streamed chunk arriving under one laid on its stand-in) and marks laid again; both are zero while nothing under a mark changes. shadowProxies is how many collider boxes, spheres and capsules the shadow march was given (the 32 nearest the camera), shadowProxiesDropped how many more qualified and were left out, and shadowVolumes the baked terrain and mesh volumes it reads: a building made of many box colliders costs lighting time near it, and these are the numbers that show it. terrainDropped is how many terrain edits have been refused since the game started because 4,096 were already waiting; zero for a game that paces with terrain.capacity(). unbatched is how many draws carried a single instance, and unbatchedBy says why each did not share a draw: { uniqueMesh=, texture=, shader=, shaderParams=, skinned= } — nothing else draws its mesh, or the other draws of that mesh bind different images, use another shader (or none), use the same shader with different parameters, or differ in being skinned. Colour, roughness, emissive, tint and the other per-material numbers never split a draw; nodes that share a mesh, the same images and the same shader with the same parameters draw as one, however each got its material (counted with collection on).

### `perf.enable`

perf.enable(true) — start collecting; perf.enable(false) stops and CLEARS the history (a stale average from before a fix looks exactly like a fix that did not work). Off by default, because a profiler that costs a frame is one people turn off.

### `perf.enabled`

perf.enabled() — is anything being measured? Safe to call while off, so a script can ask before reading.

### `perf.frames`

perf.frames([n]) → the last n frames (all that are kept, 300, when n is left out), OLDEST FIRST, each exactly as it was: { {frame=, ms=, buckets={physics=26.1, ...}, parts={physicsStatic=19.0, ...}, scripts={ {name=, ms=}, ... }, calls={ {script=, call=, n=, ms=}, ... }, samples={ {script=, fn=, line=, n=, ms=}, ... }, bodies={ {name=, ms=}, ... }, counts={nodes=, draws=, props=, ...}, paced="compositor", ticks=}, ... }. ticks is how many fixed ticks (physics and fixedUpdate) the frame ran: a game catching up runs several, so physics a tick is the frame's physics divided by it. No smoothing, so a benchmark reads each frame's own numbers rather than inverting perf.ms's rolling average, and a frame that passed with no update of the probe's (a scene swap, a load) is still there. ms is the whole frame end to end, `untimed` included. scripts are hook time costliest first; samples only while perf.sampleScripts is on; paced is set when something other than the engine's own work held the frame (see perf.pacing). Needs perf.enable(true).

### `perf.gpu`

perf.gpu() → { total = ms, passes = { {name=, ms=}, … } } — the GPU's own time for each render pass of the last frame it finished timing, in the order they ran ("depth prepass", "opaque + lighting", "glass", "particles", "post", …). nil when nothing has been timed yet: the first frames after perf.enable(true), or a GPU whose driver cannot time passes. For a game's own quality governor on the player's machine, which the CPU-side buckets cannot see. Raises while collection is off, like perf.ms.

### `perf.gpuMs`

perf.gpuMs(pass) → one pass's GPU milliseconds last frame ("opaque + lighting" is where sun shadows are paid for), or "total"; nil when the pass did not run or nothing was timed. See perf.gpu.

### `perf.hitchThreshold`

perf.hitchThreshold([ms]) → the frame time at which a frame is kept as a hitch for perf.hitches, after setting it when ms is given. 50 by default; 33 catches every frame that makes a 30 fps game miss, 20 one aiming at 60. At least 1. Readable while collection is off; it is a setting, not a measurement.

### `perf.hitches`

perf.hitches() → every frame since the last call that took the hitch threshold or more (50 ms, or what perf.hitchThreshold set), oldest first: { {frame=, ms=, top="terrain", buckets={terrain=157.4, scripts=30.1, ...}, parts={collisionRebuild=140.2, ...}, assets={ {path="shaders/ocean.flsl", part="pipelineCompile", ms=827.4}, ... }, scripts={ {name=, ms=}, ... }, calls={ {script=, call=, n=, ms=}, ... }, samples={ {script=, fn=, line=, n=, ms=}, ... }, bodies={ {name=, ms=}, ... }}, ... }. perf.worstMs looks back sixty frames, so a probe that reads every few seconds loses the spike it is hunting; this keeps each one (up to 64) until it is read, with the bucket that spent the time on top. Time no bucket measured is named `untimed`, and waiting on the GPU or the display `gpuWait` and `present`, so a hitch the engine's buckets cannot explain still says so. `parts` names work inside the buckets, in milliseconds already counted there: `terrainEdits` (applying terrain.dig and the other edits), `collisionRebuild` (a terrain collider's rebuilt triangles landing, or the frame waiting for them), `planetLanded` (a world arriving, generated or streamed in: its collider put in the game and its shadow and meshes asked for), `terrainLoad` (a world's field read from its file on the frame, because something stands where its ground will be), `terrainShadow` (the terrain's shadow volume uploaded after a change; inside it `terrainProxies`, shadow volumes made off the frame put in place, `terrainOccluders`, the static meshes' shadow bakes kept in step with the scene, `terrainShadowEdits`, the boxes edits changed uploaded, and `terrainShadowAtlas`, the atlas laid again around new volumes, a budget a frame), `terrainPalette` (the terrain's textures put on the GPU once read, off the frame, after the palette changed), `terrainMeshing` (planning and landing chunk meshes) and `terrainStreaming` (putting far ground away and taking back what was made again); in `assets`, first-use work: `assetDecode` (an image read and decoded), `assetUpload` (put on the GPU), `shaderCompile` (a .flsl compiled and checked), `pipelineCompile` (the GPU driver building a shader's pipeline, or a frame waiting on assets.warm's build), `shaderInstall` (a warmed shader put in use) and `materialBind` (binding the materials that wear a shader). `assets` lists the slowest five files the frame prepared, slowest first, each with its part and milliseconds: the file a load hitch was waiting on, which assets.warm can prepare ahead. Needs perf.enable(true).

### `perf.mirrorCause`

perf.mirrorCause() → why the scripts' copy of the scene was last rebuilt from scratch — "first sync", the scene replaced, a node id handed to a new node, or more than 8,192 changes between two passes — or nil if it never has. Changing a component's value (a camera's fovY, a light's intensity, visible) re-reads only that node; spawning, destroying, renaming, reparenting, retagging or attaching a script patches that node and the lists naming it, never the whole copy. If perf.counts().mirrorRebuilds is nonzero every frame, this names what is doing it. Readable while collection is off.

### `perf.ms`

perf.ms("scripts") — that bucket's rolling average, in milliseconds. An unknown bucket names every accepted value rather than answering 0.

### `perf.overloaded`

perf.overloaded() → true while the fixed tick costs more than the time it simulates: this machine can't run the game in real time. The game then plays in slow motion (one tick per frame) rather than dropping to a few frames a second — unless project.ron says tick_overload: CatchUp, or it is a networked session, which must keep up. For a loading screen that would otherwise wait forever for calm frames: say the machine is struggling instead. Readable while collection is off.

### `perf.pacing`

perf.pacing() → { presentMode="fifo", vsync=true, refreshHz=60, frameCap=0, frameMs=, waitMs=, gpuMs=, limitedBy="compositor", pacedFrames=58, frames=60 } — how frames reach the display, and what held the last one when the engine's own work did not: "display" (vsync, about one refresh), "compositor" (a window system handing out fewer frames than the display shows, as a hidden window or a locked session does), "frameCap" (app.setFrameCap) or "gpu". nil limitedBy is a frame the engine's work filled. pacedFrames is how many of the last `frames` something other than the engine held. A probe that measures frames should check this first: 50 ms frames that are all gpuWait at limitedBy="compositor" are not the game being slow. presentMode is "none" with no window. The setup (presentMode, vsync, refreshHz, frameCap) is known from Play's first start; refreshHz is 0 until the display has said its rate. The verdict (frameMs, waitMs, gpuMs, limitedBy) describes the last finished frame, so before one has finished frames is 0 and they read 0 and nil. Needs perf.enable(true).

### `perf.parts`

perf.parts() → { name = ms, … } — the named pieces of work inside the buckets in the last finished frame, every frame (a hitch lists them too, but only on a slow frame). scatterSettle is what the scatter bucket spent dropping arriving props onto the ground (rolling chunks and their ground queries; at most about 1 ms a frame plus one query); terrainEdits, collisionRebuild, decalRelay (checking and laying again the marks a dig or arriving ground reached) and decalBatches (packing the decal pages that changed) are inside terrain; assetDecode, assetUpload, shaderCompile, pipelineCompile, shaderInstall and materialBind are first-use work inside assets (see perf.hitches). Inside physics: physicsBroadphase (indexing the colliders), physicsStatic (bodies and assemblies moving against the static world), physicsSweeps (fast movers swept so they stop at first touch), physicsJoints, physicsPairs (movers against each other) and physicsSleep (re-checking what sleepers rest on); perf.bodies names who. Inside render: renderCull (the on-screen test), renderInstancing (walking the scene into instances, terrain and decals included) and renderPasses (the GPU passes encoded and submitted, uploads included). Inside untimed: outsideFrame (the window system and the OS between two frames), frameCap (holding to app.setFrameCap), sceneLoad (a scene swap), physicsRebuild, assemblySplit (assembly.split's engine work: the new root made, the parts moved under it, the compound split), assemblyRebuild (assembly.rebuild's compound built again) and gpuOutOfMemory (on a frame the GPU ran out of memory, the untimed time nothing else named: the allocation that failed is slow and timed by nobody); the rest of untimed is unnamed. Inside scripts: scriptOutOfMemory (the collection after a script passed the memory limit). A scene swap's steps are named inside sceneLoad: sceneSave (the old scene's terrain edits written out), sceneParse (the scene file read), sceneTeardown (the old scene's scripts told and its nodes removed), sceneSpawn (the new nodes made), sceneTerrain, sceneMaps (map geometry, paint and bakes), sceneModels (the models it draws registered), scenePhysics (its physics built), sceneAudio, sceneSession (multiplayer setup) and sceneScripts (onSceneLoaded); a step's milliseconds include what its own buckets counted (sceneModels the models bucket's). A layer loaded over the running scene (scene.load with additive = true) names its steps the same way: sceneParse, sceneSpawn, sceneMaps, sceneModels, scenePhysics, sceneAudio (its own sounds and effects started; the running scene's play on untouched) and sceneScripts. Needs perf.enable(true), like every timing.

### `perf.sampleScripts`

perf.sampleScripts(on [, intervalMs]) → whether it is on. The script sampler: every intervalMs (0.25 by default) of a script's run time it notes the function and line running, charged with the wall time since the sample before, so an engine call that ran long is charged to the line that made it. Every entry into Lua starts the clock afresh, a callback (spawn, assembly.split, assembly.rebuild) included, so the engine's work before it is never charged to its first line. Each frame keeps its hottest ten lines (perf.frames, perf.hitches); perf.samples() has every line since it was last asked. A tight loop runs slower while it samples, so it is opt-in. Needs perf.enable(true); perf.enable(false) stops it.

### `perf.samples`

perf.samples() → every line the script sampler landed on since the last call, HOTTEST FIRST: { {script="vessel_controller", fn="solveWheels", line=212, n=1840, ms=459.8}, ... }. fn is `?` for a chunk's main body or an anonymous function. Empty unless perf.sampleScripts is on. Needs perf.enable(true).

### `perf.scriptMs`

perf.scriptMs("planet_walker") — one script's own average cost, by file name. 0 for a script that has not run, which is different from an error. This is HOOK time: the per-script figures are a breakdown of part of perf.ms("scripts"), not all of it, and the difference is what the engine spends reaching a hook.

### `perf.scriptWorstMs`

perf.scriptWorstMs("planet_walker") — that script's worst frame in the last second.

### `perf.scripts`

perf.scripts() → { {name=, ms=, worstMs=, engineMs=, calls={ {script=, call="spherecast", n=12, ms=25.1}, ... }}, ... }, MOST EXPENSIVE FIRST — which is the order the question is asked in. A total for 'scripts' never answered 'which of my scripts is doing this'. engineMs and calls are LAST FRAME's engine calls made from that script (raycast, raycastMany, spherecast, capsulecast, boxcast), costliest first: the part of its time that was the engine working on its behalf, so 'planet_camera 27 ms, 25 of it in spherecast' is one read.

### `perf.slowestScript`

perf.slowestScript() → the name of the costliest script, or nil if none have run. The one-liner you actually put in an assertion message.

### `perf.worstMs`

perf.worstMs("scripts") — the WORST single frame in the last second. This is the one to watch: a 40 ms hitch once a second adds under a millisecond to a 60-frame average, so the mean hides exactly the thing you are chasing.

## accessibility — access.*

### `access.captions`

access.captions() → is the player showing captions?

### `access.colorFilter`

access.colorFilter() → the active colour-vision filter's name ("none" / "protanopia" / "deuteranopia" / "tritanopia").

### `access.colorFilterStrength`

access.colorFilterStrength() → how strongly the colour filter applies, 0–1.

### `access.filters`

access.filters() → { {name=, label=}, … } — every colour filter in menu order, so an options dropdown does not hard-code a list that can go stale. `label` is the human one ("deuteranopia (green-blind)").

### `access.reducedMotion`

access.reducedMotion() → the player asked for less movement. The engine already snaps its OWN UI transitions; read this for the motion it cannot know about — your camera shake, screen flashes, big animated wipes. The engine cannot tell which of your movement is the game.

### `access.setCaptions`

access.setCaptions(true) — turn captions on. While off, caption(...) draws nothing, so a game writes caption() beside the sound and never an `if` around it.

### `access.setColorFilter`

access.setColorFilter("deuteranopia" [, strength]) — correct the picture for a colour vision deficiency, as a stage in the post chain (so it applies to everything the player sees, and a scene cannot veto it by disabling its PostProcess node). `strength` 0–1; full correction shifts hues a lot and some players want less. An unrecognised name raises naming the four it takes — a misspelled filter that quietly meant "off" is an accessibility setting that appears to do nothing.

### `access.setReducedMotion`

access.setReducedMotion(true) — ask for less movement. UI transitions SNAP rather than hurry (a 40 ms slide is still a slide).

### `access.setTextScale`

access.setTextScale(1.5) — set the UI text multiplier, 0.5–3.0. This is the single most-used accessibility setting in games. Out of range RAISES rather than clamping: a settings slider hands over a number it already bounded, so a value outside it means the caller computed it wrong. Persist it yourself with save.set — it is the player's setting, so it belongs in the player's save.

### `access.textScale`

access.textScale() → the player's UI text multiplier (1.0 = normal). Every UI text size is multiplied by it BEFORE layout, so text scaling reflows — a fit-height box grows and its neighbours move down — rather than painting bigger glyphs into the same rect and clipping.

### `caption`

caption("a door unlocks somewhere" [, seconds]) → true if it was shown. Says a line the engine draws bottom-centre on a dark plate, at the player's text scale, oldest first — so every game gets the same readable placement instead of hand-rolling one. A no-op (returning false) while access.captions() is off. Without `seconds` the duration suits the length of the line.

## persistence — save.*

### `save`

The persistent store: save.set / save.get, named slots, and flushing to disk. A slot holds at most 10 000 keys and 4 MB — store the player's progress and settings; something the player made (a blueprint library, a level, a recording) goes in a user:// file with assets.writeJson / assets.writeBytes.

### `save.delete`

save.delete("gold") — remove a key; true if something was removed.

### `save.deleteSlot`

save.deleteSlot("slot2") — delete a slot's store file from disk ("delete this save" UIs). Deleting the ACTIVE slot also empties the in-memory store, so the slot is instantly reusable. Per-slot terrain is separate — pair with terrain.deleteSaveDir. Returns true if a file was removed.

### `save.flush`

save.flush([{ wait = true }]) — hand the store to the writer NOW (checkpoints, before risky sections) and return; the write finishes on its own thread. wait = true returns only once it is on disk. The file is replaced whole or not at all, so a crash mid-write keeps the previous save. Returns false when a write failed — this one, with wait, or an earlier one (the Console says why); the slot stays dirty and is tried again.

### `save.get`

save.get("gold" [, default]) — the stored value, else the default, else nil. save.get("who").hp reads into stored tables.

### `save.set`

save.set("gold", 42) — store persistent game data: survives Play sessions, editor restarts, and ships with exported builds. Values are plain data (numbers/strings/bools/tables, no functions or userdata), tables nested up to 16 deep (a cycle or a 17th level raises); a slot holds at most 10 000 keys / 4 MB in all — past that it raises, and large player data (a blueprint library, a level, a recording) belongs in a user:// file: assets.writeJson("user://crafts/a.json", t). Not the wire's depth-4 rule: a save stays on this machine. Flushed on Stop + every few seconds during Play, on a writer thread: the frame pays for the list of changed keys, and only those are encoded again.

```lua
save.set("hp", hp)                 -- survives scene loads and quits
hp = save.get("hp", 100)
```

### `save.slot`

save.slot("slot2") — switch the active save slot (the old one flushes first); save.slot() reads the current name. Each slot is its own file under save/.

## timers — after, every, tween

### `after`

after(seconds, fn) — run fn once after that much GAME time (tick-driven, deterministic, pauses with the game). Returns a handle: h:cancel() aborts. Capture what you need as locals — the callback gets no arguments. after(2, function() door.visible = false end)

```lua
after(0.25, function() spawnEffect("Explosion", node.pos) end)
```

### `every`

every(seconds, fn) — run fn repeatedly (first fire after one period). Anchored cadence: long sessions don't drift. Keep the handle to stop it: local h = every(1, tickDown) ... h:cancel().

```lua
-- a heartbeat that survives long sessions without drifting
every(1.0, function() hp = math.min(hp + 1, 100) end)
```

### `timer:cancel`

timer:cancel() — stop a pending after / every / tween. The handle those three return exists for exactly this: local h = every(1, tick) ... h:cancel().

### `tween`

tween(seconds, fn [, ease]) — animate: fn(alpha) runs every tick with alpha easing 0→1, final call exactly at 1.0. ease: "linear" (default), "smooth", "in", "out". tween(0.5, function(a) node.y = startY + a * 3 end, "smooth"). Returns a cancellable handle.

```lua
-- SECONDS first, then the function; alpha eases 0 -> 1 and lands on 1.0
tween(0.4, function(t) node:getcomponent("UiElement").opacity = t end, "smooth")
```

## space — orbits & time-warp

### `body.mu`

Gravitational parameter µ = GM.

### `body.name`

The celestial body's node name — what space.body() takes and space.dominant() returns.

### `body.radius`

Physical surface radius.

### `body.soi`

Sphere-of-influence radius (-1 = infinite, the root).

### `body.vx`

World velocity X — the body's own motion along its rails, which a rendezvous has to match.

### `body.vy`

World velocity Y.

### `body.vz`

World velocity Z.

### `body.x`

World X of the body's centre this tick.

### `body.y`

World Y of the body's centre.

### `body.z`

World Z of the body's centre.

### `space`

On-rails celestial mechanics: where the bodies are, which one's gravity owns a point, the orbit a craft is on, and time-warp.

### `space.belt`

space.belt{ parent, count, seed, aMin, aMax, eMax, iMax, radiusMin, radiusMax [, mu = 0] [, reach] [, name = "Rock"] [, prefab] [, made = function(rock, i, row) end] } → how many rocks it asked for. Makes a belt of small bodies on space.beltOrbits rows: each rock a node named name .. " " .. i (or an instance of prefab, renamed so), given a CelestialBody on its row's orbit about parent (mu 0 unless given, reach if given), on the rails from the next tick. The rocks are runtime nodes: made while the game plays and gone when it stops, so a belt is never saved; from the same seed it is the same belt on every machine. made runs for each rock once it exists, to give it terrain, a look or a script. shape gives each rock ground of its own, an irregular small body made from the seed when something comes near: "potato", "binary", "rubble", "mixed" (the three dealt out from the seed), a smallBody table as terrain.generatePlanet takes, or a function(i, row) returning one; rock i is terrain id terrainId + i - 1 (10000 by default), at voxel (its radius / 24 by default). Refuses: any option it does not list, a missing parent, and what space.beltOrbits refuses.

### `space.beltOrbits`

space.beltOrbits{ count, seed, aMin, aMax, eMax = 0.05, iMax = 0.05, radiusMin = 1, radiusMax = 1 } → a list of orbits for a belt of small bodies, each a table of a, e, i, lan, argPe, m0 and bodyRadius that node:setCelestial takes as it is (add the parent). Angles are radians, like setCelestial's. The belt is a function of its seed: the same seed gives the same rocks on every machine, so a game rebuilds it rather than saving it. Most orbits come out near circular and near the plane, up to eMax and iMax. Refuses: an aMin not above 0, an aMax below aMin, a count outside 0 to 100000, and any option it does not list.

### `space.bodies`

space.bodies() — every celestial body this tick: {name, x,y,z, vx,vy,vz, mu, radius, soi} in world coords (soi -1 = infinite). space.body("Pebble") grabs one by node name.

### `space.body`

space.body("Pebble") — one celestial body by node name: { name, x,y,z, vx,vy,vz, mu, radius, soi } in world coordinates, or nil. space.bodies() returns them all.

### `space.dominant`

space.dominant(x, y, z) — the name of the body whose gravity OWNS that position (deepest sphere of influence — the moon inside the planet inside the sun), or nil.

### `space.elements`

space.elements(x,y,z, vx,vy,vz) — the orbit a craft is ON around its dominant body: { body, a, e, periapsis, apoapsis, period } (apoapsis/period absent on an escape). Distances from the body CENTER. The map/HUD readout.

### `space.gravity`

space.gravity(x, y, z) — gx, gy, gz: the µ/r² pull of the dominant body at a world position (patched conics: exactly one body pulls).

### `space.propagate`

space.propagate(px,py,pz, vx,vy,vz, mu, dt) — the state (px,py,pz, vx,vy,vz) advanced dt seconds on the two-body conic about a point mass mu (elliptic OR hyperbolic, drift-free). The map's maneuver nodes + SOI-encounter walk are built from it. State is in whatever frame you pass — compose parent frames yourself.

### `space.time`

space.time() — on-rails celestial time in seconds (0 at Play start; advances with warp). Scenes with Celestial Body components put planets/moons on exact Kepler rails.

### `space.warp`

space.warp(50) — request a time-warp multiplier (1 .. 100000): rails fast-forward, local physics keeps ticking at 1×. space.warp() reads the current value.

## components — getcomponent

### `cam.active`

The play-mode view camera (1/0) — assign true to switch to it.

### `cam.fovY`

Vertical field of view, radians.

### `env.ambient2d`

find("Lighting"):getcomponent("Light") — the scene's Lighting node, read and written like any other component. THIS IS WHERE A 2D SCENE'S BRIGHTNESS LIVES: ambient2dR/G/B is the 2D base light, the whole light a flat scene has before a single 2D light is placed, so turning it down is how you get a dark room for a torch to carve a circle out of — and reading it back first is how you put it where it was. Also colorR/G/B + intensity + directionX/Y/Z (a day cycle), ambientR/G/B (the 3D fill, deliberately a different value), shadows/shadowSoftness/shadowStrength/shadowTintR/G/B/shadowQuantize/shadowDither/shadowDistance/contactShadows/contactLength/contactSteps/contactStrength, the screen-space reflections that make a shiny floor show the room standing on it rather than only the sky (reflections, reflectionDistance, reflectionSteps, reflectionThickness), reflectionClamp (the most one reflected bounce may carry — two mirrors facing each other re-reflect each other every frame and a polished metal loses almost nothing per pass, so without a ceiling the pair climbs into a white blob; 0 removes it), refractionLayers (how many depths of glass can be seen through at once — at 1 only the nearest pane shows what is behind it, so a fish tank has to be one box; raise it and a window can have a bottle standing behind it), and the whole fog set: fog, fogColorR/G/B, fogStart, fogEnd, fogDensity, fogHeight, fogFalloff, fogNoise, fogNoiseScale, fogVolumetric, fogDither, fogDitherStrength, fogSky, fogFollowsBody, and the volumetric light injection (fogLight, fogAnisotropy, fogSteps, fogShafts). Every scene has exactly one Lighting node and the loader makes it, so find("Lighting") always finds it. Writes land the same frame.

### `env.ambient2dB`

The 2D base light, blue 0..1. See ambient2dR.

### `env.ambient2dG`

The 2D base light, green 0..1. See ambient2dR.

### `env.ambient2dR`

The 2D BASE LIGHT, red 0..1 — the whole light a flat scene has before any 2D light is placed. White by default; turn it down for a dark room a torch can carve a circle out of, and read it back first so you can put it where it was.

### `env.ambientB`

3D ambient fill blue 0..1.

### `env.ambientG`

3D ambient fill green 0..1.

### `env.ambientR`

3D ambient fill red 0..1 — the fill under the key light, deliberately a different value from ambient2dR.

### `env.colorB`

Key light colour blue.

### `env.colorG`

Key light colour green.

### `env.colorR`

Key light colour red.

### `env.contactLength`

How far a contact shadow traces, in world units. Short is the point — the shadow under a foot, in a seam, behind a bolt.

### `env.contactShadows`

The small dark line where things touch (1/0). A moving mesh casts through its COLLIDER, so a character's shadow is a capsule's — this shadows from the real silhouette of whatever is on screen. Only what is ON SCREEN casts one.

### `env.contactSteps`

Samples along the contact trace (2..32). Raise it if the shadow looks striped.

### `env.contactStrength`

How dark a contact shadow gets, 0..1, before the shared shadow tint and strength.

### `env.directionX`

Key light direction X — lerp the three for a day cycle.

### `env.directionY`

Key light direction Y.

### `env.directionZ`

Key light direction Z.

### `env.fog`

Depth fog on (1/0; assign true/false).

### `env.fogAnisotropy`

Volumetric: which way the media throws light (-0.9..0.9). Positive blooms toward the sun, 0 is an even haze. Fog has no normal — this is what does that job.

### `env.fogColorB`

Fog colour blue.

### `env.fogColorG`

Fog colour green.

### `env.fogColorR`

Fog colour red — match it to the horizon or a seam shows.

### `env.fogDensity`

Volumetric: media density per world unit.

### `env.fogDither`

Dither the fog gradient to hide 8-bit banding on long ramps (1/0).

### `env.fogDitherStrength`

Dither amplitude 0..1.

### `env.fogEnd`

World distance where fog is full.

### `env.fogFalloff`

Volumetric: softness of the layer's top edge, world units.

### `env.fogFollowsBody`

Fog on a round world (1/0). On, fogHeight is an altitude over the nearest celestial body, the volumetric layer is a shell around it, and the sky blend leans on its horizon wherever you stand. Off, heights are world y, right for a flat level.

### `env.fogHeight`

Volumetric: world height (y) of the fog layer's top.

### `env.fogLight`

Volumetric: how much of the scene's light scatters IN the fog. 0 = a flat colour; 1 = lit by the sun, the point lights and the baked bounce; past 1 exaggerates. Ramp it up as a storm rolls in and the air itself starts carrying the light.

### `env.fogNoise`

Volumetric: how much drifting noise breaks up the media, 0..1.

### `env.fogNoiseScale`

Volumetric: noise feature size, world units per repeat.

### `env.fogShafts`

Volumetric (1/0): march the sun shadow at every fog step, so beams appear through windows and branches. The entire cost of lit fog lives here.

### `env.fogSky`

How much of the fog the sky takes at the horizon, 0..1. The sky is infinitely far away, so a thinner fog never makes it fainter on its own: fade this to fade the sky's fog, as a climb out of the air does.

### `env.fogStart`

World distance where fog begins (fully clear nearer than this).

### `env.fogSteps`

Volumetric: samples along each pixel's fog ray (2..64). The quality/cost dial — drop it on a weak machine.

### `env.fogVolumetric`

Volumetric mode (1/0): march real fog media instead of a distance ramp, so hills poke out of ground mist. fogStart/fogEnd do not apply.

### `env.intensity`

Brightness multiplier on the key (directional) light.

### `env.shadowDistance`

Max world distance a shadow ray marches before giving up; far geometry stops casting past it.

### `env.shadowDither`

Bayer-dither the penumbra (1/0) — the classic PS1 dithered shadow edge.

### `env.shadowMap`

Sun shadows from a shadow map instead of the field march (read/write, true/false; the Lighting panel's "shadow map"). Every drawn mesh casts — map-tool geometry, models, characters, terrain chunks — whether or not it collides, and the cost stays flat as a level grows: on a dense city at 1080p the lighting pass fell from 9.5 ms to 4.1 ms plus 0.65 ms for the map. shadowSoftness widens the filter; shadowDistance is how far around the camera the map reaches. Distance-field matter that is not drawn as a mesh (blobs) casts only in the march. In Stars mode the brightest star at the camera takes its shadows from the map and the others march, and a star giving under 1% of the brightest one's light is left out of the lighting altogether, so it costs nothing. Off by default.

### `env.shadowQuantize`

0 = smooth penumbra; 2..8 = posterize it into that many bands (toon/retro).

### `env.shadowSoftness`

0 = razor-hard edge … 1 = dreamy-soft penumbra.

### `env.shadowSteps`

Most steps a sun-shadow ray takes, 8..64 (default 64). The march is most of what sun shadows cost on the GPU, so this and shadowDistance are the levers a low quality setting turns while keeping shadows on: fewer steps end each ray sooner, and a shadow behind a long or intricate caster thins out.

### `env.shadowStrength`

How dark full shadow gets, 0..1 (ambient still fills, so never pitch black).

### `env.shadowTintB`

Shadow tint blue.

### `env.shadowTintG`

Shadow tint green.

### `env.shadowTintR`

Shadows darken toward this colour instead of black — red.

### `env.shadows`

Sun shadows on (1/0; assign true/false). Every shadow field below only applies when this is on.

### `env.stars`

Stars mode (1/0; assign true/false): luminous celestial bodies ARE the key lights.

### `light.b`

Color blue 0..1.

### `light.g`

Color green 0..1.

### `light.height`

Rect only: its height in world units.

### `light.intensity`

Brightness multiplier.

### `light.length`

Tube only: how long the bar is — a long one streaks its highlight along itself.

### `light.r`

Color red 0..1.

### `light.radius`

Sphere / disk only: its radius in world units.

### `light.range`

Reach in world units.

### `light.shape`

The surface it emits from: 0 point, 1 sphere, 2 rect, 3 disk, 4 tube. A rect and a disk face the node's FORWARD and a tube lies along its local X, so a light with a shape is aimed by rotating the node. Assigning keeps the size it had, so cross-fading a window into a bulb does not flash.

### `light.thickness`

Tube only: how thick the bar is.

### `light.twoSided`

Rect / disk only (1/0): lights out of the back as well as the front. Off is a window; on is a floating panel.

### `light.width`

Rect only: its width in world units. Reads 0 on a shape that has no width.

### `mat.alpha`

mat.alpha — opacity, 0..1. Also readable as mat.opacity.

### `mat.cell`

Which cell of the sheet draws (row-major from the top-left; clamped into the grid).

### `mat.color`

mat.color — the tint, MULTIPLIED into the texture: white leaves the picture alone, and a colour tints it. Takes a color(r, g, b) or any {r,g,b} table; reads back as a colour. The per-channel spellings mat.r / mat.g / mat.b are the same value, for animation lanes that key one number.

### `mat.emissive`

mat.emissive — light this surface gives off, scaled by emissiveStrength. A colour; the channels are also mat.emissiveR/G/B.

### `mat.emissiveMap`

mat.emissiveMap — where the surface glows and in what colour, multiplying emissive × emissiveStrength: a console's screen or a hull's windows glow on the same material as the metal around them. "" clears it (the emissive colour everywhere).

### `mat.emissiveStrength`

mat.emissiveStrength — how much light emissive gives off. 0 turns it off however bright the colour is.

### `mat.fog`

mat.fog — does the scene's fog reach this surface? false keeps a UI panel or a skybox plane out of the weather. Reads back as a BOOLEAN.

### `mat.metallic`

mat.metallic — 0 is a dielectric, 1 is bare metal. For a metal the ALBEDO is the reflection tint, so a black metal reflects nothing.

### `mat.metallicMap`

mat.metallicMap — per-pixel metalness. "" clears it.

### `mat.normalMap`

mat.normalMap — the surface's bump directions. "" clears it.

### `mat.occlusionMap`

mat.occlusionMap — baked ambient occlusion. "" clears it.

### `mat.rim`

mat.rim — the colour of the rim light around its silhouette (channels: mat.rimR/G/B).

### `mat.roughness`

mat.roughness — 0 is a mirror, 1 is chalk.

### `mat.roughnessMap`

mat.roughnessMap — per-pixel roughness. "" clears it.

### `mat.shader`

mat.shader — the .flsl surface shader this material draws with, project-relative ("shaders/ocean.flsl"), or "" when it has none. Assigning swaps it; "" goes back to the built-in look.

### `mat.sheetCols`

Sheet columns (0 = not a sheet — the whole texture).

### `mat.sheetRows`

Sheet rows.

### `mat.specular`

mat.specular — the colour of its highlight (channels: mat.specularR/G/B).

### `mat.texture`

mat.texture — the base-colour image, project-relative ("art/shirt.png"). Assigning swaps what the surface wears; "" clears it back to a flat colour. Reads back what you last set.

### `mat.unlit`

mat.unlit — draw at full brightness, ignoring every light. Reads back as a BOOLEAN.

### `mat:setShaderParam`

mat:setShaderParam("glow", 2.5) / mat:setShaderParam("nose", x, y, z) — drive a .flsl uniform on THIS material every tick: the node's own for node:material(), ONE PART's override for node:material("Head#2"). The node-level node:setShaderParam folds into the node's own Material and cannot reach a part, which is what this exists for — a character whose parts wear shaders (skin here, a face decal there) can now have a part's knobs driven at runtime. A GPU uniform write, never a recompile; unset lanes are 0. The write lands only on a part that already HAS an override wearing a .flsl — it never creates one (an override is a whole material, and a uniform must not blank a part), and a part with nowhere to put it is said once in the Console.

### `mat:setShaderTexture`

mat:setShaderTexture(slot, ref) — point one of THIS material's .flsl texture slots somewhere else, at runtime — on one part of a model when the handle came from node:material("Head#2"). `slot` is the name the shader declares (`texture face` -> "face"); `ref` is a project-relative image path, an `rt:<name>` render target, or "" to clear it. The character creator's face swap is this one line: node:material("Head#2"):setShaderTexture("face", "faces/02.png"). Same rule as setShaderParam: it lands on an existing override that wears a shader, and never creates one.

### `mat:shaderParam`

mat:shaderParam("glow") -> x, y, z, w — read a uniform back off this material: what you set this frame, else what the material carries. nil when it has no such knob. Reads in the same frame as the write, the way mat.color does.

### `mat:shaderTexture`

mat:shaderTexture(slot) -> ref — which image a texture slot points at: what you set this frame, else what the material carries; "" for a slot you cleared, nil for one nothing has named.

### `node:getComponent`

node:getComponent(name) — a component handle whose fields you can read AND assign at runtime (applies live during play), or nil if absent. Components: RigidBody (friction, restitution, gravity, kinematic 1/0 — live Dynamic/Kinematic switch, shape 0/1/2, radius, height, half_x/y/z, lock_x/y/z, lock_rot_x/y/z, two_d — 2D mode), PointLight (intensity, range, r/g/b, and the EMITTER: shape 0 point / 1 sphere / 2 rect / 3 disk / 4 tube, plus width, height, radius, length, thickness, twoSided — a rect light IS a window, so growing one softens the highlight it leaves on everything — plus shadows, which stops this lamp at the walls between it and what it lights instead of shining through them), Camera (fovY radians, active — assign true to switch cameras), ParticleSystem (play_on_start), UiElement (visible, opacity, posX/posY, width/height, radius, border, fillRGBA, textSize, textRGBA, tintRGBA, cell — spritesheet frame), UiSlider (value/min/max — drive a health bar), UiLayer (enabled, z, designHeight, worldSpace), PostProcess (enabled, bloom, bloomThreshold, bloomIntensity, vignette, vignetteStrength, vignetteRadius, aoStrength, aoRadius, posterizeBands, posterizeDither, tonemap, and the lens: dofFocus, dofRange, dofNearRange, dofBlur, dofBlades, dofBladeAngle, dofHighlight, dofSamples, plus the shutter: motionBlur, motionSamples, the grade: exposure, contrast, saturation, temperature, tint, lift, gradeGamma, gain (0 for the offsets and 1 for the scales is neutral), and the lens and film: aberration, distortion, grain (0 = off; a settings menu's toggles) — a cutscene pushing a vignette, pulling a rack focus, or opening the shutter for a slow-motion beat), LightProbes (enabled, intensity, leak, normalBias — the baked bounce's live knobs; the bake-time ones are not here because a script cannot bake), ReflectionProbe (enabled, intensity, fade — what a room reflects when what it is reflecting is off screen; the box is the node's own shape, and moving or resizing it re-captures). e.g. node:getcomponent("RigidBody").friction = 0.02 for ice.

```lua
local rb = node:getComponent("RigidBody")
if rb then rb.friction = on_ice and 0.02 or 0.6 end
```

### `rb.friction`

Grip, as a coefficient: a ramp holds while tan(its angle) <= friction. 0 is ice, 1 holds exactly 45 degrees, above 1 is grippier still.

### `rb.gravity`

Gravity pull on this body (1/0; assign true/false).

### `rb.half_x`

Box half-extent X.

### `rb.half_y`

Box half-extent Y.

### `rb.half_z`

Box half-extent Z.

### `rb.height`

Capsule total height, in the node's space: the body is this times the node's Y scale (rb.worldHeight).

### `rb.kinematic`

Transform-driven mode (1/0; assign true/false, live): never falls or gets pushed, but PUSHES dynamic bodies — platforms, elevators, grabbed objects. (Static mode is the Inspector dropdown — a baked collider, nothing to toggle here.)

### `rb.lock_rot_x`

Freeze rotation about X (1/0).

### `rb.lock_rot_y`

Freeze rotation about Y (1/0).

### `rb.lock_rot_z`

Freeze rotation about Z (1/0).

### `rb.lock_x`

Freeze world X translation (1/0).

### `rb.lock_y`

Freeze world Y translation (1/0).

### `rb.lock_z`

Freeze world Z translation (1/0).

### `rb.mass`

Mass (read and write, live). On an assembly part it is that part's share of the vessel: the compound's mass, centre of mass and inertia follow on the next tick, with no rebuild and no kick — a tank burning off or a hold filling is one write a tick.

### `rb.radius`

Sphere/capsule radius, in the node's space: the body is this times the node's scale (rb.worldRadius).

### `rb.restitution`

Bounciness 0..1 (0 = no bounce).

### `rb.shape`

Body shape: 0 = sphere, 1 = capsule, 2 = box.

### `rb.slopeLimit`

Steepest standable surface, in degrees (default 60). Past it nothing grounds the body and no grip holds it.

### `rb.two_d`

2D (1/0): keep the body in the XY plane — it keeps its depth, never drifts out of the layer, and still spins the one way a flat object spins. Collides with the same world a 3D body does.

### `rb.worldHalfX`

Read-only: the box's half-extent X as the physics builds it, half_x times the node's world scale. half_* are the authored, unscaled numbers, as are radius and height; rb.worldRadius and rb.worldHeight are those as the physics builds them.

### `rb.worldHalfY`

Read-only: the box's half-extent Y as the physics builds it (half_y times the world scale).

### `rb.worldHalfZ`

Read-only: the box's half-extent Z as the physics builds it (half_z times the world scale).

### `rb.worldHeight`

Read-only: the capsule's total height as the physics builds it, height times the node's world Y scale (never less than twice rb.worldRadius).

### `rb.worldRadius`

Read-only: the sphere's or capsule's radius as the physics builds it, radius times the node's world scale. Under an uneven scale a sphere takes the largest axis and a capsule the larger of X and Z.

## animation — node:animator

### `anim:addBonePos`

anim:addBonePos("Hips", vec3(0, -0.12, 0) [, weight]) — move one bone by this much in its parent's space, on top of the clips, for this frame only.

### `anim:addBoneRot`

anim:addBoneRot("Spine1", yaw, pitch, roll [, weight]) — turn one bone further by this much, on top of what the clips put there, for this frame only. The way to aim a chest over a run cycle without taking the run's motion off it.

### `anim:boneIndex`

anim:boneIndex("Rib12") → number — the bone's number on this model, its place in node:bones(). Every bone write (setBoneRot/addBoneRot/setBonePos/addBonePos) takes the number instead of the name, and anim:setBones takes a list of them: no name copied and looked up per call. nil until the model has posed once (so not in start); a bone the model lacks raises.

### `anim:boneWorld`

anim:boneWorld("Hand") → pos, yaw, pitch, roll — where the bone is this frame, in world space, after clips, bone writes and IK. A write made earlier in the same pass is already in the answer, counted once, so `setBonePos` then `boneWorld` reads back the moved bone (written every frame, it is not read twice). Read before any write this frame, it is the animator's pose for this frame, without last frame's writes (unless anim:holdPose(true) keeps them). nil until the model is posed. Works under `floptle run` and on a dedicated server, which read the rig without its pictures, so a hitbox can follow a hand where nothing is drawn.

### `anim:clips`

anim:clips() — every playable state name, as a list.

### `anim:crossfade`

anim:crossfade("Idle", 0.3 [, layer]) — transition with an explicit fade time (seconds).

### `anim:current`

anim:current([layer]) — alias of anim:state: the state currently showing (topmost active layer). Nil when idle.

### `anim:duration`

anim:duration("Punch") — the clip's AUTHORED length in seconds (nil if there's no such state). Reads the asset, not playback, so it works in start().

### `anim:events`

anim:events("Punch") — the clip's authored events as { {t = seconds, func = "onHitboxStart"}, … }, ascending by t; nil if there's no such state, an empty list if it has none. Reads the asset, so you can bake integer frame data at load: frame = math.floor(e.t / anim:duration(c) * totalFrames + 0.5). Prefer this to letting events DRIVE gameplay — they fire off float playback time, quantise to sample_fps, and are deliberately not re-fired on a prediction replay.

### `anim:finished`

anim:finished([layer]) — true when a non-looped state reached its end this frame (or stays true while holding the last frame).

### `anim:holdPose`

anim:holdPose(true) — keep the writes of the last frame that made any while later frames write nothing, instead of falling back to the clips: a still, non-rest pose costs nothing to keep. The first write of a later frame replaces the whole held pose; anim:holdPose(false) hands the rig back to the clips that same frame.

### `anim:isPlaying`

anim:isPlaying([state]) — is that state playing on any layer (or anything at all, with no argument)?

### `anim:layers`

anim:layers() — every layer name, base first, as a list.

### `anim:lookAt`

anim:lookAt({"Spine2", "Neck", "Head"}, target [, { weight = 1, limit = 75, axis = vec3(0, 0, 1) }]) — turn a bone, or a chain shared out root-first, so it faces a world point, for this frame only. limit caps the whole turn in degrees; axis is which way the last bone faces at rest (+Z unless your rig says otherwise).

### `anim:play`

anim:play("Run" [, fade [, layer]]) — transition to a state. The controller supplies the crossfade (default fade, per-arrow overrides, and a state's ⇥ fade-in override which beats everything — 0 = instant); pass `fade` to override the first two. Safe to call every frame — re-playing the current state is a no-op.

### `anim:reach`

anim:reach("Hand", target [, { pole = vec3, weight = 1, root = "UpperArm", mid = "Forearm" }]) — two-bone IK: bend the tip's parent and grandparent (or the named root and mid) so the tip lands on a world-space point, for this frame only. pole is a world point the middle joint bends toward: in front of a knee, behind an elbow. A target out of reach straightens the limb toward it. Runs after the clips and the bone writes. Call it from update or from lateUpdate: from lateUpdate it is solved the same frame against where the camera pass left everything, which is where hands that hold something under the camera belong.

### `anim:restart`

anim:restart("Attack" [, fade [, layer]]) — like play, but re-enters even if that state is already playing (re-trigger a one-shot).

### `anim:seek`

anim:seek(t [, layer]) — jump the current state's playhead to t seconds.

### `anim:setBonePos`

anim:setBonePos("Hips", vec3(0, 0.9, 0) [, weight]) — replace one bone's local position, in its parent's space, for this frame only. For the root or hips: drop them to plant both feet on a slope.

### `anim:setBoneRot`

anim:setBoneRot("Neck", yaw, pitch, roll [, weight]) — replace one bone's local rotation (radians, the node.yaw/pitch/roll convention) after the clips have posed the rig, for this frame only: call it every frame you want it held. weight 0..1 blends from the animated pose. On a rigged model; a bone the model doesn't have warns once.

### `anim:setBones`

anim:setBones(bones, positions, rotations [, weight]) — set many bones in one call, for this frame only: bones is a list of numbers from anim:boneIndex, positions flat x, y, z per bone (parent space), rotations flat yaw, pitch, roll per bone (radians); either list may be nil to leave that channel alone. The same pose as one setBonePos + setBoneRot per bone, for a fraction of the cost — a hundred-bone cloth or rope in one crossing.

### `anim:setCulling`

anim:setCulling("whenVisible") — override the controller's culling for this node: "always" (animate whether seen or not), "whenVisible" (skip while the model or an ancestor is hidden, or it is behind or beside the active camera, then catch up in one step when it comes back, without firing the events it missed), or "reduced" (as whenVisible, and past 30 m pose every third frame). Keep "always" for anything a bone must drive while nobody is looking, such as a hitbox on a hand. perf.counts().animatorsCulled says how many were skipped.

### `anim:setEnabled`

anim:setEnabled(false) — switch this animator off: no advance and no pose (the model holds its last pose) until anim:setEnabled(true). Unlike setSpeed(0), which stops time but still poses the skeleton every frame, off costs nothing. Survives the animator rebinding.

### `anim:setLayerWeight`

anim:setLayerWeight("Attack", 0.5) — blend a layer over the ones below (0 = off, 1 = full override).

### `anim:setParam`

anim:setParam("speed", 3.2) — set a number the controller's blend states read. A blend state mixes its clips by where one or two params fall among its points: `aimX` from -1 to 1 across aim-left, aim-center and aim-right. A param no state reads is kept and does nothing.

### `anim:setSpeed`

anim:setSpeed(2 [, layer]) — playback speed: with no layer, the whole animator's; with a layer, that layer's own rate on top of it, so a punch keeps its speed while the run under it is sped up to match the ground.

### `anim:state`

anim:state([layer]) — the state currently showing (topmost active layer), or that layer's state. Nil when idle.

### `anim:stop`

anim:stop([layer [, fade]]) — stop a layer (all layers if omitted). Higher layers release to the layers below; the base returns to its default state.

### `anim:time`

anim:time([layer]) — seconds into the current state.

## particles — effects from script

### `node:particles`

node:particles() — the particle handle for this node's Particle System component. Setters: :play/:stop/:restart/:setIntensity/:setVolume/:setTint/:setBeamEnd/:setBeamControl. Getters: :isPlaying/:alive/:asset. e.g. on a hit, node:particles():restart() to re-fire a burst.

### `particles:alive`

particles:alive() — live particle count across the effect's tracks (0 when stopped).

### `particles:asset`

particles:asset() — the effect asset key this node's Particle System references, or nil.

### `particles:isPlaying`

particles:isPlaying() — true while an instance is emitting/ageing on this node.

### `particles:play`

particles:play() — start emitting if the effect is idle (spawns a fresh instance). No-op if already playing.

### `particles:restart`

particles:restart() — re-spawn from t=0 (re-fire a one-shot burst, e.g. a muzzle flash on each shot).

### `particles:setBeamControl`

particles:setBeamControl(x, y, z) or particles:setBeamControl(vec3) — bow every Beam track toward a WORLD-space control point: the ribbon curves from the emitter to its end, pulled toward the point and passing halfway to it. A rope's sag, a tether's bow, a hose. Converted to effect-local like setBeamEnd, so re-call it per tick with setBeamEnd to follow both ends. particles:setBeamControl() with no point puts each track back to its authored control point (the Particles tab's `bow`), or straight.

### `particles:setBeamEnd`

particles:setBeamEnd(x, y, z) — aim every Beam track's endpoint at a WORLD-space point (the engine converts it to effect-local, so the beam keeps tracking the target as the node moves). Re-call per tick to follow a moving target.

### `particles:setIntensity`

particles:setIntensity(i) — live emission scale (0..~2): multiplies rates/burst counts and shades particle size. Drive an engine plume off the throttle without touching the asset.

### `particles:setTint`

particles:setTint(r, g, b [, a]) or particles:setTint(color) — multiply every particle of the node's effect by a colour, on top of the colours the effect authors. Kept for the node, so a restart keeps it. One dust effect matching each world's ground: p:setTint(0.8, 0.55, 0.4).

### `particles:setVolume`

particles:setVolume(v) — how big the space the effect fills is, as a multiple of its authored emission shape (1 = as authored): a sphere of radius 1 at volume 12 is born within 12 m of the node. Only where particles are born changes: their size, count, speed and brightness stay as authored, so one mark effect fits a 0.4 m tuft and a 12 m tree alike. setIntensity changes count and size; the node's scale changes particle size too. Kept for the node, so a restart keeps it.

### `particles:stop`

particles:stop() — stop + despawn the effect; its live particles vanish.

### `spawnEffect`

spawnEffect(key, pos [, opts]) or spawnEffect(key, x, y, z [, vx, vy, vz]) — fire a one-shot particle effect at a world point, no node needed. It plays once and despawns itself. opts: normal (vec3: the effect's +Y, so a ring lies across it and a cone fires along it; default world up, or against gravity on a round world), up (vec3: which way Vertical and Horizontal billboards stand; default normal), scale, intensity, volume (how big the space it fills is, as a multiple of its emission shape; particle size and brightness stay as authored, where scale grows the particles too), tint (a color), vel (vec3: the emitter's velocity, which inherit-velocity tracks keep a share of), frame (a node the effect rides: its emit point and its World-track particles move with it). With no frame it rides the planet on rails whose sphere of influence it is in, so dust on a moving world stays over its spot; vel is then relative to that. The point is placed against the world as it was when you asked, so a point worked out in update lands where you meant on a moving planet. Local to this machine: in a session, spawn it from state that replicates (a synced counter every peer watches), or the server's sparks stay on the server. e.g. local h = raycast(...); if h then spawnEffect("vfx/Impact", vec3(h.x, h.y, h.z), { normal = vec3(h.nx, h.ny, h.nz) }) end.

## audio — sounds & the mixer

### `audio`

Sounds and the mixer: audio.play for one-shots, audio.track for a mixer bus, node:sound() for a node's Audio Source.

### `audio.play`

audio.play(clip [, node | x, y, z] [, opts]) — play a clip with no setup: audio.play("audio/ding.ogg") is flat 2D; pass x,y,z for a world point; pass a node to follow it. Local to this machine: in a session, play it from state that replicates, or a sound the server's code plays is heard by nobody. opts: {volume, pitch, pan, mode="Spatial|Distance|Flat", falloff="Inverse|Linear|Exponential", minDistance, maxDistance, track, endBehavior="Stop|Destroy|Loop", loop=true, sends={["Verb Large"]=0.4}, lowpass=hz}. sends taps the sound into other mixer tracks beside its own, at those levels, after its volume and distance (a reverb bus heard partly); lowpass muffles it alone (a sound behind a wall), 0 = off. Returns a sound handle: :stop/:pause/:resume/:setVolume/:setPitch/:setPan/:setTrack/:setSend/:setLowpass/:setPosition/:seek/:isPlaying/:isLoading/:position. A clip's first play reads the file in the background, so the sound starts a frame or more after the call (a long music file takes longest); audio.preload removes the wait. e.g. audio.play("audio/hit.ogg", h.x, h.y, h.z, { maxDistance = 35, track = "SFX" })

```lua
audio.play("audio/footstep", node, { track = "SFX", volume = 0.6, minDistance = 4 })
```

### `audio.preload`

audio.preload({ "audio/hit", "music/level1.ogg" } [, function(failed) end]) — read sounds in the background ahead of their first play, so it starts on the frame audio.play is called, and call back once every one is in. Names are spelled as audio.play takes them (the extension is optional); `failed` lists any that could not load. A preloaded clip is kept until it has been played; after that, a clip nothing is playing is kept only while the idle ones fit in 64 MB, least recently played dropped first, and read again if it is played later. On a host with no audio output it calls back straight away.

### `audio.stopAll`

audio.stopAll() — stop every playing sound (sources and one-shots), with a click-free fade.

### `audio.track`

audio.track(name) — a live mixer-track handle ("Master" or a track from the Mixer tab), or nil when the mixer has no such track: :setVolume(db), :setPan(-1..1), :setMuted(bool), :setSoloed(bool), :setLowpass(hz), :setBypass(slot, on), :info(). Changes hold for the whole play session — across scene.load, so a Settings screen sets them once — and revert on Stop. e.g. audio.track("Music"):setVolume(-12) to duck music. A sound sent to a track the mixer does not have plays through Master, and the Console says so once.

### `audio.tracks`

audio.tracks() → { {name, gainDb, pan, muted, soloed, output, lowpass, effects}, ... } — every mixer track, Master first, as this play session has it (script changes included). output is the track it feeds, "Master" for a top-level track and nil for Master itself.

### `sound:isLoading`

True from audio.play until its clip has been read in the background; it reads as playing meanwhile, and what you do to it (seek, pause, volume, stop) applies when it starts.

### `sound:isPlaying`

Still audible (false once finished)?

### `sound:pause`

Freeze playback.

### `sound:position`

Playhead in seconds.

### `sound:resume`

Continue a paused sound.

### `sound:seek`

Jump the playhead to a time in seconds.

### `sound:setLowpass`

sound:setLowpass(850) — muffle this sound alone above that many Hz (a sound behind a wall); 0 turns it off. Glides, so moving it every frame does not click.

### `sound:setPan`

Stereo pan −1..1 (non-spatial sounds).

### `sound:setPitch`

Playback-rate pitch (0.5 = octave down, 2 = octave up).

### `sound:setPosition`

Move the emitter (stops following a node).

### `sound:setSend`

sound:setSend("Verb Large", 0.4) — tap this sound into another mixer track beside its own, at that level; 0 removes the tap. A send to a track that doesn't exist does nothing.

### `sound:setTrack`

Re-route through a mixer track (\"Master\" or a track name).

### `sound:setVolume`

Linear volume (1 = as authored).

### `sound:stop`

Fade the sound out and end it.

### `source:isLoading`

True from play until its clip has been read in the background; it reads as playing, from 0 seconds, meanwhile.

### `source:isPlaying`

Is the source audible right now?

### `source:pause`

Freeze playback (resume continues from here).

### `source:play`

Play the source's clip from the start (restarts if already playing).

### `source:position`

Playhead in seconds.

### `source:resume`

Continue a paused sound.

### `source:seek`

Jump the playhead to a time in seconds.

### `source:setClip`

Swap the clip (project-relative path like \"audio/steps.ogg\"); restarts playback if playing.

### `source:stop`

Fade the sound out (a few ms — no click).

### `track:info`

The track as audio.tracks() lists it — {name, gainDb, pan, muted, soloed, output, lowpass, effects} — or nil.

### `track:setBypass`

track:setBypass(slot, on) — bypass the effect in that slot of the track's chain, counting from 1 as the Mixer tab lists it.

### `track:setLowpass`

track:setLowpass(hz) — muffle the whole bus above that many Hz, every sound on it at once (underwater, behind a wall); glides, never clicks. 0 = off.

### `track:setMuted`

Mute / unmute the track.

### `track:setPan`

Stereo pan −1..1.

### `track:setSoloed`

Solo the track (mutes everything else).

### `track:setVolume`

Fader gain in dB (0 = unity, −60 = silent).

## assets

### `assets`

Reference files under Assets/ in code: assets.getFile(path), assets.getContents(dir), assets.preload(models, cb), assets.warm(paths, cb). Read and write your own data files: assets.readText / writeText, assets.readJson / writeJson.

### `assets.getContents`

assets.getContents("models") — an array of every file under that folder (recursive), each relative to Assets/ ("models/armor/iron.glb") as the other assets calls take it; a user:// folder lists user:// paths. Build tables of assets with it. Relative to Assets/ and inside it; at most 20 000 files, and it says when it stopped.

### `assets.getFile`

assets.getFile("models/armor.glb") — the asset's path relative to Assets/ (or nil), to hand to node.model / node.material. Path is relative to Assets/ and stays inside it: an absolute path or `..` is nil and one Console line.

### `assets.preload`

assets.preload({ "models/arm.glb", "models/leg.glb" } [, function(failed) end]) — load models in the background ahead of the moment a node needs them, and call back once every one is in. A sound file (.wav/.ogg/.mp3/.flac) in the list is preloaded as audio.preload would. `failed` lists any path that could not load (empty when all did); a single path works too. Warm a ragdoll's parts on a menu or a loading screen, so the death that spawns them draws them on its first frame. Without a GPU (floptle run, a dedicated server) there is nothing to load and it calls back straight away. Stop and scene.load drop a waiting callback; the models stay loaded.

### `assets.readBytes`

assets.readBytes("user://covers/intro.png") -> bytes, err — a file's contents as raw bytes in a Lua string: a picture, a compressed level, a replay. assets.readText refuses what is not UTF-8; this reads anything. Same paths as readText, `user://` included. Refuses: the path leaves its folder (absolute, or containing `..`); no such file; bigger than 64 MB. Each one names the call and the path.

### `assets.readJson`

assets.readJson("charts/neon.json") -> value, err — a JSON file decoded straight to a Lua value (json.decode rules: objects are tables, arrays are 1-based lists tagged with json.array, null is nil). nil and a message for a missing, unreadable or malformed file, plus one Console line. The way a rhythm chart, a dialogue tree or a level table gets into a script without being rewritten as Lua. Refuses: everything `assets.readText` refuses, plus: the bytes are not valid JSON. Each one names the call and the path.

### `assets.readText`

assets.readText("data/intro.txt") -> text, err — a text file's whole contents (UTF-8), or nil and why: missing, not text, over 64 MB, or a path outside the project. Relative to Assets/ and inside it, or a `user://` path for the player's own files (see assets.writeText). Works the same from an exported build and in a browser (it reads the bundle). Refuses: the path leaves the project (absolute, or containing `..`); no such file; the file is bigger than 64 MB; the bytes are not UTF-8 text. Each one names the call and the path.

### `assets.release`

assets.release(tex) -> bool — let go of a texture made by assets.textureFromUrl / textureFromBytes; anything still drawing it draws nothing. true if it was one of those; false for anything else (a project texture is never released).

### `assets.remove`

assets.remove("user://levels/mine.json") -> ok, err — delete one of the PLAYER's files: a level they made, a replay, a screenshot. Only `user://` paths: the game's own files ship with it and are not a script's to delete. Refuses: a path without `user://`; one that leaves the player's folder; no such file.

### `assets.textureFromBytes`

assets.textureFromBytes(bytes, function(tex, err) end) — assets.textureFromUrl for bytes you already have, such as a blob's res.body. Same formats, limits and names; at most 8 MB of bytes.

### `assets.textureFromPixels`

assets.textureFromPixels(w, h, pixels) → tex — a texture from pixels a script made: bark, leaves, a minimap, a noise pattern. `pixels` is r, g, b, a per pixel, row by row from the top, as a string of w*h*4 bytes (string.char, or a buffer read out) or a flat list of numbers 0–255. Answers a name like "img:5" at once, usable anywhere a texture path is (a material's texture, draw.quad, a UI image); it draws from the next frame. w and h are whole numbers from 1 to 4096, and the count must match exactly — anything else raises. Kept until assets.release(tex) or Stop. A host that draws nothing (floptle run, a dedicated server) answers nil, why.

### `assets.textureFromUrl`

assets.textureFromUrl(url [, opts], function(tex, err) end) — download a picture (a profile picture, a shared image) and make it a texture. `tex` is a name like "img:3" that goes anywhere a texture path does: draw.quad, a UI image's texture, a material's texture; a UI image with radius = half its size draws round. PNG, JPEG and WebP only, told apart by the bytes' own signature, at most 4096 pixels a side; anything else answers nil, why. The download is exactly http.get (same opts, Play only, public addresses, the same rate limits), and a URL already loaded this session answers from memory. The callback runs on a later frame. Kept until assets.release(tex) or Stop. A host that draws nothing (a dedicated server) answers with an error and downloads nothing. Refuses: what http.get refuses, at the call.

### `assets.warm`

assets.warm({ "textures/hull.png", "shaders/ocean.flsl", "materials/steel.ron", "models/arm.glb" } [, function(failed) end]) — prepare assets ahead of the frame that first uses them, and call back once every one is ready: behind a loading screen, so the level's first frame does not stop to decode images or build shaders. Images (.png, .jpg, …) are decoded on a few worker threads and uploaded a few milliseconds a frame; a shader (.flsl) of a mesh material, a UI element or a screen pass is compiled and its pipeline built on a worker, one shader at a time, and installed one a frame (a sky or sdf shader is built into its pass when worn, and is reported in `failed`); something the game draws while it is still warming waits for that work rather than doing it again; a project material (materials/<name>.ron) warms its images and its shader; models and sounds load as assets.preload loads them. `failed` lists any path that could not be prepared (a missing file, a shader that does not compile); a single path works too. Without a GPU (floptle run, a dedicated server) there is nothing to prepare and it calls back straight away. Stop and scene.load drop a waiting callback; what was prepared stays.

### `assets.writeBytes`

assets.writeBytes("user://covers/intro.png", bytes) -> ok, err — write a Lua string's bytes exactly, creating the folders on the way. The pair to readBytes: a camera.capture PNG, a data.deflate blob, a player's blueprint library (no size cap; a save slot stops at 4 MB). Same paths as assets.writeText; a `user://` file is replaced whole or not at all, so a crash mid-write keeps the one the player had. Refuses: the path leaves its folder (absolute, or containing `..`); the path is an existing FOLDER; the folder could not be created; the write itself failed (permissions, a full disk).

### `assets.writeJson`

assets.writeJson("charts/neon.json", value [, { pretty = true }]) -> ok, err — encode a Lua value as JSON (json.encode rules; json.array{} for an empty list) and write it under Assets/. `pretty` indents it for a person or a git diff. A chart editor built IN the game saves straight into the project, and the file it wrote is one the Asset Browser shows and the export ships. Refuses: everything `assets.writeText` refuses, plus: the value contains something JSON cannot hold (a function, a cycle).

### `assets.writeText`

assets.writeText("charts/neon.txt", text) -> ok, err — write a file under Assets/, creating the folders on the way and replacing what was there. A path starting `user://` is the PLAYER's instead (settings, progress, levels they made, replays): in an exported build it lives in a per-user data folder that survives installing a new version, apart from the game's files; in the editor and floptle run it is the project's save/user. An exported build that writes a plain path writes into its own install folder, which a new build will not see and an installed game may not be allowed to write — it still works, and the Console says so once. A path outside the project is refused (false and why). Editor or exported build alike; in a browser the write lands in the page's own storage and survives a reload. Refuses: the path leaves the project (absolute, or containing `..`); the path is an existing FOLDER; the folder could not be created; the write itself failed (permissions, a full disk).

### `mesh`

Meshes a script builds while the game runs: mesh.build and mesh.free. A built mesh is a name like "mesh:3" that goes wherever a model path does — node.model, scatter.create{ asset = }.

### `mesh.build`

mesh.build{ positions =, indices = [, normals =, uvs =, colors = ] } → name — build a mesh from numbers: a procedural plant, a fairing fitted to a part, a rope. positions is x, y, z per vertex (local space); indices is three per triangle, numbering vertices from 1, wound counter-clockwise seen from the front; normals x, y, z per vertex (left out, they are worked out from the faces, smooth); uvs u, v per vertex (0, 0 at the texture's top-left); colors r, g, b, a per vertex, 0–1, multiplied into the material. Vertex alpha fades the surface only when it blends: give the mesh a material with alpha below 1 (alpha = 0.999 is enough). On an opaque material, vertex alpha below 0.5 cuts a hole instead, as a texture's alpha does. Answers a name at once: node.model = name draws it from the next frame, with the node's own material (node:setMaterial{ texture = assets.textureFromPixels(...) }). At most 1 048 576 vertices and 3 145 728 indices. A missing or uneven list, an index out of range, or a non-finite number raises, naming the list. Kept until mesh.free(name) or Stop. One material per mesh: a part in two materials is two meshes. A host that draws nothing still answers a name, and nothing draws it. A Mesh collider does not read a built mesh.

### `mesh.free`

mesh.free(name) → true if it was a mesh mesh.build made — give it back. Nodes still wearing it draw nothing; the name is never handed out again.

## debug gizmos

### `gizmo`

Immediate-mode debug drawing (play mode): gizmo.line/ray/sphere/point show for ONE frame in the Scene view (never the Game view; the viewport gizmos toggle hides them). Call every frame you want a shape visible.

### `gizmo.line`

gizmo.line(x1,y1,z1, x2,y2,z2 [, r,g,b]) — a world-space debug line for one frame. Color is 0–1 floats (default green).

### `gizmo.point`

gizmo.point(x,y,z [, size [, r,g,b]]) — a small 3-axis cross marking a spot: hit points, waypoints, spawn locations.

### `gizmo.ray`

gizmo.ray(ox,oy,oz, dx,dy,dz [, len [, r,g,b]]) — a debug ray: origin + direction. With `len` the direction is normalized and the ray is that long — mirrors raycast(...), perfect for visualizing ground checks / line-of-sight.

### `gizmo.sphere`

gizmo.sphere(x,y,z [, radius [, r,g,b]]) — a wire debug sphere (three rings): trigger zones, blast radii, pickup ranges.

## lua stdlib

### `math.abs`

math.abs(x) — absolute value.

### `math.approach`

math.approach(current, target, maxDelta) — move toward target without ever overshooting. Pass `rate * dt`; this is the correct version of the hand-rolled move-towards that jitters at low frame rates.

```lua
-- frame-rate correct, never overshoots
throttle = math.approach(throttle, target, params.rate * dt)
```

### `math.approachAngle`

math.approachAngle(current, target, maxDelta) — math.approach for headings: turns the short way and never overshoots. Turrets, camera yaw, 'face the player'.

### `math.clamp`

math.clamp(x, lo, hi) — x held inside the range. Reversed bounds are tolerated rather than returning NaN.

```lua
hp = math.clamp(hp + heal, 0, 100)
```

### `math.cos`

math.cos(x) — cosine of x (radians).

### `math.deg`

math.deg(rad) — radians to degrees.

### `math.deltaAngle`

math.deltaAngle(a, b) — the SHORTEST signed turn from a to b, correct across the +/-pi seam (350 degrees to 10 is +20, not -340).

```lua
-- the short way round, across the +/-pi seam
local turn = math.deltaAngle(node.yaw, wanted)
node.yaw = math.approachAngle(node.yaw, wanted, params.turn_rate * dt)
```

### `math.fbm`

math.fbm(x, y, z [, octaves [, seed]]) — seeded fractal noise (default 4 octaves, rotated so features never align to the axes), about -1..1. Terrain-style variation for scripts: scatter decorations, vary spawns, wobble paths.

### `math.floor`

math.floor(x) — round down.

### `math.inverseLerp`

math.inverseLerp(a, b, x) — where x sits between a and b, 0..1. Returns 0 when a == b instead of a NaN that poisons everything downstream.

### `math.lerp`

math.lerp(a, b, t) — linear blend, UNCLAMPED (t outside 0..1 extrapolates, which is useful). Use math.mix for the clamped version.

### `math.max`

math.max(a, b, …) — largest argument.

### `math.min`

math.min(a, b, …) — smallest argument.

### `math.mix`

math.mix(a, b, t) — math.lerp with t clamped to 0..1.

### `math.noise`

math.noise(x, y, z [, seed]) — seeded value noise, one octave, about -1..1, identical on every machine (the same numbers the engine's Rust generators use). Scale the inputs to pick a frequency.

### `math.pi`

The constant π.

### `math.pingPong`

math.pingPong(t, len) — 0 to len and back, forever. Patrols, bobbing, breathing lights.

### `math.rad`

math.rad(deg) — degrees to radians.

### `math.random`

math.random() — random in [0,1); math.random(n) — 1..n.

### `math.remap`

math.remap(x, a, b, c, d) — x from the range a..b onto c..d. The one-liner behind fades, falloffs and gauge needles.

```lua
local alpha = math.remap(distance(node, player), 5, 25, 1, 0)
```

### `math.round`

math.round(x [, step]) — nearest whole number, or nearest multiple of `step`: `math.round(x, 0.25)` snaps to quarters for grid placement.

### `math.saturate`

math.saturate(x) — clamp to 0..1, the most-written clamp of all.

### `math.sign`

math.sign(x) — -1, 0 or 1. Exactly 0 for 0 (not 1, which is what math.abs tricks give you).

### `math.sin`

math.sin(x) — sine of x (radians).

### `math.smoothstep`

math.smoothstep(a, b, x) — 0..1 with eased ends, for anything that shouldn't start and stop abruptly.

### `math.sqrt`

math.sqrt(x) — square root.

### `math.wrapAngle`

math.wrapAngle(a) — an angle folded into (-pi, pi].

### `rng`

rng(seed) — a DETERMINISTIC random stream: same seed, same sequence, every machine. r:next() in [0,1), r:range(a,b), r:int(a,b) inclusive, r:pick(list). Use for gameplay that must reproduce (loot, procgen scatter, server replays); math.random stays for throwaway rolls.

### `rng:int`

Uniform integer in [a, b] inclusive.

### `rng:next`

Uniform in [0, 1).

### `rng:pick`

A uniform element of `list` (nil if empty).

### `rng:range`

Uniform in [a, b).

### `string.format`

string.format(fmt, …) — printf-style formatting.

### `table.copy`

table.copy(t) — a shallow copy (keys and values).

### `table.count`

table.count(t [, fn]) — how many entries (works on KEYED tables, which `#t` cannot), or how many satisfy the predicate.

### `table.extend`

table.extend(dst, src) — append src's items onto dst in place, and return dst.

### `table.filter`

table.filter(list, fn) — a new list of the items where fn(value, i) is true.

```lua
local ready = table.filter(ships, function(s) return s.fuel > 0 end)
```

### `table.find`

table.find(list, fn) -> value, index — the first item satisfying the PREDICATE (nil, nil if none). `table.find(ships, function(s) return s.docked end)`.

```lua
local docked, i = table.find(ships, function(s) return s.docked end)
```

### `table.indexOf`

table.indexOf(list, value) — the index of a value by plain equality, or nil.

### `table.keys`

table.keys(t) — the keys as a SORTED list. Sorted because raw `pairs` order is hash order, which a replay can't reproduce.

### `table.map`

table.map(list, fn) — a new list of fn(value, i). Never mutates the input.

```lua
local names = table.map(crew, function(m) return m.name end)
```

### `table.reverse`

table.reverse(list) — a new list, back to front.

### `table.sum`

table.sum(list [, fn]) — add the numbers, or add fn(value, i) over them: `table.sum(tanks, function(t) return t.fuel end)`.

