# Scripting in Floptle (Lua)

Game logic in Floptle is written in **Lua**. A script is a `.lua` file in your
project's `scripts/` folder. Attach it to a node and it runs every frame while the
game is playing. Scripts **hot-reload** — save the file and the running game picks
it up immediately.

**This page teaches; [`lua-api.md`](lua-api.md) lists.** The guide is in chapters:
read one end to end and you have that part of the engine, or dip into one when you
need it. The reference has every call the engine answers, grouped and searchable,
for when you already know the name and want the signature.

> The same chapters are in the editor — **Scripting** tab → **§ Docs** → **📖 Guides**,
> with the contents down the left — and every call shows up as autocomplete and hover
> hints as you type. All three are generated from one table, so they cannot disagree.

Section numbers are stable and are what the engine's own messages cite ("see
scripting.md §16"), so they stay put even when a chapter is rearranged around them.

### [Start here](scripting/start.md)

Your first script, the hooks the engine calls, and the globals every script has.

- [1. A first script](scripting/start.md#1-a-first-script)
- [2. Lifecycle: `start`, `update`, `fixedUpdate`](scripting/start.md#2-lifecycle-start-update-fixedupdate)
- [6. Globals: `params`, `time`, `dt`, `log`](scripting/start.md#6-globals-params-time-dt-log)
- [12. Recipe: a walkable first-person character](scripting/start.md#12-recipe-a-walkable-first-person-character)
- [13. Bundled example scripts](scripting/start.md#13-bundled-example-scripts)

### [Nodes & the scene](scripting/nodes.md)

Moving things, finding things, and swapping the whole world out.

- [3. `node` — the transform](scripting/nodes.md#3-node-the-transform)
- [8. Referencing other nodes & scripts](scripting/nodes.md#8-referencing-other-nodes-scripts)
- [17. Scenes: `scene.load` & the entry scene](scripting/nodes.md#17-scenes-sceneload-the-entry-scene)
- [18. Layers & tags](scripting/nodes.md#18-layers-tags)
- [21. Prefabs: `spawn` & `destroy`](scripting/nodes.md#21-prefabs-spawn-destroy)

### [Physics](scripting/physics.md)

Bodies, collisions, and asking the world what is in front of you.

- [4. `node` — the physics body](scripting/physics.md#4-node-the-physics-body)
- [20. Collision & trigger events](scripting/physics.md#20-collision-trigger-events)

### [Input](scripting/input.md)

Keyboard, mouse, and the action map.

- [5. `input` — keyboard & mouse](scripting/input.md#5-input-keyboard-mouse)

### [Assets & materials](scripting/assets.md)

Models, textures, materials and data files, swapped while the game runs.

- [7. Assets & swapping models / materials](scripting/assets.md#7-assets-swapping-models-materials)

### [Animation, particles & sound](scripting/visuals.md)

The three things that make a scene feel alive, driven from a script.

- [9. Animation: `node:animator()`](scripting/visuals.md#9-animation-nodeanimator)
- [10. Particles: `node:particles()`](scripting/visuals.md#10-particles-nodeparticles)
- [11. Audio: `audio.play`, `node:sound()` & the mixer](scripting/visuals.md#11-audio-audioplay-nodesound-the-mixer)

### [Maths & the world](scripting/world.md)

Vectors, terrain, water, scattered props, and orbital time.

- [19. Vectors & math: `vec3`, `vec2`, `distance`](scripting/world.md#19-vectors-math-vec3-vec2-distance)
- [22. Terrain: `terrain.sculpt`, `dig` & queries](scripting/world.md#22-terrain-terrainsculpt-dig-queries)
- [22a. Water: volumes, buoyancy & `water.*`](scripting/world.md#22a-water-volumes-buoyancy-water)
- [22b. Scatter: thousands of props from a seed](scripting/world.md#22b-scatter-thousands-of-props-from-a-seed)
- [25. Space: orbits, gravity & time-warp](scripting/world.md#25-space-orbits-gravity-time-warp)

### [Multiplayer](scripting/networking.md)

Replication, remote calls, rollback, and voice.

- [16. Networking: `net.*`, `synced`, `onRpc`](scripting/networking.md#16-networking-net-synced-onrpc)
- [16b. Rollback netcode: `snapshot`, `restore` & `net.random`](scripting/networking.md#16b-rollback-netcode-snapshot-restore-netrandom)
- [16c. Voice: proximity chat](scripting/networking.md#16c-voice-proximity-chat)

### [Data, time & the outside world](scripting/data.md)

Saved games, timers, HTTP, the player's account, and the settings a game offers.

- [23. Saving: `save.set`, `save.get` & slots](scripting/data.md#23-saving-saveset-saveget-slots)
- [24. Timers: `after`, `every` & `tween`](scripting/data.md#24-timers-after-every-tween)
- [26. The web: `http.*` & `json.*`](scripting/data.md#26-the-web-http-json)
- [27. The player's account: `account.*`](scripting/data.md#27-the-players-account-account)
- [30. Settings a game offers its player: `app.*`](scripting/data.md#30-settings-a-game-offers-its-player-app)

### [Steam](scripting/steam.md)

Leaderboards, lobbies, and the overlay.

- [28a. Steam leaderboards: `steam.*`](scripting/steam.md#28a-steam-leaderboards-steam)
- [28b. Steam lobbies: finding other players](scripting/steam.md#28b-steam-lobbies-finding-other-players)
- [28c. The Steam overlay](scripting/steam.md#28c-the-steam-overlay)

### [Working in the editor](scripting/workflow.md)

The in-engine IDE, the profiler, the gotchas, and why some calls refuse.

- [14. The in-engine IDE](scripting/workflow.md#14-the-in-engine-ide)
- [15. Tips & gotchas](scripting/workflow.md#15-tips-gotchas)
- [28. Where the frame went: `perf.*`](scripting/workflow.md#28-where-the-frame-went-perf)
- [29. Options that refuse — and why an error is the kind answer](scripting/workflow.md#29-options-that-refuse-and-why-an-error-is-the-kind-answer)
